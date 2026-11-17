//! Tests of `setup` on a fake host. Each says the defect it catches.

use std::cell::RefCell;
use std::rc::Rc;

use kbf_host::testkit::{FakeExec, RUNNER};
use kbf_host::{Entry, Exec, HostPath, Owner, Perms, Step};

use kbf_state::postgres::Unreached;
use kbf_state::postgres::schema::Outcome;

use super::backend::{Backend, Schemas};
use super::fake::{FakeOs, user};
use super::plan::mode_units;
use super::retired::RetiredFlags;
use super::validate::Raw;
use super::{Options, SetupError, run};
use crate::cli::Mode;
use crate::database::SchemaOps;
use crate::exit::Exit;
use crate::layout::{Layout, Platform};

const DATABASE: &str = "postgres:///kbf?host=/run/postgresql&user=kbf-server";

const SRC: &str = "/root/kbf.token";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const SECRET: &str = "abababababababababababababababababababababababababababababababab";
/// What `setup` writes.
const KBF_TOML: &str = "/etc/kbf/kbf.toml";
const INSTALLER_TOML: &str = "/etc/kbf/installer.toml";
/// The release source an earlier setup wrote for a host that fetched from a file server, as the
/// goldens of `installer.toml` keep it.
const EARLIER_SOURCE: &str =
    "release_source = \"url\"\nrelease_url = \"https://releases.example/kbf\"\n";
/// The release source setup writes where the host has no `installer.toml`: the default.
const DEFAULT_SOURCE: &str = "release_source = \"frontend\"\n";
const ENROLL_TOKEN: &str = "/etc/kbf/enroll.token";
const API_TOKEN: &str = "/etc/kbf/api-token";
const ROOT: Owner = Owner { uid: 0, gid: 0 };
const KBF_SERVER: Owner = Owner { uid: 989, gid: 989 };

/// The flags of a node mode, or of a frontend host.
fn raw(mode: Mode) -> Raw {
    let node = mode == Mode::Storage;
    Raw {
        mode,
        frontend_url: node.then(|| "https://farm.example:8443".to_owned()),
        token_file: node.then(|| SRC.to_owned()),
        min_generation: None,
        retired: RetiredFlags::default(),
        database: mode.keeps_state_in_database().then(|| DATABASE.to_owned()),
        kbf: serving_flags(mode),
        create_local_database: false,
        installer_config: None,
    }
}

/// The `kbf.toml` flags a frontend host cannot be set up without, with nothing else (the files
/// they name are in [`host`]); none for a node.
fn serving_flags(mode: Mode) -> super::kbf_settings::KbfFlags {
    let some = |s: &str| Some(s.to_owned());
    match mode {
        Mode::Frontend => super::kbf_settings::KbfFlags {
            storage_url: some("http://127.0.0.1:8990"),
            ..Default::default()
        },
        Mode::Storage => Default::default(),
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

/// What the fake server was asked, in order: `plan` and `setup`, each with the URL.
type Asked = Rc<RefCell<Vec<String>>>;

/// A server at schema version `found` (`None`: a fresh database) that answers as told.
struct Server {
    found: Option<u32>,
    down: bool,
    /// What a login is refused with, when it is.
    unreached: Option<Unreached>,
    asked: Asked,
}

impl Server {
    fn fresh() -> Server {
        Server {
            found: None,
            down: false,
            unreached: None,
            asked: Asked::default(),
        }
    }
}

struct Session {
    found: Option<u32>,
    down: bool,
    unreached: Option<Unreached>,
    url: String,
    asked: Asked,
}

impl SchemaOps for Session {
    fn reach(&self) -> Result<(), Unreached> {
        self.asked.borrow_mut().push(format!("reach {}", self.url));
        self.unreached.clone().map_or(Ok(()), Err)
    }

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
            unreached: self.unreached.clone(),
            url: url.to_owned(),
            asked: self.asked.clone(),
        }))
    }
}

fn go(os: &FakeOs, exec: &mut FakeExec, raw: &Raw, options: Options) -> Result<String, SetupError> {
    go_on(os, exec, &Server::fresh(), raw, options)
}

