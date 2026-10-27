//! The verb `kbf boot-check` ([`super`]) without a server: when the step runs and when it must
//! not, what it is asked for, each failure that stops the unit's start, how `installer.toml`
//! is read, and the command line. A fake step plays the database; the real step is reached
//! only through a schema this build does not embed and through a socket that is not there.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use clap::error::ErrorKind;

use super::{
    Args, DEFAULT_INSTALLER_CONFIG, NO_DATABASE, NO_STATE, SchemaStep, command, database_in,
    database_of, migrate_before_start, parse, report, run,
};
use crate::names;

/// A database behind a socket directory that may be there.
const SOCKET_URL: &str = "postgres:///kbf?host=/run/postgresql";

/// A database behind a socket directory that is not there.
const DOWN_URL: &str = "postgres:///kbf?host=/nonexistent-kbf-test-socket";

/// What a fake step was asked, in order.
type Asked = Rc<RefCell<Vec<u32>>>;

/// A schema step that records what it was asked and answers as told.
struct FakeStep {
    asked: Asked,
    answer: Result<String, String>,
}

impl SchemaStep for FakeStep {
    fn before_start(&mut self, wanted: u32) -> Result<String, String> {
        self.asked.borrow_mut().push(wanted);
        self.answer.clone()
    }
}

fn fake_step(answer: Result<&str, &str>) -> (Asked, FakeStep) {
    let asked = Asked::default();
    let step = FakeStep {
        asked: Rc::clone(&asked),
        answer: answer.map(str::to_owned).map_err(str::to_owned),
    };
    (asked, step)
}

/// The check for `mode` over a database whose step answers `answer`: what it returned and the
/// schemas the step was asked for.
fn migrate(mode: &str, answer: Result<&str, &str>) -> (Result<String, String>, Vec<u32>) {
    let (asked, step) = fake_step(answer);
    let said = migrate_before_start(mode, || Ok(Some(step)));
    let seen = asked.borrow().clone();
    (said, seen)
}

/// A scratch directory removed when it drops. A unit test gets no `CARGO_TARGET_TMPDIR`, so it
/// is under the system's temporary directory.
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    let temp = std::fs::canonicalize(std::env::temp_dir()).expect("canonical");
    kbf_testkit::dir::TestDir::under(temp, &format!("kbf-cli-boot-check-{name}"))
}

/// The path of an `installer.toml` with `body`, in the directory `dir`.
fn installer_toml(dir: &Path, body: &str) -> PathBuf {
    let file = dir.join("installer.toml");
    std::fs::write(&file, body).expect("written");
    file
}

/// The words of `kbf boot-check` followed by `rest`.
fn boot(rest: &[&str]) -> Vec<OsString> {
    ["kbf", "boot-check"]
        .iter()
        .chain(rest)
        .map(OsString::from)
        .collect()
}

/// Asserts that a unit of the frontend or all-in-one is migrated once, to the schema this
/// build declares as `state_schema` in `kbf --version --json`, before the check returns, and
/// that the line returned is the step's own after the words that say where it comes from.
#[test]
fn the_step_is_asked_once_for_the_schema_this_build_declares() {
    let declared = crate::version::identity()["state_schema"]
        .as_u64()
        .expect("a number");
    assert_eq!(declared, u64::from(kbf_pack_index::schema::latest()));
    for mode in [names::FRONTEND, names::ALL_IN_ONE] {
        let (said, asked) = migrate(mode, Ok("migrated the schema from 6 to 7"));
        let want = Ok("schema step: migrated the schema from 6 to 7".to_owned());
        assert_eq!(said, want, "{mode}");
        assert_eq!(asked, [kbf_pack_index::schema::latest()], "{mode}");
    }
}

/// Asserts that a step that refuses is an error, with the step's one line after the words that
/// say where it comes from, and that the step was asked once: the unit must not start.
#[test]
fn a_step_that_refuses_is_an_error_and_was_asked_once() {
    let (said, asked) = migrate(names::ALL_IN_ONE, Err("a contract step"));
    assert_eq!(said, Err("state schema: a contract step".to_owned()));
    assert_eq!(asked.len(), 1);
}

