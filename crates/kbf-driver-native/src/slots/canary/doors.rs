//! The doors the profile canaries try beyond a plain file write: a Mach service name
//! checked in with launchd, an application opened through Launch Services, a disk image
//! attached on a mount point of the lease, and a launchd agent bootstrapped from a file
//! of the lease. Each is a call (or a system tool) that exists on a Mac alone; what it
//! says is read back as a [`Seen`] by a function that does not need a Mac, so the
//! reading is tested on Linux, and a [`Refusal`] is accepted only in the one form the
//! door gives ([`Op::refusals`]).

use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use super::{Fs, Op, Seen};

/// `BOOTSTRAP_NOT_PRIVILEGED`: launchd (or the sandbox in its place) refused the
/// name.
pub const BOOTSTRAP_NOT_PRIVILEGED: i32 = 1100;
/// The error `launchctl bootstrap` prints for a job launchd will not take from a
/// sandboxed process: `Bootstrap failed: 5: Input/output error`.
pub const BOOTSTRAP_IO_ERROR: i32 = 5;
/// `permErr` (-54): what Launch Services answers when the sandbox refuses it.
pub const PERM_ERR: i32 = -54;

/// How a door says no.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A system call failed with this errno.
    Errno(i32),
    /// A Mach call answered this `kern_return_t`.
    Kern(i32),
    /// `launchctl bootstrap` failed with this error.
    Bootstrap(i32),
    /// Launch Services answered this `OSStatus`.
    OsStatus(i32),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Errno(n) => write!(f, "errno={n}"),
            Self::Kern(n) => write!(f, "kern={n}"),
            Self::Bootstrap(n) => write!(f, "bootstrap={n}"),
            Self::OsStatus(n) => write!(f, "osstatus={n}"),
        }
    }
}

impl Refusal {
    /// The refusal `text` spells, exactly as [`Display`](fmt::Display) writes it.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (kind, n) = text.split_once('=')?;
        let n = n.parse::<i32>().ok()?;
        match kind {
            "errno" => Some(Self::Errno(n)),
            "kern" => Some(Self::Kern(n)),
            "bootstrap" => Some(Self::Bootstrap(n)),
            "osstatus" => Some(Self::OsStatus(n)),
            _ => None,
        }
    }
}

/// What a tool the probe ran gave back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Out {
    /// The exit code; `None` if a signal ended it.
    pub code: Option<i32>,
    pub stderr: String,
}

/// Runs `program` with `args`, no environment and no input, and waits.
///
/// # Errors
/// The program could not be started.
pub fn run(program: &str, args: &[String]) -> io::Result<Out> {
    let out = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    Ok(Out {
        code: out.status.code(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// `bootstrap_check_in` of `name` with launchd: the `kern_return_t` it answers.
///
/// # Errors
/// Never on a Mac; elsewhere there is no such call.
#[cfg(target_os = "macos")]
pub fn check_in(name: &str) -> io::Result<i32> {
    use std::ffi::{CString, c_char};

    unsafe extern "C" {
        static bootstrap_port: u32;
        fn bootstrap_check_in(bootstrap: u32, name: *const c_char, service: *mut u32) -> i32;
    }
    let name = CString::new(name).map_err(io::Error::other)?;
    let mut port = 0_u32;
    // SAFETY: `name` is a NUL-terminated string that outlives the call, `port` is a
    // valid place for the receive right it writes (the process exit drops it), and
    // `bootstrap_port` is libSystem's own, set before `main`.
    Ok(unsafe { bootstrap_check_in(bootstrap_port, name.as_ptr(), &raw mut port) })
}

/// `bootstrap_check_in` of `name` with launchd.
///
/// # Errors
/// Always: there is no such call off a Mac.
#[cfg(not(target_os = "macos"))]
pub fn check_in(_name: &str) -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a Mach service can be checked in on macOS only",
    ))
}

/// `refusal` as a refusal of `op` if that is how `op`'s door says no; otherwise a
/// failure that names it, which proves nothing about the door.
fn refused(op: Op, refusal: Refusal) -> Seen {
    if op.refusals().contains(&refusal) {
        Seen::Refused(refusal)
    } else {
        Seen::Failed(format!("not a refusal of this door: {refusal}"))
    }
}

/// The first line of what a tool said, for a failure.
fn said(out: &Out) -> String {
    let line = out.stderr.lines().find(|l| !l.trim().is_empty());
    format!("exit {:?}: {}", out.code, line.unwrap_or("").trim())
}

/// The integer written right after `marker` in `text`.
fn number_after(text: &str, marker: &str) -> Option<i32> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let end = rest
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && *c == '-')))
        .map_or(rest.len(), |(i, _)| i);
    rest[..end].parse().ok()
}

