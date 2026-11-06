//! Secrets as files of the job: what the job service is given, what is logged, and what a host
//! that cannot start the job comes to.

use std::sync::{Arc, Mutex, PoisonError};

use js_proto::v1::StartFailureCode;
use kbf_front::Finished;
use kbf_server::executor::{Config, Delivery};

use crate::recovery::until_jobs;
use crate::rig::{Rig, submits};

const HOST: &str = "node1";
const TOKEN: &str = "read-token-0001";
const SIGNATURE: &str = "s3cr3t";

fn files(cfg: &mut Config) {
    cfg.delivery = Delivery::Files;
    cfg.insecure_argv_secrets = false;
}

/// The log of a test, as text.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Log);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.0)
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter(self.clone())
    }
}

impl Log {
    fn text(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Takes every line this thread logs, at any level, until the guard drops.
    fn capture(&self) -> tracing::dispatcher::DefaultGuard {
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .finish(),
        );
        tracing::dispatcher::set_default(&dispatch)
    }
}

/// Catches a secret that reaches the job any way but a file: the presigned url and the read
/// token are in the two files the job carries and in no argument and no label; the task is told
/// the paths; the job still ends end to end.
#[tokio::test(start_paused = true)]
async fn the_job_holds_the_secrets_only_in_its_files() {
    let mut rig = Rig::start(&[HOST], 1, files).await;
    rig.pump().await;
    let caller = rig.submit(3);
    rig.pump().await;
    let jobs = rig.jobs_of(3);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let job = &jobs[0];
    assert_eq!(job.spec.files.len(), 2, "{:?}", job.spec.files);
    assert_eq!(job.file("grant-token"), Some(TOKEN));
    let url = job.file("pack-url").expect("the pack url");
    assert!(url.starts_with("https://store.test/cas/b/"), "{url}");
    assert!(url.contains(SIGNATURE), "{url}");
    for arg in &job.spec.args {
        assert!(!arg.contains(TOKEN), "the token is in {arg:?}");
        assert!(!arg.contains(SIGNATURE), "the signature is in {arg:?}");
        assert!(!arg.contains("store.test"), "the url is in {arg:?}");
    }
    for label in job.spec.labels.values() {
        assert!(!label.contains(TOKEN) && !label.contains(url), "{label}");
    }
    assert!(job.spec.env.is_empty());
    for flag in ["--pack-url", "--grant-token", "--insecure-argv-secrets"] {
        assert!(!job.spec.args.iter().any(|a| a == flag), "{flag}");
    }
    assert_eq!(job.arg("--pack-url-file"), Some("/run/js-files/pack-url"));
    assert_eq!(
        job.arg("--grant-token-file"),
        Some("/run/js-files/grant-token")
    );
    rig.host_finishes(job.id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a fall back to arguments: a host that could not start a job whose secrets are files
/// (the job ends failed to start, the agent refusing it) must not make the executor send the
/// action's next job with the secrets as arguments. The action is the farm's to rerun as the
/// infrastructure failure it is, and the rerun job carries its files and no secret in an
/// argument. (The fake's own start failure carries no agent detail; the class each detail,
/// such as `files-unsupported`, comes to is held by the table in `outcome_tests`.)
#[tokio::test(start_paused = true)]
async fn files_delivery_never_falls_back_to_argv_after_a_host_could_not_start_the_job() {
    let mut rig = Rig::start(&[HOST], 1, files).await;
    rig.pump().await;
    let caller = rig.submit(3);
    rig.pump().await;
    let first = rig.jobs_of(3).remove(0);
    assert!(
        rig.fake
            .fail_to_start(first.id(), StartFailureCode::AgentRefused)
    );
    rig.pump().await;
    assert!(caller.finished().is_none(), "{:?}", caller.finished());
    assert_eq!(rig.counter("kbf_executor_runs_total{outcome=\"infra\"}"), 1);
    let jobs = until_jobs(&mut rig, 3, 2, 12).await;
    let rerun = &jobs[1];
    assert_ne!(rerun.id(), first.id());
    assert_eq!(rerun.spec.files.len(), 2, "{:?}", rerun.spec.files);
    assert_eq!(rerun.file("grant-token"), Some(TOKEN));
    for job in &jobs {
        for arg in &job.spec.args {
            assert!(!arg.contains(TOKEN), "the token is in {arg:?}");
            assert!(!arg.contains(SIGNATURE), "the signature is in {arg:?}");
        }
        assert!(job.arg("--pack-url").is_none());
    }
    rig.host_finishes(rerun.id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a secret that travels where it should not, in any of the places a job is shown: with
/// files, the submit the job service decodes holds the url and the token in the job's files and
/// in no argument; the spec printed with `{:?}` holds neither; and no line logged while the job
/// is submitted, read, ingested and settled holds either value.
#[tokio::test(start_paused = true)]
async fn the_files_carry_the_grant_and_the_put_url_and_nothing_else_does() {
    let log = Log::default();
    let _logged = log.capture();
    let mut rig = Rig::start(&[HOST], 1, files).await;
    rig.pump().await;
    let caller = rig.submit(4);
    rig.pump().await;
    let sent = submits(&rig.fake.calls());
    assert_eq!(sent.len(), 1, "{sent:?}");
    let spec = &sent[0];
    let mut named: Vec<(&str, &str)> = spec
        .files
        .iter()
        .map(|file| (file.name.as_str(), file.contents.as_str()))
        .collect();
    named.sort_unstable();
    assert_eq!(named.len(), 2, "{named:?}");
    assert_eq!(named[0], ("grant-token", TOKEN));
    let (name, url) = named[1];
    assert_eq!(name, "pack-url");
    assert!(url.starts_with("https://store.test/cas/b/"), "{url}");
    assert!(url.ends_with(&format!("?sig={SIGNATURE}")), "{url}");
    for arg in &spec.args {
        assert!(!arg.contains(TOKEN) && !arg.contains(SIGNATURE), "{arg:?}");
    }
    let printed = format!("{spec:?}");
    for secret in [TOKEN, SIGNATURE, url] {
        assert!(!printed.contains(secret), "{secret} is in {printed}");
    }
    let job = rig.jobs_of(4).remove(0);
    rig.host_finishes(job.id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
    let said = log.text();
    assert!(!said.is_empty(), "nothing was logged");
    for secret in [TOKEN, SIGNATURE, url] {
        assert!(!said.contains(secret), "{secret} is in the log");
    }
}

/// Catches the operator's choice of arguments not being what it says, or a secret in a file
/// as well: with `InsecureArgv` and `insecure_argv_secrets` the url and the token are the
/// values of `--pack-url` and `--grant-token` after `--insecure-argv-secrets`, the job has no
/// file, and still no line logged while the job is submitted, read, ingested and settled holds
/// either value.
#[tokio::test(start_paused = true)]
async fn insecure_argv_puts_the_secrets_in_the_arguments_and_in_no_file_or_log() {
    let log = Log::default();
    let _logged = log.capture();
    let mut rig = Rig::start(&[HOST], 1, |cfg| {
        cfg.delivery = Delivery::InsecureArgv;
        cfg.insecure_argv_secrets = true;
    })
    .await;
    rig.pump().await;
    let caller = rig.submit(4);
    rig.pump().await;
    let sent = submits(&rig.fake.calls());
    assert_eq!(sent.len(), 1, "{sent:?}");
    let spec = &sent[0];
    assert!(spec.files.is_empty(), "{:?}", spec.files);
    assert!(spec.args.iter().any(|a| a == "--insecure-argv-secrets"));
    let at = |flag: &str| {
        let at = spec.args.iter().position(|a| a == flag)?;
        spec.args.get(at + 1).cloned()
    };
    assert_eq!(at("--grant-token").as_deref(), Some(TOKEN));
    let url = at("--pack-url").expect("a pack url argument");
    assert!(url.starts_with("https://store.test/cas/b/"), "{url}");
    assert!(url.ends_with(&format!("?sig={SIGNATURE}")), "{url}");
    assert!(at("--pack-url-file").is_none() && at("--grant-token-file").is_none());
    let job = rig.jobs_of(4).remove(0);
    rig.host_finishes(job.id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
    let said = log.text();
    assert!(!said.is_empty(), "nothing was logged");
    for secret in [TOKEN, SIGNATURE, url.as_str()] {
        assert!(!said.contains(secret), "{secret} is in the log");
    }
}
