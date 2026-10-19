//! Tests of `setup` on a fake host. Each says the defect it catches; the mutants planted to
//! see them go red are in the pull request.

use std::cell::RefCell;
use std::rc::Rc;

use kbf_host::testkit::{FakeExec, RUNNER};
use kbf_host::{Entry, Exec, HostPath, Owner, Perms, Step};

use kbf_state::postgres::schema::Outcome;

use super::backend::{Backend, Releases, Schemas};
use super::fake::{FakeOs, user};
use super::paths::{API_TOKEN, ENROLL_TOKEN, INSTALLER_TOML, KBF_TOML};
use super::validate::Raw;
use super::{Options, SetupError, run};
use crate::CommitId;
use crate::cli::{Mode, ReleaseSource};
use crate::database::SchemaOps;
use crate::exit::Exit;
use crate::helper::fetcher::FetchError;
use crate::installer_toml::Source;
use crate::layout::{Layout, Platform};

const DATABASE: &str = "postgres:///kbf?host=/run/postgresql";

const SRC: &str = "/root/kbf.token";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const SECRET: &str = "abababababababababababababababababababababababababababababababab";
const ROOT: Owner = Owner { uid: 0, gid: 0 };
const KBF: Owner = Owner { uid: 990, gid: 990 };
const KBF_SERVER: Owner = Owner { uid: 989, gid: 989 };

/// The flags of a node mode, or of a frontend or all-in-one host.
fn raw(mode: Mode) -> Raw {
    let node = matches!(mode, Mode::Storage | Mode::Daemon | Mode::StorageAndDaemon);
    Raw {
        mode,
        frontend_url: node.then(|| "https://farm.example:8443".to_owned()),
        token_file: node.then(|| SRC.to_owned()),
        release_source: None,
        release_url: (!node).then(|| "https://releases.example/kbf".to_owned()),
        min_generation: None,
        commit: None,
        database: mode.keeps_state_in_database().then(|| DATABASE.to_owned()),
        kbf: super::kbf_settings::KbfFlags::default(),
        installer_config: INSTALLER_TOML.to_owned(),
    }
}

/// `releases/current` on a host of `platform`.
fn current_release(platform: Platform) -> String {
    Layout::new(kbf_host::Root::new("/").expect("root"), platform)
        .current()
        .to_string()
}

fn hp(p: &str) -> HostPath {
    HostPath::new(p).expect("a path")
}

fn put(exec: &mut FakeExec, path: &str, perms: u32, owner: Owner, bytes: &str) {
    exec.entries.insert(
        hp(path),
        Entry::File {
            perms: Perms::new(perms),
            owner,
            bytes: bytes.as_bytes().to_vec(),
        },
    );
}

fn host() -> FakeExec {
    let mut exec = FakeExec::new(0);
    put(&mut exec, SRC, 0o600, RUNNER, "tok-abc123\n");
    exec
}

/// A host with a release installed: `releases/current` is a link.
fn released(mut exec: FakeExec, platform: Platform) -> FakeExec {
    exec.entries.insert(
        hp(&current_release(platform)),
        Entry::Link {
            target: COMMIT.into(),
        },
    );
    exec
}

const APPLY: Options = Options {
    apply: true,
    no_start: false,
};

/// A downloader that has no fetcher behind it, as the binary's own has none yet.
struct Unwired;

impl Releases for Unwired {
    fn install_first(&self, _: &CommitId) -> Result<(), SetupError> {
        Err(SetupError::Fetch(FetchError::NotWired))
    }
}

/// What the fake server was asked, in order: `plan` and `setup`, each with the URL.
type Asked = Rc<RefCell<Vec<String>>>;

/// A server at schema version `found` (`None`: a fresh database) that answers as told.
struct Server {
    found: Option<u32>,
    down: bool,
    asked: Asked,
}

impl Server {
    fn fresh() -> Server {
        Server {
            found: None,
            down: false,
            asked: Asked::default(),
        }
    }
}

struct Session {
    found: Option<u32>,
    down: bool,
    url: String,
    asked: Asked,
}

impl SchemaOps for Session {
    fn setup(&self) -> Result<Outcome, String> {
        self.asked.borrow_mut().push(format!("setup {}", self.url));
        if self.down {
            return Err("no server".to_owned());
        }
        Ok(Outcome {
            from: self.found,
            to: 3,
            ran: if self.found == Some(3) {
                vec![]
            } else {
                vec![3]
            },
        })
    }

    fn plan(&self) -> Result<(Option<u32>, u32), String> {
        self.asked.borrow_mut().push(format!("plan {}", self.url));
        if self.down {
            return Err("no server".to_owned());
        }
        Ok((self.found, 3))
    }
}

impl Schemas for Server {
    fn of(&self, url: &str) -> Result<Box<dyn SchemaOps>, SetupError> {
        Ok(Box::new(Session {
            found: self.found,
            down: self.down,
            url: url.to_owned(),
            asked: self.asked.clone(),
        }))
    }
}

fn go(os: &FakeOs, exec: &mut FakeExec, raw: &Raw, options: Options) -> Result<String, SetupError> {
    go_with(os, exec, &Unwired, raw, options)
}

fn go_with(
    os: &FakeOs,
    exec: &mut FakeExec,
    releases: &dyn Releases,
    raw: &Raw,
    options: Options,
) -> Result<String, SetupError> {
    go_on(os, exec, releases, &Server::fresh(), raw, options)
}

fn go_on(
    os: &FakeOs,
    exec: &mut FakeExec,
    releases: &dyn Releases,
    server: &Server,
    raw: &Raw,
    options: Options,
) -> Result<String, SetupError> {
    let mut out = Vec::new();
    let backend = Backend {
        releases,
        schemas: server,
    };
    run(os, exec, &backend, raw, options, &mut out)?;
    Ok(String::from_utf8(out).expect("text"))
}

fn go_ok(os: &FakeOs, exec: &mut FakeExec, raw: &Raw) -> String {
    go(os, exec, raw, APPLY).expect("setup")
}

