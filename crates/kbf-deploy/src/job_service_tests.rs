//! `job-service check` against `kbf-job-client`'s in-process job service, on loopback with a
//! port the system picks, and against an address on which connecting is refused. No test reads
//! a clock or sleeps: the one call either answers or fails.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use js_proto::v1::{Constraints, ExecMode, JobSpec, Needs, Shape};
use kbf_job_client::fake::{Call, FakeJobServer, Fault, Machine, TOKEN};
use kbf_testkit::dir::TestDir;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use super::*;

/// A token the fake does not take, distinctive enough that finding it in any output would be
/// the token leaking and nothing else.
const WRONG_TOKEN: &str = "kbf-check-WRONG-token-5f0c2e9a7b13d468";

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-deploy-job-service-{name}"),
    )
}

/// A file named `name` in `dir`, holding `text`, made with `mode`.
fn file(dir: &Path, name: &str, mode: u32, text: &str) -> PathBuf {
    let path = dir.join(name);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
    path
}

fn args(url: &str, token_file: &Path, ca_file: Option<PathBuf>) -> CheckArgs {
    CheckArgs {
        url: url.to_owned(),
        token_file: token_file.to_path_buf(),
        ca_file,
    }
}

/// What a run of the check gave: its exit code, stdout and stderr.
struct Ran {
    code: ExitCode,
    out: String,
    err: String,
}

impl Ran {
    /// Asserts that `secret` is in neither stream.
    fn holds_no(&self, secret: &str) {
        assert!(
            !self.out.contains(secret),
            "stdout holds the token: {}",
            self.out
        );
        assert!(
            !self.err.contains(secret),
            "stderr holds the token: {}",
            self.err
        );
    }
}

