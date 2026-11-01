//! The start of `storage`, `daemon` and `storage-and-daemon` against the in-process
//! frontend of `kbf-enroll`'s tests (`kbf.enroll.v1` on a loopback listener, the real node
//! library, real files in a scratch root): the mode resolves from the flag or the file, the
//! first start enrolls for exactly the roles of the mode, a restart renews without the
//! token, a token that lacks a role or an enrollment that does not cover the mode stops the
//! start with exit 2, a frontend that is down is retried, `--check` enrolls nothing, and the
//! renewer keeps the certificates fresh while the mode runs.
//!
//! The mode itself is not started: `run_with` takes the function that runs an
//! [`Invocation`], and the tests give it one that records what it was given and looks at
//! the files at that moment.

#[path = "../../kbf-enroll/tests/support/mod.rs"]
mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_cli::enroll::{Env, Failure, Want, ensure_enrolled, load};
use kbf_cli::node::{Driver, Renewer, Start, run_with};
use kbf_cli::{Invocation, names};
use kbf_enroll::client::{CallError, ConnectError, EnrollError};
use kbf_enroll::config::Host;
use kbf_enroll::store::Store;
use kbf_enroll::token::TokenFileError;
use kbf_types::{CaPin, RefusalReason, Token, TokenId, TokenSecret};
use support::fake::{self, Fake, LIFE_S, Running, Spoil};
use support::scratch;
use tokio::sync::watch;
use tokio::time::timeout;

/// Writes a `kbf.toml` that a start accepts whatever the umask of the test run is: the
/// file may not be writable by group or other.
fn write_config(path: &Path, text: &str) {
    std::fs::write(path, text).expect("kbf.toml");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("mode");
}

struct Harness {
    run: Running,
    root: kbf_testkit::dir::TestDir,
    /// The waits a start asked for before trying again.
    waits: Arc<Mutex<Vec<Duration>>>,
    /// How many waits are honoured (the start then tries again); the next one gives up.
    patience: Arc<Mutex<usize>>,
    /// Runs on every wait that is honoured, before the start tries again.
    on_wait: Arc<Mutex<Option<Hook>>>,
}

/// What runs on a wait that is honoured.
type Hook = Box<dyn Fn() + Send>;

impl Harness {
    fn new(name: &str) -> Self {
        Self {
            run: fake::start(),
            root: scratch(name),
            waits: Arc::default(),
            patience: Arc::default(),
            on_wait: Arc::default(),
        }
    }

    fn fake(&self) -> &Arc<Fake> {
        &self.run.fake
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("kbf.toml")
    }

    fn token_path(&self) -> PathBuf {
        self.root.join("enroll.token")
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// Writes `kbf.toml` naming the fake frontend, with `extra` lines (tables of roles).
    fn write_config(&self, port: u16, extra: &str) {
        self.write_config_with(port, "", "", extra);
    }

    /// As [`Harness::write_config`], with `trust` as `[tls]` lines and `node` as extra
    /// lines of the `[node]` table.
    fn write_config_with(&self, port: u16, node: &str, trust: &str, extra: &str) {
        let text = format!(
            "[frontend]\nurl = \"https://localhost:{port}\"\n[node]\nid = \"node-02\"\n{node}\n\
             [enroll]\ntoken_file = \"{}\"\n[dirs]\nstate = \"{}\"\n{trust}\n{extra}\n",
            self.token_path().display(),
            self.state().display(),
        );
        write_config(&self.config_path(), &text);
    }

    /// The fake's own frontend, a one-use token, and the files a start reads.
    fn ready(&self, extra: &str) -> Token {
        self.write_config(self.run.enroll_addr.port(), extra);
        let token = self.fake().mint(1, 1);
        self.put_token(&token);
        token
    }

    fn put_token(&self, token: &Token) {
        std::fs::write(self.token_path(), token.to_line()).expect("token file");
        std::fs::set_permissions(self.token_path(), std::fs::Permissions::from_mode(0o600))
            .expect("token mode");
    }

    fn env(&self) -> Env {
        let fake = Arc::clone(self.fake());
        let (waits, patience, on_wait) = (
            Arc::clone(&self.waits),
            Arc::clone(&self.patience),
            Arc::clone(&self.on_wait),
        );
        Env {
            host: Host {
                hostname: "node-02".to_owned(),
                cpus: 8,
            },
            euid: rustix::process::geteuid().as_raw(),
            now: Arc::new(move || fake.now()),
            pause: Arc::new(move |wait| {
                waits.lock().expect("waits").push(wait);
                let mut left = patience.lock().expect("patience");
                if *left == 0 {
                    return false;
                }
                *left -= 1;
                if let Some(hook) = on_wait.lock().expect("hook").as_ref() {
                    hook();
                }
                true
            }),
        }
    }

    /// A start of `mode` on this host's `kbf.toml`.
    fn start_of(&self, mode: Option<&'static str>) -> Start {
        Start {
            mode,
            config: self.config_path(),
            check: false,
        }
    }

    /// Writes `mode = "..."` at the top of `kbf.toml`.
    fn file_says(&self, mode: &str) {
        let text = std::fs::read_to_string(self.config_path()).expect("kbf.toml");
        write_config(&self.config_path(), &format!("mode = \"{mode}\"\n{text}"));
    }

    /// The roles the frontend was asked for, one list per `Enroll` call.
    fn asked(&self) -> Vec<Vec<String>> {
        self.fake()
            .enrolls()
            .iter()
            .map(|e| e.csrs.iter().map(|c| c.role.clone()).collect())
            .collect()
    }

    fn node_json(&self) -> String {
        std::fs::read_to_string(self.state().join("pki/node.json")).expect("node.json")
    }
}

/// What the recording launcher saw.
#[derive(Default)]
struct Launches(Mutex<Vec<&'static str>>);

impl Launches {
    fn seen(&self) -> Vec<&'static str> {
        self.0.lock().expect("launches").clone()
    }
}

fn variant(invocation: &Invocation) -> &'static str {
    match invocation {
        Invocation::Daemon(_) => names::DAEMON,
        Invocation::AllInOne(_) => names::ALL_IN_ONE,
        Invocation::Storage(_) => names::STORAGE,
        Invocation::StorageAndDaemon(_) => names::STORAGE_AND_DAEMON,
        Invocation::Frontend(_) => names::FRONTEND,
        Invocation::Registered(_) => "registered",
    }
}

/// Runs a start of the daemon mode on a thread of its own (it makes runtimes of its own),
/// with a launcher that records the mode and returns exit status 7.
async fn start(
    h: &Harness,
    launches: &Arc<Launches>,
    before_exit: impl Fn() + Send + 'static,
) -> Result<ExitCode, Failure> {
    start_with(h, h.start_of(Some(names::DAEMON)), launches, before_exit).await
}