/// The commands the steps ran, as printed.
fn cmds(exec: &FakeExec) -> Vec<String> {
    exec.log
        .iter()
        .filter_map(|s| match s {
            Step::Run(_) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

fn write_index(exec: &FakeExec, path: &str) -> Option<usize> {
    exec.log
        .iter()
        .position(|s| matches!(s, Step::WriteFile { path: p, .. } if p.as_str() == path))
}

fn text(exec: &FakeExec, path: &str) -> String {
    match exec.entries.get(&hp(path)) {
        Some(Entry::File { bytes, .. }) => String::from_utf8_lossy(bytes).into_owned(),
        other => panic!("{path}: {other:?}"),
    }
}

fn file_meta(exec: &FakeExec, path: &str) -> (u32, Owner) {
    match exec.entries.get(&hp(path)) {
        Some(Entry::File { perms, owner, .. }) => (perms.bits(), *owner),
        other => panic!("{path}: {other:?}"),
    }
}

/// What a finished daemon setup leaves, as the host's facts.
fn settled(units: &[&str]) -> FakeOs {
    let mut os = FakeOs::linux();
    os.users.push(user("kbf-fetch", 991));
    os.subuid = "kbf:100000:65536\n".to_owned();
    os.subgid = os.subuid.clone();
    os.lingering.insert("kbf".to_owned());
    for u in units.iter().chain(&["kbf-installer.service"]) {
        os.enabled.insert((*u).to_owned());
    }
    os
}

const DAEMON_TOML: &str =
    "# Written by `kbf-installer setup`. The mode's other settings are added below the
# keys it writes; run setup again to change these.
mode = \"daemon\"

[frontend]
url = \"https://farm.example:8443\"

[enroll]
token_file = \"/etc/kbf/enroll.token\"
";

const FRONTEND_TOML: &str =
    "# Written by `kbf-installer setup`. The mode's other settings are added below the
# keys it writes; run setup again to change these.
mode = \"frontend\"

[tls]
api-token-file = \"/etc/kbf/api-token\"

[scheduler]
state-store = \"postgres:///kbf?host=/run/postgresql\"
";

const DAEMON_INSTALLER_TOML: &str = "release_source = \"frontend\"
confirm_window_s = 900
confirm_window_max_s = 1800
min_generation = 0
min_protocol = 0
fetch_timeout_s = 900
max_component_bytes = 536870912
min_activation_interval_s = 60
node_cert = \"/var/lib/kbf/pki/daemon.pem\"
node_key = \"/var/lib/kbf/pki/daemon.key\"
";

/// Catches a setup that leaves a prerequisite out or in the wrong order: the whole daemon
/// plan on a bare host, step for step, and the two files it writes byte for byte (the
/// golden texts are what `kbf-enroll`'s loader and `InstallerToml::parse` read back).
#[test]
fn a_bare_daemon_host_gets_accounts_dirs_token_settings_config_then_units() {
    let mut exec = released(host(), Platform::Linux);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    assert_eq!(
        cmds(&exec),
        [
            "run /usr/sbin/useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin -- kbf-fetch",
            "run /usr/sbin/usermod --add-subuids 100000-165535 -- kbf",
            "run /usr/sbin/usermod --add-subgids 100000-165535 -- kbf",
            "run /usr/bin/loginctl enable-linger kbf",
            "run /usr/bin/systemctl enable --now -- kbf-daemon.service",
            "run /usr/bin/systemctl enable --now -- kbf-installer.service",
        ]
    );
    let dirs: Vec<_> = exec
        .log
        .iter()
        .filter_map(|s| match s {
            Step::Mkdir { path, perms, owner } => Some((path.as_str(), perms.bits(), *owner)),
            _ => None,
        })
        .collect();
    assert_eq!(
        dirs,
        [
            ("/etc/kbf", 0o755, Some(ROOT)),
            ("/var/lib/kbf-installer", 0o755, Some(ROOT)),
            ("/var/lib/kbf-installer/orders", 0o700, Some(KBF)),
        ]
    );
    assert_eq!(text(&exec, ENROLL_TOKEN), "tok-abc123\n");
    assert_eq!(file_meta(&exec, ENROLL_TOKEN), (0o600, KBF));
    assert_eq!(text(&exec, KBF_TOML), DAEMON_TOML);
    assert_eq!(
        file_meta(&exec, KBF_TOML),
        (0o640, Owner { uid: 0, gid: 990 })
    );
    assert_eq!(text(&exec, INSTALLER_TOML), DAEMON_INSTALLER_TOML);
    assert_eq!(file_meta(&exec, INSTALLER_TOML), (0o644, ROOT));
    assert!(
        !exec.entries.contains_key(&hp(API_TOKEN)),
        "a node makes no operator token"
    );
    assert!(
        !out.contains("tok-abc123"),
        "the token is never printed: {out}"
    );
}

/// Catches a config written before the token (or before the units' other files): a crash
/// between them would leave a `kbf.toml` that passes the units' condition and a service that
/// cannot start. `kbf.toml` follows the token and `installer.toml`, and no unit is enabled
/// before it.
#[test]
fn kbf_toml_is_written_after_the_other_files_and_before_any_unit() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    let toml = write_index(&exec, KBF_TOML);
    for before in [ENROLL_TOKEN, INSTALLER_TOML] {
        assert!(write_index(&exec, before) < toml, "{before} then {toml:?}");
    }
    let first_enable = exec
        .log
        .iter()
        .position(|s| matches!(s, Step::Run(c) if c.to_string().contains("systemctl")));
    assert!(
        toml < first_enable,
        "config {toml:?} before units {first_enable:?}"
    );
}

/// Catches a setup that is not safe to interrupt: a failure while writing the token must
/// leave no `kbf.toml` behind, and one while writing `installer.toml` likewise.
#[test]
fn a_failed_write_leaves_no_config() {
    for victim in [ENROLL_TOKEN, INSTALLER_TOML] {
        let mut exec = released(host(), Platform::Linux);
        let at = {
            let mut probe = released(host(), Platform::Linux);
            go_ok(&FakeOs::linux(), &mut probe, &raw(Mode::Daemon));
            write_index(&probe, victim).expect("step")
        };
        exec.fail_at = Some(at);
        let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), APPLY);
        assert!(matches!(err, Err(SetupError::Host(_))), "{victim}: {err:?}");
        assert!(!exec.entries.contains_key(&hp(KBF_TOML)), "{victim}");
    }
}

/// Catches a re-run that does work: the same flags on a finished host change nothing, and
/// say so. Also catches a plan that compares a file's owner or mode loosely.
#[test]
fn a_rerun_with_the_same_flags_is_a_no_op() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    let steps = exec.log.len();
    let out = go_ok(
        &settled(&["kbf-daemon.service"]),
        &mut exec,
        &raw(Mode::Daemon),
    );
    assert_eq!(
        exec.log.len(),
        steps,
        "a step ran: {:?}",
        &exec.log[steps..]
    );
    assert_eq!(out, "nothing to do\n");
}

/// Catches a re-run that ignores a changed flag, or one that rewrites more than it must:
/// only `kbf.toml` is rewritten for a new frontend URL, the token and units are left, and
/// the operator is told a running kbf keeps the old file.
#[test]
fn a_changed_frontend_url_rewrites_only_the_config() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    let steps = exec.log.len();
    let mut changed = raw(Mode::Daemon);
    changed.frontend_url = Some("https://other.example".to_owned());
    let out = go_ok(&settled(&["kbf-daemon.service"]), &mut exec, &changed);
    assert_eq!(exec.log.len(), steps + 1, "{:?}", &exec.log[steps..]);
    assert!(text(&exec, KBF_TOML).contains("url = \"https://other.example\""));
    assert!(out.contains("does not restart"), "{out}");
}

