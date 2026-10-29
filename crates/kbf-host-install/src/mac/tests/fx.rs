//! A Mac under a scratch directory, and a runner that plays its tools.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use kbf_testkit::dir::TestDir;
use sha2::{Digest, Sha256};

use crate::mac::layout::{APPLY_LABEL, DAEMON_LABEL, MacHost};
use crate::mac::release::{bundle_name, tarball_name};
use crate::mac::stage::sha256_of;
use crate::pace::fake::Fake;
use crate::runner::{Cmd, Exec, Runner};
use crate::trust::Trust;

/// What the Mac's tools printed (`tests/fixtures/mac/README.md`).
pub mod real {
    pub const IDLE: &str = include_str!("../../../tests/fixtures/mac/print_idle.stdout");
    pub const RUNNING: &str = include_str!("../../../tests/fixtures/mac/print_running.stdout");
    pub const CLEAN_EXIT: &str =
        include_str!("../../../tests/fixtures/mac/print_clean_exit.stdout");
    pub const NONZERO_EXIT: &str =
        include_str!("../../../tests/fixtures/mac/print_nonzero_exit.stdout");
    pub const KILLED: &str = include_str!("../../../tests/fixtures/mac/print_killed.stdout");
    pub const NESTED: &str = include_str!("../../../tests/fixtures/mac/print_nested.stdout");
    pub const MISSING_STDERR: &str =
        include_str!("../../../tests/fixtures/mac/print_missing.stderr");
    pub const MISSING_CODE: &str = include_str!("../../../tests/fixtures/mac/print_missing.code");
    pub const LSOF: &str = include_str!("../../../tests/fixtures/mac/lsof_running.stdout");
    pub const CODESIGN_HARDENED: &str =
        include_str!("../../../tests/fixtures/mac/codesign_describe_hardened.stderr");
    pub const CODESIGN_PLAIN: &str =
        include_str!("../../../tests/fixtures/mac/codesign_describe_plain.stderr");
    pub const DSCL_NOBODY: &str = include_str!("../../../tests/fixtures/mac/dscl_nobody.stdout");
    pub const XATTR_QUARANTINED: &str =
        include_str!("../../../tests/fixtures/mac/xattr_quarantined.stdout");
    pub const XATTR_CLEAN: &str = include_str!("../../../tests/fixtures/mac/xattr_clean.stdout");

    /// The listing `text` of some service as the listing of service `label` (its first line).
    pub fn as_service(text: &str, label: &str) -> String {
        let (_, rest) = text.split_once('\n').unwrap();
        format!("system/{label} = {{\n{rest}")
    }

    /// The pid in a running listing.
    pub fn pid_of(text: &str) -> &str {
        text.lines()
            .find_map(|line| line.strip_prefix("\tpid = "))
            .unwrap()
    }
}

pub const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const NEW: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
pub const OLD_VERSION: &str = "0.1.5+gaaaaaaaaaaaa-2";
pub const NEW_VERSION: &str = "0.1.7+gbbbbbbbbbbbb-3";
pub const INSTALL: &str = "install kbf=0.1.7+gbbbbbbbbbbbb-3 from stable";

/// The header of an arm64 Mach-O executable.
pub const MACHO: [u8; 8] = [0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01];

