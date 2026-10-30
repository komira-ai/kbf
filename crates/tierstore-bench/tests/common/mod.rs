//! Shared by the cold-put and cleanup tests: a loopback S3 service, a configuration that reaches
//! it, a client that writes without leaving a record, and scripted load sources.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use clap::Parser as _;
use kbf_objstore::s3::fake::FakeS3;
use kbf_testkit::dir::TestDir;
use tierstore_bench::cells::{CellContext, CellPlan};
use tierstore_bench::cli::{Cli, Command};
use tierstore_bench::config::RunConfig;
use tierstore_bench::error::BenchError;
use tierstore_bench::gate::LoadSample;
use tierstore_bench::loadmeter::LoadSource;
use tierstore_bench::requestlog::{RequestLog, RequestRecord};
use tierstore_bench::result::Provenance;
use tierstore_bench::role::Role;
use tierstore_bench::s3::{BenchS3, Timeouts};

/// The bucket the loopback service serves.
pub const BUCKET: &str = "kbf-fake";

/// Everything a test of the cold-put cell needs.
pub struct Rig {
    pub dir: TestDir,
    pub fake: FakeS3,
    pub ctx: CellContext,
    pub plan: CellPlan,
    pub out: PathBuf,
}

/// A loopback service, a run file naming it as the cold store, and the cell's context and plan
/// as the `run` verb builds them from `extra` flags (after `--run-id r1 --cold-rps 1000000`, so
/// that the request-rate cap does not slow a test).
pub async fn rig(name: &str, extra: &[&str]) -> Rig {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let fake = FakeS3::start().await;
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let run_file = dir.path().join("run.json");
    std::fs::write(
        &run_file,
        format!(
            r#"{{"stores": {{"cold": {{"endpoint": "{}", "bucket": "{BUCKET}",
                 "access_key_id": "fake-key", "secret_access_key": "fake-secret"}}}}}}"#,
            fake.endpoint()
        ),
    )
    .unwrap();
    let out_arg = out.to_str().unwrap();
    let run_arg = run_file.to_str().unwrap();
    let mut argv = vec![
        "tierstore-bench",
        "run",
        "--result-dir",
        out_arg,
        "--run-file",
        run_arg,
        "--cell",
        "cold-put",
        "--run-id",
        "r1",
        "--cold-rps",
        "1000000",
    ];
    argv.extend_from_slice(extra);
    let Command::Run(args) = Cli::try_parse_from(argv).unwrap().command else {
        panic!("not the run verb");
    };
    let config = RunConfig::load(&args.config).unwrap();
    let log = Arc::new(RequestLog::create(&out.join("requests.jsonl")).unwrap());
    let plan = args.plan();
    let ctx = CellContext {
        config,
        log,
        provenance: Provenance::unrecorded(),
    };
    Rig {
        dir,
        fake,
        ctx,
        plan,
        out,
    }
}

impl Rig {
    /// Writes `key` to the service the way another client would: no record in the run's log.
    pub async fn put_elsewhere(&self, key: &str) {
        let s3 = BenchS3::new(
            Role::Cold,
            self.ctx.config.store(Role::Cold).unwrap(),
            Timeouts::default(),
            Arc::new(RequestLog::to_writer(std::io::sink())),
        )
        .unwrap();
        let out = s3.put(key, Bytes::from_static(b"not ours")).await.unwrap();
        assert!(out.is_ok(), "{key}");
    }

    /// The rows of `samples.csv` without the header, split at commas.
    pub fn csv_rows(&self) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(self.out.join("samples.csv")).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some(tierstore_bench::result::Sample::CSV_HEADER)
        );
        lines
            .map(|l| l.split(',').map(str::to_owned).collect())
            .collect()
    }

    /// The records of the request log.
    pub fn log_records(&self) -> Vec<RequestRecord> {
        std::fs::read_to_string(self.out.join("requests.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

/// A machine that is never busy.
pub fn calm() -> LoadSample {
    LoadSample {
        loadavg_1m: 0.5,
        psi_cpu_avg10: 0.0,
        psi_io_avg10: 0.0,
        disk_util_pct: 1.0,
    }
}

/// Answers the samples it is given, in order, then calm ones.
pub struct Script(Mutex<VecDeque<LoadSample>>);

impl Script {
    pub fn new(samples: impl IntoIterator<Item = LoadSample>) -> Self {
        Self(Mutex::new(samples.into_iter().collect()))
    }

    pub fn calm() -> Self {
        Self::new([])
    }
}

impl LoadSource for Script {
    fn sample(&self) -> Result<LoadSample, BenchError> {
        Ok(self.0.lock().unwrap().pop_front().unwrap_or_else(calm))
    }
}