/// As [`start`], for `mode` on a start of the test's own making.
async fn start_with(
    h: &Harness,
    node: Start,
    launches: &Arc<Launches>,
    before_exit: impl Fn() + Send + 'static,
) -> Result<ExitCode, Failure> {
    let env = h.env();
    let launches = Arc::clone(launches);
    tokio::task::spawn_blocking(move || {
        run_with(&node, &env, Driver::Container, &|invocation| {
            launches
                .0
                .lock()
                .expect("launches")
                .push(variant(invocation));
            before_exit();
            ExitCode::from(7)
        })
    })
    .await
    .expect("the start did not panic")
}

/// Starts `mode` on the host's `kbf.toml`.
async fn start_mode(
    h: &Harness,
    mode: &'static str,
    launches: &Arc<Launches>,
) -> Result<ExitCode, Failure> {
    start_with(h, h.start_of(Some(mode)), launches, || {}).await
}

async fn enrolling(h: &Harness, mode: &'static str) -> Result<kbf_cli::enroll::Outcome, Failure> {
    let (config, env) = (h.config_path(), h.env());
    tokio::task::spawn_blocking(move || {
        let loaded = load(&config, &env)?;
        ensure_enrolled(&loaded, Want::of(mode).expect("a mode"), &env)
    })
    .await
    .expect("no panic")
}

fn is_7(code: ExitCode) -> bool {
    format!("{code:?}") == format!("{:?}", ExitCode::from(7))
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// Catches a first start that does not enroll, enrolls but starts another mode than
/// the one named, leaves the key readable, keeps the token file, or does not hand the mode's
/// exit status back. The launcher looks at the files at the moment the mode would start:
/// the certificate and key of the daemon role are there, the key private, and the token
/// file is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_start_enrolls_and_runs_the_mode() {
    let h = Harness::new("node-first");
    h.ready("[daemon]\n");
    let launches = Arc::new(Launches::default());
    let (state, token) = (h.state(), h.token_path());
    let at_launch = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&at_launch);

    let code = start(&h, &launches, move || {
        let pki = state.join("pki");
        *seen.lock().expect("seen") = Some((
            pki.join("daemon.pem").exists(),
            mode_of(&pki.join("daemon.key")),
            mode_of(&pki),
            token.exists(),
        ));
    })
    .await
    .expect("a start");

    assert!(is_7(code), "the mode's exit status is returned: {code:?}");
    assert_eq!(launches.seen(), [names::DAEMON]);
    assert_eq!(
        *at_launch.lock().expect("seen"),
        Some((true, 0o600, 0o700, false))
    );
    assert_eq!(h.fake().enrolls().len(), 1);
    assert!(h.node_json().contains("node-02"));
}

/// Catches a restart that enrolls again (it would spend a second use, or fail because the
/// token file is gone): a node with `node.json` reads no token and calls `Enroll` no more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_does_not_enroll_again() {
    let h = Harness::new("node-restart");
    h.ready("[daemon]\n");
    let launches = Arc::new(Launches::default());

    start(&h, &launches, || {}).await.expect("the first start");
    assert!(!h.token_path().exists());
    start(&h, &launches, || {})
        .await
        .expect("a start with no token file");
    start(&h, &launches, || {}).await.expect("a third start");

    assert_eq!(launches.seen(), [names::DAEMON; 3]);
    assert_eq!(h.fake().enrolls().len(), 1, "one Enroll call, ever");
    assert_eq!(
        h.fake().uses(&Token::new(
            TokenId::from_bits(1),
            TokenSecret::from_bytes([1; 32]),
            h.fake().pki.pin(),
        )),
        1
    );
}

/// Catches a refusal that is lost on the way to the operator, or a start that goes on
/// after one: for each reason in the closed set the frontend can answer an enrollment
/// with, the start fails with that reason named in its message, exits 2 when a restart
/// cannot fix it and 1 when waiting may, starts no mode and leaves no `node.json` and the
/// token file where it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_token_stops_the_start_with_the_typed_reason() {
    let fixed = [
        RefusalReason::TokenExpired,
        RefusalReason::TokenUsedUp,
        RefusalReason::TokenRevoked,
        RefusalReason::TokenRefused,
        RefusalReason::RoleNotGranted,
        RefusalReason::NodeIdInUse,
        RefusalReason::NodeIdNotAllowed,
    ];
    let passing = [RefusalReason::StoreUnavailable, RefusalReason::RateLimited];
    for (reason, code) in fixed
        .iter()
        .map(|r| (*r, 2))
        .chain(passing.iter().map(|r| (*r, 1)))
    {
        let h = Harness::new(&format!("node-refused-{}", reason.name()));
        h.ready("[daemon]\n");
        h.fake().refuse_with(Some(reason));
        let launches = Arc::new(Launches::default());

        let failure = start(&h, &launches, || {}).await.expect_err("refused");

        assert!(
            matches!(
                &failure,
                Failure::Enroll(EnrollError::Call(CallError::Refused(r))) if *r == reason
            ),
            "{reason:?}: {failure:?}"
        );
        assert!(failure.to_string().contains(reason.message()), "{failure}");
        assert_eq!(failure.exit_code(), code, "{reason:?}");
        assert!(launches.seen().is_empty());
        assert!(!h.state().join("pki/node.json").exists());
        assert!(h.token_path().exists());
    }
}

/// Catches a token that does not carry the pin being honoured: a frontend whose CA is not
/// the one the token names is refused before the secret is sent (exit 2: the operator
/// must look, it is not a network failure), and a frontend that is not there is exit 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_pin_is_refused_and_a_frontend_that_is_down_is_retried() {
    let h = Harness::new("node-pin");
    h.ready("[daemon]\n");
    let wrong = Token::new(
        TokenId::from_bits(1),
        TokenSecret::from_bytes([1; 32]),
        CaPin::from_bytes([9; 32]),
    );
    h.put_token(&wrong);
    let launches = Arc::new(Launches::default());
    let failure = start(&h, &launches, || {}).await.expect_err("untrusted");
    assert!(
        matches!(&failure, Failure::Connect(ConnectError::Untrusted(_))),
        "{failure:?}"
    );
    assert_eq!(failure.exit_code(), 2);
    assert!(h.fake().enrolls().is_empty(), "the secret was not sent");

    let closed = kbf_testkit::port::dead_addr().port();
    h.write_config(closed, "[daemon]\n");
    let failure = start(&h, &launches, || {}).await.expect_err("unreachable");
    assert!(
        matches!(&failure, Failure::Connect(ConnectError::Unreachable(_))),
        "{failure:?}"
    );
    assert_eq!(failure.exit_code(), 1);
    assert!(launches.seen().is_empty());
}