fn go_on(
    os: &FakeOs,
    exec: &mut FakeExec,
    server: &Server,
    raw: &Raw,
    options: Options,
) -> Result<String, SetupError> {
    let mut out = Vec::new();
    let backend = Backend { schemas: server };
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

/// What a finished storage setup leaves, as the host's facts.
fn settled(units: &[&str]) -> FakeOs {
    let mut os = FakeOs::linux();
    os.users.push(user("kbf-fetch", 991));
    for u in units {
        os.enabled.insert((*u).to_owned());
    }
    os
}

const STORAGE_TOML: &str =
    "# Written by `kbf-installer setup`; run setup again to change these keys. Another setting
# goes into the table it belongs to (add the key there, never a second table of the same name).
mode = \"storage\"

[frontend]
url = \"https://farm.example:8443\"

[enroll]
token_file = \"/etc/kbf/enroll.token\"
";

const FRONTEND_TOML: &str =
    "# Written by `kbf-installer setup`; run setup again to change these keys. Another setting
# goes into the table it belongs to (add the key there, never a second table of the same name).
mode = \"frontend\"

[listener]
api-listen = \"127.0.0.1:8982\"

[tls]
api-token-file = \"/etc/kbf/api-token\"

[storage-client]
storage = \"http://127.0.0.1:8990\"

[scheduler]
state-store = \"postgres:///kbf?host=/run/postgresql&user=kbf-server\"
";

const STORAGE_INSTALLER_TOML: &str = "release_source = \"frontend\"
confirm_window_s = 900
confirm_window_max_s = 1800
min_generation = 0
min_protocol = 0
fetch_timeout_s = 900
max_component_bytes = 536870912
min_activation_interval_s = 60
node_cert = \"/var/lib/kbf/pki/storage.pem\"
node_key = \"/var/lib/kbf/pki/storage.key\"
";

/// Catches a setup that leaves a prerequisite out or in the wrong order: the whole storage
/// plan on a bare host, step for step, and the two files it writes byte for byte (the
/// golden texts are what `kbf-enroll`'s loader and `InstallerToml::parse` read back).
#[test]
fn a_bare_storage_host_gets_accounts_dirs_token_settings_config_then_units() {
    let mut exec = released(host(), Platform::Linux);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
    assert_eq!(
        cmds(&exec),
        [
            "run /usr/sbin/useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin -- kbf-fetch",
            "run /usr/bin/systemctl enable --now -- kbf-storage.service",
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
            ("/var/lib/kbf-installer/orders", 0o700, Some(KBF_SERVER)),
        ]
    );
    assert_eq!(text(&exec, ENROLL_TOKEN), "tok-abc123\n");
    assert_eq!(file_meta(&exec, ENROLL_TOKEN), (0o600, KBF_SERVER));
    assert_eq!(text(&exec, KBF_TOML), STORAGE_TOML);
    assert_eq!(
        file_meta(&exec, KBF_TOML),
        (0o640, Owner { uid: 0, gid: 989 })
    );
    assert_eq!(text(&exec, INSTALLER_TOML), STORAGE_INSTALLER_TOML);
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
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
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
            go_ok(&FakeOs::linux(), &mut probe, &raw(Mode::Storage));
            write_index(&probe, victim).expect("step")
        };
        exec.fail_at = Some(at);
        let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Storage), APPLY);
        assert!(matches!(err, Err(SetupError::Host(_))), "{victim}: {err:?}");
        assert!(!exec.entries.contains_key(&hp(KBF_TOML)), "{victim}");
    }
}