/// Asserts that a mode that keeps no state in the database runs nothing and loads nothing:
/// storage, daemon and storage-and-daemon pass without the `installer.toml` being read or the
/// step being asked.
#[test]
fn a_mode_that_keeps_no_state_in_the_database_runs_nothing() {
    for mode in [names::STORAGE, names::DAEMON, names::STORAGE_AND_DAEMON] {
        let loads = Cell::new(0);
        let (asked, step) = fake_step(Ok("migrated"));
        let said = migrate_before_start(mode, || {
            loads.set(loads.get() + 1);
            Ok(Some(step))
        });
        assert_eq!(said, Ok(NO_STATE.to_owned()), "{mode}");
        assert_eq!(loads.get(), 0, "{mode}");
        assert!(asked.borrow().is_empty(), "{mode}");
    }
}

/// Asserts that a host whose `installer.toml` names no database passes and says why it did
/// nothing, and that an `installer.toml` that cannot be used stops the start with the loader's
/// own line as it came.
#[test]
fn a_host_with_no_database_passes_and_an_unusable_installer_toml_stops_the_start() {
    let said = migrate_before_start(names::FRONTEND, || Ok(None::<FakeStep>));
    assert_eq!(said, Ok(NO_DATABASE.to_owned()));
    let load = || Err::<Option<FakeStep>, String>("unreadable".to_owned());
    let said = migrate_before_start(names::ALL_IN_ONE, load);
    assert_eq!(said, Err("unreadable".to_owned()));
}

/// Asserts that the real step of this build refuses what it cannot do and says so in one line
/// that does not name the socket: asked for the schema one past the newest it embeds, it
/// refuses with the line that says so; asked for its own over a socket that is not there, it
/// refuses with the database's reason.
#[test]
fn the_real_step_of_this_build_refuses_what_it_cannot_do() {
    let dir = scratch("real-step");
    let file = installer_toml(&dir, &format!("database = \"{DOWN_URL}\"\n"));
    let mut step = database_of(&file).expect("a usable url").expect("a step");
    let ahead = kbf_pack_index::schema::latest() + 1;
    let why = step.before_start(ahead).unwrap_err();
    let wanted = format!("needs state schema {ahead}");
    assert!(why.contains(&wanted), "{why}");
    let why = migrate_before_start(names::FRONTEND, || database_of(&file)).unwrap_err();
    assert!(why.starts_with("state schema: "), "{why}");
    assert!(!why.contains("nonexistent-kbf-test-socket"), "{why}");
}

/// Asserts that the `database` key of an `installer.toml` is read as a step bounded by 30 s,
/// that a file without the key (or with a key that is not a string) gives none, and that a file
/// that is missing, is not TOML, or whose URL is unusable, asks for TLS or carries a password is
/// an error that does not quote the URL.
#[test]
fn the_database_of_an_installer_toml_is_a_step_or_none_or_an_error_without_the_url() {
    let dir = scratch("database-of");
    let named = format!("release_source = \"bundled\"\ndatabase = \"{SOCKET_URL}\"\n");
    let step = database_of(&installer_toml(&dir, &named))
        .expect("a usable url")
        .expect("a step");
    let shown = format!("{step:?}");
    assert!(shown.contains("lock_wait: 30s"), "{shown}");
    for body in ["release_source = \"bundled\"\n", "database = 5\n"] {
        let found = database_of(&installer_toml(&dir, body)).expect("a file");
        assert!(found.is_none(), "{body}");
    }
    let missing = dir.join("no-such-installer.toml");
    let why = database_of(&missing).unwrap_err();
    assert!(why.contains("no-such-installer.toml"), "{why}");
    let why = database_of(&installer_toml(&dir, "database = = =")).unwrap_err();
    assert!(why.contains("is not valid TOML"), "{why}");
}

/// Asserts that a `database` URL that carries a password (in the user information or as a
/// parameter), asks for TLS or is not a Postgres URL is refused with the file named and the
/// reason, and that no refusal quotes the password or the host.
#[test]
fn a_url_that_cannot_be_used_is_refused_without_quoting_it() {
    let with_password = "postgres://kbf:hunter2@db/kbf";
    let as_parameter = "postgres:///kbf?host=/run/postgresql&password=hunter2";
    let wants_tls = "postgres://db.example/kbf?sslmode=require";
    let refused = [
        (with_password, "carries a password", "hunter2"),
        (as_parameter, "carries a password", "hunter2"),
        (wants_tls, "is not usable", "db.example"),
        ("mysql://db.example/kbf", "is not usable", "db.example"),
    ];
    for (url, says, secret) in refused {
        let body = format!("database = \"{url}\"\n");
        let why = database_in("installer.toml", &body).unwrap_err();
        assert!(why.contains("installer.toml"), "{url}: {why}");
        assert!(why.contains(says), "{url}: {why}");
        assert!(!why.contains(secret), "{url}: {why}");
    }
}

