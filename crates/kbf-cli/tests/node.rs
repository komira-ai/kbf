//! `kbf node` and `kbf enroll` against the in-process frontend of `kbf-enroll`'s tests
//! (`kbf.enroll.v1` on a loopback listener, the real node library, real files in a scratch
//! root): the first start enrolls and picks the mode, a restart does not enroll again, a
//! refused token stops the start with the reason, and the renewer keeps the certificates
//! fresh while the mode runs.
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

use kbf_cli::enroll::{Enroll, Env, Failure, enroll_with, ensure_enrolled, load};
use kbf_cli::node::{Driver, Node, Renewer, run_with};
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

struct Harness {
    run: Running,
    root: PathBuf,
}

impl Harness {
    fn new(name: &str) -> Self {
        Self {
            run: fake::start(),
            root: scratch(name),
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
        std::fs::write(self.config_path(), text).expect("kbf.toml");
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
        Env {
            host: Host {
                hostname: "node-02".to_owned(),
                cpus: 8,
            },
            euid: rustix::process::geteuid().as_raw(),
            now: Arc::new(move || fake.now()),
        }
    }

    fn node(&self) -> Node {
        Node {
            config: self.config_path(),
            labels: Vec::new(),
        }
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
    }
}

/// Runs `kbf node` on a thread of its own (it makes runtimes of its own), with a launcher
/// that records the mode and returns exit status 7.
async fn start(
    h: &Harness,
    launches: &Arc<Launches>,
    before_exit: impl Fn() + Send + 'static,
) -> Result<ExitCode, Failure> {
    let (node, env) = (h.node(), h.env());
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

async fn enrolling(h: &Harness) -> Result<kbf_cli::enroll::Outcome, Failure> {
    let (args, env) = (
        Enroll {
            config: h.config_path(),
        },
        h.env(),
    );
    tokio::task::spawn_blocking(move || enroll_with(&args, &env))
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

/// Catches a first start that does not enroll, enrolls but starts another mode than the
/// roles say, leaves the key readable, keeps the token file, or does not hand the mode's
/// exit status back. The launcher looks at the files at the moment the mode would start:
/// the certificate and key of the daemon role are there, the key private, and the token
/// file is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_start_enrolls_and_runs_the_mode_the_roles_name() {
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
    assert!(matches!(failure, Failure::Config(_)), "{failure:?}");
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
    std::fs::write(h.config_path(), text.replace("node-02", "node-03")).expect("rewrite");
    let failure = start(&h, &launches, || {}).await.expect_err("renamed");
    assert!(
        matches!(&failure, Failure::RenamedNode { enrolled, configured }
            if enrolled == "node-02" && configured == "node-03"),
        "{failure:?}"
    );
    assert_eq!(failure.exit_code(), 2);
    assert_eq!(launches.seen().len(), 1);
}

/// Catches `kbf enroll` that is not idempotent: the first run enrolls, the next ones
/// report the node as enrolled and call the frontend no more, so it can run before every
/// start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_enrolls_once_and_then_does_nothing() {
    let h = Harness::new("enroll-twice");
    h.ready("[daemon]\n");
    let first = enrolling(&h).await.expect("enrolled");
    assert!(first.fresh);
    std::fs::remove_file(h.token_path()).ok();
    let second = enrolling(&h).await.expect("already enrolled");
    assert!(!second.fresh);
    assert_eq!(second.node, first.node);
    assert_eq!(h.fake().enrolls().len(), 1);
}

/// Catches the wrong set of roles being asked for: the tables of `kbf.toml` name the
/// roles, and a file that names neither (or both) asks for both. A request for a role the
/// token lacks would be refused whole, so asking for no more than the file names is what
/// lets a token that grants one role enroll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tables_of_kbf_toml_name_the_roles_asked_for() {
    let cases = [
        ("[daemon]\n", vec!["daemon"]),
        ("[storage]\n", vec!["storage"]),
        ("", vec!["storage", "daemon"]),
        ("[storage]\n[daemon]\n", vec!["storage", "daemon"]),
    ];
    for (i, (extra, want)) in cases.into_iter().enumerate() {
        let h = Harness::new(&format!("enroll-roles-{i}"));
        h.ready(extra);
        enrolling(&h).await.expect("enrolled");
        let asked: Vec<String> = h.fake().enrolls()[0]
            .csrs
            .iter()
            .map(|c| c.role.clone())
            .collect();
        assert_eq!(asked, want, "{extra:?}");
    }
}

/// Catches a node granted both roles being started as one when the frontend sent the
/// storage half no cold store: the start is refused after enrollment (the node is
/// enrolled, and keeps the enrollment), saying what is missing, and a second start
/// enrolls nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_configuration_missing_the_cold_store_is_refused_after_enrolling() {
    let h = Harness::new("node-no-cold-store");
    h.ready("");
    let launches = Arc::new(Launches::default());
    for _ in 0..2 {
        let failure = start(&h, &launches, || {})
            .await
            .expect_err("no cold store");
        assert!(
            matches!(&failure, Failure::Plan(m) if m.contains("cold store")),
            "{failure:?}"
        );
        assert_eq!(failure.exit_code(), 2);
    }
    assert!(launches.seen().is_empty());
    assert_eq!(h.fake().enrolls().len(), 1);
    assert!(h.node_json().contains("storage"));
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

/// Catches a mode that refuses the flags `kbf node` built (here a label the daemon does not
/// accept) starting anyway, or being reported as anything but a usage error: the node is
/// enrolled, the mode's own parser says what it refused, and nothing runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_label_the_mode_refuses_stops_the_start() {
    let h = Harness::new("node-bad-label");
    h.ready("[daemon]\n");
    let launches = Arc::new(Launches::default());
    let (node, env) = (
        Node {
            config: h.config_path(),
            labels: vec!["no-equals-sign".to_owned()],
        },
        h.env(),
    );

    let failure = tokio::task::spawn_blocking(move || {
        run_with(&node, &env, Driver::Container, &|_| ExitCode::SUCCESS)
    })
    .await
    .expect("no panic")
    .expect_err("refused");

    assert!(matches!(failure, Failure::Mode(_)), "{failure:?}");
    assert_eq!(failure.exit_code(), 2);
    assert!(failure.to_string().contains("--label"), "{failure}");
    assert!(launches.seen().is_empty());
    assert!(h.node_json().contains("node-02"), "enrolled all the same");
}

/// An enrolled node and a renewer for it, with the clock at the fake's.
async fn renewing(h: &Harness) -> (Renewer, Store) {
    h.ready("[daemon]\n");
    let (env, config) = (h.env(), h.config_path());
    tokio::task::spawn_blocking(move || {
        let loaded = load(&config, &env).expect("loaded");
        let outcome = ensure_enrolled(&loaded, &env).expect("enrolled");
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