/// Catches a re-run that does work: the same flags on a finished host change nothing, and
/// say so. Also catches a plan that compares a file's owner or mode loosely.
#[test]
fn a_rerun_with_the_same_flags_is_a_no_op() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
    let steps = exec.log.len();
    let out = go_ok(
        &settled(&["kbf-storage.service"]),
        &mut exec,
        &raw(Mode::Storage),
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
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
    let steps = exec.log.len();
    let mut changed = raw(Mode::Storage);
    changed.frontend_url = Some("https://other.example".to_owned());
    let out = go_ok(&settled(&["kbf-storage.service"]), &mut exec, &changed);
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
    let err = go(&os, &mut exec, &raw(Mode::Storage), APPLY);
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
    let err = go(&os, &mut host(), &raw(Mode::Storage), APPLY);
    assert!(matches!(err, Err(SetupError::Untrusted(_))), "{err:?}");
    let mut exec = host();
    let dry = Options {
        apply: false,
        no_start: false,
    };
    go(&os, &mut exec, &raw(Mode::Storage), dry).expect("a dry run");
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
/// says nothing about a new one. A node never gets one.
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

    let mut node = host();
    go_ok(&FakeOs::linux(), &mut node, &raw(Mode::Storage));
    assert!(!node.entries.contains_key(&hp(API_TOKEN)));
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
        let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Storage), APPLY)
            .expect_err(want)
            .to_string();
        assert!(err.contains(want), "{want}: {err}");
        assert!(exec.log.is_empty(), "{want}");
    }
}

/// Catches a setup on a host the package has not prepared: it names the missing user
/// instead of writing files for a service that cannot run. The user is the mode's own
/// (`kbf-server`, for a frontend and a storage node alike).
#[test]
fn a_missing_service_user_is_refused_before_any_write() {
    for (mode, name) in [
        (Mode::Storage, "kbf-server"),
        (Mode::Frontend, "kbf-server"),
    ] {
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
/// the mode's units start with `--now`; the installer's service always last. Asserts that
/// `mode_units(mode)` names the same units, with no proxy unit.
#[test]
fn each_mode_enables_its_own_units() {
    let want: [(Mode, &[&str]); 2] = [
        (Mode::Frontend, &["kbf-frontend.service"]),
        (Mode::Storage, &["kbf-storage.service"]),
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
            .collect();
        assert_eq!(enables, expected, "{mode:?}");
        assert_eq!(mode_units(mode), units, "{mode:?}");
    }
}

/// Catches a unit started where its binary does not exist: with no release installed the
/// mode's units are enabled only (the installer's own service starts, it has nothing to wait
/// for), and `--no-start` enables everything and starts nothing.
#[test]
fn nothing_of_the_mode_starts_without_a_release_or_with_no_start() {
    let mut bare = host();
    let out = go_ok(&FakeOs::linux(), &mut bare, &raw(Mode::Storage));
    assert_eq!(
        cmds(&bare)
            .into_iter()
            .filter(|c| c.contains("systemctl"))
            .collect::<Vec<_>>(),
        ["run /usr/bin/systemctl enable -- kbf-storage.service",]
    );
    assert!(out.contains("no release is installed yet"), "{out}");
    let mut exec = released(host(), Platform::Linux);
    let none = Options {
        apply: true,
        no_start: true,
    };
    let out = go(&FakeOs::linux(), &mut exec, &raw(Mode::Storage), none).expect("setup");
    assert!(
        cmds(&exec).iter().all(|c| !c.contains("--now")),
        "{:?}",
        cmds(&exec)
    );
    assert!(out.contains("--no-start"), "{out}");
}

/// Asserts that a node host with the kbf package (`/usr/bin/kbf`) and no `releases/current`
/// has its mode's unit started, as a host with a release has, and is not told that no release
/// is installed; and that a frontend host with only the package still has its unit enabled and
/// left stopped, because starting a frontend is the operator's.
#[test]
fn a_host_with_the_kbf_package_and_no_release_tree_starts_its_units() {
    let mut exec = host();
    put(&mut exec, "/usr/bin/kbf", 0o755, ROOT, "kbf");
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
    let systemctl: Vec<String> = cmds(&exec)
        .into_iter()
        .filter(|c| c.contains("systemctl"))
        .collect();
    assert_eq!(
        systemctl,
        ["run /usr/bin/systemctl enable --now -- kbf-storage.service",]
    );
    assert!(!out.contains("no release is installed yet"), "{out}");
    let mut exec = host();
    put(&mut exec, "/usr/bin/kbf", 0o755, ROOT, "kbf");
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend));
    let systemctl: Vec<String> = cmds(&exec)
        .into_iter()
        .filter(|c| c.contains("systemctl"))
        .collect();
    assert_eq!(
        systemctl,
        ["run /usr/bin/systemctl enable -- kbf-frontend.service",]
    );
}