/// Catches a setup that runs as an ordinary user: it refuses before it reads or writes
/// anything.
#[test]
fn a_non_root_run_is_refused_before_any_step() {
    let mut os = FakeOs::linux();
    os.euid = 1000;
    let mut exec = host();
    let err = go(&os, &mut exec, &raw(Mode::Daemon), APPLY);
    assert!(
        matches!(err, Err(SetupError::NotRoot { euid: 1000 })),
        "{err:?}"
    );
    assert!(exec.log.is_empty() && exec.queries.borrow().is_empty());
}

/// Catches root running a binary that anyone else can replace, when it is about to change
/// the host; a dry run changes nothing and is not refused.
#[test]
fn an_untrusted_binary_is_refused_to_apply_but_not_to_look() {
    let mut os = FakeOs::linux();
    os.untrusted = true;
    let err = go(&os, &mut host(), &raw(Mode::Daemon), APPLY);
    assert!(matches!(err, Err(SetupError::Untrusted(_))), "{err:?}");
    let mut exec = host();
    let dry = Options {
        apply: false,
        no_start: false,
    };
    go(&os, &mut exec, &raw(Mode::Daemon), dry).expect("a dry run");
}

/// Catches a dry run that changes the host, or prints steps as done: every step is a
/// `DRY-RUN:` line, none is applied, the closing note says so, and no secret is printed.
#[test]
fn a_dry_run_prints_the_steps_and_changes_nothing() {
    let mut exec = host();
    let dry = Options {
        apply: false,
        no_start: false,
    };
    let out = go(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend), dry).expect("dry run");
    assert!(exec.log.is_empty(), "{:?}", exec.log);
    assert!(
        out.lines()
            .any(|l| l.starts_with("DRY-RUN: write /etc/kbf/kbf.toml")),
        "{out}"
    );
    assert!(!out.contains("\n+ "), "{out}");
    assert!(
        out.contains("dry run: nothing changed. Re-run with --apply."),
        "{out}"
    );
    assert!(!out.contains(SECRET), "{out}");
}

/// Catches the first operator token being made wrong: on a frontend host it is a 64-hex
/// secret in `/etc/kbf/api-token`, mode 0600, owned by the user whose process reads it
/// (`kbf-server`; the server refuses a token file another user owns), `kbf.toml` names it,
/// and the output names the file but never the secret.
#[test]
fn a_frontend_host_gets_its_first_operator_token_once_and_it_is_not_printed() {
    let mut exec = host();
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend));
    assert_eq!(text(&exec, API_TOKEN), format!("{SECRET}\n"));
    assert_eq!(file_meta(&exec, API_TOKEN), (0o600, KBF_SERVER));
    assert_eq!(text(&exec, KBF_TOML), FRONTEND_TOML);
    assert!(out.contains("/etc/kbf/api-token"), "{out}");
    assert!(!out.contains(SECRET), "the secret is never printed: {out}");
    assert!(exec.log.iter().all(|s| !s.to_string().contains(SECRET)));
    // The orders directory belongs to the user the frontend runs as.
    let orders = exec.entries.get(&hp("/var/lib/kbf-installer/orders"));
    assert!(
        matches!(orders, Some(Entry::Dir { owner, .. }) if *owner == KBF_SERVER),
        "{orders:?}"
    );
}

/// Catches a second operator token: once any token file is there, whatever it holds and
/// whoever owns it, a re-run (and a run with another random source) leaves it alone and
/// says nothing about a new one. An all-in-one host gets one like a frontend, a node never.
#[test]
fn an_existing_operator_token_is_never_replaced() {
    let mut exec = host();
    put(
        &mut exec,
        API_TOKEN,
        0o400,
        ROOT,
        "the-operators-own-token\n",
    );
    let mut os = FakeOs::linux();
    os.random_byte = 0x11;
    let out = go_ok(&os, &mut exec, &raw(Mode::Frontend));
    assert_eq!(text(&exec, API_TOKEN), "the-operators-own-token\n");
    assert_eq!(file_meta(&exec, API_TOKEN), (0o400, ROOT));
    assert!(write_index(&exec, API_TOKEN).is_none());
    assert!(!out.contains("operator token"), "{out}");

    let mut again = host();
    go_ok(&FakeOs::linux(), &mut again, &raw(Mode::AllInOne));
    assert_eq!(file_meta(&again, API_TOKEN), (0o600, KBF));
    for mode in [Mode::Storage, Mode::Daemon, Mode::StorageAndDaemon] {
        let mut node = host();
        go_ok(&FakeOs::linux(), &mut node, &raw(mode));
        assert!(!node.entries.contains_key(&hp(API_TOKEN)), "{mode:?}");
    }
}

/// Catches the bad token files an operator hands over: world-readable (the secret is
/// already out), missing, empty, several words, a directory, too large. Each refuses before
/// any write.
#[test]
fn bad_token_files_are_refused() {
    let big = "x".repeat(5000);
    type Plant = Box<dyn Fn(&mut FakeExec)>;
    let cases: Vec<(&str, Plant)> = vec![
        (
            "lets others read",
            Box::new(|e| put(e, SRC, 0o644, RUNNER, "tok\n")),
        ),
        (
            "lets others read",
            Box::new(|e| put(e, SRC, 0o640, RUNNER, "tok\n")),
        ),
        (
            "no such file",
            Box::new(|e| {
                e.entries.remove(&hp(SRC));
            }),
        ),
        ("empty", Box::new(|e| put(e, SRC, 0o600, RUNNER, " \n"))),
        (
            "more than one word",
            Box::new(|e| put(e, SRC, 0o600, RUNNER, "a b\n")),
        ),
        (
            "not text",
            Box::new(|e| {
                let (perms, owner) = (Perms::new(0o600), RUNNER);
                let bytes = vec![0xff, 0xfe];
                e.entries.insert(
                    hp(SRC),
                    Entry::File {
                        perms,
                        owner,
                        bytes,
                    },
                );
            }),
        ),
        (
            "larger than 4 KiB",
            Box::new(move |e| put(e, SRC, 0o600, RUNNER, &big)),
        ),
        (
            "not a regular file",
            Box::new(|e| {
                e.entries.insert(
                    hp(SRC),
                    Entry::Dir {
                        perms: Perms::new(0o700),
                        owner: RUNNER,
                    },
                );
            }),
        ),
    ];
    for (want, plant) in cases {
        let mut exec = host();
        plant(&mut exec);
        let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), APPLY)
            .expect_err(want)
            .to_string();
        assert!(err.contains(want), "{want}: {err}");
        assert!(exec.log.is_empty(), "{want}");
    }
}

/// Catches a setup on a host the package has not prepared: it names the missing user
/// instead of writing files for a service that cannot run. The user is the mode's own
/// (`kbf-server` for a frontend, `kbf` for a daemon).
#[test]
fn a_missing_service_user_is_refused_before_any_write() {
    for (mode, name) in [(Mode::Daemon, "kbf"), (Mode::Frontend, "kbf-server")] {
        let mut os = FakeOs::linux();
        os.users.retain(|u| u.name != name);
        let mut exec = host();
        let err = go(&os, &mut exec, &raw(mode), APPLY);
        assert!(
            matches!(&err, Err(SetupError::NoUser { name: n }) if *n == name),
            "{err:?}"
        );
        assert!(exec.log.is_empty());
    }
}