/// Checks in a Mach service name (unique to this process) with launchd.
pub fn mach_register(fs: &dyn Fs) -> Seen {
    let name = format!("com.kbf.canary.{}", std::process::id());
    match fs.check_in(&name) {
        Ok(0) => Seen::Allowed,
        Ok(n) => refused(Op::MachRegister, Refusal::Kern(n)),
        Err(e) => Seen::Failed(e.to_string()),
    }
}

/// What `open` said about `app`: opened, or answered with an `OSStatus`.
fn read_open(out: &Out) -> Seen {
    if out.code == Some(0) {
        return Seen::Allowed;
    }
    let status = number_after(&out.stderr, "Code=").or_else(|| number_after(&out.stderr, "error "));
    status.map_or_else(
        || Seen::Failed(said(out)),
        |n| refused(Op::LsOpen, Refusal::OsStatus(n)),
    )
}

/// Asks Launch Services, through `open`, to open `app` hidden and in the background.
pub fn ls_open(fs: &dyn Fs, app: &Path) -> Seen {
    let args = ["-g", "-j", "-n", &app.to_string_lossy()].map(str::to_owned);
    match fs.run("/usr/bin/open", &args) {
        Ok(out) => read_open(&out),
        Err(e) => Seen::Failed(format!("could not start open: {e}")),
    }
}

/// What `hdiutil attach` said: attached, or refused with `EPERM`.
fn read_attach(out: &Out) -> Seen {
    if out.code == Some(0) {
        Seen::Allowed
    } else if out.stderr.contains("Operation not permitted") {
        refused(Op::Mount, Refusal::Errno(libc::EPERM))
    } else {
        Seen::Failed(said(out))
    }
}

/// Makes a disk image at `image` and attaches it on the new directory `mount`; what an
/// open door attached is detached, and the image and the directory are removed.
pub fn mount(fs: &dyn Fs, image: &Path, mount: &Path) -> Seen {
    let seen = mount_image(fs, image, mount);
    if seen == Seen::Allowed {
        let _ = fs.run(
            "/usr/bin/hdiutil",
            &["detach".into(), "-force".into(), path(mount)],
        );
    }
    fs.remove(mount);
    fs.remove(image);
    seen
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn mount_image(fs: &dyn Fs, image: &Path, mount: &Path) -> Seen {
    let make = [
        "create", "-size", "2m", "-fs", "HFS+", "-volname", "kbf", "-quiet",
    ]
    .map(str::to_owned)
    .into_iter()
    .chain([path(image)])
    .collect::<Vec<_>>();
    match fs.run("/usr/bin/hdiutil", &make) {
        Ok(out) if out.code == Some(0) => {}
        Ok(out) => return Seen::Failed(format!("hdiutil create: {}", said(&out))),
        Err(e) => return Seen::Failed(format!("could not start hdiutil: {e}")),
    }
    if let Err(e) = fs.make_dir(mount) {
        return Seen::Failed(format!("mount point: {e}"));
    }
    let attach = [
        "attach",
        "-nobrowse",
        "-noverify",
        "-noautoopen",
        "-quiet",
        "-mountpoint",
    ]
    .map(str::to_owned)
    .into_iter()
    .chain([path(mount), path(image)])
    .collect::<Vec<_>>();
    match fs.run("/usr/bin/hdiutil", &attach) {
        Ok(out) => read_attach(&out),
        Err(e) => Seen::Failed(format!("could not start hdiutil: {e}")),
    }
}

/// The label of the job the bootstrap canary offers launchd.
fn job_label() -> String {
    format!("com.kbf.canary.{}", std::process::id())
}

/// The job file: a `Background` agent that launchd would keep alive.
fn job_plist(label: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
<key>Label</key><string>{label}</string>\n\
<key>ProgramArguments</key><array><string>/bin/sleep</string><string>300</string></array>\n\
<key>ProcessType</key><string>Background</string>\n\
<key>KeepAlive</key><true/>\n\
<key>RunAtLoad</key><true/>\n\
</dict>\n\
</plist>\n"
    )
}