/// Asserts that a check that did something says it in one line on standard error and stands
/// for exit status 0, and one that refuses says why in one line and stands for exit status 1.
#[test]
fn the_verdict_is_one_line_and_exit_status_0_or_1() {
    let mut said: Vec<u8> = Vec::new();
    let status = report(&Ok("schema step: nothing to apply".to_owned()), &mut said);
    assert_eq!(status, ExitCode::SUCCESS);
    let line = "kbf boot-check: schema step: nothing to apply\n";
    assert_eq!(String::from_utf8_lossy(&said), line);
    let mut refused: Vec<u8> = Vec::new();
    let status = report(&Err("state schema: busy".to_owned()), &mut refused);
    assert_eq!(status, ExitCode::FAILURE);
    let line = "kbf boot-check: state schema: busy\n";
    assert_eq!(String::from_utf8_lossy(&refused), line);
}

/// Asserts that `run` passes a mode that keeps no state in the database without reading the
/// `installer.toml` it names (the file is not there), and refuses a frontend whose
/// `installer.toml` is not there.
#[test]
fn run_passes_a_mode_with_no_state_and_refuses_a_frontend_with_no_installer_toml() {
    let args = Args {
        mode: names::DAEMON.to_owned(),
        installer_config: PathBuf::from("/nonexistent/installer.toml"),
    };
    assert_eq!(run(&args), ExitCode::SUCCESS);
    let args = Args {
        mode: names::FRONTEND.to_owned(),
        installer_config: PathBuf::from("/nonexistent/installer.toml"),
    };
    assert_eq!(run(&args), ExitCode::FAILURE);
}

/// Asserts that the command line takes `--mode` with one of the five modes and optionally
/// `--installer-config`, which defaults to `/etc/kbf/installer.toml`.
#[test]
fn the_command_line_takes_a_mode_and_optionally_the_installer_toml() {
    let parsed = parse(&boot(&["--mode=frontend"])).expect("parses");
    let want = Args {
        mode: names::FRONTEND.to_owned(),
        installer_config: PathBuf::from("/etc/kbf/installer.toml"),
    };
    assert_eq!(parsed, want);
    assert_eq!(DEFAULT_INSTALLER_CONFIG, "/etc/kbf/installer.toml");
    let given = ["--mode", "storage", "--installer-config=/x/i.toml"];
    let parsed = parse(&boot(&given)).expect("parses");
    assert_eq!(parsed.mode, names::STORAGE);
    assert_eq!(parsed.installer_config, Path::new("/x/i.toml"));
    for mode in names::MODES.iter().filter(|m| m.name != names::ALL_IN_ONE) {
        let flag = format!("--mode={}", mode.name);
        let parsed = parse(&boot(&[flag.as_str()])).expect("a mode");
        assert_eq!(parsed.mode, mode.name);
    }
}

/// Asserts that a command line without `--mode`, with a mode `kbf` does not have (there is no
/// `node` mode), with a flag of a mode, or with a word that is no flag is refused as a usage
/// error, so that the unit's `RestartPreventExitStatus=2` does not restart it.
#[test]
fn a_command_line_that_is_not_a_mode_and_nothing_else_is_a_usage_error() {
    let refused = |line: &[&str]| parse(&boot(line)).expect_err("refused").kind();
    assert_eq!(refused(&[]), ErrorKind::MissingRequiredArgument);
    assert_eq!(refused(&["--mode"]), ErrorKind::InvalidValue);
    assert_eq!(refused(&["--mode=node"]), ErrorKind::InvalidValue);
    let unknown = ErrorKind::UnknownArgument;
    assert_eq!(refused(&["--mode=daemon", "--check"]), unknown);
    assert_eq!(refused(&["--mode=daemon", "frontend"]), unknown);
    assert_eq!(refused(&["--mode=daemon", "--kbf=/usr/bin/kbf"]), unknown);
}

/// Asserts that the command's help names the verb as `kbf boot-check`, lists the five modes as
/// the values of `--mode` and shows the default `installer.toml`.
#[test]
fn the_help_names_the_verb_the_modes_and_the_default_installer_toml() {
    let help = command().render_long_help().to_string();
    assert!(help.contains("kbf boot-check"), "{help}");
    for mode in names::MODES.iter().filter(|m| m.name != names::ALL_IN_ONE) {
        assert!(help.contains(mode.name), "{}: {help}", mode.name);
    }
    assert!(!help.contains("all-in-one"), "{help}");
    assert!(help.contains(DEFAULT_INSTALLER_CONFIG), "{help}");
}