/// Catches the units of a mode drifting from the package's files, and an enable that starts
/// what should not start: one `systemctl enable` per unit not yet enabled; with a release
/// the mode's units start with `--now`; the installer's service always last.
#[test]
fn each_mode_enables_its_own_units() {
    let want: [(Mode, &[&str]); 5] = [
        (Mode::Frontend, &["kbf-frontend@a.service"]),
        (Mode::Storage, &["kbf-storage.service"]),
        (Mode::Daemon, &["kbf-daemon.service"]),
        (Mode::StorageAndDaemon, &["kbf-storage-and-daemon.service"]),
        (Mode::AllInOne, &["kbf-all-in-one.service"]),
    ];
    for (mode, units) in want {
        let mut exec = released(host(), Platform::Linux);
        go_ok(&FakeOs::linux(), &mut exec, &raw(mode));
        let enables: Vec<String> = cmds(&exec)
            .into_iter()
            .filter(|c| c.contains("systemctl"))
            .collect();
        // A unit that makes a frontend is enabled and left stopped, whatever is installed.
        let now = if mode.keeps_state_in_database() {
            ""
        } else {
            " --now"
        };
        let expected: Vec<String> = units
            .iter()
            .map(|u| format!("run /usr/bin/systemctl enable{now} -- {u}"))
            .chain(["run /usr/bin/systemctl enable --now -- kbf-installer.service".to_owned()])
            .collect();
        assert_eq!(enables, expected, "{mode:?}");
    }
}

/// Catches a unit started where its binary does not exist: with no release installed the
/// mode's units are enabled only (the installer's own service starts, it has nothing to wait
/// for), and `--no-start` enables everything and starts nothing.
#[test]
fn nothing_of_the_mode_starts_without_a_release_or_with_no_start() {
    let mut bare = host();
    let out = go_ok(&FakeOs::linux(), &mut bare, &raw(Mode::Daemon));
    assert_eq!(
        cmds(&bare)
            .into_iter()
            .filter(|c| c.contains("systemctl"))
            .collect::<Vec<_>>(),
        [
            "run /usr/bin/systemctl enable -- kbf-daemon.service",
            "run /usr/bin/systemctl enable --now -- kbf-installer.service",
        ]
    );
    assert!(out.contains("no release is installed yet"), "{out}");
    let mut exec = released(host(), Platform::Linux);
    let none = Options {
        apply: true,
        no_start: true,
    };
    let out = go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), none).expect("setup");
    assert!(
        cmds(&exec).iter().all(|c| !c.contains("--now")),
        "{:?}",
        cmds(&exec)
    );
    assert!(out.contains("--no-start"), "{out}");
}

/// Catches an enable of a unit that is already enabled (a `--now` would then start a unit
/// the operator stopped).
#[test]
fn an_enabled_unit_is_left_alone() {
    let mut exec = released(host(), Platform::Linux);
    let mut os = FakeOs::linux();
    os.enabled.insert("kbf-daemon.service".to_owned());
    go_ok(&os, &mut exec, &raw(Mode::Daemon));
    assert!(
        cmds(&exec)
            .iter()
            .all(|c| !c.contains("kbf-daemon.service"))
    );
}

/// Catches a new subid range that overlaps another user's (a container escape of one
/// account into another), and the grant of a file that already has one.
#[test]
fn a_subid_range_starts_above_every_existing_one() {
    let mut os = FakeOs::linux();
    os.subuid = "alice:100000:65536\nbob:165536:65536\n".to_owned();
    os.subgid = "kbf:200000:65536\n".to_owned();
    let mut exec = released(host(), Platform::Linux);
    go_ok(&os, &mut exec, &raw(Mode::StorageAndDaemon));
    let c = cmds(&exec);
    assert!(
        c.contains(&"run /usr/sbin/usermod --add-subuids 231072-296607 -- kbf".to_owned()),
        "{c:?}"
    );
    assert!(c.iter().all(|c| !c.contains("--add-subgids")), "{c:?}");
}

/// A range keyed by the uid counts as the user's own (subuid(5) allows either).
#[test]
fn a_range_keyed_by_uid_is_the_users() {
    let mut os = FakeOs::linux();
    os.subuid = "990:100000:65536\n".to_owned();
    os.subgid = os.subuid.clone();
    let mut exec = released(host(), Platform::Linux);
    go_ok(&os, &mut exec, &raw(Mode::Daemon));
    assert!(cmds(&exec).iter().all(|c| !c.contains("--add-sub")));
}

/// Catches rootless prerequisites asked of a host that runs no actions.
#[test]
fn a_storage_or_frontend_host_gets_no_subids_or_lingering() {
    for mode in [Mode::Storage, Mode::Frontend] {
        let mut exec = released(host(), Platform::Linux);
        go_ok(&FakeOs::linux(), &mut exec, &raw(mode));
        assert!(
            cmds(&exec)
                .iter()
                .all(|c| !c.contains("usermod") && !c.contains("linger")),
            "{mode:?}"
        );
    }
}

#[test]
fn subid_space_exhaustion_is_a_refusal() {
    let mut os = FakeOs::linux();
    os.subuid = "x:4294900000:60000\n".to_owned();
    let err = go(&os, &mut host(), &raw(Mode::Daemon), APPLY);
    assert!(
        matches!(err, Err(SetupError::SubIdSpace("subuid"))),
        "{err:?}"
    );
}

// installer.toml

/// Catches the settings a host is given going wrong: a frontend host fetches from the file
/// server it was told (url source, no node identity), a node's frontend source lends the
/// pair enrollment writes for its role (daemon's for storage-and-daemon, as the contract
/// says), and `min_generation` reaches the file.
#[test]
fn installer_toml_follows_the_mode_and_the_flags() {
    let mut exec = host();
    let mut r = raw(Mode::Frontend);
    r.min_generation = Some(7);
    go_ok(&FakeOs::linux(), &mut exec, &r);
    let t = text(&exec, INSTALLER_TOML);
    assert!(
        t.starts_with("release_source = \"url\"\nrelease_url = \"https://releases.example/kbf\"\n"),
        "{t}"
    );
    assert!(
        t.contains("min_generation = 7\n") && !t.contains("node_cert"),
        "{t}"
    );

    for (mode, role) in [
        (Mode::Storage, "storage"),
        (Mode::Daemon, "daemon"),
        (Mode::StorageAndDaemon, "daemon"),
    ] {
        let mut exec = host();
        go_ok(&FakeOs::linux(), &mut exec, &raw(mode));
        let t = text(&exec, INSTALLER_TOML);
        assert!(
            t.contains(&format!("node_cert = \"/var/lib/kbf/pki/{role}.pem\"\n")),
            "{mode:?}: {t}"
        );
        assert!(
            t.contains(&format!("node_key = \"/var/lib/kbf/pki/{role}.key\"\n")),
            "{mode:?}: {t}"
        );
    }
    // Moving a node to the url source drops the pair, which the file refuses beside it.
    let mut exec = host();
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    let mut r = raw(Mode::Daemon);
    r.release_source = Some(ReleaseSource::Url);
    r.release_url = Some("https://m.example".to_owned());
    go_ok(&settled(&["kbf-daemon.service"]), &mut exec, &r);
    assert!(!text(&exec, INSTALLER_TOML).contains("node_"));
}

