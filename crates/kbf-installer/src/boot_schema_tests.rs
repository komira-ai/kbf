//! The schema step of a bundled host's `boot-check` ([`super`]): what is asked of the binary,
//! when the step runs and when it must not, and each failure that stops the unit's start. A
//! fake runner plays the binary and a fake step plays the database; the real step is reached
//! only through a release the installer does not embed and through a socket that is not there.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use super::{NO_DATABASE, NO_STATE, before_start, database_of, migrate_before_start};
use crate::cli::Mode;
use crate::database::SchemaStep;
use crate::fixtures::{FakeRunner, ran, version_json};
use crate::run::Ran;
use crate::testkit::{A, B};

const KBF: &str = "/usr/bin/kbf";

/// An `installer.toml` that names a database the installer can build a step for.
const DATABASE: &str = "database = \"postgres:///kbf?host=/run/postgresql\"\n";

/// An `installer.toml` that names a database behind a socket that is not there.
const DOWN: &str = "database = \"postgres:///kbf?host=/nonexistent-kbf-test-socket\"\n";

/// An `installer.toml` with no `database` key.
const NO_KEY: &str = "release_source = \"bundled\"\n";

/// An `installer.toml` whose database URL carries a password.
const SECRET: &str = "database = \"postgres://kbf:hunter2@db/kbf\"\n";

/// What a fake step was asked, in order.
type Asked = Rc<RefCell<Vec<u32>>>;

/// A schema step that records what it was asked and answers as told.
struct FakeStep {
    asked: Asked,
    answer: Result<String, String>,
}

