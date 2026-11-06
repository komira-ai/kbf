//! `kbf-deploy job-service check`, run as the binary against `kbf-job-client`'s in-process
//! job service on a loopback port the system picks. The fake serves from this test's runtime;
//! the binary makes its one call and exits.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kbf_job_client::fake::{FakeJobServer, TOKEN};
use kbf_testkit::dir::TestDir;

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-deploy-bin-job-service-{name}"),
    )
}

fn token_file(dir: &Path, token: &str) -> PathBuf {
    let path = dir.join("token");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    writeln!(f, "{token}").unwrap();
    path
}

/// The binary's check of `url` with the token in `token_file`. It runs on a blocking thread,
/// so the fake keeps serving on the test's runtime meanwhile.
async fn check(url: String, token_file: PathBuf) -> Output {
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_kbf-deploy"))
            .args(["job-service", "check", "--url", &url, "--token-file"])
            .arg(&token_file)
            .output()
            .expect("the binary runs")
    })
    .await
    .unwrap()
}

/// Behaviour: the binary passes (exit 0) against a service that answers its token with an
/// empty list, and fails (exit 1) as unauthorized against the same service with a token it
/// refuses; neither output holds the token, and both name the token file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binary_passes_an_answering_service_and_fails_a_refused_token() {
    let server = FakeJobServer::start().await.unwrap();
    let url = server.origin();

    let good = scratch("good");
    let good_file = token_file(good.path(), TOKEN);
    let ran = check(url.clone(), good_file.clone()).await;
    let out = String::from_utf8_lossy(&ran.stdout);
    let err = String::from_utf8_lossy(&ran.stderr);
    assert_eq!(ran.status.code(), Some(0), "{err}");
    assert_eq!(
        out,
        format!(
            "job service: {url}\ntoken file: {}\n\
             pass: the job service answered ListJobs (no job of this client yet)\n",
            good_file.display()
        )
    );
    assert_eq!(err, "");
    assert!(!out.contains(TOKEN));

    let wrong_token = "kbf-bin-test-WRONG-token-9d2b71c4e8a0";
    let wrong = scratch("wrong");
    let wrong_file = token_file(wrong.path(), wrong_token);
    let ran = check(url.clone(), wrong_file.clone()).await;
    let out = String::from_utf8_lossy(&ran.stdout);
    let err = String::from_utf8_lossy(&ran.stderr);
    assert_eq!(ran.status.code(), Some(1), "{err}");
    assert_eq!(
        err,
        format!(
            "FAIL: unauthorized: the job service at {url} refused the token in {} \
             (Unauthenticated: the bearer token is missing or wrong)\n",
            wrong_file.display()
        )
    );
    assert!(!out.contains(wrong_token) && !err.contains(wrong_token));
}