/// What `launchctl bootstrap` said: loaded, or `Bootstrap failed: <n>`.
fn read_bootstrap(out: &Out) -> Seen {
    if out.code == Some(0) {
        return Seen::Allowed;
    }
    number_after(&out.stderr, "Bootstrap failed: ").map_or_else(
        || Seen::Failed(said(out)),
        |n| refused(Op::LaunchdBootstrap, Refusal::Bootstrap(n)),
    )
}

/// Writes the job file `job` and asks launchd to bootstrap it into the user's domain;
/// an agent an open door loaded is booted out, and the file is removed.
pub fn bootstrap(fs: &dyn Fs, job: &Path) -> Seen {
    let label = job_label();
    if let Err(e) = fs.write(job, &job_plist(&label)) {
        return Seen::Failed(format!("job file: {e}"));
    }
    let domain = format!("user/{}", fs.uid());
    let seen = match fs.run(
        "/bin/launchctl",
        &["bootstrap".into(), domain.clone(), path(job)],
    ) {
        Ok(out) => read_bootstrap(&out),
        Err(e) => Seen::Failed(format!("could not start launchctl: {e}")),
    };
    if seen == Seen::Allowed {
        let _ = fs.run(
            "/bin/launchctl",
            &["bootout".into(), format!("{domain}/{label}")],
        );
    }
    fs.remove(job);
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(code: i32, stderr: &str) -> Out {
        Out {
            code: Some(code),
            stderr: stderr.to_owned(),
        }
    }

    /// Catches a number read from a tool's text that takes another one (a digit run
    /// that continues, a sign in the middle), reads none as zero, or panics at the end
    /// of the text.
    #[test]
    fn a_number_is_read_after_its_marker_only() {
        assert_eq!(number_after("Code=-54 x", "Code="), Some(-54));
        assert_eq!(number_after("Code=-54", "Code="), Some(-54));
        assert_eq!(number_after("a Code=7, Code=8", "Code="), Some(7));
        assert_eq!(number_after("Code=", "Code="), None);
        assert_eq!(number_after("Code=x", "Code="), None);
        assert_eq!(number_after("Code=-", "Code="), None);
        assert_eq!(number_after("Code=5-4", "Code="), Some(5));
        assert_eq!(number_after("none", "Code="), None);
    }

    /// Catches an `open` that is refused in a form this reads as open (it exited 0 on a
    /// refusal), a refusal read from text that holds no status, and the two spellings
    /// of the status `open` prints.
    #[test]
    fn open_is_read_as_opened_or_refused_with_its_status() {
        assert_eq!(read_open(&out(0, "")), Seen::Allowed);
        let domain =
            "The application cannot be opened. Error Domain=NSOSStatusErrorDomain Code=-54";
        assert_eq!(
            read_open(&out(1, domain)),
            Seen::Refused(Refusal::OsStatus(-54))
        );
        let old = "_LSOpenURLsWithRole() failed with error -54.";
        assert_eq!(
            read_open(&out(1, old)),
            Seen::Refused(Refusal::OsStatus(-54))
        );
        assert!(matches!(read_open(&out(1, "no status")), Seen::Failed(_)));
        assert!(matches!(read_open(&out(1, "Code=-600")), Seen::Failed(_)));
        let killed = Out {
            code: None,
            stderr: String::new(),
        };
        assert!(matches!(read_open(&killed), Seen::Failed(_)));
    }

    /// Catches an attach or a bootstrap read as refused for the wrong text, and a failure
    /// report that loses what the tool said.
    #[test]
    fn attach_and_bootstrap_are_read_by_their_own_words() {
        assert_eq!(read_attach(&out(0, "")), Seen::Allowed);
        let no = "hdiutil: attach failed - Operation not permitted";
        assert_eq!(
            read_attach(&out(1, no)),
            Seen::Refused(Refusal::Errno(libc::EPERM))
        );
        let busy = read_attach(&out(1, "\nhdiutil: attach failed - Resource busy\n"));
        assert_eq!(
            busy,
            Seen::Failed("exit Some(1): hdiutil: attach failed - Resource busy".to_owned())
        );
        assert_eq!(read_bootstrap(&out(0, "")), Seen::Allowed);
        let five = "Bootstrap failed: 5: Input/output error";
        assert_eq!(
            read_bootstrap(&out(5, five)),
            Seen::Refused(Refusal::Bootstrap(5))
        );
        assert!(matches!(read_bootstrap(&out(1, "usage")), Seen::Failed(_)));
        assert!(matches!(
            read_bootstrap(&out(1, "Bootstrap failed: 113")),
            Seen::Failed(_)
        ));
    }
}