/// The check of `a`, run on the test's own runtime.
async fn checked(a: &CheckArgs) -> Ran {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = match check(a, &mut out).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let word = if e.code() == 2 { "REFUSED" } else { "FAIL" };
            writeln!(err, "{word}: {e}").unwrap();
            ExitCode::from(e.code())
        }
    };
    Ran {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

/// The two lines every check that reaches the call prints first.
fn header(url: &str, token_file: &Path) -> String {
    format!("job service: {url}\ntoken file: {}\n", token_file.display())
}

/// The calls the fake read, which the check's must be: one `ListJobs` of one job.
fn one_list_of_one(server: &FakeJobServer) {
    let calls = server.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let Call::ListJobs(request) = &calls[0] else {
        panic!("not ListJobs: {calls:?}")
    };
    assert_eq!(request.page_size, 1);
    assert!(request.page_token.is_empty() && request.states.is_empty());
}

/// A host of 4 cores and 16 GiB running containers on Linux, and the smallest job it admits.
fn host_and_job() -> (Machine, JobSpec) {
    let machine = Machine {
        shape: Shape {
            cpu_millis: 4000,
            memory_bytes: 16 << 30,
            ..Shape::default()
        },
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
    };
    let spec = JobSpec {
        job_id: "check-1".to_owned(),
        image: format!("registry.example/kbf-task@sha256:{}", "0".repeat(64)),
        command: "/bin/build".to_owned(),
        needs: Some(Needs {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            ..Needs::default()
        }),
        constraints: Some(Constraints {
            os: "linux".to_owned(),
            ..Constraints::default()
        }),
        wall_limit_seconds: Some(600),
        ..JobSpec::default()
    };
    (machine, spec)
}

/// Behaviour: a service that answers ListJobs with no job passes: a farm with no job yet is a
/// working farm. The output names the service and the token file, then the pass. Catches a
/// check that takes an empty farm for a failure.
#[tokio::test]
async fn an_empty_list_passes() {
    let dir = scratch("empty");
    let token = file(dir.path(), "token", 0o600, &format!("{TOKEN}\n"));
    let server = FakeJobServer::start().await.unwrap();
    let url = server.origin();

    let ran = checked(&args(&url, &token, None)).await;

    assert_eq!(ran.code, ExitCode::SUCCESS, "{}", ran.err);
    assert_eq!(
        ran.out,
        format!(
            "{}pass: the job service answered ListJobs (no job of this client yet)\n",
            header(&url, &token)
        )
    );
    assert_eq!(ran.err, "");
    one_list_of_one(&server);
}

/// Behaviour: a service that answers with a job passes too, and says it listed jobs.
#[tokio::test]
async fn a_list_with_a_job_passes() {
    let dir = scratch("listed");
    let token = file(dir.path(), "token", 0o400, TOKEN);
    let server = FakeJobServer::start().await.unwrap();
    let (machine, spec) = host_and_job();
    server.add_host("host-a", machine);
    server.record(spec).unwrap();
    let url = server.origin();

    let ran = checked(&args(&url, &token, None)).await;

    assert_eq!(ran.code, ExitCode::SUCCESS, "{}", ran.err);
    assert_eq!(
        ran.out,
        format!(
            "{}pass: the job service answered ListJobs (jobs of this client listed)\n",
            header(&url, &token)
        )
    );
    one_list_of_one(&server);
}

/// Behaviour: a service that refuses the token (`UNAUTHENTICATED`, what the job service's
/// bearer layer answers a token of no client) fails with exit 1, and the message says
/// unauthorized, names the address, the token file and the service's own reason. Catches a
/// check that reports success on any answer.
#[tokio::test]
async fn a_refused_token_fails_as_unauthorized() {
    let dir = scratch("unauthorized");
    let token = file(dir.path(), "token", 0o600, WRONG_TOKEN);
    let server = FakeJobServer::start().await.unwrap();
    let url = server.origin();

    let ran = checked(&args(&url, &token, None)).await;

    assert_eq!(ran.code, ExitCode::from(1));
    assert_eq!(ran.out, header(&url, &token));
    assert_eq!(
        ran.err,
        format!(
            "FAIL: unauthorized: the job service at {url} refused the token in {} \
             (Unauthenticated: the bearer token is missing or wrong)\n",
            token.display()
        )
    );
    // The fake lists no call it refused for its token: nothing was answered.
    assert!(server.calls().is_empty());
}

/// Behaviour: `PERMISSION_DENIED` (a token the service knows but that may not make the call)
/// is unauthorized as well, with its code and message.
#[tokio::test]
async fn a_token_that_may_not_list_fails_as_unauthorized() {
    let dir = scratch("denied");
    let token = file(dir.path(), "token", 0o600, TOKEN);
    let server = FakeJobServer::start().await.unwrap();
    server.inject(Fault::Status {
        code: Code::PermissionDenied,
        message: "operator-only".to_owned(),
    });
    let url = server.origin();

    let ran = checked(&args(&url, &token, None)).await;

    assert_eq!(ran.code, ExitCode::from(1));
    assert_eq!(
        ran.err,
        format!(
            "FAIL: unauthorized: the job service at {url} refused the token in {} \
             (PermissionDenied: operator-only)\n",
            token.display()
        )
    );
}

/// Behaviour: the job service's own bearer layer refuses a token with `UNAUTHENTICATED` and an
/// empty message; the failure then shows the code alone, with no dangling `: `.
#[tokio::test]
async fn a_refusal_with_no_message_shows_the_code_alone() {
    let dir = scratch("unauthorized-bare");
    let token = file(dir.path(), "token", 0o600, TOKEN);
    let server = FakeJobServer::start().await.unwrap();
    server.inject(Fault::Status {
        code: Code::Unauthenticated,
        message: String::new(),
    });
    let url = server.origin();

    let ran = checked(&args(&url, &token, None)).await;

    assert_eq!(ran.code, ExitCode::from(1));
    assert_eq!(
        ran.err,
        format!(
            "FAIL: unauthorized: the job service at {url} refused the token in {} \
             (Unauthenticated)\n",
            token.display()
        )
    );
}

/// Behaviour: an address on which connecting is refused fails with exit 1, and the message
/// says unreachable, names the address and carries the connect failure down to the system's
/// own words. Nothing was answered, so there is no pass line.
#[tokio::test]
async fn an_unreachable_address_fails_naming_it_and_the_connect_error() {
    let dir = scratch("unreachable");
    let token = file(dir.path(), "token", 0o600, TOKEN);
    let url = format!("http://{}", kbf_testkit::port::dead_addr());

    let ran = checked(&args(&url, &token, None)).await;

    assert_eq!(ran.code, ExitCode::from(1));
    assert_eq!(ran.out, header(&url, &token));
    // The system's own words end the message, with its error number (which differs by
    // system), and nothing of the client's internals is dumped after them.
    let want = format!(
        "FAIL: unreachable: cannot connect to the job service at {url}: connecting: \
         Connection refused (os error "
    );
    assert!(ran.err.starts_with(&want), "{}", ran.err);
    assert!(ran.err.ends_with(")\n"), "{}", ran.err);
    assert_eq!(ran.err.lines().count(), 1, "{}", ran.err);
    assert!(!ran.err.contains("tonic"), "{}", ran.err);
}

/// Behaviour: any other error is a failure naming the address and the service's answer: here
/// `UNIMPLEMENTED`, what a server without the job service answers.
#[tokio::test]
async fn another_error_fails_naming_the_answer() {
    let dir = scratch("unimplemented");
    let token = file(dir.path(), "token", 0o600, TOKEN);
    let server = FakeJobServer::start().await.unwrap();
    server.inject(Fault::Status {
        code: Code::Unimplemented,
        message: "unknown service jobscheduler.v1.JobService".to_owned(),
    });
    let url = server.origin();

    let ran = checked(&args(&url, &token, None)).await;

    assert_eq!(ran.code, ExitCode::from(1));
    assert_eq!(
        ran.err,
        format!(
            "FAIL: the job service at {url} did not answer ListJobs: the job service answered \
             Unimplemented: unknown service jobscheduler.v1.JobService\n"
        )
    );
}

/// Behaviour: whether it passes or fails, the output names the token file by its path and
/// never holds the token: not the token the service takes, not one it refuses.
#[tokio::test]
async fn the_output_names_the_token_file_and_never_the_token() {
    let dir = scratch("no-leak");
    let server = FakeJobServer::start().await.unwrap();
    let url = server.origin();

    let good = file(dir.path(), "good", 0o600, TOKEN);
    let passed = checked(&args(&url, &good, None)).await;
    assert_eq!(passed.code, ExitCode::SUCCESS);
    assert!(
        passed
            .out
            .contains(&format!("token file: {}\n", good.display()))
    );
    passed.holds_no(TOKEN);

    let wrong = file(dir.path(), "wrong", 0o600, WRONG_TOKEN);
    let refused = checked(&args(&url, &wrong, None)).await;
    assert_eq!(refused.code, ExitCode::from(1));
    assert!(
        refused
            .out
            .contains(&format!("token file: {}\n", wrong.display()))
    );
    assert!(refused.err.contains(&wrong.display().to_string()));
    refused.holds_no(WRONG_TOKEN);

    // A token file the client refuses for its mode names the path, not the content.
    let open = file(dir.path(), "open", 0o644, WRONG_TOKEN);
    let bad = checked(&args(&url, &open, None)).await;
    assert_eq!(bad.code, ExitCode::from(2));
    bad.holds_no(WRONG_TOKEN);
}

/// A CA and a certificate for `localhost` under it, as PEM, and the server side of them.
fn pki() -> (String, Arc<rustls::ServerConfig>) {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca =
        rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().unwrap()).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    let chain: Vec<_> = CertificateDer::pem_slice_iter(leaf.pem().as_bytes())
        .map(Result::unwrap)
        .collect();
    let key = PrivateKeyDer::from_pem_slice(key.serialize_pem().as_bytes()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    (ca.pem(), Arc::new(config))
}

/// Behaviour: an `https` service is checked over TLS against the CA of `--ca-file` and passes;
/// the same service with another CA fails as unreachable (the TLS session is part of the
/// connection), naming the address.
#[tokio::test]
async fn an_https_service_is_checked_against_the_ca_file() {
    let dir = scratch("https");
    let token = file(dir.path(), "token", 0o600, TOKEN);
    let (ca, server_config) = pki();
    let server = FakeJobServer::start_tls(server_config).await.unwrap();
    let url = server.origin();
    let ca_file = file(dir.path(), "ca.pem", 0o644, &ca);

    let ran = checked(&args(&url, &token, Some(ca_file))).await;
    assert_eq!(ran.code, ExitCode::SUCCESS, "{}", ran.err);
    one_list_of_one(&server);

    let (other_ca, _) = pki();
    let other = file(dir.path(), "other.pem", 0o644, &other_ca);
    let ran = checked(&args(&url, &token, Some(other))).await;
    assert_eq!(ran.code, ExitCode::from(1));
    let want = format!(
        "FAIL: unreachable: cannot connect to the job service at {url}: the TLS handshake: \
         invalid peer certificate: "
    );
    assert!(ran.err.starts_with(&want), "{}", ran.err);
}

/// Behaviour: what cannot be a check is refused with exit 2 before anything is dialled, and
/// the message names the flag: an origin the client refuses, a token file that is missing,
/// open to others or holds no token, an `https` origin without a CA, an `http` one with a CA,
/// and a CA file that is missing or holds no certificate.
#[tokio::test]
async fn what_cannot_be_checked_is_refused_naming_the_flag() {
    let dir = scratch("refused");
    let token = file(dir.path(), "token", 0o600, TOKEN);
    let server = FakeJobServer::start().await.unwrap();
    let url = server.origin();
    let missing = dir.path().join("missing");
    let open = file(dir.path(), "open", 0o644, TOKEN);
    let empty = file(dir.path(), "empty", 0o600, "\n");
    let not_pem = file(dir.path(), "not.pem", 0o644, "not a certificate\n");

    let cases: Vec<(CheckArgs, String)> = vec![
        (
            args("http://jobs.example:8443", &token, None),
            "REFUSED: --url: origin \"http://jobs.example:8443\" is plain http off loopback; \
             the token would cross the network in clear"
                .to_owned(),
        ),
        (
            args("ftp://127.0.0.1:1", &token, None),
            "REFUSED: --url: origin \"ftp://127.0.0.1:1\": the scheme must be http or https"
                .to_owned(),
        ),
        (
            args(&url, &missing, None),
            format!(
                "REFUSED: --token-file: token file {}: No such file or directory (os error 2)",
                missing.display()
            ),
        ),
        (
            args(&url, &open, None),
            format!(
                "REFUSED: --token-file: token file {}: mode is not 0600 or 0400",
                open.display()
            ),
        ),
        (
            args(&url, &empty, None),
            format!(
                "REFUSED: --token-file: token file {}: the token is empty, or not printable \
                 ASCII without spaces",
                empty.display()
            ),
        ),
        (
            args("https://localhost:1", &token, None),
            "REFUSED: --url is https: give --ca-file".to_owned(),
        ),
        (
            args(&url, &token, Some(not_pem.clone())),
            "REFUSED: --ca-file is for an https --url".to_owned(),
        ),
        (
            args("https://localhost:1", &token, Some(missing.clone())),
            format!(
                "REFUSED: --ca-file {}: No such file or directory (os error 2)",
                missing.display()
            ),
        ),
    ];
    for (a, want) in cases {
        let ran = checked(&a).await;
        assert_eq!(ran.code, ExitCode::from(2), "{want}");
        assert_eq!(ran.err, format!("{want}\n"));
        assert_eq!(ran.out, "", "{want}");
    }

    let a = args("https://localhost:1", &token, Some(not_pem));
    let ran = checked(&a).await;
    assert_eq!(ran.code, ExitCode::from(2));
    assert!(
        ran.err
            .starts_with("REFUSED: --ca-file: the CA PEM is unusable: "),
        "{}",
        ran.err
    );
    assert!(server.calls().is_empty(), "a refused check dialled");
}

/// Behaviour: the verb as the command line runs it, on a runtime of its own: a pass exits 0
/// and writes nothing to stderr, a refused token exits 1 with `FAIL: unauthorized`. The fake
/// serves from a runtime of the test's, on its own threads.
#[test]
fn the_verb_runs_on_its_own_runtime() {
    let dir = scratch("verb");
    let good = file(dir.path(), "good", 0o600, TOKEN);
    let wrong = file(dir.path(), "wrong", 0o600, WRONG_TOKEN);
    let serving = tokio::runtime::Runtime::new().unwrap();
    let server = serving.block_on(FakeJobServer::start()).unwrap();
    let url = server.origin();
    let exec = kbf_host::testkit::FakeExec::new(501);
    let mut host = crate::cutover_apply::OnHost { exec: &exec };

    let (mut out, mut err) = (Vec::new(), Vec::new());
    let verb = JobService::Check(args(&url, &good, None));
    assert_eq!(
        run(&verb, &mut host, 0, &mut out, &mut err),
        ExitCode::SUCCESS
    );
    assert!(
        String::from_utf8(out)
            .unwrap()
            .ends_with("(no job of this client yet)\n")
    );
    assert!(err.is_empty());

    let (mut out, mut err) = (Vec::new(), Vec::new());
    let verb = JobService::Check(args(&url, &wrong, None));
    assert_eq!(
        run(&verb, &mut host, 0, &mut out, &mut err),
        ExitCode::from(1)
    );
    let err = String::from_utf8(err).unwrap();
    assert!(err.starts_with("FAIL: unauthorized: "), "{err}");
}

/// Behaviour: when no runtime can be made, the verb fails (exit 1) naming the address and why,
/// and calls nothing.
#[test]
fn no_runtime_is_a_failure_naming_why() {
    let a = args("http://127.0.0.1:1", Path::new("/nonexistent/token"), None);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let runtime = Err(io::Error::other("out of threads"));
    assert_eq!(run_on(runtime, &a, &mut out, &mut err), ExitCode::from(1));
    assert!(out.is_empty());
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "FAIL: the job service at http://127.0.0.1:1 did not answer ListJobs: no runtime to \
         call it from: out of threads\n"
    );
}