/// Catches a re-run that wipes what the operator tuned: the keys setup has no flag for
/// (confirm window, caps) stay, and the file is not rewritten when nothing changes.
#[test]
fn a_rerun_keeps_the_settings_it_has_no_flag_for() {
    let mut exec = host();
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    let tuned =
        text(&exec, INSTALLER_TOML).replace("confirm_window_s = 900", "confirm_window_s = 300");
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &tuned);
    let steps = exec.log.len();
    go_ok(
        &settled(&["kbf-daemon.service"]),
        &mut exec,
        &raw(Mode::Daemon),
    );
    assert_eq!(exec.log.len(), steps, "{:?}", &exec.log[steps..]);
    let mut changed = raw(Mode::Daemon);
    changed.min_generation = Some(9);
    go_ok(&settled(&["kbf-daemon.service"]), &mut exec, &changed);
    let t = text(&exec, INSTALLER_TOML);
    assert!(
        t.contains("confirm_window_s = 300\n") && t.contains("min_generation = 9\n"),
        "{t}"
    );
}

/// Catches a floor lowered by a re-run (a replay protection undone by a forgotten flag): a
/// lower `--min-generation` refuses (exit 5) and writes nothing; the same or higher goes
/// through, and leaving the flag out keeps the file's.
#[test]
fn the_generation_floor_only_goes_up() {
    let mut exec = host();
    let mut r = raw(Mode::Daemon);
    r.min_generation = Some(10);
    go_ok(&FakeOs::linux(), &mut exec, &r);
    let steps = exec.log.len();
    r.min_generation = Some(9);
    let err = go(&settled(&["kbf-daemon.service"]), &mut exec, &r, APPLY).expect_err("lower");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(err.to_string().contains("only goes up"), "{err}");
    assert_eq!(exec.log.len(), steps);
    r.min_generation = None;
    go_ok(&settled(&["kbf-daemon.service"]), &mut exec, &r);
    assert!(text(&exec, INSTALLER_TOML).contains("min_generation = 10\n"));
}

/// Catches an `installer.toml` the installer would refuse being overwritten or ignored: an
/// invalid file stops setup (exit 5) with the file's own problems, untouched.
#[test]
fn an_invalid_installer_toml_is_left_for_the_operator() {
    let mut exec = host();
    put(
        &mut exec,
        INSTALLER_TOML,
        0o644,
        ROOT,
        "confirm_window_s = 5\nsurprise = 1\n",
    );
    let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(err.to_string().contains("installer.toml"), "{err}");
    assert!(exec.log.is_empty());
    assert_eq!(
        text(&exec, INSTALLER_TOML),
        "confirm_window_s = 5\nsurprise = 1\n"
    );
}

// kbf.toml

/// Catches setup clobbering settings it cannot see: a kbf.toml that holds the mode's own
/// flags is left alone when the keys setup writes already match, and is refused (exit 5,
/// the differing keys named) when they do not; an invalid file is refused too. A file that
/// holds only setup's keys is rewritten.
#[test]
fn a_kbf_toml_with_other_settings_is_kept_or_refused_never_rewritten() {
    let with_flags = format!("{DAEMON_TOML}\n[listener]\nstorage-listen = \"0.0.0.0:7000\"\n");
    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, KBF_TOML, 0o640, ROOT, &with_flags);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    assert_eq!(text(&exec, KBF_TOML), with_flags);
    assert!(out.contains("other settings"), "{out}");

    let mut changed = raw(Mode::Daemon);
    changed.frontend_url = Some("https://other.example".to_owned());
    let mut exec = released(host(), Platform::Linux);
    let other_mode = with_flags.replace("mode = \"daemon\"", "mode = \"storage\"");
    put(&mut exec, KBF_TOML, 0o640, ROOT, &other_mode);
    let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), APPLY).expect_err("refused");
    assert!(
        err.to_string().contains("mode should be \"daemon\""),
        "{err}"
    );
    put(&mut exec, KBF_TOML, 0o640, ROOT, &with_flags);
    let err = go(&FakeOs::linux(), &mut exec, &changed, APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(
        err.to_string()
            .contains("[frontend] url should be \"https://other.example\""),
        "{err}"
    );
    assert_eq!(text(&exec, KBF_TOML), with_flags);

    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, KBF_TOML, 0o640, ROOT, "mode = [");
    let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(err.to_string().contains("not valid TOML"), "{err}");

    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, KBF_TOML, 0o640, ROOT, "mode = \"storage\"\n");
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    assert_eq!(text(&exec, KBF_TOML), DAEMON_TOML);
}

/// Catches a settings file or a config that is not a file being replaced: a directory where
/// `installer.toml` or `kbf.toml` should be stops setup for the operator (exit 5), and
/// setup writes nothing over it.
#[test]
fn a_directory_where_a_config_file_belongs_is_left_for_the_operator() {
    for path in [INSTALLER_TOML, KBF_TOML] {
        let mut exec = released(host(), Platform::Linux);
        exec.entries.insert(
            hp(path),
            Entry::Dir {
                perms: Perms::new(0o755),
                owner: ROOT,
            },
        );
        let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), APPLY).expect_err("refused");
        assert_eq!(err.exit(), Exit::NeedsOperator, "{path}");
        assert!(
            err.to_string()
                .contains(&format!("{path} is not a regular file")),
            "{err}"
        );
        let wrote = format!("write {path}");
        assert!(exec.log.iter().all(|s| !s.to_string().contains(&wrote)));
    }
}

/// Catches a config with the right text and the wrong mode being trusted: kbf.toml must be
/// root's and 0640 for its group, so a copy that is 0644 is put right, with one step and
/// nothing else.
#[test]
fn a_config_with_the_right_text_and_the_wrong_mode_is_corrected() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    let owner = Owner { uid: 0, gid: 990 };
    put(&mut exec, KBF_TOML, 0o644, owner, DAEMON_TOML);
    let steps = exec.log.len();
    go_ok(
        &settled(&["kbf-daemon.service"]),
        &mut exec,
        &raw(Mode::Daemon),
    );
    assert_eq!(exec.log.len(), steps + 1, "{:?}", &exec.log[steps..]);
    assert_eq!(file_meta(&exec, KBF_TOML), (0o640, owner));
}

// The first release

struct Installs(RefCell<Vec<String>>);

impl Releases for Installs {
    fn install_first(&self, commit: &CommitId) -> Result<(), SetupError> {
        self.0.borrow_mut().push(commit.to_string());
        Ok(())
    }
}

