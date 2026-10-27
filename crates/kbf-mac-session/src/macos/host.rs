//! The macOS host: user records through OpenDirectory's local node, launchd through
//! `launchctl`, processes through `libproc` and `kill`.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_foundation::{NSError, NSString};
use objc2_open_directory::{ODNode, ODRecord, ODSession, kODNodeTypeLocalNodes};
use security_framework::random::SecRandom;

use crate::autologin::{self, CommandOutput};
use crate::helper::{self, Account, Host, LiveProcess, NewUser, Process};

const USERS: &str = "dsRecTypeStandard:Users";
const GROUPS: &str = "dsRecTypeStandard:Groups";
/// The group whose members are administrators.
const ADMIN_GROUP: &str = "admin";
/// A new lease user's login shell.
const SHELL: &str = "/bin/zsh";

/// `libproc`'s filters (`sys/proc_info.h`): processes by effective and by real uid.
const PROC_UID_ONLY: u32 = 4;
const PROC_RUID_ONLY: u32 = 5;
/// `pbi_status` of a process that has exited and waits to be reaped.
const SZOMB: u32 = 5;

/// The macOS host.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacHost;

impl Host for MacHost {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn uid_taken(&self, uid: u32) -> io::Result<bool> {
        // SAFETY: a zeroed passwd is a valid value for getpwuid_r to overwrite.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buffer = vec![0 as libc::c_char; 1 << 16];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is to a live, writable value of the right type, and
        // `buffer.len()` is the buffer's size.
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                &raw mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut found,
            )
        };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        Ok(!found.is_null())
    }

    fn account(&self, name: &str) -> io::Result<Option<Account>> {
        let c_name = std::ffi::CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the name holds NUL"))?;
        // SAFETY: a zeroed passwd is a valid value for getpwnam_r to overwrite.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buffer = vec![0 as libc::c_char; 1 << 16];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is to a live, writable value of the right type, the name
        // is NUL-terminated, and `buffer.len()` is the buffer's size.
        let code = unsafe {
            libc::getpwnam_r(
                c_name.as_ptr(),
                &raw mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut found,
            )
        };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        Ok((!found.is_null()).then_some(Account {
            uid: entry.pw_uid,
            gid: entry.pw_gid,
        }))
    }

    fn make_home(&self, homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()> {
        helper::make_home(homes, name, uid, gid)
    }

    fn create_user(&self, user: &NewUser) -> io::Result<()> {
        autoreleasepool(|_| {
            let node = local_node()?;
            let mut error: Option<Retained<NSError>> = None;
            // SAFETY: the record type and name are NSStrings; no attributes yet.
            let record = unsafe {
                node.createRecordWithRecordType_name_attributes_error(
                    Some(&NSString::from_str(USERS)),
                    Some(&NSString::from_str(&user.name)),
                    None,
                    Some(&mut error),
                )
            }
            .ok_or_else(|| od_error("creating the record", error))?;
            let home = user.home.to_string_lossy();
            for (attribute, value) in [
                ("dsAttrTypeStandard:UniqueID", user.uid.to_string()),
                ("dsAttrTypeStandard:PrimaryGroupID", user.gid.to_string()),
                ("dsAttrTypeStandard:NFSHomeDirectory", home.into_owned()),
                ("dsAttrTypeStandard:UserShell", SHELL.to_owned()),
                ("dsAttrTypeStandard:RealName", user.name.clone()),
                // Kept off the login window's list of users.
                ("dsAttrTypeNative:IsHidden", "1".to_owned()),
            ] {
                set(&record, attribute, &value)?;
            }
            // A random password nobody is told: the account cannot be logged into with
            // a password, and macOS gives it no secure token.
            random_password(&record)?;
            if user.admin {
                let admin = group(&node, ADMIN_GROUP)?;
                let mut error = None;
                // SAFETY: both are records of the local node.
                if !unsafe { admin.addMemberRecord_error(Some(&record), Some(&mut error)) } {
                    return Err(od_error("adding to the admin group", error));
                }
            }
            Ok(())
        })
    }

    fn reset_password(&self, name: &str) -> io::Result<String> {
        autoreleasepool(|_| {
            let node = local_node()?;
            let mut error = None;
            // SAFETY: the record type and name are NSStrings; no attributes needed.
            let record = unsafe {
                node.recordWithRecordType_name_attributes_error(
                    Some(&NSString::from_str(USERS)),
                    Some(&NSString::from_str(name)),
                    None,
                    Some(&mut error),
                )
            }
            .ok_or_else(|| od_error("looking the user up", error))?;
            random_password(&record)
        })
    }

    fn command(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        autologin::run_command(program, args)
    }

    fn delete_user(&self, name: &str) -> io::Result<bool> {
        autoreleasepool(|_| {
            let node = local_node()?;
            let mut error = None;
            // SAFETY: the record type and name are NSStrings; no attributes needed.
            let record = unsafe {
                node.recordWithRecordType_name_attributes_error(
                    Some(&NSString::from_str(USERS)),
                    Some(&NSString::from_str(name)),
                    None,
                    Some(&mut error),
                )
            };
            let Some(record) = record else {
                return match error {
                    None => Ok(false),
                    error => Err(od_error("looking the user up", error)),
                };
            };
            // A stale membership would make the next holder of the name an
            // administrator; names are never reused, but the group is cleaned anyway.
            let admin = group(&node, ADMIN_GROUP)?;
            let mut ignored = None;
            // SAFETY: both are records of the local node. Not a member is an error
            // that changes nothing.
            unsafe { admin.removeMemberRecord_error(Some(&record), Some(&mut ignored)) };
            let mut error = None;
            // SAFETY: a record of the local node.
            if !unsafe { record.deleteRecordAndReturnError(Some(&mut error)) } {
                return Err(od_error("deleting the record", error));
            }
            Ok(true)
        })
    }

    fn domain_exists(&self, domain: &str) -> io::Result<bool> {
        let status = Command::new("/bin/launchctl")
            .args(["print", domain])
            .env_clear()
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        Ok(status.success())
    }

    fn bootout(&self, domain: &str) -> io::Result<()> {
        let output = Command::new("/bin/launchctl")
            .args(["bootout", domain])
            .env_clear()
            .output()?;
        if output.status.success() {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    fn kill_all(&self, uid: u32) -> io::Result<()> {
        // SAFETY: the child calls only async-signal-safe functions (setuid, kill,
        // _exit) before it exits, as a fork of a threaded process must.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // As the uid, kill(-1) signals every process the uid may signal (those
            // whose real or saved uid it is), in one call the uid's processes cannot
            // outrun by forking. On macOS the sender is one of them, so this child
            // usually ends here by SIGKILL; elsewhere the loop ends with ESRCH.
            // SAFETY: async-signal-safe calls only; `_exit` never returns.
            unsafe {
                if libc::setuid(uid) != 0 {
                    libc::_exit(1);
                }
                while libc::kill(-1, libc::SIGKILL) == 0 {}
                libc::_exit(0);
            }
        }
        let mut status = 0;
        // SAFETY: `pid` is this process's child; `status` is writable.
        if unsafe { libc::waitpid(pid, &raw mut status, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // macOS's kill(-1) signals the sender too (measured on the CI runner), so the
        // killer ending by SIGKILL is success: the signals to every other process of
        // the uid were posted in the same call. An exit of 1 is a failed setuid.
        let killed_itself = libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL;
        let exited_clean = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
        if !killed_itself && !exited_clean {
            return Err(io::Error::other(format!(
                "the killer for uid {uid} failed to drop to it ({status:#x})"
            )));
        }
        Ok(())
    }

    fn live_processes(&self, uid: u32) -> io::Result<Vec<LiveProcess>> {
        let mut pids = BTreeSet::new();
        for filter in [PROC_UID_ONLY, PROC_RUID_ONLY] {
            pids.extend(list_pids(filter, uid)?);
        }
        Ok(pids.into_iter().filter_map(live).collect())
    }

    fn processes(&self) -> io::Result<Vec<Process>> {
        // Room for more pids than the kernel reports, so a few new ones still fit.
        // SAFETY: a null buffer asks only for the count.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pids = vec![0 as libc::pid_t; count.unsigned_abs() as usize + 64];
        let bytes =
            i32::try_from(std::mem::size_of_val(pids.as_slice())).map_err(io::Error::other)?;
        // SAFETY: the buffer holds `bytes` bytes of pids and outlives the call.
        let wrote = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if wrote < 0 {
            return Err(io::Error::last_os_error());
        }
        pids.truncate(wrote.unsigned_abs() as usize);
        Ok(pids.into_iter().filter_map(process).collect())
    }

    fn kill_group(&self, pgid: i32) -> io::Result<()> {
        // SAFETY: killpg(2) takes plain integers; the caller refused a group of 1 or less.
        if unsafe { libc::killpg(pgid, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // A group with no process left is what the caller wants.
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn pause(&self) {
        std::thread::sleep(Duration::from_millis(100));
    }

    fn clone_file(
        &self,
        src: std::os::fd::BorrowedFd<'_>,
        dir: std::os::fd::BorrowedFd<'_>,
        name: &str,
    ) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        /// `CLONE_NOOWNCOPY` (`sys/clonefile.h`): the clone is owned by the caller, not the
        /// source's owner.
        const CLONE_NOOWNCOPY: u32 = 0x0002;
        let c_name = std::ffi::CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the name holds NUL"))?;
        // SAFETY: both descriptors are open for the call and the name is NUL-terminated.
        let code = unsafe {
            libc::fclonefileat(
                src.as_raw_fd(),
                dir.as_raw_fd(),
                c_name.as_ptr(),
                CLONE_NOOWNCOPY,
            )
        };
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn clear_user_flags(&self, file: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        // SAFETY: the descriptor is open for the call.
        if unsafe { libc::fchflags(file.as_raw_fd(), 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn boot_session(&self) -> io::Result<String> {
        let mut buffer = [0u8; 128];
        let mut len = buffer.len();
        // SAFETY: the name is a NUL-terminated string, `buffer` is writable for `len`
        // bytes and `len` is a writable size_t the call sets to what it wrote; nothing
        // is written to the sysctl (null new value, length 0).
        let code = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buffer.as_mut_ptr().cast(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if code != 0 {
            return Err(io::Error::last_os_error());
        }
        let value = buffer.get(..len).unwrap_or_default();
        let value = value.strip_suffix(&[0]).unwrap_or(value);
        match std::str::from_utf8(value) {
            Ok(text) if !text.is_empty() => Ok(text.to_owned()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "kern.bootsessionuuid is not a non-empty string",
            )),
        }
    }
}

/// Sets a fresh password of 32 random bytes, in hex, on `record`, and returns it.
fn random_password(record: &ODRecord) -> io::Result<String> {
    let mut bytes = [0u8; 32];
    SecRandom::default().copy_bytes(&mut bytes)?;
    let password = hex::encode(bytes);
    let mut error = None;
    // SAFETY: a record of the local node and an NSString; no old password, as root on
    // the local node.
    if !unsafe {
        record.changePassword_toPassword_error(
            None,
            Some(&NSString::from_str(&password)),
            Some(&mut error),
        )
    } {
        return Err(od_error("setting the password", error));
    }
    Ok(password)
}

/// The local directory node (`/Local/Default`).
fn local_node() -> io::Result<Retained<ODNode>> {
    // SAFETY: plain constructors; the session may be the default (nil).
    let session = unsafe { ODSession::defaultSession() };
    let mut error = None;
    // SAFETY: as above.
    unsafe {
        ODNode::nodeWithSession_type_error(
            session.as_deref(),
            kODNodeTypeLocalNodes,
            Some(&mut error),
        )
    }
    .ok_or_else(|| od_error("opening the local node", error))
}

/// The group record `name` of `node`.
fn group(node: &ODNode, name: &str) -> io::Result<Retained<ODRecord>> {
    let mut error = None;
    // SAFETY: the record type and name are NSStrings; no attributes needed.
    unsafe {
        node.recordWithRecordType_name_attributes_error(
            Some(&NSString::from_str(GROUPS)),
            Some(&NSString::from_str(name)),
            None,
            Some(&mut error),
        )
    }
    .ok_or_else(|| od_error(&format!("looking up group {name}"), error))
}

/// Sets the single value of `attribute` on `record`.
fn set(record: &ODRecord, attribute: &str, value: &str) -> io::Result<()> {
    let value = NSString::from_str(value);
    let value: &AnyObject = &value;
    let mut error = None;
    // SAFETY: the value is an NSString and the attribute name an NSString.
    if unsafe {
        record.setValue_forAttribute_error(
            Some(value),
            Some(&NSString::from_str(attribute)),
            Some(&mut error),
        )
    } {
        return Ok(());
    }
    Err(od_error(&format!("setting {attribute}"), error))
}

fn od_error(what: &str, error: Option<Retained<NSError>>) -> io::Error {
    let why = error.map_or_else(
        || "no error given".to_owned(),
        |error| error.localizedDescription().to_string(),
    );
    io::Error::other(format!("OpenDirectory, {what}: {why}"))
}

/// The pids `libproc` lists for `filter` (by effective or real uid) and `uid`.
fn list_pids(filter: u32, uid: u32) -> io::Result<Vec<libc::pid_t>> {
    // SAFETY: a null buffer asks only for the size needed.
    let bytes = unsafe { libc::proc_listpids(filter, uid, std::ptr::null_mut(), 0) };
    if bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    // Room for some more pids than the kernel reported, should some start meanwhile.
    let size = std::mem::size_of::<libc::pid_t>();
    let mut pids = vec![0 as libc::pid_t; bytes.unsigned_abs() as usize / size + 64];
    let room = i32::try_from(pids.len() * size).map_err(io::Error::other)?;
    // SAFETY: the buffer holds `room` bytes of pids and outlives the call.
    let wrote = unsafe { libc::proc_listpids(filter, uid, pids.as_mut_ptr().cast(), room) };
    if wrote < 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(wrote.unsigned_abs() as usize / size);
    pids.retain(|&pid| pid > 0);
    Ok(pids)
}

/// `pid` with its command name, unless it has exited: it waits to be reaped, or is
/// gone already.
fn live(pid: libc::pid_t) -> Option<LiveProcess> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
        .expect("proc_bsdinfo is a few hundred bytes");
    // SAFETY: `info` is a writable proc_bsdinfo of `size` bytes.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    let error = (wrote <= 0).then(io::Error::last_os_error);
    // SAFETY: read only when the call filled the whole struct.
    let info = (wrote == size).then(|| unsafe { info.assume_init() });
    if exited_from(info.map(|info| info.pbi_status), error.as_ref()) {
        return None;
    }
    let command = info.map_or_else(|| "?".to_owned(), |info| c_name(&info.pbi_comm));
    Some(LiveProcess { pid, command })
}

/// `pid` as the process table shows it; `None` once it is gone or its info is short.
fn process(pid: libc::pid_t) -> Option<Process> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a writable proc_bsdinfo of `size` bytes.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if wrote != size {
        return None;
    }
    // SAFETY: the call filled the whole struct (checked above).
    let info = unsafe { info.assume_init() };
    Some(Process {
        pid,
        pgid: i32::try_from(info.pbi_pgid).ok()?,
        uid: info.pbi_uid,
        ruid: info.pbi_ruid,
        start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        zombie: info.pbi_status == SZOMB,
    })
}

/// A NUL-terminated name of at most `raw.len()` bytes, as text.
fn c_name(raw: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| u8::from_ne_bytes(c.to_ne_bytes()))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// What `proc_pidinfo` said, as [`live`] decides it: a full answer is a zombie or a
/// live process by its status; otherwise only "no such process" (`ESRCH`) is gone.
/// Any other failure, or a short answer, counts the process as live, so that
/// `user-delete` refuses rather than under-counts.
fn exited_from(status: Option<u32>, error: Option<&io::Error>) -> bool {
    match status {
        Some(status) => status == SZOMB,
        None => error.and_then(io::Error::raw_os_error) == Some(libc::ESRCH),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: `user-delete` going ahead while a process it could not read lives
    /// (any failure of `proc_pidinfo` taken for "gone", which under-counts).
    #[test]
    fn only_a_zombie_or_a_vanished_process_has_exited() {
        let esrch = io::Error::from_raw_os_error(libc::ESRCH);
        let eperm = io::Error::from_raw_os_error(libc::EPERM);
        assert!(exited_from(Some(SZOMB), None));
        assert!(!exited_from(Some(2), None));
        assert!(exited_from(None, Some(&esrch)));
        assert!(!exited_from(None, Some(&eperm)));
        assert!(!exited_from(None, None), "a short answer is not an exit");
    }

    /// The same against the kernel: this process is live, an exited child not yet
    /// reaped is a zombie, and a reaped one is gone. A live one is named by its
    /// command (catches: the name read from the wrong field, or not cut at its NUL).
    #[test]
    fn the_kernel_agrees() {
        let me = libc::pid_t::try_from(std::process::id()).unwrap();
        assert_eq!(live(me).map(|p| p.pid), Some(me));
        let mut sleeper = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = libc::pid_t::try_from(sleeper.id()).unwrap();
        // Polled: the name is the parent's until the child has executed sleep.
        let mut named = live(pid);
        for _ in 0..100 {
            if named.as_ref().is_some_and(|p| p.command == "sleep") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            named = live(pid);
        }
        sleeper.kill().unwrap();
        sleeper.wait().unwrap();
        assert_eq!(
            named,
            Some(LiveProcess {
                pid,
                command: "sleep".to_owned()
            })
        );
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        let mut zombie = false;
        for _ in 0..100 {
            zombie = live(pid).is_none();
            if zombie {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(zombie, "the exited child was never a zombie");
        child.wait().unwrap();
        assert_eq!(live(pid), None);
    }

    /// Catches: a name that runs past its NUL, or a full buffer with no NUL misread.
    #[test]
    fn a_c_name_stops_at_its_nul() {
        let raw: Vec<libc::c_char> = b"sleep\0junk"
            .iter()
            .map(|&b| libc::c_char::from_ne_bytes([b]))
            .collect();
        assert_eq!(c_name(&raw), "sleep");
        let full: Vec<libc::c_char> = b"abcd"
            .iter()
            .map(|&b| libc::c_char::from_ne_bytes([b]))
            .collect();
        assert_eq!(c_name(&full), "abcd");
    }
}