/// Catches the files a start reads not being checked before it acts on them: no
/// `kbf.toml`, a key it does not know (all problems listed), a token file others can
/// read, and no token file at all are each refused with exit 2 and start nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_that_are_not_right_stop_the_start() {
    let h = Harness::new("node-files");
    let launches = Arc::new(Launches::default());
    let failure = start(&h, &launches, || {}).await.expect_err("no config");
    assert!(matches!(failure, Failure::ReadConfig { .. }), "{failure:?}");
    assert_eq!(failure.exit_code(), 2);

    h.ready("[daemon]\nbogus = 1\n");
    let failure = start(&h, &launches, || {}).await.expect_err("bad config");
    assert!(matches!(failure, Failure::Settings(_)), "{failure:?}");
    assert!(failure.to_string().contains("bogus"), "{failure}");

    h.ready("[daemon]\n");
    std::fs::set_permissions(h.token_path(), std::fs::Permissions::from_mode(0o644)).expect("mode");
    let failure = start(&h, &launches, || {}).await.expect_err("open token");
    assert!(
        matches!(failure, Failure::Token(TokenFileError::Mode { .. })),
        "{failure:?}"
    );
    std::fs::remove_file(h.token_path()).expect("rm");
    let failure = start(&h, &launches, || {}).await.expect_err("no token");
    assert!(
        matches!(failure, Failure::Token(TokenFileError::Io { .. })),
        "{failure:?}"
    );
    assert_eq!(failure.exit_code(), 2);

    let state = h.state();
    std::fs::create_dir_all(&state).expect("state");
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).expect("mode");
    h.put_token(&h.fake().mint(2, 1));
    let failure = start(&h, &launches, || {}).await.expect_err("open dir");
    assert!(matches!(failure, Failure::Paths(_)), "{failure:?}");
    assert!(
        failure.to_string().contains("dirs.state") && failure.to_string().contains("0700"),
        "{failure}"
    );
    assert!(launches.seen().is_empty());
    assert!(h.fake().enrolls().is_empty());
}

/// Catches a node that is renamed after it enrolled running with certificates that name
/// another node: the start fails saying both ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_id_that_changed_since_enrollment_is_refused() {
    let h = Harness::new("node-renamed");
    h.ready("[daemon]\n");
    let launches = Arc::new(Launches::default());
    start(&h, &launches, || {}).await.expect("first start");

    let text = std::fs::read_to_string(h.config_path()).expect("kbf.toml");
    write_config(&h.config_path(), &text.replace("node-02", "node-03"));
    let failure = start(&h, &launches, || {}).await.expect_err("renamed");
    assert!(
        matches!(&failure, Failure::RenamedNode { enrolled, configured }
            if enrolled == "node-02" && configured == "node-03"),
        "{failure:?}"
    );
    assert_eq!(failure.exit_code(), 2);
    assert_eq!(launches.seen().len(), 1);
}

/// Catches enrollment that is not idempotent: the first call enrolls, the next ones report
/// the node as enrolled and call the frontend no more, so every start can run it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enrolling_once_and_then_does_nothing() {
    let h = Harness::new("enroll-twice");
    h.ready("[daemon]\n");
    let first = enrolling(&h, names::DAEMON).await.expect("enrolled");
    assert!(first.fresh);
    std::fs::remove_file(h.token_path()).ok();
    let second = enrolling(&h, names::DAEMON)
        .await
        .expect("already enrolled");
    assert!(!second.fresh);
    assert_eq!(second.node, first.node);
    assert_eq!(h.fake().enrolls().len(), 1);
}

/// Catches the wrong set of roles being asked for. The mode names the roles, whatever the
/// tables of `kbf.toml` say (they are settings only): `storage` asks for storage,
/// `daemon` for daemon, `storage-and-daemon` for both, and the first start of each
/// enrolls exactly those and starts exactly that mode. The tables are written to
/// disagree with the mode on purpose: a `[daemon]` table must not make a storage node
/// ask for the daemon role, and a file with no table must not make a daemon ask for both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_start_of_each_mode_enrolls_for_exactly_its_roles() {
    let cases = [
        (names::STORAGE, "[daemon]\n", vec!["storage"]),
        (names::DAEMON, "[storage]\n", vec!["daemon"]),
        (names::DAEMON, "", vec!["daemon"]),
        (names::STORAGE, "", vec!["storage"]),
        (
            names::STORAGE_AND_DAEMON,
            "[daemon]\n",
            vec!["storage", "daemon"],
        ),
        (names::STORAGE_AND_DAEMON, "", vec!["storage", "daemon"]),
    ];
    for (i, (mode, extra, want)) in cases.into_iter().enumerate() {
        let h = Harness::new(&format!("enroll-roles-{i}"));
        h.ready(extra);
        h.fake()
            .set_cold_store("s3://bucket/prefix/?endpoint=https://objects.example");
        let launches = Arc::new(Launches::default());
        start_mode(&h, mode, &launches)
            .await
            .unwrap_or_else(|e| panic!("{mode} with {extra:?}: {e}"));
        assert_eq!(launches.seen(), [mode], "{mode} with {extra:?}");
        assert_eq!(
            h.asked(),
            std::slice::from_ref(&want),
            "{mode} with {extra:?}"
        );
        let on_disk: Vec<String> = ["storage", "daemon"]
            .into_iter()
            .filter(|r| h.state().join("pki").join(format!("{r}.pem")).exists())
            .map(str::to_owned)
            .collect();
        assert_eq!(on_disk, want, "{mode}: certificates on disk");
    }
}

/// Catches a node granted both roles refused after enrolling because the frontend sent the
/// storage half no cold store (defect: the missing store is a plan error again): the store
/// is the registration's, which the node waits for, so every start goes on, the node is
/// enrolled once, and it sends no `Renew` to ask the frontend for a store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_the_frontend_sent_no_cold_store_still_starts_registered() {
    let h = Harness::new("node-no-cold-store");
    h.ready("");
    let launches = Arc::new(Launches::default());
    for _ in 0..2 {
        start_mode(&h, names::STORAGE_AND_DAEMON, &launches)
            .await
            .expect("a node with no cold store sent starts");
    }
    assert_eq!(
        launches.seen(),
        [names::STORAGE_AND_DAEMON, names::STORAGE_AND_DAEMON]
    );
    assert_eq!(h.fake().enrolls().len(), 1);
    assert_eq!(h.fake().renews().len(), 0, "no Renew to ask for a store");
}