fn first_release(mode: Mode) -> Raw {
    let mut r = raw(mode);
    r.commit = Some(COMMIT.parse().expect("a commit"));
    r.release_source = Some(ReleaseSource::Url);
    r.release_url = Some("https://releases.example/kbf".to_owned());
    r
}

/// Catches a first release fetched at the wrong time or from the wrong place: it comes after
/// `installer.toml` (which says where from) and before `kbf.toml`; the mode's units start
/// because it is there; the source is the one `installer.toml` got.
#[test]
fn the_first_release_is_installed_between_the_settings_and_the_config() {
    let installs = Installs(RefCell::default());
    let mut exec = host();
    let out = go_with(
        &FakeOs::linux(),
        &mut exec,
        &installs,
        &first_release(Mode::Daemon),
        APPLY,
    )
    .expect("setup");
    assert_eq!(*installs.0.borrow(), [COMMIT]);
    // The order the output shows: the settings, the fetch, then the config.
    let at = |needle: &str| {
        out.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {out}"))
    };
    let fetch = at(&format!("+ fetch and verify release {COMMIT}"));
    assert!(at(INSTALLER_TOML) < fetch && fetch < at(KBF_TOML), "{out}");
    assert!(
        cmds(&exec)
            .contains(&"run /usr/bin/systemctl enable --now -- kbf-daemon.service".to_owned())
    );
    assert!(
        out.contains(&format!("+ fetch and verify release {COMMIT}")),
        "{out}"
    );
}

/// Catches a setup that says it is done when the release could not be installed: this
/// build has no fetcher, so `--commit` fails (exit 1, saying why) after the settings and
/// before `kbf.toml`, and no unit is enabled.
#[test]
fn a_failed_first_release_leaves_no_config_and_no_unit() {
    let mut exec = host();
    let err = go(
        &FakeOs::linux(),
        &mut exec,
        &first_release(Mode::Daemon),
        APPLY,
    )
    .expect_err("fails");
    assert!(
        matches!(err, SetupError::Fetch(FetchError::NotWired)),
        "{err:?}"
    );
    assert_eq!((err.exit(), err.code()), (Exit::Failed, "not_implemented"));
    assert!(exec.entries.contains_key(&hp(INSTALLER_TOML)));
    assert!(!exec.entries.contains_key(&hp(KBF_TOML)));
    assert!(cmds(&exec).iter().all(|c| !c.contains("systemctl")));
}

/// Catches a release fetched twice or replaced by setup: with one installed, `--commit` is
/// not fetched (updates are the update path's) and the run says so; a dry run names the
/// fetch without doing it and plans the units as starting.
#[test]
fn a_release_already_installed_is_not_fetched_and_a_dry_run_only_names_the_fetch() {
    let installs = Installs(RefCell::default());
    let mut exec = released(host(), Platform::Linux);
    let out = go_with(
        &FakeOs::linux(),
        &mut exec,
        &installs,
        &first_release(Mode::Daemon),
        APPLY,
    )
    .expect("setup");
    assert!(installs.0.borrow().is_empty());
    assert!(out.contains("a release is installed already"), "{out}");

    let mut exec = host();
    let dry = Options {
        apply: false,
        no_start: false,
    };
    let out = go_with(
        &FakeOs::linux(),
        &mut exec,
        &installs,
        &first_release(Mode::Daemon),
        dry,
    )
    .expect("dry run");
    assert!(installs.0.borrow().is_empty());
    assert!(
        out.contains(&format!("DRY-RUN: fetch and verify release {COMMIT}")),
        "{out}"
    );
    assert!(
        out.contains("DRY-RUN: run /usr/bin/systemctl enable --now -- kbf-daemon.service"),
        "{out}"
    );
}

// Flags

fn problems(mutate: impl Fn(&mut Raw)) -> String {
    let mut r = raw(Mode::Daemon);
    mutate(&mut r);
    r.validate().expect_err("refused").join("; ")
}

/// Catches a flag check that stops at the first problem, or lets a bad one through: every
/// problem is named, one line each, and nothing is read or written.
#[test]
fn every_bad_flag_is_named_and_nothing_is_written() {
    let bad = Raw {
        mode: Mode::Daemon,
        frontend_url: Some("http://user@farm.example/x?y#z".to_owned()),
        token_file: Some("rel/token".to_owned()),
        release_source: Some(ReleaseSource::Url),
        release_url: None,
        min_generation: Some(0),
        commit: None,
        database: Some(DATABASE.to_owned()),
        kbf: super::kbf_settings::KbfFlags::default(),
        installer_config: "/tmp/i.toml".to_owned(),
    };
    let mut exec = host();
    let err = go(&FakeOs::linux(), &mut exec, &bad, APPLY).expect_err("refused");
    let SetupError::Invalid(found) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(found.len(), 6, "{found:#?}");
    assert_eq!(err.exit(), Exit::Usage);
    assert!(exec.log.is_empty() && exec.queries.borrow().is_empty());
}

/// Catches the flags of the modes that enroll and of those that do not drifting apart: a
/// node needs its frontend and token; a frontend or all-in-one host takes neither (it does
/// not enroll), and neither may name the frontend as its release source.
#[test]
fn the_modes_that_enroll_need_a_frontend_and_a_token_and_the_others_refuse_them() {
    assert!(problems(|r| r.frontend_url = None).contains("--frontend-url is required"));
    assert!(problems(|r| r.token_file = None).contains("--token-file is required"));
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let mut r = raw(mode);
        r.frontend_url = Some("https://farm.example".to_owned());
        r.token_file = Some(SRC.to_owned());
        let found = r.validate().expect_err("refused").join("; ");
        assert!(
            found.contains("--frontend-url is for the modes that enroll"),
            "{found}"
        );
        assert!(
            found.contains("--token-file is for the modes that enroll"),
            "{found}"
        );
        let mut r = raw(mode);
        r.release_source = Some(ReleaseSource::Frontend);
        r.release_url = None;
        let found = r.validate().expect_err("refused").join("; ");
        assert!(found.contains("does not enroll"), "{found}");
        let mut r = raw(mode);
        r.release_url = None;
        assert!(
            r.validate()
                .expect_err("url needed")
                .join(";")
                .contains("needs --release-url")
        );
    }
}

/// Catches URLs that would send the installer, or a mirror fetch, somewhere unintended.
#[test]
fn urls_are_https_with_nothing_hidden() {
    for url in [
        "http://farm.example",
        "https://u:p@farm.example",
        "https://farm.example/path",
        "https://farm.example?q=1",
        "https://farm.example#f",
        "https://farm.example:0",
        "https://farm.example:99999",
        "https://-bad.example",
        "https://fa rm.example",
        "https://",
        "farm.example",
    ] {
        assert!(
            !problems(|r| r.frontend_url = Some(url.to_owned())).is_empty(),
            "{url}"
        );
    }
    let ok = |url: &str| {
        let mut r = raw(Mode::Daemon);
        r.frontend_url = Some(url.to_owned());
        r.validate().expect("valid").frontend_url
    };
    assert_eq!(
        ok("https://Farm.Example/").as_deref(),
        Some("https://farm.example")
    );
    assert_eq!(
        ok("https://[::1]:8443").as_deref(),
        Some("https://[::1]:8443")
    );
}

