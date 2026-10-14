//! `kbf-mac-session` for real, as root, on a macOS runner (docs/design/fleet-updates-security.md
//! S4.2, S4.3 and S10). The unit tests run the verbs' rules on every platform against a
//! fake host; these run the macOS host itself: OpenDirectory users, launchd, libproc,
//! the sweep, the leak scan, and the caller check by code signature.
//!
//! The helper is a copy of `CARGO_BIN_EXE_kbf-mac-session` signed by `cargo xtask
//! sign-darwin`'s own function (`xtask::darwin::sign`: ad hoc, hardened runtime, and its
//! checks). The caller is this test binary: copied, signed ad hoc with the hardened
//! runtime and named [`CLIENT`], it answers as the client in `as_root/client.rs`, and
//! the helper is started with that copy's cdhash as the daemon requirement, so it stands
//! in for `kbf-daemon`. A copy signed under another identifier (so another cdhash) and
//! named [`IMPOSTOR`] is the impostor. The client runs as the user sudo was run by, who
//! reaches the socket through the `admin` group. A third copy, named `kbf-act`, is the
//! program the tests' actions run: kbf-testkit's act steps (`kbf_testkit::act`), so no
//! action needs a shell.
//!
//! The leak scan runs for real over the runner's own folders, Background Task Management
//! and directory: a baseline through a baseline window the tests open, a clean scan, a
//! lease user it must report, and a clean scan once every lease user is gone.
//! `reboot-dirty` is only ever sent right after a scan that said `clean`, when it must
//! be refused: after a dirty one it would reboot the runner.
//!
//! One helper serves every test, started by the first test to run, over the state a
//! power cut mid-session leaves. What must come first (the boot reset before anything
//! is served, the baseline before any lease user exists) is done then and kept for the
//! tests that read it. Every other test makes its own leases (term = a number of its
//! own), ends them, and ends them also when it fails, so one red test does not fail the
//! others. Every lease user gets the next uid of the range: [`Mac::create`] checks it.
//!
//! Every test is `#[ignore]`: each needs root and changes this machine. CI runs them in
//! `native-macos` with `cargo xtask sudo-test -p kbf-mac-session --test as_root --
//! --test-threads=1 --nocapture`, which builds this file as the runner's user and runs
//! it through `sudo -n` (GitHub's hosted runners allow passwordless sudo). The tests
//! also take one lock, so they never overlap. The helper's log goes to this process's
//! stderr. The file builds on every platform, so clippy reads it on Linux; run anywhere
//! but macOS, each test stops at its first line.

#[cfg(target_os = "macos")]
#[path = "as_root/client.rs"]
mod client;

use std::fs;
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-mac-session");
/// The file name of the signed copy of this binary that answers as the client.
const CLIENT: &str = "kbf-ci-client";
/// The file name of the copy signed under another identifier.
const IMPOSTOR: &str = "kbf-ci-impostor";
/// The tests' own directory, where every user may search (the runner's home is not):
/// the copies, the grant keys, the login window's files, lease directories. Root's.
const SCRATCH: &str = "/private/tmp/kbf-mac-session-as-root";
const RUN_DIR: &str = "/private/var/run/kbf-ci-mac-session";
const SOCKET: &str = "/private/var/run/kbf-ci-mac-session/socket";
const STATE: &str = "/private/var/db/kbf-ci-mac-session";
/// The lease uids: free on the runner (checked), below the 690 the native driver's
/// root tests start at.
const FIRST_UID: u32 = 650;
const LAST_UID: u32 = 679;
const NEEDS_ROOT: &str =
    "needs root: run by `cargo xtask sudo-test -p kbf-mac-session --test as_root`";

// Runs this binary as the client when it is started through a copy named `CLIENT` or
// `IMPOSTOR`, before the test harness reads the arguments; otherwise returns, and the
// tests run.
#[cfg(target_os = "macos")]
const _: () = {
    extern "C" fn client_hook() {
        let argv0 = std::env::args_os().next().unwrap_or_default();
        let name = Path::new(&argv0).file_name();
        if name.is_some_and(|n| n == CLIENT || n == IMPOSTOR) {
            std::process::exit(client::main());
        }
    }
    #[used]
    #[unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs")]
    static CLIENT_HOOK: extern "C" fn() = client_hook;
};

// Runs this binary as kbf-testkit's `kbf-act` when it is started through the copy of
// that name (`Mac::act`): the program the tests' actions run.
kbf_testkit::act_test_hook!();