/// Catches a node enrolled before the farm registered a cold store staying unable to
/// start its storage half: the first start is refused (no store), then the store is
/// registered, and the next start asks the frontend for the configuration (one `Renew`)
/// and runs the storage mode on the store it was sent. A node that holds one asks nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_enrolled_before_the_store_was_registered_asks_for_it_at_its_next_start() {
    let h = Harness::new("node-cold-store-late");
    h.ready("");
    let launches = Arc::new(Launches::default());
    start_mode(&h, names::STORAGE, &launches)
        .await
        .expect_err("no cold store yet");
    assert!(launches.seen().is_empty());
    assert_eq!(h.fake().enrolls().len(), 1);

    h.fake().set_cold_store(COLD);
    h.fake().set_generation(2);
    start_mode(&h, names::STORAGE, &launches)
        .await
        .expect("the store was sent");
    assert_eq!(launches.seen(), [names::STORAGE]);
    assert_eq!(h.fake().enrolls().len(), 1, "no second enrollment");
    assert!(
        h.node_json().contains("objects.example"),
        "kept in node.json"
    );
    let asked = h.fake().renews().len();
    assert_eq!(
        asked, 1,
        "one Renew, to ask; the renewer's first look is not due"
    );

    start_mode(&h, names::STORAGE, &launches)
        .await
        .expect("a node with a store");
    assert_eq!(
        h.fake().renews().len(),
        asked,
        "a node that holds a cold store asks for nothing"
    );
}

/// Catches a start that dies with the frontend's refusal instead of saying what is missing:
/// a storage node holding no cold store whose frontend refuses the one `Renew` it sends at
/// start still gets the plan error naming the cold store, exit 2, and starts nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_cold_store_ask_leaves_the_plan_error() {
    let h = Harness::new("node-cold-ask-refused");
    h.ready("");
    let launches = Arc::new(Launches::default());
    start_mode(&h, names::STORAGE, &launches)
        .await
        .expect_err("no cold store yet");
    h.fake().refuse_with(Some(RefusalReason::StoreUnavailable));
    let failure = start_mode(&h, names::STORAGE, &launches)
        .await
        .expect_err("the frontend refused");
    assert!(
        matches!(&failure, Failure::Plan(m) if m.contains("cold store")),
        "{failure:?}"
    );
    assert_eq!(failure.exit_code(), 2);
    assert_eq!(h.fake().renews().len(), 1, "the second start asked once");
    assert!(launches.seen().is_empty());
}

/// Catches a node with no storage role asking the frontend for a cold store at start: a
/// daemon-only node holds none by design (the frontend sends it none), so a start that
/// asked would send a `Renew` at every start of every daemon, and reach a frontend that is
/// down for a node that has no use for the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_only_node_asks_for_no_cold_store_at_start() {
    let h = Harness::new("node-daemon-no-cold-ask");
    h.ready("");
    // The fake sends no cold store until told to: the daemon holds none, as in life.
    let launches = Arc::new(Launches::default());
    for _ in 0..2 {
        start_mode(&h, names::DAEMON, &launches)
            .await
            .expect("a daemon starts with no cold store");
    }
    assert_eq!(launches.seen(), [names::DAEMON, names::DAEMON]);
    assert_eq!(h.fake().enrolls().len(), 1);
    assert_eq!(
        h.fake().renews().len(),
        0,
        "a daemon sends no Renew at start"
    );
}

/// Catches an answer the node refused being retried as if the network were at fault, or a
/// node with a wrong clock being given up on for good: a frontend whose clock is ten
/// minutes off is exit 1 (the clock may be set right and the start tried again), an answer
/// that is not what was asked for (a CA bundle that is not PEM) is exit 2; neither installs
/// anything or removes the token file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_the_node_refuses_installs_nothing() {
    for (spoil, code) in [(Spoil::Skew(600), 1), (Spoil::BundleNotPem, 2)] {
        let h = Harness::new(&format!("node-answer-{code}"));
        h.ready("[daemon]\n");
        h.fake().spoil(Some(spoil.clone()));
        let launches = Arc::new(Launches::default());

        let failure = start(&h, &launches, || {}).await.expect_err("refused");

        assert!(
            matches!(&failure, Failure::Enroll(EnrollError::Answer(_))),
            "{spoil:?}: {failure:?}"
        );
        assert_eq!(failure.exit_code(), code, "{spoil:?}");
        assert!(launches.seen().is_empty());
        assert!(!h.state().join("pki/node.json").exists());
        assert!(h.token_path().exists());
    }
}

/// Catches `tls.trust = "public"` being ignored (the node would send the token over a
/// connection the pin verified, or not verify at all): with the system's roots as the
/// anchor, the fake's private CA is not trusted, so the secret is not sent and the start
/// fails (exit 2: not a network failure).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_trust_does_not_accept_a_private_ca() {
    let h = Harness::new("node-public");
    h.write_config_with(
        h.run.enroll_addr.port(),
        "",
        "[tls]\ntrust = \"public\"",
        "[daemon]\n",
    );
    h.put_token(&h.fake().mint(1, 1));
    let launches = Arc::new(Launches::default());

    let failure = start(&h, &launches, || {}).await.expect_err("untrusted");

    assert!(
        matches!(
            &failure,
            Failure::Connect(ConnectError::Untrusted(_) | ConnectError::Anchor(_))
        ),
        "{failure:?}"
    );
    assert_eq!(failure.exit_code(), 2);
    assert!(h.fake().enrolls().is_empty(), "the secret was not sent");
}

/// Catches a `kbf.toml` that disagrees with the roles granted going unremarked: an
/// `advertise` on a node that asked for the daemon role only is enrolled all the same (the
/// frontend decides), and the start goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_setting_for_a_role_the_node_does_not_hold_is_a_warning_not_a_refusal() {
    let h = Harness::new("node-warning");
    h.write_config_with(
        h.run.enroll_addr.port(),
        "advertise = \"node-02.farm.example:8983\"",
        "",
        "[daemon]\n",
    );
    h.put_token(&h.fake().mint(1, 1));
    let launches = Arc::new(Launches::default());
    start(&h, &launches, || {}).await.expect("a start");
    assert_eq!(launches.seen(), [names::DAEMON]);
}