/// Catches an enable of a unit that is already enabled (a `--now` would then start a unit
/// the operator stopped).
#[test]
fn an_enabled_unit_is_left_alone() {
    let mut exec = released(host(), Platform::Linux);
    let mut os = FakeOs::linux();
    os.enabled.insert("kbf-storage.service".to_owned());
    go_ok(&os, &mut exec, &raw(Mode::Storage));
    assert!(
        cmds(&exec)
            .iter()
            .all(|c| !c.contains("kbf-storage.service"))
    );
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

// installer.toml

/// Asserts that the settings a host is given follow its mode and flags: a frontend host gets the
/// default source and no node identity, a node's frontend source lends the pair enrollment
/// writes for its role (storage's, as the contract says), and `min_generation` reaches the
/// file.
#[test]
fn installer_toml_follows_the_mode_and_the_flags() {
    let mut exec = host();
    let mut r = raw(Mode::Frontend);
    r.min_generation = Some(7);
    go_ok(&FakeOs::linux(), &mut exec, &r);
    let t = text(&exec, INSTALLER_TOML);
    assert!(t.starts_with(DEFAULT_SOURCE), "{t}");
    assert!(t.contains("min_generation = 7\n"), "{t}");
    assert!(!t.contains("node_cert"), "{t}");

    let mut exec = host();
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
    let t = text(&exec, INSTALLER_TOML);
    assert!(
        t.contains("node_cert = \"/var/lib/kbf/pki/storage.pem\"\n"),
        "{t}"
    );
    assert!(
        t.contains("node_key = \"/var/lib/kbf/pki/storage.key\"\n"),
        "{t}"
    );
}

/// Asserts that the release source an earlier setup wrote is kept: over an `installer.toml` with
/// a `url` source and the file server's CA, setup leaves the source and the key as they are, in a
/// node mode and in a frontend mode alike, and a node whose file has a source other than the
/// frontend's gets no node identity, which only the frontend source takes.
#[test]
fn the_release_source_an_earlier_setup_wrote_is_kept() {
    let earlier = format!("{EARLIER_SOURCE}release_ca = \"/var/lib/kbf-installer/ca.pem\"\n");
    for mode in [Mode::Storage, Mode::Frontend] {
        let mut exec = host();
        put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &earlier);
        go_ok(&FakeOs::linux(), &mut exec, &raw(mode));
        let t = text(&exec, INSTALLER_TOML);
        assert!(t.starts_with(&earlier), "{mode:?}: {t}");
        assert!(!t.contains("node_"), "{mode:?}: {t}");
    }
}

/// Asserts that setup on a host with no release installs none: the file `releases/current` is
/// not made, nothing is fetched or announced, and the files it writes are the same as before,
/// with the release source at its default (a node's two files byte for byte).
#[test]
fn setup_on_a_host_with_no_release_installs_none_and_writes_the_same_files() {
    for mode in [Mode::Storage, Mode::Frontend] {
        let mut exec = host();
        let out = go_ok(&FakeOs::linux(), &mut exec, &raw(mode));
        let current = current_release(Platform::Linux);
        assert!(!exec.entries.contains_key(&hp(&current)), "{mode:?}");
        let paths: Vec<&str> = exec.entries.keys().map(HostPath::as_str).collect();
        assert!(
            paths.iter().all(|path| !path.contains("releases")),
            "{mode:?}: {paths:?}"
        );
        assert!(!out.contains("fetch and verify"), "{mode:?}: {out}");
        assert!(!out.contains("package carries"), "{mode:?}: {out}");
    }
    let mut exec = host();
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
    assert_eq!(text(&exec, KBF_TOML), STORAGE_TOML);
    assert_eq!(text(&exec, INSTALLER_TOML), STORAGE_INSTALLER_TOML);
    assert!(out.contains("no release is installed yet"), "{out}");
}