/// The helper's pid, for [`stop_helper`] at exit.
static HELPER_PID: AtomicI32 = AtomicI32::new(0);

/// Stops the helper when this process exits, passed or failed (libtest exits through
/// `exit`, which runs this), so no root helper outlives the tests.
extern "C" fn stop_helper() {
    let pid = HELPER_PID.swap(0, Ordering::AcqRel);
    if pid > 0 {
        // SAFETY: kill takes plain integers; a pid that is gone is an error ignored.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// Stops a run as the runner's user, where every check below would fail for the wrong
/// reason. Asks the kernel, not the code under test.
fn assert_root() {
    if cfg!(not(target_os = "macos")) {
        panic!("these tests run on macOS only");
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "not root: {NEEDS_ROOT}"
    );
}

/// What one program printed, and its exit status.
struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Out {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Stdout without its trailing newlines, as `$(...)` reads it.
    fn said(&self) -> &str {
        self.stdout.trim_end_matches('\n')
    }

    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Runs `program args` and prints what it printed.
fn run(program: &Path, args: &[&str], cmd: &mut Command) -> Out {
    let o = cmd
        .output()
        .unwrap_or_else(|e| panic!("{}: {e}", program.display()));
    let out = Out {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    };
    println!("$ {} {}", program.display(), args.join(" "));
    for line in out.stdout.lines().chain(out.stderr.lines()) {
        println!("  | {line}");
    }
    println!("  exit {:?}", out.code);
    out
}

/// Runs a program as root (this process).
fn root(program: &str, args: &[&str]) -> Out {
    run(Path::new(program), args, Command::new(program).args(args))
}

/// Runs a program as root without printing what it printed (a long listing).
fn quiet(program: &str, args: &[&str]) -> Out {
    let o = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    Out {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

/// The output of a program run as root that must succeed.
#[track_caller]
fn succeeds(program: &str, args: &[&str]) -> String {
    let out = root(program, args);
    assert!(out.ok(), "{program} {args:?}: {}", out.all());
    out.said().to_owned()
}

/// Asserts `out` succeeded and printed exactly `want`.
#[track_caller]
fn says(out: &Out, want: &str, what: &str) {
    assert!(out.ok(), "{what}: failed: {}", out.all());
    assert_eq!(out.said(), want, "{what}");
}

/// Asserts `out` failed, saying `pattern`.
#[track_caller]
fn refused(out: &Out, pattern: &str, what: &str) {
    assert!(!out.ok(), "{what}: expected a refusal, got: {}", out.all());
    assert!(
        out.all().contains(pattern),
        "{what}: refused, but not for {pattern:?}: {}",
        out.all()
    );
}

fn set_mode(path: &str, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {path}: {e}"));
}

/// Writes `text` to `path` as root, mode 0644 (writable by no one else).
fn write_root(path: &str, text: &str) {
    fs::write(path, text).unwrap_or_else(|e| panic!("write {path}: {e}"));
    set_mode(path, 0o644);
}

/// Whether anything is at `path`, a link included, never following one.
fn exists(path: &str) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn user_exists(name: &str) -> bool {
    root("/usr/bin/id", &[name]).ok()
}

fn is_admin(name: &str) -> bool {
    root(
        "/usr/sbin/dseditgroup",
        &["-o", "checkmember", "-m", name, "admin"],
    )
    .ok()
}

/// The pids `pgrep args` lists (none when it finds nothing).
fn pgrep(args: &[&str]) -> Vec<String> {
    root("/usr/bin/pgrep", args)
        .said()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The home folder of a user named `name`.
fn home(name: &str) -> String {
    Path::new("/Users").join(name).display().to_string()
}

/// A fresh directory in [`SCRATCH`], mode `mode`, by its real path.
fn scratch_dir(name: &str, mode: u32) -> String {
    let dir = format!("{SCRATCH}/{name}");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {dir}: {e}"));
    set_mode(&dir, mode);
    let real = fs::canonicalize(dir).expect("the directory's real path");
    real.display().to_string()
}

/// The user sudo was run by.
struct Runner {
    uid: u32,
    gid: u32,
    name: String,
}

impl Runner {
    fn from_sudo() -> Runner {
        let var = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| panic!("{name} unset: {NEEDS_ROOT}"))
        };
        let id = |name: &str| var(name).parse::<u32>().expect(name);
        Runner {
            uid: id("SUDO_UID"),
            gid: id("SUDO_GID"),
            name: var("SUDO_USER"),
        }
    }
}

/// Signs `binary` in place as `cargo xtask sign-darwin <binary>` does, and runs that
/// verb's checks on it.
#[cfg(target_os = "macos")]
fn sign_darwin(binary: &Path) {
    let allowlist = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(xtask::entitlements::ALLOWLIST);
    let minos = xtask::darwin::minos_of(binary).expect("the helper's minos");
    let lines = xtask::darwin::sign(binary, &allowlist, &minos, None)
        .unwrap_or_else(|e| panic!("sign-darwin {}: {e}", binary.display()));
    for line in lines {
        println!("  | {line}");
    }
}

#[cfg(not(target_os = "macos"))]
fn sign_darwin(_binary: &Path) {
    panic!("these tests run on macOS only");
}

/// The cdhash `codesign` reads from `binary`'s signature.
fn cdhash(binary: &Path) -> String {
    let path = binary.to_str().expect("a UTF-8 path");
    let out = root("/usr/bin/codesign", &["--display", "--verbose=4", path]);
    // codesign prints the description on stderr.
    let all = out.all();
    let hash = all.lines().find_map(|line| line.strip_prefix("CDHash="));
    hash.unwrap_or_else(|| panic!("no CDHash for {path}: {all}"))
        .to_owned()
}

/// A copy of this test binary at `SCRATCH/name`, mode 0755, signed ad hoc with the
/// hardened runtime, under `identifier` if one is given.
fn signed_copy(name: &str, identifier: Option<&str>) -> PathBuf {
    let path = Path::new(SCRATCH).join(name);
    fs::copy(std::env::current_exe().expect("this binary"), &path).expect("copy this binary");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod the copy");
    let mut args = vec!["--force", "--sign", "-", "--options", "runtime"];
    if let Some(identifier) = identifier {
        args.extend(["--identifier", identifier]);
    }
    args.push(path.to_str().expect("a UTF-8 path"));
    succeeds("/usr/bin/codesign", &args);
    path
}

/// What the tests that must come first saw, in this order, on the helper's first start:
/// the boot reset, then the baseline.
struct FirstContact {
    /// `kill-uid` before the boot reset.
    before_reset: Out,
    /// The boot reset, naming a held lease that has no user.
    reset: Out,
    /// `defaults read` of the login window's `autoLoginUser` after it.
    auto_login: Out,
    /// The intent record and kcpassword, where they survived the reset.
    survived: Vec<String>,
    reset_again: Out,
    /// `scan` with no baseline recorded.
    scan_unrecorded: Out,
    /// `baseline` with no window.
    baseline_closed: Out,
    /// This boot's `kern.bootsessionuuid`.
    boot: String,
    /// `baseline` with a window opened in this boot.
    baseline_this_boot: Out,
    /// `baseline` with a window opened in an earlier boot.
    baseline: Out,
    /// Background Task Management entries, `sfltool dumpbtm` read right after it.
    btm: usize,
    /// Entries in `/Library/LaunchAgents` and `/Library/LaunchDaemons` then.
    agents: usize,
    daemons: usize,
    /// The scan right after the baseline, and `reboot-dirty` after it if it said clean.
    scan: Out,
    reboot_dirty: Option<Out>,
}

/// The helper and its callers, set up once.
struct Mac {
    helper: PathBuf,
    client: PathBuf,
    impostor: PathBuf,
    /// The copy that runs kbf-testkit's act steps, which every user may run.
    act: PathBuf,
    /// The client copy's cdhash: the daemon requirement.
    genuine: String,
    runner: Runner,
    serial: String,
    process: Mutex<Option<Child>>,
    /// The uid the next lease user must get.
    next_uid: Mutex<u32>,
    first: FirstContact,
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("a UTF-8 path")
}

impl Mac {
    fn prepare() -> Mac {
        stop_helper();
        let _ = fs::remove_dir_all(SCRATCH);
        fs::create_dir_all(SCRATCH).expect("make the scratch directory");
        set_mode(SCRATCH, 0o755);
        let runner = Runner::from_sudo();

        println!("== the helper, signed by sign-darwin; the client and the impostor");
        let helper = Path::new(SCRATCH).join("kbf-mac-session");
        fs::copy(BIN, &helper).expect("copy the helper");
        sign_darwin(&helper);
        let client = signed_copy(CLIENT, None);
        let impostor = signed_copy(IMPOSTOR, Some("kbf.ci.impostor"));
        let act = signed_copy(kbf_testkit::act::NAME, None);
        let genuine = cdhash(&client);
        let other = cdhash(&impostor);
        println!("client cdhash {genuine}, impostor cdhash {other}");
        assert_ne!(genuine, other, "the impostor must have a cdhash of its own");

        // Free uids for the leases, and a group the runner is in that is neither staff
        // nor the lease users' group (the helper refuses those).
        let users = quiet("/usr/bin/dscl", &[".", "-list", "/Users", "UniqueID"]);
        assert!(users.ok(), "dscl: {}", users.all());
        let in_use: Vec<String> = users
            .stdout
            .lines()
            .filter(|line| {
                let uid = line.split_whitespace().nth(1);
                let uid = uid.and_then(|u| u.parse::<u32>().ok());
                uid.is_some_and(|uid| (FIRST_UID..=LAST_UID).contains(&uid))
            })
            .map(str::to_owned)
            .collect();
        assert!(
            in_use.is_empty(),
            "uids {FIRST_UID}-{LAST_UID} are in use: {in_use:?}"
        );
        let groups = succeeds("/usr/bin/id", &["-Gn", runner.name.as_str()]);
        assert!(
            groups.split_whitespace().any(|g| g == "admin"),
            "the runner is not in the admin group: {groups}"
        );
        let ioreg = quiet("/usr/sbin/ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"]);
        let serial = kbf_mac_session::start::parse_serial(&ioreg.stdout).expect("a serial number");
        println!("serial: {serial}");

        let _ = fs::remove_dir_all(RUN_DIR);
        let _ = fs::remove_dir_all(STATE);
        let mut mac = Mac {
            helper,
            client,
            impostor,
            act,
            genuine,
            runner,
            serial,
            process: Mutex::new(None),
            next_uid: Mutex::new(FIRST_UID),
            first: FirstContact::empty(),
        };
        let pubkey = mac.client(&["pubkey", "07"]);
        assert!(pubkey.ok(), "pubkey: {}", pubkey.all());
        write_root(&format!("{SCRATCH}/grant-keys"), &pubkey.stdout);

        println!("== a power cut mid-session, as the boot finds it");
        // Auto-login names a lease user, and an intent record names a lease the server
        // no longer holds.
        fs::create_dir_all(STATE).expect("make the state directory");
        set_mode(STATE, 0o700);
        write_root(&intent(), "9.9\n");
        succeeds(
            "/usr/bin/defaults",
            &[
                "write",
                &loginwindow(),
                "autoLoginUser",
                "-string",
                "kbf-lease-9-9",
            ],
        );
        write_root(&kcpassword(), "x\n");
        mac.start_helper();
        mac.first = mac.first_contact();
        mac
    }

    /// The helper's flags; every start of it takes the same.
    fn helper_args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "--socket",
            SOCKET,
            "--socket-group",
            "admin",
            "--state-dir",
            STATE,
        ]
        .map(str::to_owned)
        .to_vec();
        args.extend([
            "--uid-range".to_owned(),
            format!("{FIRST_UID}-{LAST_UID}"),
            "--daemon-requirement".to_owned(),
            format!("cdhash H\"{}\"", self.genuine),
            "--kcpassword".to_owned(),
            kcpassword(),
            "--loginwindow".to_owned(),
            loginwindow(),
            "--grant-keys".to_owned(),
            format!("{SCRATCH}/grant-keys"),
        ]);
        args
    }

    /// Starts the helper (its log on this process's stderr) and waits for its socket.
    fn start_helper(&self) {
        let child = Command::new(&self.helper)
            .args(self.helper_args())
            .stdin(Stdio::null())
            .spawn()
            .expect("start the helper");
        HELPER_PID.store(i32::try_from(child.id()).expect("a pid"), Ordering::Release);
        // SAFETY: registers a function that takes nothing; registering it again only
        // makes a second kill of a pid already cleared.
        unsafe { libc::atexit(stop_helper) };
        *self.process.lock().unwrap_or_else(PoisonError::into_inner) = Some(child);
        let give_up = Instant::now() + Duration::from_secs(10);
        while !fs::symlink_metadata(SOCKET).is_ok_and(|m| m.file_type().is_socket()) {
            assert!(Instant::now() < give_up, "the helper did not bind {SOCKET}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Stops the helper, starts it again over the same state, and runs its boot reset
    /// naming `held`: the reset's answer.
    fn restart(&self, held: &[&str]) -> String {
        if let Some(mut child) = self
            .process
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        let child = Command::new(&self.helper)
            .args(self.helper_args())
            .stdin(Stdio::null())
            .spawn()
            .expect("restart the helper");
        HELPER_PID.store(i32::try_from(child.id()).expect("a pid"), Ordering::Release);
        *self.process.lock().unwrap_or_else(PoisonError::into_inner) = Some(child);
        let mut args = vec!["reset", SOCKET];
        args.extend(held);
        let give_up = Instant::now() + Duration::from_secs(10);
        loop {
            let out = self.client(&args);
            if out.ok() {
                return out.said().to_owned();
            }
            assert!(
                Instant::now() < give_up,
                "the restarted helper did not finish its boot reset: {}",
                out.all()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Runs `program args` as the runner (`sudo -u`, so with the runner's groups).
    fn as_runner(&self, program: &Path, args: &[&str]) -> Out {
        let mut cmd = Command::new("/usr/bin/sudo");
        cmd.args(["-n", "-u", self.runner.name.as_str()])
            .arg(program)
            .args(args);
        run(program, args, &mut cmd)
    }

    fn client(&self, args: &[&str]) -> Out {
        self.as_runner(&self.client, args)
    }

    fn impostor(&self, args: &[&str]) -> Out {
        self.as_runner(&self.impostor, args)
    }

    /// The argument vector of a process that runs the act `steps` (`kbf_testkit::act`).
    fn act<'a>(&'a self, steps: &[&'a str]) -> Vec<&'a str> {
        let mut argv = vec![path_str(&self.act)];
        argv.extend(steps);
        argv
    }

    /// `user-create` for `lease` (with a gate grant), which must give the next uid.
    #[track_caller]
    fn create(&self, lease: &str, grant: Option<&str>) -> Lease<'_> {
        let mut next = self.next_uid.lock().unwrap_or_else(PoisonError::into_inner);
        let mut args = vec!["create", SOCKET, lease];
        args.extend(grant);
        let out = self.client(&args);
        assert!(out.ok(), "user-create {lease}: {}", out.all());
        let uid: u32 = out.said().parse().expect("user-create prints the uid");
        let made = Lease {
            mac: self,
            id: lease.to_owned(),
            uid,
        };
        let want = *next;
        *next = want.max(uid + 1);
        assert_eq!(
            uid, want,
            "lease {lease}: uids go in order from {FIRST_UID}, a deleted one's never again"
        );
        made
    }

    fn first_contact(&self) -> FirstContact {
        println!("== first contact: the boot reset");
        let before_reset = self.client(&["kill", SOCKET, "9.1"]);
        let reset = self.client(&["reset", SOCKET, "9.7"]);
        let auto_login = root(
            "/usr/bin/defaults",
            &["read", &loginwindow(), "autoLoginUser"],
        );
        let survived = [kcpassword(), intent()]
            .into_iter()
            .filter(|path| exists(path))
            .collect();
        let reset_again = self.client(&["reset", SOCKET]);

        println!("== first contact: the baseline, only in its window");
        let scan_unrecorded = self.client(&["scan", SOCKET]);
        let baseline_closed = self.client(&["baseline", SOCKET]);
        let boot = succeeds("/usr/sbin/sysctl", &["-n", "kern.bootsessionuuid"]);
        assert!(!boot.is_empty(), "no kern.bootsessionuuid");
        write_root(&window(), &format!("{boot}\n"));
        let baseline_this_boot = self.client(&["baseline", SOCKET]);
        // As an apply in an earlier boot leaves it.
        write_root(&window(), "ci-earlier-boot\n");
        let baseline = self.client(&["baseline", SOCKET]);
        // Each count read independently, right away: the parser and the folder walk
        // must read what is there.
        let dump = quiet("/usr/bin/sfltool", &["dumpbtm"]);
        assert!(dump.ok(), "sfltool dumpbtm: {}", dump.stderr);
        let btm = dump.stdout.lines().filter(|l| is_btm_entry(l)).count();
        let entries = |dir: &str| fs::read_dir(dir).map_or(0, Iterator::count);
        let agents = entries("/Library/LaunchAgents");
        let daemons = entries("/Library/LaunchDaemons");
        let scan = self.client(&["scan", SOCKET]);
        // Never after a dirty scan: it would reboot the runner.
        let reboot_dirty =
            (scan.ok() && scan.said() == "clean").then(|| self.client(&["reboot-dirty", SOCKET]));
        FirstContact {
            before_reset,
            reset,
            auto_login,
            survived,
            reset_again,
            scan_unrecorded,
            baseline_closed,
            boot,
            baseline_this_boot,
            baseline,
            btm,
            agents,
            daemons,
            scan,
            reboot_dirty,
        }
    }
}

impl FirstContact {
    fn empty() -> FirstContact {
        let none = || Out {
            code: None,
            stdout: String::new(),
            stderr: String::new(),
        };
        FirstContact {
            before_reset: none(),
            reset: none(),
            auto_login: none(),
            survived: Vec::new(),
            reset_again: none(),
            scan_unrecorded: none(),
            baseline_closed: none(),
            boot: String::new(),
            baseline_this_boot: none(),
            baseline: none(),
            btm: 0,
            agents: 0,
            daemons: 0,
            scan: none(),
            reboot_dirty: None,
        }
    }
}

/// An entry header of `sfltool dumpbtm`: `#<n>:` alone on its line.
fn is_btm_entry(line: &str) -> bool {
    line.trim()
        .strip_prefix('#')
        .and_then(|rest| rest.strip_suffix(':'))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn intent() -> String {
    format!("{STATE}/session-intent")
}

fn window() -> String {
    format!("{STATE}/baseline-window")
}

fn kcpassword() -> String {
    format!("{SCRATCH}/kcpassword")
}

/// The session switch's files are the tests' own, never the runner's login window: the
/// boot reset clears auto-login, and nothing here may restart the runner.
fn loginwindow() -> String {
    format!("{SCRATCH}/loginwindow")
}

/// A lease user a test made. When the test fails, the user is killed and deleted.
struct Lease<'a> {
    mac: &'a Mac,
    id: String,
    uid: u32,
}

impl Lease<'_> {
    fn name(&self) -> String {
        format!("kbf-lease-{}", self.id.replace('.', "-"))
    }

    fn uid(&self) -> String {
        self.uid.to_string()
    }

    fn call(&self, verb: &str) -> Out {
        self.mac.client(&[verb, SOCKET, self.id.as_str()])
    }

    /// `run` in the background, in `dir`.
    fn run(&self, dir: &str, argv: &[&str]) -> Out {
        let mut args = vec!["run", SOCKET, self.id.as_str(), dir];
        args.extend(argv);
        self.mac.client(&args)
    }

    /// [`Lease::run`] with the action's stdout and stderr `/dev/null`, for an action
    /// that leaves a process running: it holds none of the pipes this process reads to
    /// the end.
    fn run_quiet(&self, dir: &str, argv: &[&str]) -> Out {
        let mut args = vec!["run-quiet", SOCKET, self.id.as_str(), dir];
        args.extend(argv);
        self.mac.client(&args)
    }

    #[track_caller]
    fn kill(&self) {
        let out = self.call("kill");
        assert!(out.ok(), "kill-uid {}: {}", self.id, out.all());
    }

    /// `kill-uid`, then `user-delete`, which must find the user.
    #[track_caller]
    fn end(&self) {
        self.kill();
        says(&self.call("delete"), "existed", "user-delete");
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            println!("== ending lease {} after a failure", self.id);
            self.call("kill");
            self.call("delete");
        }
    }
}

/// Runs `f` when dropped during a panic: puts back what a failing test changed by hand.
struct OnPanic<F: FnMut()>(F);

impl<F: FnMut()> Drop for OnPanic<F> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            (self.0)();
        }
    }
}

/// Removes the session switch's files: a failing test must not leave a lease held.
fn clear_session() {
    let _ = fs::remove_file(intent());
    let _ = fs::remove_file(kcpassword());
    root(
        "/usr/bin/defaults",
        &["delete", &loginwindow(), "autoLoginUser"],
    );
}

static SERIAL: Mutex<()> = Mutex::new(());
static MAC: OnceLock<Mac> = OnceLock::new();

/// The lock every test holds, and the helper set up (by the first test to get here).
fn mac() -> (MutexGuard<'static, ()>, &'static Mac) {
    let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    assert_root();
    (serial, MAC.get_or_init(Mac::prepare))
}

#[path = "as_root/tests.rs"]
mod tests;