/// Catches a `label` key left in `kbf.toml` (a node label, removed: placement uses what
/// the node reports) starting the node, or being found only after the token is spent: the
/// start is refused with the key and its replacement named, exit 2, before the token is
/// sent, with nothing installed and the token file kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_label_in_kbf_toml_stops_the_start_before_enrolling() {
    let h = Harness::new("node-bad-label");
    h.write_config_with(
        h.run.enroll_addr.port(),
        "",
        "",
        "[identity]\nlabel = [\"pool=mac\"]\n\n[daemon]\n",
    );
    h.put_token(&h.fake().mint(1, 1));
    let launches = Arc::new(Launches::default());

    let failure = start(&h, &launches, || {}).await.expect_err("refused");

    assert!(matches!(failure, Failure::Settings(_)), "{failure:?}");
    assert_eq!(failure.exit_code(), 2);
    let said = failure.to_string();
    assert!(said.contains("`identity.label`"), "{said}");
    assert!(
        said.contains(&h.config_path().display().to_string()),
        "{said}"
    );
    assert!(
        said.contains("`label` was removed: placement uses what the node reports (OSFamily, ISA); delete the key"),
        "{said}"
    );
    assert!(launches.seen().is_empty());
    assert!(h.fake().enrolls().is_empty(), "the token was not sent");
    assert!(!h.state().join("pki/node.json").exists());
    assert!(h.token_path().exists());
}

/// An enrolled node and a renewer for it, with the clock at the fake's.
async fn renewing(h: &Harness) -> (Renewer, Store) {
    h.ready("[daemon]\n");
    let (env, config) = (h.env(), h.config_path());
    tokio::task::spawn_blocking(move || {
        let loaded = load(&config, &env).expect("loaded");
        let outcome = ensure_enrolled(&loaded, Want::of(names::DAEMON).expect("daemon"), &env)
            .expect("enrolled");
        (Renewer::new(&loaded, &outcome.node, &env, 0), loaded.store)
    })
    .await
    .expect("no panic")
}

fn serial_of(store: &Store) -> String {
    store.load().expect("load").expect("enrolled").certs[0]
        .serial
        .clone()
}

/// Catches a renewer that renews early, never, or too often, one that does not move its
/// next attempt to the new `renew_after`, one that does not back off after failures (300 s
/// doubling), and one that backs off for good: before half the life nothing is sent; at
/// half a request goes and the certificate changes; failures wait 300 s, then 600 s, and a
/// success starts the schedule again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_renewer_renews_at_half_life_and_backs_off_after_a_failure() {
    let h = Harness::new("renew-schedule");
    let (mut renewer, store) = renewing(&h).await;
    let start = h.fake().now();
    let first_serial = serial_of(&store);

    let wait = renewer.round().await;
    assert_eq!(wait, LIFE_S / 2, "the first attempt is at half the life");
    assert!(h.fake().renews().is_empty());

    h.fake().set_now(start + LIFE_S / 2 + 10);
    let wait = renewer.round().await;
    assert_eq!(h.fake().renews().len(), 1);
    assert_ne!(
        serial_of(&store),
        first_serial,
        "a new certificate is installed"
    );
    assert_eq!(
        wait,
        LIFE_S / 2,
        "the next attempt is at half of the new life"
    );

    let mut now = start + LIFE_S / 2 + 10 + wait;
    h.fake().set_now(now);
    h.fake().refuse_with(Some(RefusalReason::StoreUnavailable));
    for expected in [300, 600, 1200] {
        assert_eq!(renewer.round().await, expected);
        let calls = h.fake().renews().len();
        assert_eq!(
            renewer.round().await,
            expected,
            "no attempt before it is due"
        );
        assert_eq!(h.fake().renews().len(), calls);
        now += expected;
        h.fake().set_now(now);
    }

    h.fake().refuse_with(None);
    assert_eq!(
        renewer.round().await,
        LIFE_S / 2,
        "a success starts the schedule again"
    );
    // The failures it had are forgotten: the next one waits 300 s again, not 2400.
    now += LIFE_S / 2;
    h.fake().set_now(now);
    h.fake().refuse_with(Some(RefusalReason::StoreUnavailable));
    assert_eq!(renewer.round().await, 300);
}

fn generation(h: &Harness) -> u64 {
    let record: serde_json::Value = serde_json::from_str(&h.node_json()).expect("node.json");
    record["config"]["generation"]
        .as_u64()
        .expect("a generation")
}

/// Catches the renewer losing a configuration change (the new generation must reach
/// `node.json`, where the next start reads it), a failure close to expiry stopping the
/// loop instead of being retried, and a renewal that cannot even begin (a node that is
/// not enrolled) crashing it instead of backing off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_renewer_stores_a_new_configuration_and_survives_failures() {
    let h = Harness::new("renew-config");
    let (mut renewer, store) = renewing(&h).await;
    assert_eq!(generation(&h), 1);
    h.fake().set_generation(2);
    h.fake().set_now(h.fake().now() + LIFE_S / 2 + 10);
    renewer.round().await;
    assert_eq!(generation(&h), 2);

    // Within a day of expiry a failure is an error in the log, and the loop goes on.
    let not_after = store.load().expect("load").expect("enrolled").certs[0].not_after;
    h.fake().refuse_with(Some(RefusalReason::NodeRevoked));
    h.fake().set_now(not_after - 3_600);
    assert_eq!(renewer.round().await, 300);

    let empty = Harness::new("renew-not-enrolled");
    empty.write_config(empty.run.enroll_addr.port(), "[daemon]\n");
    let (env, config) = (empty.env(), empty.config_path());
    let mut renewer = tokio::task::spawn_blocking(move || {
        let loaded = load(&config, &env).expect("loaded");
        Renewer::new(&loaded, &unenrolled_record(), &env, 0)
    })
    .await
    .expect("no panic");
    assert_eq!(renewer.round().await, 300, "backs off, does not stop");
}

/// A record of a node that exists only in memory, due for renewal.
fn unenrolled_record() -> kbf_enroll::store::Node {
    kbf_enroll::store::Node {
        node_id: "node-02".to_owned(),
        pin: "ab".repeat(32),
        certs: Vec::new(),
        renew_after: 1,
        config: kbf_enroll::store::StoredConfig::default(),
    }
}