impl SchemaStep for FakeStep {
    fn before_start(&mut self, release_schema: u32) -> Result<String, String> {
        self.asked.borrow_mut().push(release_schema);
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

fn answering(answer: Ran) -> FakeRunner {
    FakeRunner::new(move |_| Ok(answer.clone()))
}

/// A binary that exits 0 and prints `stdout`.
fn says(stdout: &str) -> FakeRunner {
    answering(ran(0, stdout, ""))
}

fn migrate_with<S: SchemaStep>(
    runner: &FakeRunner,
    mode: Mode,
    load: impl FnOnce() -> Result<Option<S>, String>,
) -> Result<String, String> {
    migrate_before_start(runner, Path::new(KBF), mode, load)
}

/// The check for `mode` over a database whose step answers `answer`: what it returned and the
/// schemas the step was asked for.
fn migrate(
    runner: &FakeRunner,
    mode: Mode,
    answer: Result<&str, &str>,
) -> (Result<String, String>, Vec<u32>) {
    let (asked, step) = fake_step(answer);
    let said = migrate_with(runner, mode, || Ok(Some(step)));
    let seen = asked.borrow().clone();
    (said, seen)
}

/// An `installer.toml` with `body`, in a directory of its own.
fn installer_toml(name: &str, body: &str) -> PathBuf {
    let dir = kbf_host::testkit::scratch(&format!("boot-schema-{name}"));
    let file = dir.join("installer.toml");
    fs::write(&file, body).expect("written");
    file
}

/// The version object `version` (the fixture's) with its `state_schema` replaced by `schema`.
fn with_schema(version: &str, schema: u32) -> String {
    let fixture = "\"state_schema\":7";
    version.replace(fixture, &format!("\"state_schema\":{schema}"))
}

/// Catches a bundled host starting on a database that is not at what its binary needs, and a
/// step asked after the check ended or for another schema than the binary's. The binary is
/// run the way the proxy's migrator runs a release (`--version --json`, empty environment,
/// in `/`, as the caller, for at most 10 s); the step is asked once, with the `state_schema`
/// it printed (7), before the check returns; the line returned is the step's own. Frontend
/// and all-in-one are the modes that do it.
///
/// Mutants it catches: the step skipped; the step run after the return (nothing asked yet);
/// the schema asked for the installer's newest instead of the binary's; the environment
/// passed through, `run_as` set, the bound removed or `--json` dropped in the probe.
#[test]
fn the_step_is_asked_once_for_the_schema_the_binary_declares_before_the_check_returns() {
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let runner = says(&version_json(A, "release"));
        let (said, asked) = migrate(&runner, mode, Ok("migrated the schema from 6 to 7"));
        let want = Ok("schema step: migrated the schema from 6 to 7".to_owned());
        assert_eq!(said, want);
        assert_eq!(asked, [7]);
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        let spawn = &calls[0];
        assert_eq!(spawn.program, Path::new(KBF));
        assert_eq!(spawn.args, ["--version", "--json"]);
        assert!(spawn.env.is_empty());
        assert_eq!(spawn.cwd, Path::new("/"));
        assert_eq!(spawn.run_as, None);
        assert_eq!(spawn.timeout, Duration::from_secs(10));
    }
}

/// Catches a bundled binary being refused for a commit or a profile nobody asked about: a
/// host that holds one file and no release has no commit to compare it with, so any build
/// that prints a version object is read.
///
/// Mutants it catches: the probe checked against a commit (the build of `B` is refused); the
/// profile required to be `release`.
#[test]
fn a_bundled_binary_is_read_whatever_commit_and_profile_it_names() {
    let runner = says(&version_json(B, "debug"));
    let (said, asked) = migrate(&runner, Mode::Frontend, Ok("nothing to apply"));
    assert!(said.is_ok(), "{said:?}");
    assert_eq!(asked, [7]);
}

/// Catches a refused migration being ignored: the unit must not start. The step's one line
/// is the error (after the words that say where it comes from), and the step was asked once.
///
/// Mutant it catches: the step's error swallowed into an `Ok` line (exit 0).
#[test]
fn a_step_that_refuses_is_an_error_and_was_asked_once() {
    let runner = says(&version_json(A, "release"));
    let (said, asked) = migrate(&runner, Mode::AllInOne, Err("a contract step"));
    assert_eq!(said, Err("state schema: a contract step".to_owned()));
    assert_eq!(asked, [7]);
}

/// Catches a binary that cannot say what schema it needs being migrated for anyway: it does
/// not run; runs past the bound; is ended by a signal; exits non-zero; prints something that
/// is not a version object; or declares a schema that is not a `u32`. Each ends the check
/// with the binary named and the probe's own word, and the step is never asked.
///
/// Mutants it catches: the version that will not parse replaced by a default schema; the
/// exit status or the timeout not looked at; the `u32` conversion saturating.
#[test]
fn a_binary_that_does_not_say_its_schema_stops_the_start_and_the_step_is_not_asked() {
    let version = version_json(A, "release");
    let mut timed_out = ran(0, &version, "");
    timed_out.timed_out = true;
    let mut signalled = ran(0, &version, "");
    signalled.code = None;
    let huge = version.replace("\"state_schema\":7", "\"state_schema\":4294967296");
    let cases = [
        FakeRunner::new(|_| Err(std::io::ErrorKind::NotFound.into())),
        answering(timed_out),
        answering(signalled),
        answering(ran(1, &version, "")),
        says("not json"),
        says(&huge),
    ];
    for runner in cases {
        let (said, asked) = migrate(&runner, Mode::Frontend, Ok("migrated"));
        let why = said.unwrap_err();
        assert!(why.starts_with(KBF), "{why}");
        assert!(why.contains("did not say its schema"), "{why}");
        assert!(why.contains("schema_unknown: "), "{why}");
        assert!(asked.is_empty(), "{why}");
    }
}

/// Catches a mode that keeps no state in the database being migrated for, or its
/// `installer.toml` being read: daemon, storage and storage-and-daemon hosts run nothing, ask
/// nothing, load nothing, and pass.
///
/// Mutants it catches: the mode check removed (the daemon runs the step); the check placed
/// after the load (the file of a host that has no database is read).
#[test]
fn a_mode_that_keeps_no_state_in_the_database_runs_nothing() {
    for mode in [Mode::Daemon, Mode::Storage, Mode::StorageAndDaemon] {
        let runner = says(&version_json(A, "release"));
        let loads = Cell::new(0);
        let (asked, step) = fake_step(Ok("migrated"));
        let said = migrate_with(&runner, mode, || {
            loads.set(loads.get() + 1);
            Ok(Some(step))
        });
        assert_eq!(said, Ok(NO_STATE.to_owned()));
        assert!(runner.calls().is_empty());
        assert_eq!(loads.get(), 0);
        assert!(asked.borrow().is_empty());
    }
}

/// Catches a host whose `installer.toml` names no database being refused or migrated: the
/// check passes, says why it did nothing, and runs nothing.
///
/// Mutants it catches: the missing database taken for an error (exit non-zero); the binary
/// run before the database is known.
#[test]
fn a_host_whose_installer_toml_names_no_database_runs_nothing() {
    let runner = says(&version_json(A, "release"));
    let said = migrate_with(&runner, Mode::Frontend, || Ok(None::<FakeStep>));
    assert_eq!(said, Ok(NO_DATABASE.to_owned()));
    assert!(runner.calls().is_empty());
}

/// Catches an `installer.toml` that cannot be used being taken for "no database": the error
/// ends the check as it came, and the binary is not run.
///
/// Mutant it catches: the loader's error turned into a skip.
#[test]
fn an_installer_toml_that_cannot_be_used_stops_the_start() {
    let runner = says(&version_json(A, "release"));
    let load = || Err::<Option<FakeStep>, String>("unreadable".to_owned());
    let said = migrate_with(&runner, Mode::AllInOne, load);
    assert_eq!(said, Err("unreadable".to_owned()));
    assert!(runner.calls().is_empty());
}

/// Catches the real loader taking the wrong view of `installer.toml`: a `database` key gives
/// a step bounded by the migrator's lock wait (30 s); a file without the key gives none; a
/// file that is missing, or whose URL carries a password, is an error that does not quote the
/// URL.
///
/// Mutants it catches: the no-key arm turned into an error (or the error arm into `None`);
/// the bound dropped or changed; the password refusal removed.
#[test]
fn the_database_of_an_installer_toml_is_a_bounded_step_or_none_or_an_error_without_the_url() {
    let named = installer_toml("named", DATABASE);
    let step = database_of(&named).expect("a usable url").expect("a step");
    let shown = format!("{step:?}");
    assert!(shown.contains("lock_wait: 30s"), "{shown}");
    let none = installer_toml("none", NO_KEY);
    assert!(database_of(&none).expect("a file").is_none());
    let missing = kbf_host::testkit::scratch("boot-schema-missing").join("installer.toml");
    let why = database_of(&missing).unwrap_err();
    assert!(why.contains("installer.toml"), "{why}");
    let secret = installer_toml("secret", SECRET);
    let why = database_of(&secret).unwrap_err();
    assert!(why.contains("password"), "{why}");
    assert!(!why.contains("hunter2"), "{why}");
}

/// Catches the host's check reading the wrong thing or running the wrong step: a daemon host
/// does not read its `installer.toml` (it is not there); a frontend without the file is an
/// error and runs nothing; one whose file names no database passes and runs nothing; one
/// whose file names a database gets the binary run once and the real step asked, which
/// refuses a schema newer than the installer embeds with the installer's line, and, for a
/// schema it embeds, reaches for a socket that is not there and refuses too.
///
/// Mutants it catches: the file read before the mode is known; the host's step replaced by a
/// fake that passes; the step's error swallowed.
#[test]
fn the_host_check_reads_installer_toml_only_where_it_matters_and_runs_the_real_step() {
    let version = version_json(A, "release");
    let runner = says(&version);
    let kbf = Path::new(KBF);
    let missing = kbf_host::testkit::scratch("boot-schema-wrap").join("installer.toml");
    let skipped = before_start(&runner, &missing, kbf, Mode::Daemon);
    assert_eq!(skipped, Ok(NO_STATE.to_owned()));
    let why = before_start(&runner, &missing, kbf, Mode::Frontend).unwrap_err();
    assert!(why.contains("installer.toml"), "{why}");
    let none = installer_toml("wrap-none", NO_KEY);
    let skipped = before_start(&runner, &none, kbf, Mode::AllInOne);
    assert_eq!(skipped, Ok(NO_DATABASE.to_owned()));
    assert!(runner.calls().is_empty());

    let down = installer_toml("wrap-down", DOWN);
    let unshipped = kbf_state::postgres::schema::latest() + 1;
    let ahead = says(&with_schema(&version, unshipped));
    let why = before_start(&ahead, &down, kbf, Mode::Frontend).unwrap_err();
    assert!(why.starts_with("state schema: "), "{why}");
    assert!(why.contains("update the kbf-installer package"), "{why}");
    assert_eq!(ahead.calls().len(), 1);

    let one = says(&with_schema(&version, 1));
    let why = before_start(&one, &down, kbf, Mode::Frontend).unwrap_err();
    assert!(why.starts_with("state schema: "), "{why}");
    assert_eq!(one.calls().len(), 1);
}