pub fn write(path: &Path, mode: u32, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

pub fn mkdir(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

pub fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().mode() & 0o7777
}

/// A binary: the Mach-O header and some bytes.
pub fn binary(tail: &str) -> Vec<u8> {
    let mut bytes = MACHO.to_vec();
    bytes.extend_from_slice(tail.as_bytes());
    bytes
}

/// A gzipped tar of `members` (path, type, bytes).
pub fn tarball(members: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, kind, bytes) in members {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(*kind);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, *bytes).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    gzip(&tar)
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A Mac: the kit with release `OLD` running, a channel `stable` that offers `NEW`.
pub struct Fx {
    pub dir: TestDir,
    pub host: MacHost,
    /// Where the channel's release files are.
    pub channel_dir: PathBuf,
    pub sums: PathBuf,
    /// The binary inside the offered tarball.
    pub new_binary: Vec<u8>,
    /// The time of this host's waits: it moves only when they sleep.
    pub clock: Fake,
}

impl Fx {
    pub fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        mkdir(&dir, 0o755);
        let uid = fs::metadata(&*dir).unwrap().uid();
        let mut host = MacHost::for_test(&dir, Trust::owned_by(uid, &dir));
        host.self_exe = dir.join("libexec/kbf-host-install");
        let clock = Fake::new();
        host.pace = std::rc::Rc::new(clock.pace());
        mkdir(&dir.join("libexec"), 0o755);
        write(&host.self_exe, 0o755, b"program");
        for sub in [
            "kit",
            "kit/releases",
            "kit/etc",
            "kit/log",
            "trust",
            "channel",
        ] {
            mkdir(&dir.join(sub), 0o755);
        }
        let old = host.release(OLD);
        mkdir(&old, 0o755);
        write(&old.join("kbf"), 0o755, &binary("old"));
        write(
            &old.join("VERSION"),
            0o644,
            format!("{OLD_VERSION}\n").as_bytes(),
        );
        symlink(format!("releases/{OLD}"), host.current()).unwrap();
        write(
            &host.node_conf,
            0o644,
            b"FRONTEND_NAME=frontend.example.invalid\nNODE_ID=mac-a\n",
        );
        write(&host.err_log, 0o644, b"");
        write(&host.daemon_plist, 0o644, b"<plist/>");
        write(&host.daemon_config, 0o644, b"mode = \"daemon\"\n");
        write(&host.gh, 0o755, b"gh");
        write(&host.curl, 0o755, b"curl");
        write(&host.trusted_root, 0o644, b"{}");

        let channel_dir = dir.join("channel");
        let new_binary = binary("new");
        let tar = tarball(&[("kbf", tar::EntryType::Regular, &new_binary)]);
        write(&channel_dir.join(tarball_name(NEW)), 0o644, &tar);
        write(
            &channel_dir.join(bundle_name(NEW)),
            0o644,
            b"{\"bundle\":1}\n",
        );
        let sums = dir.join("stable.SHA256SUMS");
        write(
            &sums,
            0o644,
            format!(
                "{}  {}\n{}  {}\n",
                sha256(b"another release"),
                tarball_name(OLD),
                sha256(&tar),
                tarball_name(NEW)
            )
            .as_bytes(),
        );
        write(
            &host.allowlist,
            0o644,
            format!(
                "[channels.stable]\ndir = \"{}\"\nsums = \"{}\"\n",
                channel_dir.display(),
                sums.display()
            )
            .as_bytes(),
        );
        Self {
            dir,
            host,
            channel_dir,
            sums,
            new_binary,
            clock,
        }
    }

    pub fn sim(&self) -> Sim {
        let meta = fs::metadata(&*self.dir).unwrap();
        Sim::new(&self.host, meta.uid(), meta.gid())
    }

    /// Replace the offered tarball (and its line in the sums).
    pub fn offer(&self, tar: &[u8]) {
        write(&self.channel_dir.join(tarball_name(NEW)), 0o644, tar);
        write(
            &self.sums,
            0o644,
            format!("{}  {}\n", sha256(tar), tarball_name(NEW)).as_bytes(),
        );
    }

    /// Offer release `commit` with this binary: its tarball, bundle and line of the sums
    /// (the sums keep the lines already there).
    pub fn offer_release(&self, commit: &str, bytes: &[u8]) {
        let tar = tarball(&[("kbf", tar::EntryType::Regular, bytes)]);
        write(&self.channel_dir.join(tarball_name(commit)), 0o644, &tar);
        write(&self.channel_dir.join(bundle_name(commit)), 0o644, b"{}\n");
        let name = tarball_name(commit);
        let kept: String = fs::read_to_string(&self.sums)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.ends_with(&name))
            .map(|line| format!("{line}\n"))
            .collect();
        let mut sums = kept;
        sums.push_str(&format!("{}  {}\n", sha256(&tar), tarball_name(commit)));
        write(&self.sums, 0o644, sums.as_bytes());
    }

    /// Make `commit` the installed release, as `version`.
    pub fn install_release(&self, commit: &str, version: &str, bytes: &[u8]) {
        let dir = self.host.release(commit);
        mkdir(&dir, 0o755);
        write(&dir.join("kbf"), 0o755, bytes);
        write(
            &dir.join("VERSION"),
            0o644,
            format!("{version}\n").as_bytes(),
        );
        let _ = fs::remove_file(self.host.current());
        symlink(format!("releases/{commit}"), self.host.current()).unwrap();
    }

    pub fn tarball_path(&self) -> PathBuf {
        self.channel_dir.join(tarball_name(NEW))
    }

    pub fn bundle_path(&self) -> PathBuf {
        self.channel_dir.join(bundle_name(NEW))
    }

    /// Run a request as the root process.
    pub fn handle(&self, sim: &Sim, command: &str) -> Answer {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = crate::mac::handle_root(&self.host, sim, Some(command), &mut out, &mut err);
        Answer {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    pub fn audit_lines(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(&self.host.audit)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub fn status(&self) -> String {
        fs::read_to_string(self.host.status_file()).unwrap_or_default()
    }

    pub fn link(&self, path: &Path) -> Option<String> {
        fs::read_link(path)
            .ok()
            .map(|t| t.to_string_lossy().into_owned())
    }
}

pub struct Answer {
    pub code: u8,
    pub out: String,
    pub err: String,
}

impl Answer {
    pub fn json(&self) -> serde_json::Value {
        assert!(
            self.out.ends_with('\n') && self.out.matches('\n').count() == 1,
            "{:?}",
            self.out
        );
        serde_json::from_str(&self.out).unwrap()
    }
}

/// A fake launchd, codesign, xattr, dscl, lsof, gh, sudo and a `kbf` that checks a
/// configuration. State is what a Mac would keep: the loaded services, the quarantined files.
/// A look at the host as a command is about to run.
pub type Hook = Box<dyn Fn(&Cmd)>;

pub struct Sim {
    host: MacHost,
    ids: (u32, u32),
    pub calls: RefCell<Vec<Cmd>>,
    pub loaded: RefCell<BTreeSet<String>>,
    /// Whether the apply job (when loaded) says `state = running`.
    pub apply_running: Cell<bool>,
    /// Whether the apply job (when loaded) is scheduled and has not started.
    pub apply_spawning: Cell<bool>,
    pub quarantined: RefCell<BTreeSet<PathBuf>>,
    /// Things that go wrong, by name.
    pub broken: RefCell<BTreeSet<&'static str>>,
    /// Whether the daemon, once bootstrapped, logs `welcomed` (from its `welcome_from`th
    /// bootstrap on).
    pub welcome: Cell<bool>,
    pub welcome_from: Cell<u32>,
    /// How many bootstraps of the daemon fail before one works.
    pub bootstrap_fails: Cell<u32>,
    /// How many more times `print` finds a service that was booted out.
    pub lingering: Cell<u32>,
    /// What `lsof` says the daemon's program is.
    pub running_path: RefCell<Option<PathBuf>>,
    /// The ids `dscl` gives for `nobody`, when they are not the daemon's.
    pub nobody_ids: Cell<Option<(u32, u32)>>,
    /// The answer to `sudo`.
    pub sudo: RefCell<Option<io::Result<Exec>>>,
    pub before: RefCell<Option<Hook>>,
    /// GitHub: the pages of the releases listing (the JSON the API prints, one text per
    /// page), and the files each release (by tag) has, by asset name.
    pub pages: RefCell<Vec<String>>,
    pub assets: RefCell<BTreeMap<String, BTreeMap<String, Vec<u8>>>>,
    pub on_daemon_bootstrap: RefCell<Option<Box<dyn Fn()>>>,
    pub daemon_bootstraps: Cell<u32>,
    pub daemon_bootouts: Cell<u32>,
}

pub fn exec(code: i32, stdout: &str, stderr: &str) -> io::Result<Exec> {
    Ok(Exec {
        code: Some(code),
        signal: None,
        stdout: stdout.into(),
        stderr: stderr.into(),
    })
}

impl Sim {
    pub fn new(host: &MacHost, uid: u32, gid: u32) -> Self {
        Self {
            host: host.clone(),
            ids: (uid, gid),
            calls: RefCell::default(),
            loaded: RefCell::new(BTreeSet::from([DAEMON_LABEL.to_owned()])),
            apply_running: Cell::new(false),
            apply_spawning: Cell::new(false),
            quarantined: RefCell::default(),
            broken: RefCell::default(),
            welcome: Cell::new(true),
            welcome_from: Cell::new(1),
            bootstrap_fails: Cell::new(0),
            lingering: Cell::new(0),
            running_path: RefCell::new(Some(host.release(OLD).join("kbf"))),
            nobody_ids: Cell::new(None),
            sudo: RefCell::new(None),
            before: RefCell::new(None),
            pages: RefCell::default(),
            assets: RefCell::default(),
            on_daemon_bootstrap: RefCell::new(None),
            daemon_bootstraps: Cell::new(0),
            daemon_bootouts: Cell::new(0),
        }
    }

    pub fn break_(&self, what: &'static str) {
        self.broken.borrow_mut().insert(what);
    }

    pub fn is_broken(&self, what: &str) -> bool {
        self.broken.borrow().contains(what)
    }

    /// The calls, each as `program arg arg`.
    pub fn lines(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .map(|c| format!("{} {}", c.program, c.args.join(" ")))
            .collect()
    }

    /// How many calls start with `prefix`.
    pub fn count(&self, prefix: &str) -> usize {
        self.lines()
            .iter()
            .filter(|l| l.starts_with(prefix))
            .count()
    }

    fn launchctl(&self, args: &[String]) -> io::Result<Exec> {
        let label_of = |text: &str| text.strip_prefix("system/").unwrap_or(text).to_owned();
        match args.first().map(String::as_str) {
            Some("print") => {
                let label = label_of(&args[1]);
                let mut loaded = self.loaded.borrow().contains(&label);
                if label == DAEMON_LABEL && !loaded && self.lingering.get() > 0 {
                    self.lingering.set(self.lingering.get() - 1);
                    loaded = true;
                }
                if !loaded {
                    let stderr = real::MISSING_STDERR
                        .replace("com.example.kbf-real-mac-test.idle.3110", &label);
                    return exec(real::MISSING_CODE.trim().parse().unwrap(), "", &stderr);
                }
                // Real listings (the fixtures), as this service and with this pid.
                let listing = if label != APPLY_LABEL || self.apply_running.get() {
                    real::RUNNING.to_owned()
                } else if self.apply_spawning.get() {
                    // Not captured: a real "not running" listing in the state launchd names
                    // for a job that is scheduled to start.
                    real::IDLE.replace("\tstate = not running\n", "\tstate = spawn scheduled\n")
                } else {
                    real::CLEAN_EXIT.to_owned()
                };
                let mut text = real::as_service(&listing, &label);
                if listing == real::RUNNING {
                    let pid_line = format!("\tpid = {}\n", real::pid_of(&listing));
                    let ours = if self.is_broken("no-pid") {
                        String::new()
                    } else {
                        "\tpid = 4242\n".to_owned()
                    };
                    text = text.replace(&pid_line, &ours);
                }
                exec(0, &text, "")
            }
            Some("bootout") => {
                let label = label_of(&args[1]);
                if label == DAEMON_LABEL {
                    self.daemon_bootouts.set(self.daemon_bootouts.get() + 1);
                }
                let was = self.loaded.borrow_mut().remove(&label);
                let stuck = if label == DAEMON_LABEL {
                    "stuck"
                } else {
                    "stuck-apply"
                };
                if was && self.is_broken(stuck) {
                    self.loaded.borrow_mut().insert(label);
                }
                exec(if was { 0 } else { 3 }, "", "")
            }
            Some("bootstrap") => {
                let plist = Path::new(&args[2]);
                if plist == self.host.daemon_plist {
                    self.daemon_bootstraps.set(self.daemon_bootstraps.get() + 1);
                    if let Some(hook) = &*self.on_daemon_bootstrap.borrow() {
                        hook();
                    }
                    if self.lingering.get() > 0 {
                        // launchd still has the service: the gap after a bootout.
                        return exec(5, "", "Bootstrap failed: 5: Input/output error");
                    }
                    if self.bootstrap_fails.get() > 0 {
                        self.bootstrap_fails.set(self.bootstrap_fails.get() - 1);
                        return exec(5, "", "Bootstrap failed: 5: Input/output error");
                    }
                    self.loaded.borrow_mut().insert(DAEMON_LABEL.to_owned());
                    // The daemon executes the release `current` names, unless a test says
                    // the file it runs is not that one.
                    let named = fs::read_link(self.host.current()).ok();
                    if let (false, Some(target)) = (self.is_broken("runs-stale"), named) {
                        *self.running_path.borrow_mut() =
                            Some(self.host.kit.join(target).join("kbf"));
                    }
                    if self.welcome.get() && self.daemon_bootstraps.get() >= self.welcome_from.get()
                    {
                        use std::io::Write;
                        let mut log = fs::OpenOptions::new()
                            .append(true)
                            .open(&self.host.err_log)
                            .unwrap();
                        writeln!(
                            log,
                            "\x1b[32m INFO\x1b[0m kbf_daemon::daemon: welcomed server=x"
                        )
                        .unwrap();
                    }
                    exec(0, "", "")
                } else if self.is_broken("bootstrap-apply") {
                    exec(5, "", "Bootstrap failed: 5: Input/output error")
                } else {
                    self.loaded.borrow_mut().insert(APPLY_LABEL.to_owned());
                    exec(0, "", "")
                }
            }
            _ => Err(io::Error::other("unscripted launchctl")),
        }
    }
}

impl Sim {
    /// `curl --include ... --url https://api.github.com/repos/<repo>/releases?per_page=N&page=P`:
    /// the response with page P of the scripted listing, an empty one past the last. The
    /// broken kinds are answers of GitHub with a status that is not 200 ("list-http": a 500;
    /// "list-rate": the exhausted rate limit as the API prints it, with the reset time;
    /// "list-rate-wait": a secondary limit, which asks for a wait; "list-rate-message": a
    /// 403 that only the message says is a limit; "list-forbidden": a 403 that is not one;
    /// "list-interim": a `100 Continue` before the page) and "list-nohttp", which is no
    /// response at all.
    fn curl_list(&self, cmd: &Cmd) -> io::Result<Exec> {
        let response = |status: &str, headers: &str, body: &str| {
            exec(0, &format!("HTTP/2 {status}\r\n{headers}\r\n{body}"), "")
        };
        if self.is_broken("list-http") {
            return response("500 ", "", "{\"message\":\"Server Error\"}");
        }
        if self.is_broken("list-rate") {
            return response(
                "403 ",
                "x-ratelimit-limit: 60\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset: 1790000000\r\n",
                "{\"message\":\"API rate limit exceeded for your address.\"}",
            );
        }
        if self.is_broken("list-rate-wait") {
            return response("429 ", "Retry-After: 60\r\n", "{}");
        }
        if self.is_broken("list-rate-message") {
            return response(
                "403 ",
                "x-ratelimit-remaining: 12\r\n",
                "{\"message\":\"You have exceeded a secondary rate limit.\"}",
            );
        }
        if self.is_broken("list-forbidden") {
            return response(
                "403 ",
                "x-ratelimit-remaining: 12\r\n",
                "{\"message\":\"Forbidden\"}",
            );
        }
        if self.is_broken("list-nohttp") {
            return exec(0, "<html>not a response</html>", "");
        }
        if self.is_broken("list-timeout") {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ran past its limit",
            ));
        }
        if self.is_broken("list-signal") {
            return Ok(Exec {
                code: None,
                signal: Some(13),
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        if self.is_broken("list-net") {
            return exec(1, "", "error connecting to api.github.com");
        }
        if self.is_broken("list-junk") {
            return response("200 ", "", "<html>not json</html>");
        }
        let page: usize = cmd
            .args
            .last()
            .expect("the url")
            .rsplit("page=")
            .next()
            .and_then(|n| n.parse().ok())
            .expect("a page number");
        let pages = self.pages.borrow();
        let interim = if self.is_broken("list-interim") {
            "HTTP/1.1 100 Continue\r\n\r\n"
        } else {
            ""
        };
        exec(
            0,
            &format!(
                "{interim}HTTP/2 200 \r\ncontent-type: application/json\r\n\r\n{}",
                pages.get(page - 1).map_or("[]", String::as_str)
            ),
            "",
        )
    }

    /// `gh release download <tag> --repo R --pattern A --pattern B --dir D`: writes the
    /// release's files that match into D.
    fn gh_download(&self, cmd: &Cmd) -> io::Result<Exec> {
        let args = &cmd.args;
        let tag = &args[2];
        let value_of = |flag: &str| {
            args.windows(2)
                .filter(|w| w[0] == flag)
                .map(|w| w[1].clone())
                .collect::<Vec<_>>()
        };
        let dir = PathBuf::from(&value_of("--dir")[0]);
        if self.is_broken("download-net") {
            return exec(1, "", "error connecting to api.github.com");
        }
        let assets = self.assets.borrow();
        let Some(files) = assets.get(tag) else {
            return exec(1, "", "release not found");
        };
        for name in value_of("--pattern") {
            let Some(bytes) = files.get(&name) else {
                return exec(1, "", &format!("no assets match {name}"));
            };
            let mut bytes = bytes.clone();
            if self.is_broken("download-big") && !name.ends_with(".sigstore.json") {
                bytes.resize(self.host.tuning.tarball_max as usize + 1, 0);
            }
            if self.is_broken("download-big-bundle") && name.ends_with(".sigstore.json") {
                bytes.resize(self.host.tuning.bundle_max as usize + 1, 0);
            }
            write(&dir.join(&name), 0o644, &bytes);
        }
        if self.is_broken("download-timeout") {
            // A real runner kills the command at its limit; what it wrote stays.
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ran past its limit",
            ));
        }
        exec(0, "", "")
    }
}

impl Runner for Sim {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        if let Some(hook) = &*self.before.borrow() {
            hook(cmd);
        }
        self.calls.borrow_mut().push(cmd.clone());
        let args = &cmd.args;
        let first = args.first().map(String::as_str);
        match cmd.program.as_str() {
            crate::mac::tools::LAUNCHCTL if self.is_broken("launchctl") => {
                Err(io::Error::other("launchctl cannot start"))
            }
            crate::mac::tools::LAUNCHCTL => self.launchctl(args),
            crate::mac::tools::XATTR => {
                if self.is_broken("xattr") {
                    return exec(1, "", "xattr: No such file");
                }
                if first == Some("-d") {
                    if self.is_broken("xattr-d") {
                        return exec(1, "", "cannot remove");
                    }
                    if !self.is_broken("xattr-sticky") {
                        self.quarantined.borrow_mut().remove(Path::new(&args[2]));
                    }
                    return exec(0, "", "");
                }
                let listing = if self.quarantined.borrow().contains(Path::new(&args[0])) {
                    "com.apple.quarantine\ncom.apple.provenance\n"
                } else {
                    "com.apple.provenance\n"
                };
                exec(0, listing, "")
            }
            crate::mac::tools::CODESIGN => match first {
                Some("--verify") if self.is_broken("codesign-verify") => {
                    exec(1, "", "invalid signature")
                }
                Some("--verify") => exec(0, "", ""),
                // `codesign -d` describes on stderr.
                _ if self.is_broken("no-runtime") => exec(0, "", real::CODESIGN_PLAIN),
                _ => exec(0, "", real::CODESIGN_HARDENED),
            },
            crate::mac::tools::DSCL if self.is_broken("dscl") => exec(1, "", "no such user"),
            crate::mac::tools::DSCL => {
                let ids = match self.nobody_ids.get() {
                    Some(ids) if args.iter().any(|a| a.ends_with("/nobody")) => ids,
                    _ => self.ids,
                };
                exec(
                    0,
                    &format!("UniqueID: {}\nPrimaryGroupID: {}\n", ids.0, ids.1),
                    "",
                )
            }
            crate::mac::tools::LSOF if self.is_broken("lsof") => exec(1, "", ""),
            crate::mac::tools::LSOF if self.is_broken("lsof-start") => {
                Err(io::Error::other("lsof cannot start"))
            }
            crate::mac::tools::LSOF => {
                let path = self.running_path.borrow().clone();
                // The real listing of a running process, with this pid and program.
                let text = path.map_or_else(
                    || "p4242\n".to_owned(),
                    |p| {
                        real::LSOF
                            .replace("p3114", "p4242")
                            .replace("n/bin/sleep", &format!("n{}", p.display()))
                    },
                );
                // lsof exits 1 when it only warns, with its listing whole.
                if self.is_broken("lsof-warns") {
                    return exec(
                        1,
                        &text,
                        "lsof: WARNING: can't stat() apfs file system /x\n",
                    );
                }
                exec(0, &text, "")
            }
            crate::mac::tools::SUDO => self
                .sudo
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err(io::Error::other("no sudo reply"))),
            program if Path::new(program) == self.host.curl => self.curl_list(cmd),
            program if Path::new(program) == self.host.gh && first == Some("release") => {
                self.gh_download(cmd)
            }
            program if Path::new(program) == self.host.gh => {
                let former = args.iter().any(|a| a.contains("komira-build-farm"));
                if self.is_broken("gh") || (self.is_broken("gh-current") && !former) {
                    exec(1, "", "no matching attestations")
                } else {
                    exec(0, "", "")
                }
            }
            program if program.ends_with("/kbf") && args.iter().any(|a| a == "--check") => {
                if self.is_broken("check") {
                    exec(2, "", "kbf: bad config")
                } else {
                    exec(0, "kbf: --check: ok\n", "")
                }
            }
            other => Err(io::Error::other(format!("unscripted: {other}"))),
        }
    }
}

/// The sha256 of a staged or laid out file, through the code under test.
pub fn digest(path: &Path) -> String {
    sha256_of(path).unwrap()
}
