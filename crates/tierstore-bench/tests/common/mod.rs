//! Shared by the cold-put and cleanup tests: a loopback S3 service, a configuration that reaches
//! it, a client that writes without leaving a record, and scripted load sources.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Calm for the first `ok` samples, then unreadable.
pub struct Unreadable {
    ok: usize,
    seen: AtomicUsize,
}

impl Unreadable {
    pub fn after(ok: usize) -> Self {
        Self {
            ok,
            seen: AtomicUsize::new(0),
        }
    }
}

impl LoadSource for Unreadable {
    fn sample(&self) -> Result<LoadSample, BenchError> {
        if self.seen.fetch_add(1, Ordering::SeqCst) < self.ok {
            Ok(calm())
        } else {
            Err(BenchError::Request("the load cannot be read"))
        }
    }
}

/// Calm, and creates `file` on its `nth` sample (the first is 1).
pub struct TouchOn {
    file: PathBuf,
    nth: usize,
    seen: AtomicUsize,
}

impl TouchOn {
    pub fn new(file: PathBuf, nth: usize) -> Self {
        Self {
            file,
            nth,
            seen: AtomicUsize::new(0),
        }
    }
}

impl LoadSource for TouchOn {
    fn sample(&self) -> Result<LoadSample, BenchError> {
        if self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.nth {
            std::fs::write(&self.file, "").unwrap();
        }
        Ok(calm())
    }
}

/// A made-up `/proc` and `/sys/block` under `root`, calm except for the load average given:
/// `(proc, sys_block)`. One whole disk, `sda`, and the loop, ram and zram devices the gate leaves
/// out, and `ghost`, which `diskstats` lists and `/sys/block` does not.
pub fn proc_tree(root: &std::path::Path, loadavg: &str) -> (PathBuf, PathBuf) {
    let proc = root.join("proc");
    let sys = root.join("sys_block");
    std::fs::create_dir_all(proc.join("pressure")).unwrap();
    for disk in ["sda", "loop0", "ram0", "zram0"] {
        std::fs::create_dir_all(sys.join(disk)).unwrap();
    }
    std::fs::write(
        proc.join("loadavg"),
        format!("{loadavg} 0.40 0.30 1/100 999\n"),
    )
    .unwrap();
    let psi = |some: &str| {
        format!(
            "some avg10={some} avg60=0.00 avg300=0.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=1\n"
        )
    };
    std::fs::write(proc.join("pressure/cpu"), psi("0.50")).unwrap();
    std::fs::write(proc.join("pressure/io"), psi("1.50")).unwrap();
    let mut stats = String::new();
    for (i, name) in ["sda", "loop0", "ram0", "zram0", "ghost"]
        .iter()
        .enumerate()
    {
        stats.push_str(&format!(
            "   8 {i} {name} 1 0 2 3 4 0 5 6 0 {} 7\n",
            1000 * (i + 1)
        ));
    }
    std::fs::write(proc.join("diskstats"), stats).unwrap();
    (proc, sys)
}

/// A writer whose writes all fail, for a request log that cannot be kept.
pub struct Refusing;

impl std::io::Write for Refusing {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("disk full"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Rig {
    /// Writes `count` objects named `<prefix><00000..>` the way another client would.
    pub async fn put_many_elsewhere(&self, prefix: &str, count: usize) {
        let s3 = BenchS3::new(
            Role::Cold,
            self.ctx.config.store(Role::Cold).unwrap(),
            Timeouts::default(),
            Arc::new(RequestLog::to_writer(std::io::sink())),
        )
        .unwrap();
        for i in 0..count {
            let out = s3
                .put(&format!("{prefix}{i:05}"), Bytes::from_static(b"x"))
                .await
                .unwrap();
            assert!(out.is_ok());
        }
    }

    /// Points the cold store at a port nothing listens on.
    pub fn store_goes_away(&mut self) {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        self.ctx
            .config
            .stores
            .get_mut(&Role::Cold)
            .unwrap()
            .endpoint = endpoint;
    }
}