#[test]
fn the_release_source_flags_agree() {
    let check = |s: ReleaseSource, u: Option<&str>| {
        let mut r = raw(Mode::Daemon);
        r.release_source = Some(s);
        r.release_url = u.map(str::to_owned);
        r.validate()
    };
    assert!(check(ReleaseSource::Url, None).is_err());
    assert!(check(ReleaseSource::Frontend, Some("https://m.example")).is_err());
    assert!(check(ReleaseSource::Url, Some("http://m.example")).is_err());
    assert!(check(ReleaseSource::Url, Some("https://m.example/a/../b")).is_err());
    let ok = check(ReleaseSource::Url, Some("https://M.example:8443/releases/")).expect("valid");
    assert_eq!(
        ok.source,
        Source::Url("https://m.example:8443/releases".to_owned())
    );
    assert_eq!(
        check(ReleaseSource::Frontend, None).expect("valid").source,
        Source::Frontend
    );
}

/// Catches `--commit` with the frontend as source: before the host has enrolled there is
/// no identity to fetch with, so the first release must come from a file server.
#[test]
fn a_first_release_cannot_come_from_the_frontend() {
    let mut r = raw(Mode::Daemon);
    r.commit = Some(COMMIT.parse().expect("commit"));
    let found = r.validate().expect_err("refused").join("; ");
    assert!(found.contains("cannot come from the frontend"), "{found}");
}

/// The exit code and word of each way setup stops, which scripts branch on.
#[test]
fn each_failure_has_its_exit_code_and_word() {
    let cases: Vec<(SetupError, Exit, &str)> = vec![
        (SetupError::NotRoot { euid: 5 }, Exit::Usage, "needs_root"),
        (SetupError::Invalid(vec![]), Exit::Usage, "invalid_flags"),
        (SetupError::Token("x".into()), Exit::Usage, "token_file"),
        (SetupError::NoUser { name: "kbf" }, Exit::Usage, "no_user"),
        (
            SetupError::NeedsOperator("x".into()),
            Exit::NeedsOperator,
            "needs_operator",
        ),
        (
            SetupError::Fetch(FetchError::NotWired),
            Exit::Failed,
            "not_implemented",
        ),
        (
            SetupError::Fetch(FetchError::Rejected("x".into())),
            Exit::Refused,
            "fetch",
        ),
        (
            SetupError::Fetch(FetchError::Unavailable("x".into())),
            Exit::Failed,
            "fetch",
        ),
        (
            SetupError::OldLayout("/x".into()),
            Exit::NeedsOperator,
            "old_layout",
        ),
        (
            SetupError::Release(crate::release::ReleaseError::NoPrevious),
            Exit::Failed,
            "release",
        ),
        (SetupError::Database("x".into()), Exit::Failed, "database"),
        (SetupError::NoFreeId, Exit::Failed, "no_room"),
    ];
    for (err, exit, code) in cases {
        assert_eq!((err.exit(), err.code()), (exit, code), "{err}");
    }
}

// macOS

/// A Mac is a node: no frontend, no subids, a hidden fetch account made with `dscl` at a
/// free id, the launchd daemon bootstrapped only with a release; and the orders directory
/// and token are root's (the daemon is root there).
#[test]
fn a_mac_node_gets_its_fetch_account_and_launchd_jobs() {
    let mut os = FakeOs::mac();
    os.dscl_users = "root 0\n_kbf-pool-0 300\n_other 301\n".to_owned();
    os.dscl_groups = "wheel 0\n_x 302\n".to_owned();
    let mut exec = released(host(), Platform::MacOs);
    go_ok(&os, &mut exec, &raw(Mode::Daemon));
    let c = cmds(&exec);
    assert_eq!(c.len(), 8, "{c:?}");
    assert_eq!(
        c[0],
        "run /usr/bin/dscl . -create /Groups/_kbf-fetch PrimaryGroupID 303"
    );
    assert_eq!(
        c[1],
        concat!(
            "run /usr/bin/dscl . -create /User",
            "s/_kbf-fetch UniqueID 303"
        )
    );
    assert_eq!(
        c[6],
        "run /bin/launchctl bootstrap system /Library/LaunchDaemons/com.komira.kbf-installer.plist"
    );
    assert_eq!(
        c[7],
        "run /bin/launchctl bootstrap system /Library/LaunchDaemons/com.komira.kbf-daemon.plist"
    );
    assert_eq!(file_meta(&exec, ENROLL_TOKEN), (0o600, ROOT));
    assert_eq!(file_meta(&exec, KBF_TOML), (0o640, ROOT));
    let orders = exec.entries.get(&hp("/var/db/kbf-installer/orders"));
    assert!(matches!(
        orders,
        Some(Entry::Dir {
            owner: Owner { uid: 0, .. },
            ..
        })
    ));
    assert!(text(&exec, INSTALLER_TOML).contains("node_cert = \"/var/db/kbf/pki/daemon.pem\""));
}

#[test]
fn a_mac_refuses_the_frontend_and_waits_for_a_release() {
    let err = go(&FakeOs::mac(), &mut host(), &raw(Mode::Frontend), APPLY);
    assert!(matches!(err, Err(SetupError::Unsupported(_))), "{err:?}");
    let mut exec = host();
    let out = go_ok(&FakeOs::mac(), &mut exec, &raw(Mode::Daemon));
    assert!(
        cmds(&exec).iter().all(|c| !c.contains("kbf-daemon.plist")),
        "{:?}",
        cmds(&exec)
    );
    assert!(out.contains("no release"), "{out}");
    let mut os = FakeOs::mac();
    os.loaded.insert("com.komira.kbf-daemon".to_owned());
    os.loaded.insert("com.komira.kbf-installer".to_owned());
    let mut exec = released(host(), Platform::MacOs);
    go_ok(&os, &mut exec, &raw(Mode::Daemon));
    assert!(cmds(&exec).iter().all(|c| !c.contains("launchctl")));
}

#[test]
fn no_free_mac_id_is_a_refusal() {
    let mut os = FakeOs::mac();
    os.dscl_users = (300..400).map(|i| format!("u{i} {i}\n")).collect();
    let err = go(&os, &mut host(), &raw(Mode::Daemon), APPLY);
    assert!(matches!(err, Err(SetupError::NoFreeId)), "{err:?}");
}

// The real host, read through an `Exec`