/// Waits until the frontend has seen `n` `Renew` calls.
async fn renews_seen(fake: &Arc<Fake>, n: usize) {
    let fake = Arc::clone(fake);
    timeout(Duration::from_secs(10), async move {
        while fake.renews().len() < n {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the frontend did not see {n} renewals"));
}

/// Catches a renew loop that does not go round again (a nap that is not cut to
/// `nap_max` would sleep for the days to the next renewal and miss a clock that jumped),
/// one that cannot be stopped while it sleeps (the mode has exited and the process would
/// wait an hour), and one that does not stop when its sender is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_loop_renews_naps_and_stops_when_told() {
    let h = Harness::new("renew-loop");
    let (mut renewer, _store) = renewing(&h).await;
    renewer.nap_max = Duration::from_millis(10);
    h.fake().set_now(h.fake().now() + LIFE_S / 2 + 10);
    let (stop, stopped) = watch::channel(false);
    let looping = tokio::spawn(renewer.run(stopped));

    renews_seen(h.fake(), 1).await;
    // Half a life later the loop, which has been napping for 10 ms at a time, renews again.
    h.fake().set_now(h.fake().now() + LIFE_S / 2 + 10);
    renews_seen(h.fake(), 2).await;
    stop.send(true).expect("running");
    timeout(Duration::from_secs(10), looping)
        .await
        .expect("the loop stops when told")
        .expect("no panic");

    // Stopped in the middle of an hour's nap.
    let h = Harness::new("renew-loop-nap");
    let (renewer, _store) = renewing(&h).await;
    let (stop, stopped) = watch::channel(false);
    let looping = tokio::spawn(renewer.run(stopped));
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.send(true).expect("running");
    timeout(Duration::from_secs(10), looping)
        .await
        .expect("the loop stops while it sleeps")
        .expect("no panic");

    // A sender that is gone stops it too.
    let (renewer, _store) = renewing(&Harness::new("renew-loop-dropped")).await;
    let (stop, stopped) = watch::channel(false);
    drop(stop);
    timeout(Duration::from_secs(10), renewer.run(stopped))
        .await
        .expect("a loop whose sender is gone stops");
}

/// Catches a stop that waits for a renewal in flight (the mode has exited, the frontend
/// does not answer, and the process would wait out the connection timeout): the renewer
/// dials a listener that never speaks TLS, and the stop ends the loop at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_does_not_wait_for_a_renewal_in_flight() {
    let h = Harness::new("renew-inflight");
    let (_renewer, _store) = renewing(&h).await;
    let mute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    h.write_config(mute.local_addr().expect("addr").port(), "[daemon]\n");
    let (env, config) = (h.env(), h.config_path());
    let renewer = tokio::task::spawn_blocking(move || {
        let loaded = load(&config, &env).expect("loaded");
        let node = loaded
            .store
            .recover()
            .expect("node.json")
            .expect("enrolled");
        Renewer::new(&loaded, &node, &env, 0)
    })
    .await
    .expect("no panic");
    h.fake().set_now(h.fake().now() + LIFE_S / 2 + 10);
    let (stop, stopped) = watch::channel(false);
    let looping = tokio::spawn(renewer.run(stopped));

    tokio::time::sleep(Duration::from_millis(300)).await;
    stop.send(true).expect("running");
    timeout(Duration::from_secs(5), looping)
        .await
        .expect("the stop does not wait for the dial")
        .expect("no panic");
    assert!(h.fake().renews().is_empty());
    drop(mute);
}

/// Catches a renewer that does not run beside the mode: the node starts with its
/// certificates already past half life, and while the mode "runs" (the launcher waits)
/// the frontend sees the `Renew`. When the mode returns, the start returns too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mode_runs_beside_a_renewer() {
    let h = Harness::new("node-beside");
    h.ready("[daemon]\n");
    let launches = Arc::new(Launches::default());
    start(&h, &launches, || {}).await.expect("first start");
    h.fake().set_now(h.fake().now() + LIFE_S / 2 + 10);

    let fake = Arc::clone(h.fake());
    let renewed = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&renewed);
    let code = start(&h, &launches, move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while fake.renews().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        *flag.lock().expect("flag") = !fake.renews().is_empty();
    })
    .await
    .expect("second start");

    assert!(
        *renewed.lock().expect("flag"),
        "the frontend saw a Renew while the mode ran"
    );
    assert!(is_7(code));
    assert_eq!(h.fake().enrolls().len(), 1);
}

/// The cold store the storage half needs, so that a storage mode can start.
const COLD: &str = "s3://bucket/prefix/?endpoint=https://objects.example";

/// Catches a later start that enrolls again or needs the token (the file is gone after the
/// first start), and one that does not renew: for each mode that reads `kbf.toml`, the
/// first start enrolls (one `Enroll`), the token file is removed, the clock moves past
/// half the certificates' life, and the second start sends one `Renew` while the mode
/// runs and no second `Enroll`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_start_of_each_mode_renews_without_the_token() {
    for mode in [names::STORAGE, names::DAEMON, names::STORAGE_AND_DAEMON] {
        let h = Harness::new(&format!("later-{mode}"));
        h.ready("");
        h.fake().set_cold_store(COLD);
        let launches = Arc::new(Launches::default());
        start_mode(&h, mode, &launches).await.expect("first start");
        assert_eq!(h.fake().enrolls().len(), 1, "{mode}");
        assert!(
            !h.token_path().exists(),
            "{mode}: enrolling removes the token file, so the second start has none"
        );
        h.fake().set_now(h.fake().now() + LIFE_S / 2 + 10);

        let fake = Arc::clone(h.fake());
        start_with(&h, h.start_of(Some(mode)), &launches, move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while fake.renews().is_empty() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        })
        .await
        .unwrap_or_else(|e| panic!("{mode}: a start with no token file: {e}"));

        assert_eq!(launches.seen(), [mode, mode]);
        assert_eq!(h.fake().enrolls().len(), 1, "{mode}: one Enroll, ever");
        let renews = h.fake().renews();
        assert_eq!(renews.len(), 1, "{mode}");
        let roles: Vec<&str> = renews[0].csrs.iter().map(|c| c.role.as_str()).collect();
        let want: &[&str] = match mode {
            names::STORAGE => &["storage"],
            names::DAEMON => &["daemon"],
            _ => &["storage", "daemon"],
        };
        assert_eq!(roles, want, "{mode}: the renewal covers the mode's roles");
    }
}

/// Catches a token that does not grant a role the mode needs being used anyway, or
/// retried, or reported as a network failure: the frontend's typed refusal stops the start
/// with exit 2 (a restart cannot fix it), the reason is in the message, nothing is
/// launched or installed and the token file stays. A token that grants more than the mode
/// needs enrolls only what the mode asks for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_lacking_a_needed_role_exits_2_with_the_typed_refusal() {
    let cases = [
        (names::DAEMON, vec!["storage"]),
        (names::STORAGE, vec!["daemon"]),
        (names::STORAGE_AND_DAEMON, vec!["storage"]),
        (names::STORAGE_AND_DAEMON, vec!["daemon"]),
    ];
    for (i, (mode, granted)) in cases.into_iter().enumerate() {
        let h = Harness::new(&format!("lacking-{i}"));
        h.ready("");
        h.put_token(&h.fake().mint_roles(1, 1, &granted));
        let launches = Arc::new(Launches::default());

        let failure = start_mode(&h, mode, &launches).await.expect_err("refused");

        assert!(
            matches!(
                &failure,
                Failure::Enroll(EnrollError::Call(CallError::Refused(
                    RefusalReason::RoleNotGranted
                )))
            ),
            "{mode} with a token for {granted:?}: {failure:?}"
        );
        assert_eq!(failure.exit_code(), 2, "{mode}");
        assert!(
            failure
                .to_string()
                .contains(RefusalReason::RoleNotGranted.message()),
            "{failure}"
        );
        assert!(launches.seen().is_empty());
        assert!(h.waits.lock().expect("waits").is_empty(), "not retried");
        assert!(!h.state().join("pki/node.json").exists());
        assert!(h.token_path().exists());
    }

    let h = Harness::new("lacking-more");
    h.ready("");
    h.put_token(&h.fake().mint_roles(1, 1, &["storage", "daemon"]));
    let launches = Arc::new(Launches::default());
    start_mode(&h, names::DAEMON, &launches)
        .await
        .expect("a token for more than the mode needs");
    assert_eq!(h.asked(), [vec!["daemon".to_owned()]]);
}