/// Catches a re-run that wipes what the operator tuned: the keys setup has no flag for
/// (confirm window, caps) stay, and the file is not rewritten when nothing changes.
#[test]
fn a_rerun_keeps_the_settings_it_has_no_flag_for() {
    let mut exec = host();
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
    let tuned =
        text(&exec, INSTALLER_TOML).replace("confirm_window_s = 900", "confirm_window_s = 300");
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &tuned);
    let steps = exec.log.len();
    go_ok(
        &settled(&["kbf-storage.service"]),
        &mut exec,
        &raw(Mode::Storage),
    );
    assert_eq!(exec.log.len(), steps, "{:?}", &exec.log[steps..]);
    let mut changed = raw(Mode::Storage);
    changed.min_generation = Some(9);
    go_ok(&settled(&["kbf-storage.service"]), &mut exec, &changed);
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
    let mut r = raw(Mode::Storage);
    r.min_generation = Some(10);
    go_ok(&FakeOs::linux(), &mut exec, &r);
    let steps = exec.log.len();
    r.min_generation = Some(9);
    let err = go(&settled(&["kbf-storage.service"]), &mut exec, &r, APPLY).expect_err("lower");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(err.to_string().contains("only goes up"), "{err}");
    assert_eq!(exec.log.len(), steps);
    r.min_generation = None;
    go_ok(&settled(&["kbf-storage.service"]), &mut exec, &r);
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
    let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Storage), APPLY).expect_err("refused");
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
    let with_flags = format!("{STORAGE_TOML}\n[listener]\nstorage-listen = \"0.0.0.0:7000\"\n");
    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, KBF_TOML, 0o640, ROOT, &with_flags);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
    assert_eq!(text(&exec, KBF_TOML), with_flags);
    assert!(out.contains("other settings"), "{out}");

    let mut changed = raw(Mode::Storage);
    changed.frontend_url = Some("https://other.example".to_owned());
    let mut exec = released(host(), Platform::Linux);
    let other_mode = with_flags.replace("mode = \"storage\"", "mode = \"frontend\"");
    put(&mut exec, KBF_TOML, 0o640, ROOT, &other_mode);
    let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Storage), APPLY).expect_err("refused");
    assert!(
        err.to_string().contains("mode should be \"storage\""),
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
    let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Storage), APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(err.to_string().contains("not valid TOML"), "{err}");

    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, KBF_TOML, 0o640, ROOT, "mode = \"storage\"\n");
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
    assert_eq!(text(&exec, KBF_TOML), STORAGE_TOML);
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
        let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Storage), APPLY).expect_err("refused");
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
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Storage));
    let owner = Owner { uid: 0, gid: 989 };
    put(&mut exec, KBF_TOML, 0o644, owner, STORAGE_TOML);
    let steps = exec.log.len();
    go_ok(
        &settled(&["kbf-storage.service"]),
        &mut exec,
        &raw(Mode::Storage),
    );
    assert_eq!(exec.log.len(), steps + 1, "{:?}", &exec.log[steps..]);
    assert_eq!(file_meta(&exec, KBF_TOML), (0o640, owner));
}

// The retired flags

/// The flags of the release source, the release keys and the first release, every one given.
fn retired() -> RetiredFlags {
    let some = |text: &str| Some(text.to_owned());
    RetiredFlags {
        release_source: some("url"),
        release_url: some("https://releases.example/kbf"),
        release_ca: some("/var/lib/kbf-installer/release-ca.pem"),
        frontend_worker_url: some("https://frontend.lan:8981"),
        frontend_ca: some("/var/lib/kbf-installer/frontend-ca.pem"),
        frontend_ca_sha256: some("ab"),
        commit: some(COMMIT),
    }
}