fn answer(exec: &mut FakeExec) {
    use kbf_host::testkit::reply;
    exec.answer = Box::new(|cmd| {
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        let program = cmd
            .program
            .as_str()
            .rsplit('/')
            .next()
            .expect("a name")
            .to_owned();
        Ok(match (program.as_str(), args.as_slice()) {
            ("getent", ["passwd", "--", "kbf"]) => {
                reply(0, "kbf:x:990:990:kbf:/var/lib/kbf:/usr/sbin/nologin\n", "")
            }
            ("getent", _) => reply(2, "", ""),
            ("id", ["-G", "--", "kbf"]) => reply(0, "990\n", ""),
            ("systemctl", ["is-enabled", "--", "kbf-daemon.service"]) => reply(0, "enabled\n", ""),
            ("systemctl", ["is-enabled", "--", "kbf-static.service"]) => reply(0, "static\n", ""),
            ("systemctl", _) => reply(1, "disabled\n", ""),
            ("launchctl", ["print", "system/com.komira.kbf-daemon"]) => reply(0, "", ""),
            ("launchctl", _) => reply(113, "", "not found"),
            ("dscl", [".", "-list", "/Users", "UniqueID"]) => reply(0, "root 0\n", ""),
            _ => reply(1, "", "no such list"),
        })
    });
}

/// Catches the production `Os` reading the host wrong: it asks `getent`/`id` for a user, reads
/// the subid files and the lingering marker as files (a directory or a missing one is "none"),
/// takes a unit as enabled only when `systemctl is-enabled` exits 0 and says `enabled` (not
/// `static`), a launchd job as loaded when `print` succeeds, and a failing `dscl` as an error.
#[test]
fn the_system_os_reads_the_host_through_exec() {
    use super::os::{Os, SystemOs};
    let mut exec = FakeExec::new(0);
    answer(&mut exec);
    put(&mut exec, "/etc/subuid", 0o644, ROOT, "kbf:100000:65536\n");
    exec.entries.insert(
        hp("/etc/subgid"),
        Entry::Dir {
            perms: Perms::new(0o755),
            owner: ROOT,
        },
    );
    put(&mut exec, "/var/lib/systemd/linger/kbf", 0o644, ROOT, "");
    let os = SystemOs::new(&exec, Platform::Linux);
    assert_eq!((os.euid(), os.platform()), (0, Platform::Linux));
    assert_eq!(os.root_owner(), ROOT);
    assert!(os.trusted_binary().is_ok());
    assert_eq!(os.user("kbf").expect("lookup").expect("a user").uid, 990);
    assert!(os.user("nobody-here").expect("lookup").is_none());
    assert_eq!(os.subuid().expect("read"), "kbf:100000:65536\n");
    assert_eq!(
        os.subgid().expect("read"),
        "",
        "a directory is no subid file"
    );
    assert!(os.lingers("kbf").expect("stat"));
    assert!(!os.lingers("nobody-here").expect("stat"));
    assert!(os.unit_enabled("kbf-daemon.service").expect("query"));
    assert!(
        !os.unit_enabled("kbf-static.service")
            .expect("a static unit is not enabled")
    );
    assert!(!os.unit_enabled("kbf-storage.service").expect("query"));
    assert!(os.launchd_loaded("com.komira.kbf-daemon").expect("query"));
    assert!(!os.launchd_loaded("com.komira.other").expect("query"));
    assert_eq!(os.dscl_list("/Users UniqueID").expect("list"), "root 0\n");
    assert!(os.dscl_list("/Groups PrimaryGroupID").is_err());
    let mut exec = FakeExec::new(0);
    exec.untrusted = true;
    assert!(
        SystemOs::new(&exec, Platform::MacOs)
            .trusted_binary()
            .is_err()
    );
}

/// Catches an operator token that is not random: the real source gives the length asked, and
/// two draws differ (a constant would make every host's first token the same).
#[test]
fn the_system_os_draws_random_bytes() {
    use super::os::{Os, SystemOs};
    let exec = FakeExec::new(0);
    let os = SystemOs::new(&exec, Platform::Linux);
    let (a, b) = (
        os.random(32).expect("random"),
        os.random(32).expect("random"),
    );
    assert_eq!((a.len(), b.len()), (32, 32));
    assert_ne!(a, b);
    assert!(os.random(0).expect("random").is_empty());
}

/// Every way setup stops says what is wrong in a line a person can act on.
#[test]
fn every_failure_reads_as_a_sentence() {
    let cases: Vec<(SetupError, &str)> = vec![
        (SetupError::NotRoot { euid: 5 }, "run it with sudo"),
        (
            SetupError::Untrusted(kbf_host::Untrusted::Writable {
                path: "/x".into(),
                mode: 0o775,
            }),
            "binary",
        ),
        (SetupError::Invalid(vec!["a".into(), "b".into()]), "a\nb"),
        (
            SetupError::Token("/t: empty".into()),
            "token file /t: empty",
        ),
        (SetupError::Unsupported("no".into()), "no"),
        (SetupError::NoUser { name: "kbf" }, "no user kbf"),
        (SetupError::SubIdSpace("subuid"), "subuid range"),
        (SetupError::NoFreeId, "between 300 and 399"),
        (SetupError::NeedsOperator("fix it".into()), "fix it"),
        (
            SetupError::Fetch(FetchError::NotWired),
            "no release fetcher",
        ),
        (
            SetupError::OldLayout("/usr/local/kbf".into()),
            "/usr/local/kbf",
        ),
        (
            SetupError::Release(crate::release::ReleaseError::NoPrevious),
            "no previous release",
        ),
        (SetupError::Database("down".into()), "database step: down"),
        (
            SetupError::Path(kbf_host::HostPath::new("rel").expect_err("relative")),
            "rel",
        ),
        (SetupError::Io(std::io::Error::other("disk")), "disk"),
        (
            SetupError::Host(kbf_host::HostError::IsRoot),
            "without sudo",
        ),
    ];
    for (err, want) in cases {
        assert!(err.to_string().contains(want), "{err}: want {want}");
        let _ = err.code();
        let _ = err.exit();
    }
}

/// Catches the view a plan reads the host through forwarding wrongly: every read goes to the
/// host it wraps, and a step is refused (a plan is made by reading and applies nothing).
#[test]
fn a_view_forwards_reads_and_refuses_steps() {
    use super::os::View;
    let mut fake = FakeExec::new(7);
    put(&mut fake, "/etc/x", 0o600, RUNNER, "x");
    fake.http = Ok(204);
    let mut view = View(&fake);
    assert_eq!(view.euid(), 7);
    assert!(view.inspect(&hp("/etc/x")).expect("read").is_some());
    let meta = view.stat(&hp("/etc/x")).expect("stat").expect("there");
    assert_eq!(meta.len, 1);
    let cmd = kbf_host::Cmd::new(hp("/bin/true"));
    assert!(view.query(&cmd).expect("query").status.success());
    let url = kbf_host::HttpUrl::new("http://127.0.0.1:1/").expect("url");
    assert_eq!(view.http_status(&url).expect("status"), 204);
    assert!(view.check_binary().is_ok());
    let step = Step::Remove { path: hp("/etc/x") };
    assert!(view.apply(&step).is_err());
}

#[path = "more_tests.rs"]
mod more;