/// Catches a mode changed after enrollment running on certificates that do not cover it,
/// or enrolling again behind the operator's back: a node enrolled for the daemon role,
/// now configured `storage` or `storage-and-daemon`, exits 2 naming the roles it holds
/// and the mode, calls the frontend no more and launches nothing; the mode it is
/// enrolled for, or one that needs less, still starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enrolled_roles_that_do_not_cover_the_mode_exit_2() {
    let h = Harness::new("not-covered");
    h.ready("");
    h.fake().set_cold_store(COLD);
    let launches = Arc::new(Launches::default());
    start_mode(&h, names::DAEMON, &launches)
        .await
        .expect("daemon");
    // A fresh token would let a silent re-enrollment succeed, which is the bug.
    h.put_token(&h.fake().mint(2, 5));

    for mode in [names::STORAGE, names::STORAGE_AND_DAEMON] {
        let failure = start_mode(&h, mode, &launches)
            .await
            .expect_err("not covered");
        let text = failure.to_string();
        assert!(
            matches!(failure, Failure::NotCovered { .. }),
            "{mode}: {text}"
        );
        assert_eq!(failure.exit_code(), 2, "{mode}");
        assert!(
            text.contains(mode)
                && text.contains("enrolled for [daemon]")
                && text.contains("storage"),
            "names the mode and what is enrolled: {text}"
        );
    }
    assert_eq!(
        launches.seen(),
        [names::DAEMON],
        "only the first start ran a mode"
    );
    assert_eq!(h.fake().enrolls().len(), 1, "no silent re-enrollment");
    assert!(h.token_path().exists());

    // Enrolled for both, a mode of one role is covered; the file's mode can be changed.
    let both = Harness::new("covered-by-more");
    both.ready("");
    both.fake().set_cold_store(COLD);
    let launches = Arc::new(Launches::default());
    start_mode(&both, names::STORAGE_AND_DAEMON, &launches)
        .await
        .expect("both");
    start_mode(&both, names::STORAGE, &launches)
        .await
        .expect("storage is covered");
    start_mode(&both, names::DAEMON, &launches)
        .await
        .expect("daemon is covered");
    assert_eq!(
        launches.seen(),
        [names::STORAGE_AND_DAEMON, names::STORAGE, names::DAEMON]
    );
    assert_eq!(both.fake().enrolls().len(), 1);
}

/// Catches the mode's sources being mixed up: the flag alone, the file alone (a start with
/// no mode flag), the flag beating a file that says another mode (and enrolling for the
/// flag's roles), and neither source being a refusal that names both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mode_is_the_flag_else_the_file_else_exit_2() {
    let h = Harness::new("mode-source");
    h.ready("");
    h.file_says("daemon");
    let launches = Arc::new(Launches::default());
    start_with(&h, h.start_of(None), &launches, || {})
        .await
        .expect("the file's mode");
    assert_eq!(launches.seen(), [names::DAEMON]);
    assert_eq!(h.asked(), [vec!["daemon".to_owned()]]);

    let h = Harness::new("mode-flag-wins");
    h.ready("");
    h.fake().set_cold_store(COLD);
    h.file_says("storage-and-daemon");
    let launches = Arc::new(Launches::default());
    start_mode(&h, names::STORAGE, &launches)
        .await
        .expect("the flag's mode");
    assert_eq!(launches.seen(), [names::STORAGE]);
    assert_eq!(
        h.asked(),
        [vec!["storage".to_owned()]],
        "the flag's roles, not the file's"
    );

    let h = Harness::new("mode-neither");
    h.ready("");
    let launches = Arc::new(Launches::default());
    let failure = start_with(&h, h.start_of(None), &launches, || {})
        .await
        .expect_err("no mode");
    let text = failure.to_string();
    assert!(matches!(failure, Failure::NoMode { .. }), "{text}");
    assert_eq!(failure.exit_code(), 2);
    assert!(
        text.contains("--mode") && text.contains("mode = ") && text.contains("kbf.toml"),
        "{text}"
    );
    assert!(launches.seen().is_empty() && h.fake().enrolls().is_empty());

    // No file either: the refusal still names both, and why the file does not help.
    let gone = Start {
        config: h.root.join("none.toml"),
        ..h.start_of(None)
    };
    let failure = start_with(&h, gone, &launches, || {})
        .await
        .expect_err("no file");
    let text = failure.to_string();
    assert!(matches!(failure, Failure::NoMode { .. }), "{text}");
    assert!(
        text.contains("none.toml") && text.contains("cannot read"),
        "{text}"
    );

    // A name in the file that is not a mode.
    h.file_says("node");
    let failure = start_with(&h, h.start_of(None), &launches, || {})
        .await
        .expect_err("not a mode");
    assert!(matches!(failure, Failure::BadMode { .. }), "{failure}");

    // A mode that takes flags, not a file.
    let h = Harness::new("mode-flags-only");
    h.ready("");
    h.file_says("frontend");
    let failure = start_with(&h, h.start_of(None), &launches, || {})
        .await
        .expect_err("flags only");
    assert!(matches!(failure, Failure::FlagsOnly(_)), "{failure}");
}

/// A start of `mode` that only checks.
fn checking(h: &Harness, mode: Option<&'static str>) -> Start {
    Start {
        check: true,
        ..h.start_of(mode)
    }
}