/// Asserts that the flags of the release source, the release keys and the first release change
/// nothing: setup with all of them plans the steps, writes the files and prints the notes that
/// setup without them does, installs no release, and says once, ahead of everything else, that
/// it ignored them.
#[test]
fn the_retired_flags_are_ignored_and_warned_about_once() {
    let warning = retired().warning().expect("a warning");
    for mode in [Mode::Storage, Mode::Frontend] {
        let mut plain = host();
        let said = go_ok(&FakeOs::linux(), &mut plain, &raw(mode));
        let mut flagged = host();
        let with = Raw {
            retired: retired(),
            ..raw(mode)
        };
        let out = go_ok(&FakeOs::linux(), &mut flagged, &with);
        assert_eq!(out, format!("{warning}\n{said}"), "{mode:?}");
        let steps =
            |exec: &FakeExec| -> Vec<String> { exec.log.iter().map(ToString::to_string).collect() };
        assert_eq!(steps(&flagged), steps(&plain), "{mode:?}");
        for path in [INSTALLER_TOML, KBF_TOML] {
            let want = text(&plain, path);
            assert_eq!(text(&flagged, path), want, "{mode:?}: {path}");
        }
        let current = current_release(Platform::Linux);
        assert!(!flagged.entries.contains_key(&hp(&current)), "{mode:?}");
        assert_eq!(out.matches("warning:").count(), 1, "{mode:?}: {out}");
    }
}

/// Asserts that a dry run warns too, and that a run that gives none of the retired flags does
/// not.
#[test]
fn the_warning_comes_with_a_dry_run_and_only_when_a_retired_flag_was_given() {
    let dry = Options {
        apply: false,
        no_start: false,
    };
    let with = Raw {
        retired: retired(),
        ..raw(Mode::Storage)
    };
    let out = go(&FakeOs::linux(), &mut host(), &with, dry).expect("dry run");
    assert!(
        out.starts_with("warning: ignored: --release-source"),
        "{out}"
    );
    let out = go(&FakeOs::linux(), &mut host(), &raw(Mode::Storage), dry).expect("dry run");
    assert!(!out.contains("warning"), "{out}");
}

/// Asserts that the warning names the retired flags that were given, and only those, in the
/// order the help lists them, and that there is no warning when none was given.
#[test]
fn the_warning_names_the_retired_flags_that_were_given_and_only_those() {
    assert_eq!(RetiredFlags::default().given(), Vec::<&str>::new());
    assert_eq!(RetiredFlags::default().warning(), None);
    assert_eq!(retired().given().len(), 7);
    let some = RetiredFlags {
        commit: Some("0123".to_owned()),
        release_url: Some("https://r.example".to_owned()),
        ..RetiredFlags::default()
    };
    assert_eq!(some.given(), ["--release-url", "--commit"]);
    let said = some.warning().expect("a warning");
    assert!(said.contains(": --release-url, --commit: "), "{said}");
    assert!(!said.contains("--release-source"), "{said}");
}

// Flags

fn problems(mutate: impl Fn(&mut Raw)) -> String {
    let mut r = raw(Mode::Storage);
    mutate(&mut r);
    r.validate(Platform::Linux).expect_err("refused").join("; ")
}

/// Catches a flag check that stops at the first problem, or lets a bad one through: every
/// problem is named, one line each, and nothing is read or written.
#[test]
fn every_bad_flag_is_named_and_nothing_is_written() {
    let bad = Raw {
        mode: Mode::Storage,
        frontend_url: Some("http://user@farm.example/x?y#z".to_owned()),
        token_file: Some("rel/token".to_owned()),
        min_generation: Some(0),
        retired: RetiredFlags::default(),
        database: Some(DATABASE.to_owned()),
        kbf: super::kbf_settings::KbfFlags::default(),
        create_local_database: false,
        installer_config: Some("/tmp/i.toml".to_owned()),
    };
    let mut exec = host();
    let err = go(&FakeOs::linux(), &mut exec, &bad, APPLY).expect_err("refused");
    let SetupError::Invalid(found) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(found.len(), 5, "{found:#?}");
    assert_eq!(err.exit(), Exit::Usage);
    assert!(exec.log.is_empty() && exec.queries.borrow().is_empty());
}