/// Catches `--check` that fails a host that is configured and not enrolled yet (the unit's
/// `ExecStartPre` would keep the first start from ever enrolling), or that enrolls,
/// dials the frontend, reads the token away or starts a mode: for each mode the check
/// passes with a valid `kbf.toml` and a token file and no `node.json`, and nothing
/// changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_passes_before_enrollment_and_changes_nothing() {
    for mode in [names::STORAGE, names::DAEMON, names::STORAGE_AND_DAEMON] {
        let h = Harness::new(&format!("check-before-{mode}"));
        h.ready("");
        let launches = Arc::new(Launches::default());
        let code = start_with(&h, checking(&h, Some(mode)), &launches, || {})
            .await
            .unwrap_or_else(|e| panic!("{mode}: {e}"));
        assert_eq!(
            format!("{code:?}"),
            format!("{:?}", ExitCode::SUCCESS),
            "{mode}"
        );
        assert!(launches.seen().is_empty(), "{mode}: --check starts nothing");
        assert!(
            h.fake().enrolls().is_empty(),
            "{mode}: --check enrolls nothing"
        );
        assert!(
            h.token_path().exists(),
            "{mode}: the token file is untouched"
        );
        assert!(!h.state().join("pki/node.json").exists(), "{mode}");
    }
    // The mode from the file passes the same.
    let h = Harness::new("check-file-mode");
    h.ready("");
    h.file_says("daemon");
    start_with(
        &h,
        checking(&h, None),
        &Arc::new(Launches::default()),
        || {},
    )
    .await
    .expect("the file's mode checks");
}

/// Catches `--check` that passes what a start would refuse: a config that is not right (a
/// key it does not know, no file), a token file that is missing or open, and an
/// enrolled `node.json` that does not cover the mode or names another node are each exit 2
/// with the cause; and one that fails an enrolled host that a start would run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_exits_2_for_a_host_that_cannot_start() {
    let launches = Arc::new(Launches::default());
    let h = Harness::new("check-bad");

    let failure = start_with(&h, checking(&h, Some(names::DAEMON)), &launches, || {})
        .await
        .expect_err("no config");
    assert!(matches!(failure, Failure::ReadConfig { .. }), "{failure:?}");
    assert_eq!(failure.exit_code(), 2);

    h.ready("[daemon]\nbogus = 1\n");
    let failure = start_with(&h, checking(&h, Some(names::DAEMON)), &launches, || {})
        .await
        .expect_err("bad config");
    assert!(matches!(failure, Failure::Settings(_)), "{failure:?}");
    assert!(failure.to_string().contains("bogus"), "{failure}");
    assert_eq!(failure.exit_code(), 2);

    h.ready("");
    std::fs::set_permissions(h.token_path(), std::fs::Permissions::from_mode(0o644)).expect("mode");
    let failure = start_with(&h, checking(&h, Some(names::DAEMON)), &launches, || {})
        .await
        .expect_err("open token");
    assert!(
        matches!(failure, Failure::Token(TokenFileError::Mode { .. })),
        "{failure:?}"
    );
    std::fs::remove_file(h.token_path()).expect("rm");
    let failure = start_with(&h, checking(&h, Some(names::DAEMON)), &launches, || {})
        .await
        .expect_err("no token");
    assert!(
        matches!(failure, Failure::Token(TokenFileError::Io { .. })),
        "{failure:?}"
    );
    assert_eq!(failure.exit_code(), 2);

    // Enrolled for the daemon only: the daemon checks, the storage modes do not.
    let h = Harness::new("check-enrolled");
    h.ready("");
    h.fake().set_cold_store(COLD);
    start_mode(&h, names::DAEMON, &launches)
        .await
        .expect("enrolled");
    start_with(&h, checking(&h, Some(names::DAEMON)), &launches, || {})
        .await
        .expect("an enrolled host checks");
    for mode in [names::STORAGE, names::STORAGE_AND_DAEMON] {
        let failure = start_with(&h, checking(&h, Some(mode)), &launches, || {})
            .await
            .expect_err("not covered");
        assert!(
            matches!(failure, Failure::NotCovered { .. }),
            "{mode}: {failure:?}"
        );
        assert_eq!(failure.exit_code(), 2);
    }
    let text = std::fs::read_to_string(h.config_path()).expect("kbf.toml");
    write_config(&h.config_path(), &text.replace("node-02", "node-03"));
    let failure = start_with(&h, checking(&h, Some(names::DAEMON)), &launches, || {})
        .await
        .expect_err("renamed");
    assert!(
        matches!(failure, Failure::RenamedNode { .. }),
        "{failure:?}"
    );
    assert_eq!(launches.seen(), [names::DAEMON], "no check started a mode");
    assert_eq!(h.fake().enrolls().len(), 1);
}

/// Catches a frontend that is down failing the first start for good, or hammering it: the
/// start waits 5 seconds, then 10, then 20 (it asks the wait to be made through the
/// environment, so the test does not sleep), and gives up with the failure and exit 1 only
/// when the environment says so; a frontend that comes back while the start waits is
/// enrolled with, and the mode starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_that_is_down_is_retried_with_a_backoff() {
    let h = Harness::new("retry-down");
    h.ready("");
    h.fake().refuse_with(Some(RefusalReason::StoreUnavailable));
    *h.patience.lock().expect("patience") = 2;
    let launches = Arc::new(Launches::default());

    let failure = start(&h, &launches, || {}).await.expect_err("gives up");

    assert_eq!(failure.exit_code(), 1, "{failure:?}");
    assert_eq!(
        *h.waits.lock().expect("waits"),
        [5, 10, 20].map(Duration::from_secs),
        "three waits were asked for, two were honoured"
    );
    assert_eq!(
        h.fake().enrolls().len(),
        3,
        "one attempt before each wait, and one after"
    );
    assert!(launches.seen().is_empty());
    assert!(h.token_path().exists());

    // It comes back while the start waits.
    let h = Harness::new("retry-back");
    h.ready("");
    h.fake().refuse_with(Some(RefusalReason::RateLimited));
    *h.patience.lock().expect("patience") = 5;
    let fake = Arc::clone(h.fake());
    *h.on_wait.lock().expect("hook") = Some(Box::new(move || fake.refuse_with(None)));
    let launches = Arc::new(Launches::default());
    let code = start(&h, &launches, || {})
        .await
        .expect("enrolled after a wait");
    assert!(is_7(code));
    assert_eq!(*h.waits.lock().expect("waits"), [Duration::from_secs(5)]);
    assert_eq!(launches.seen(), [names::DAEMON]);

    // A failure a restart cannot fix is never waited on.
    let h = Harness::new("retry-never");
    h.ready("");
    h.fake().refuse_with(Some(RefusalReason::TokenExpired));
    *h.patience.lock().expect("patience") = 5;
    let failure = start(&h, &Arc::new(Launches::default()), || {})
        .await
        .expect_err("refused");
    assert_eq!(failure.exit_code(), 2);
    assert!(h.waits.lock().expect("waits").is_empty());
    assert_eq!(h.fake().enrolls().len(), 1);
}