/// Asserts that the flags of the modes that enroll and of those that do not stay apart: a node
/// needs its frontend and token; a frontend host takes neither (it does not enroll).
#[test]
fn the_modes_that_enroll_need_a_frontend_and_a_token_and_the_others_refuse_them() {
    assert!(problems(|r| r.frontend_url = None).contains("--frontend-url is required"));
    assert!(problems(|r| r.token_file = None).contains("--token-file is required"));
    let mut r = raw(Mode::Frontend);
    r.frontend_url = Some("https://farm.example".to_owned());
    r.token_file = Some(SRC.to_owned());
    let found = r.validate(Platform::Linux).expect_err("refused").join("; ");
    assert!(
        found.contains("--frontend-url is for the modes that enroll"),
        "{found}"
    );
    assert!(
        found.contains("--token-file is for the modes that enroll"),
        "{found}"
    );
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
        let mut r = raw(Mode::Storage);
        r.frontend_url = Some(url.to_owned());
        r.validate(Platform::Linux).expect("valid").frontend_url
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

/// Asserts that the retired flags are not checked: values that were refused before (a source
/// that does not fit the mode, a URL that is not https, a commit that is not 40 hex digits, a
/// first release beside the bundled source) are accepted now, and nothing of them reaches the
/// flags that are valid.
#[test]
fn the_retired_flags_are_not_checked() {
    let junk = RetiredFlags {
        release_source: Some("frontend".to_owned()),
        release_url: Some("http://m.example/a/../b".to_owned()),
        release_ca: Some("relative.pem".to_owned()),
        frontend_worker_url: Some("not a url".to_owned()),
        frontend_ca: Some("relative.pem".to_owned()),
        frontend_ca_sha256: Some("ABC".to_owned()),
        commit: Some("HEAD".to_owned()),
    };
    for mode in [Mode::Storage, Mode::Frontend] {
        let plain = raw(mode).validate(Platform::Linux);
        let with = Raw {
            retired: junk.clone(),
            ..raw(mode)
        };
        assert_eq!(with.validate(Platform::Linux), plain, "{mode:?}");
        assert!(plain.is_ok(), "{mode:?}: {plain:?}");
    }
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
            SetupError::ShadowUnit {
                path: "/x".into(),
                unit: "u",
            },
            Exit::NeedsOperator,
            "unit_shadow",
        ),
        (SetupError::Database("x".into()), Exit::Failed, "database"),
    ];
    for (err, exit, code) in cases {
        assert_eq!((err.exit(), err.code()), (exit, code), "{err}");
    }
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
            ("systemctl", ["is-enabled", "--", "kbf-frontend.service"]) => {
                reply(0, "enabled\n", "")
            }
            ("systemctl", ["is-enabled", "--", "kbf-static.service"]) => reply(0, "static\n", ""),
            ("systemctl", _) => reply(1, "disabled\n", ""),
            _ => reply(1, "", "no such list"),
        })
    });
}

/// Catches the production `Os` reading the host wrong: it asks `getent`/`id` for a user, reads
/// and takes a unit as enabled only when `systemctl is-enabled` exits 0 and says `enabled` (not
/// `static`).
#[test]
fn the_system_os_reads_the_host_through_exec() {
    use super::os::{Os, SystemOs};
    let mut exec = FakeExec::new(0);
    answer(&mut exec);
    let os = SystemOs::new(&exec, Platform::Linux);
    assert_eq!((os.euid(), os.platform()), (0, Platform::Linux));
    assert_eq!(os.root_owner(), ROOT);
    assert!(os.trusted_binary().is_ok());
    assert_eq!(os.user("kbf").expect("lookup").expect("a user").uid, 990);
    assert!(os.user("nobody-here").expect("lookup").is_none());
    assert!(os.unit_enabled("kbf-frontend.service").expect("query"));
    assert!(
        !os.unit_enabled("kbf-static.service")
            .expect("a static unit is not enabled")
    );
    assert!(!os.unit_enabled("kbf-storage.service").expect("query"));
    let mut exec = FakeExec::new(0);
    exec.untrusted = true;
    assert!(
        SystemOs::new(&exec, Platform::Linux)
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
        (SetupError::NeedsOperator("fix it".into()), "fix it"),
        (
            SetupError::ShadowUnit {
                path: "/etc/systemd/system/a.service".into(),
                unit: "a.service",
            },
            "shadows the packaged unit a.service",
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

#[path = "kbf_settings_tests.rs"]
mod kbf_settings_tests;
#[path = "provision_tests.rs"]
mod provision;

#[path = "proxy_tests.rs"]
mod proxy_tests;

#[path = "old_backup_tests.rs"]
mod old_backup;
