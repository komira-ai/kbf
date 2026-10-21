use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use super::*;
use kbf_host::testkit::{FakeExec, reply, scratch};

const ZEROS: &str = "# TYPE kbf_write_tier_unflushed_bytes gauge\nkbf_write_tier_unflushed_bytes 0\nkbf_write_tier_unflushed_blocks 0\nkbf_write_tier_flush_conflicts_total 0\nkbf_write_tier_unflushed_bytes_other 7\n";

/// A metadata directory with a log record, an empty hot store, and where the archive
/// goes, under a fresh scratch directory.
struct Node {
    root: PathBuf,
    meta: PathBuf,
    hot: PathBuf,
    out: PathBuf,
}

fn node(name: &str) -> Node {
    let root = scratch(&format!("meta_backup_{name}"));
    let hot = root.join("hot");
    let meta = hot.join("meta");
    fs::create_dir_all(&meta).expect("meta");
    fs::write(
        meta.join(RECORD),
        b"kbf raft log record 1\nlog_id 0000000000000007\nserver 1\nvoters 1\n",
    )
    .expect("record");
    fs::write(meta.join("hardstate"), b"hs").expect("hardstate");
    // A flushed copy: never a reason to refuse.
    fs::write(
        hot.join("0000000000000001-0000000000000001.seg"),
        b"flushed",
    )
    .expect("seg");
    let out = root.join("backups").join("meta.tar");
    fs::create_dir(out.parent().expect("parent")).expect("backups");
    Node {
        root,
        meta,
        hot,
        out,
    }
}

fn path(p: &std::path::Path) -> HostPath {
    HostPath::new(p.to_str().expect("UTF-8")).expect("absolute")
}

impl Node {
    fn backup(&self) -> Backup {
        Backup {
            meta_dir: path(&self.meta),
            hot_dir: Some(path(&self.hot)),
            out: path(&self.out),
            metrics: HttpUrl::new("http://127.0.0.1:8982/metrics").expect("url"),
            unit: "kbf-server".into(),
            ready: READY,
        }
    }
}

fn answer(body: &'static str) -> impl Fn(&HttpUrl) -> io::Result<(u16, Vec<u8>)> {
    move |_| Ok((200, body.as_bytes().to_vec()))
}

fn run(
    exec: &mut FakeExec,
    b: &Backup,
    body: &'static str,
    mode: Mode,
) -> (Result<(), MetaError>, String) {
    let mut out = Vec::new();
    let done = backup(exec, b, &answer(body), mode, &mut out);
    (done, String::from_utf8(out).expect("UTF-8"))
}

/// The commands the fake host was given, as `systemctl <verb> <unit>` or `wait`.
fn steps(exec: &FakeExec) -> Vec<String> {
    exec.log
        .iter()
        .map(|s| match s {
            Step::Run(c) => format!("systemctl {}", c.args.join(" ")),
            Step::WaitHttp { url, .. } => format!("wait {url}"),
            other => format!("{other}"),
        })
        .collect()
}

/// A test's check of an error.
type Want = fn(&MetaError) -> bool;
/// A `/metrics` answer a test plays.
type GetFn = dyn Fn(&HttpUrl) -> io::Result<(u16, Vec<u8>)>;

const STOP_START_WAIT: [&str; 3] = [
    "systemctl stop kbf-server",
    "systemctl start kbf-server",
    "wait http://127.0.0.1:8982/readyz",
];

/// Catches: a backup that leaves the server stopped, does its stop and start out of
/// order, writes no archive or one that does not restore, or does not say how long the
/// server was down (the RTO table's first row).
#[test]
fn an_apply_stops_archives_starts_and_prints_the_downtime() {
    let n = node("apply");
    let mut exec = FakeExec::new(0);
    let (done, out) = run(&mut exec, &n.backup(), ZEROS, Mode::Apply);
    done.expect("backup");
    assert_eq!(steps(&exec), STOP_START_WAIT);
    assert!(out.contains("+ check "), "{out}");
    assert!(
        out.contains(&format!("archive: {} (2 files, ", n.out.display())),
        "{out}"
    );
    assert!(
        out.contains("downtime: ")
            && out.contains(" s from the stop to http://127.0.0.1:8982/readyz answering 200"),
        "{out}"
    );
    let r = archive::extract(&n.out, &n.root.join("restored")).expect("the archive restores");
    assert_eq!(r.log_id, "0000000000000007");
}

/// Catches: a dry run that stops the server, writes an archive, or skips the reads an
/// apply would refuse on (it still refuses unflushed bytes: see the next test).
#[test]
fn a_dry_run_runs_nothing() {
    let n = node("dry_run");
    let mut exec = FakeExec::new(1000);
    let (done, out) = run(&mut exec, &n.backup(), ZEROS, Mode::DryRun);
    done.expect("dry run");
    assert!(exec.log.is_empty(), "{:?}", exec.log);
    assert!(!n.out.exists());
    assert_eq!(
        fs::read_dir(n.out.parent().expect("dir"))
            .expect("list")
            .count(),
        0
    );
    for want in [
        "DRY-RUN: run /usr/bin/systemctl stop kbf-server",
        "DRY-RUN: check ",
        "DRY-RUN: archive ",
        "DRY-RUN: run /usr/bin/systemctl start kbf-server",
        "DRY-RUN: wait up to 300s for http://127.0.0.1:8982/readyz",
        kbf_host::plan::DRY_RUN_NOTE,
    ] {
        assert!(out.contains(want), "{want:?} not in {out}");
    }
}

/// Catches: a backup taken while the write tier still holds acknowledged bytes (the
/// copy would index blocks only this node's disk has), or after a flush conflict. Each
/// reading alone refuses, before any step; the mutants "check bytes only", "check
/// blocks only" and "skip the conflict check" each pass one row.
#[test]
fn the_metrics_refuse_unflushed_bytes_blocks_and_conflicts() {
    let n = node("unflushed");
    let rows: [(&str, Want); 3] = [
        (
            "kbf_write_tier_unflushed_bytes 5\nkbf_write_tier_unflushed_blocks 0\nkbf_write_tier_flush_conflicts_total 0\n",
            |e| {
                matches!(
                    e,
                    MetaError::Unflushed {
                        bytes: 5,
                        blocks: 0
                    }
                )
            },
        ),
        (
            "kbf_write_tier_unflushed_bytes 0\nkbf_write_tier_unflushed_blocks 2\nkbf_write_tier_flush_conflicts_total 0\n",
            |e| {
                matches!(
                    e,
                    MetaError::Unflushed {
                        bytes: 0,
                        blocks: 2
                    }
                )
            },
        ),
        (
            "kbf_write_tier_unflushed_bytes 0\nkbf_write_tier_unflushed_blocks 0\nkbf_write_tier_flush_conflicts_total 1\n",
            |e| matches!(e, MetaError::Conflicts(1)),
        ),
    ];
    for (body, want) in rows {
        for mode in [Mode::Apply, Mode::DryRun] {
            let mut exec = FakeExec::new(0);
            let (done, _) = run(&mut exec, &n.backup(), body, mode);
            let e = done.expect_err(body);
            assert!(want(&e), "{body}: {e}");
            assert_eq!(e.exit_code(), 2);
            assert!(exec.log.is_empty(), "{body}: a step ran");
        }
    }
}

/// Catches: a backup that goes ahead when it cannot tell whether the tier is flushed:
/// no answer, a non-200, a value that does not parse, a family given twice or missing
/// beside the others; and one that reads a longer name as the family.
#[test]
fn unreadable_metrics_refuse() {
    let n = node("unreadable");
    let b = n.backup();
    let gets: Vec<Box<GetFn>> = vec![
        Box::new(|_| Err(io::Error::from(io::ErrorKind::ConnectionRefused))),
        Box::new(|_| Ok((503, ZEROS.as_bytes().to_vec()))),
        Box::new(|_| Ok((200, vec![0xff, 0xfe]))),
        Box::new(|_| Ok((200, ZEROS.replace("blocks 0", "blocks 0.5").into_bytes()))),
        Box::new(|_| {
            Ok((
                200,
                format!("{ZEROS}kbf_write_tier_unflushed_bytes 0\n").into_bytes(),
            ))
        }),
        Box::new(|_| {
            Ok((
                200,
                b"kbf_write_tier_unflushed_bytes 0\nkbf_write_tier_flush_conflicts_total 0\n"
                    .to_vec(),
            ))
        }),
        Box::new(|_| {
            Ok((
                200,
                b"kbf_write_tier_unflushed_blocks 0\nkbf_write_tier_flush_conflicts_total 0\n"
                    .to_vec(),
            ))
        }),
        Box::new(|_| {
            Ok((
                200,
                b"kbf_write_tier_unflushed_bytes 0\nkbf_write_tier_unflushed_blocks 0\n".to_vec(),
            ))
        }),
    ];
    for (i, get) in gets.iter().enumerate() {
        let mut exec = FakeExec::new(0);
        let e = backup(&mut exec, &b, get.as_ref(), Mode::Apply, &mut Vec::new())
            .expect_err(&format!("row {i} went ahead"));
        assert!(
            matches!(e, MetaError::Metrics { .. } | MetaError::Family { .. }),
            "row {i}: {e}"
        );
        assert_eq!(e.exit_code(), 2);
        assert!(exec.log.is_empty(), "row {i}: a step ran");
    }
}

/// Catches: a write tier the operator forgot to name (its blocks would go unchecked
/// after the stop), and a hot store named for a server that has no tier.
#[test]
fn the_hot_dir_must_match_the_server() {
    let n = node("hot_match");
    let mut b = n.backup();
    b.hot_dir = None;
    let mut exec = FakeExec::new(0);
    let (done, _) = run(&mut exec, &b, ZEROS, Mode::Apply);
    assert!(matches!(done, Err(MetaError::NoHotDir { .. })), "{done:?}");

    let (done, _) = run(
        &mut exec,
        &n.backup(),
        "kbf_reapi_requests_total 3\n",
        Mode::Apply,
    );
    assert!(matches!(done, Err(MetaError::NoTier { .. })), "{done:?}");

    // No tier and no hot dir: a backup.
    let (done, out) = run(&mut exec, &b, "kbf_reapi_requests_total 3\n", Mode::Apply);
    done.expect("a server with no write tier");
    assert!(!out.contains("check "), "{out}");
    assert_eq!(steps(&exec), STOP_START_WAIT);
}

/// Catches: an archive taken over blocks that landed between the metrics read and the
/// stop, and a refusal after the stop that leaves the server down (the mutant "return
/// the error before the start" leaves only the stop in the log).
#[test]
fn a_block_after_the_stop_refuses_and_restarts() {
    let n = node("block");
    let hot = n.hot.clone();
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(move |c| {
        if c.args.first().map(String::as_str) == Some("stop") {
            fs::write(hot.join("0000000000000003-0000000000000009.blk"), b"late").expect("a block");
        }
        Ok(reply(0, "", ""))
    });
    let (done, _) = run(&mut exec, &n.backup(), ZEROS, Mode::Apply);
    let e = done.expect_err("archived over an unflushed block");
    assert!(
        matches!(&e, MetaError::Restarted(inner) if matches!(**inner, MetaError::Blocks { .. })),
        "{e}"
    );
    assert!(
        e.to_string()
            .contains("0000000000000003-0000000000000009.blk"),
        "{e}"
    );
    assert_eq!(e.exit_code(), 1);
    assert_eq!(steps(&exec), STOP_START_WAIT);
    assert!(!n.out.exists());
}

/// Catches: a failure while archiving (a directory appeared in the metadata directory)
/// that leaves the server stopped or a partial archive behind; a failed stop (which may
/// still have stopped it) that skips the start, or archives a live log.
#[test]
fn a_failure_after_the_stop_restarts_the_unit() {
    let n = node("fail_after_stop");
    let meta = n.meta.clone();
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(move |c| {
        if c.args.first().map(String::as_str) == Some("stop") {
            fs::create_dir(meta.join("surprise")).expect("a directory");
        }
        Ok(reply(0, "", ""))
    });
    let (done, _) = run(&mut exec, &n.backup(), ZEROS, Mode::Apply);
    let e = done.expect_err("archived a directory");
    assert!(
        matches!(&e, MetaError::Restarted(inner) if matches!(**inner, MetaError::NotFile { .. })),
        "{e}"
    );
    assert_eq!(steps(&exec), STOP_START_WAIT);
    assert_eq!(
        fs::read_dir(n.out.parent().expect("dir"))
            .expect("list")
            .count(),
        0
    );

    let n = node("failed_stop");
    let mut exec = FakeExec::new(0);
    exec.fail_at = Some(0);
    let (done, _) = run(&mut exec, &n.backup(), ZEROS, Mode::Apply);
    let e = done.expect_err("a failed stop");
    assert!(matches!(e, MetaError::Restarted(_)), "{e}");
    assert_eq!(steps(&exec), STOP_START_WAIT);
    assert!(!n.out.exists(), "archived after a failed stop");
}

/// Catches: a failed start, or a server that never answers `/readyz`, reported as
/// success; and a failed restart hiding the failure before it.
#[test]
fn a_failed_restart_is_a_failure() {
    let n = node("failed_start");
    let mut exec = FakeExec::new(0);
    exec.fail_at = Some(2);
    let (done, _) = run(&mut exec, &n.backup(), ZEROS, Mode::Apply);
    let e = done.expect_err("never ready");
    assert!(
        matches!(
            e,
            MetaError::Host(kbf_host::HostError::Step { index: 2, .. })
        ),
        "{e}"
    );
    assert_eq!(e.exit_code(), 1);
    assert!(n.out.exists(), "the archive was written before the start");

    let n = node("both_fail");
    let hot = n.hot.clone();
    let mut exec = FakeExec::new(0);
    exec.fail_at = Some(1);
    exec.answer = Box::new(move |c| {
        if c.args.first().map(String::as_str) == Some("stop") {
            fs::write(hot.join("0000000000000003-0000000000000009.blk"), b"late").expect("a block");
        }
        Ok(reply(0, "", ""))
    });
    let (done, _) = run(&mut exec, &n.backup(), ZEROS, Mode::Apply);
    let e = done.expect_err("both failed");
    let text = e.to_string();
    assert!(matches!(e, MetaError::RestartFailed { .. }), "{text}");
    assert!(
        text.contains(".blk") && text.contains("the restart failed"),
        "{text}"
    );
}

/// Catches: an apply as a user other than root (the stop needs root, and the archive
/// would miss files the user cannot read), and the checks of the paths before the stop.
#[test]
fn refusals_before_the_stop() {
    let n = node("refusals");
    let mut exec = FakeExec::new(1000);
    let (done, _) = run(&mut exec, &n.backup(), ZEROS, Mode::Apply);
    assert!(
        matches!(
            done,
            Err(MetaError::Host(kbf_host::HostError::NotRoot { euid: 1000 }))
        ),
        "{done:?}"
    );

    let mut exec = FakeExec::new(0);
    exec.untrusted = true;
    let (done, _) = run(&mut exec, &n.backup(), ZEROS, Mode::Apply);
    assert!(
        matches!(
            done,
            Err(MetaError::Host(kbf_host::HostError::Untrusted(_)))
        ),
        "{done:?}"
    );

    let mut b = n.backup();
    b.out = path(&n.meta.join("in-meta.tar"));
    let (done, _) = run(&mut FakeExec::new(0), &b, ZEROS, Mode::Apply);
    assert!(matches!(done, Err(MetaError::Inside { .. })), "{done:?}");

    fs::write(&n.out, b"earlier").expect("earlier");
    let (done, _) = run(&mut FakeExec::new(0), &n.backup(), ZEROS, Mode::DryRun);
    assert!(matches!(done, Err(MetaError::Exists(_))), "{done:?}");
    fs::remove_file(&n.out).expect("remove");

    let mut b = n.backup();
    b.out = path(&n.root.join("nowhere").join("meta.tar"));
    let (done, _) = run(&mut FakeExec::new(0), &b, ZEROS, Mode::DryRun);
    assert!(matches!(done, Err(MetaError::NotDir(_))), "{done:?}");

    let mut b = n.backup();
    b.unit = "kbf-server; reboot".into();
    let (done, _) = run(&mut FakeExec::new(0), &b, ZEROS, Mode::DryRun);
    assert!(matches!(done, Err(MetaError::Unit(_))), "{done:?}");

    b.unit = String::new();
    let (done, _) = run(&mut FakeExec::new(0), &b, ZEROS, Mode::DryRun);
    assert!(matches!(done, Err(MetaError::Unit(_))), "{done:?}");

    let mut b = n.backup();
    b.meta_dir = path(&n.root.join("no-meta"));
    let (done, _) = run(&mut FakeExec::new(0), &b, ZEROS, Mode::DryRun);
    assert!(matches!(done, Err(MetaError::NotDir(_))), "{done:?}");

    let mut b = n.backup();
    b.hot_dir = Some(path(&n.root.join("no-hot")));
    let (done, _) = run(&mut FakeExec::new(0), &b, ZEROS, Mode::DryRun);
    assert!(matches!(done, Err(MetaError::NotDir(_))), "{done:?}");

    fs::remove_file(n.meta.join(RECORD)).expect("drop the record");
    let (done, _) = run(&mut FakeExec::new(0), &n.backup(), ZEROS, Mode::DryRun);
    let e = done.expect_err("no record");
    assert!(matches!(e, MetaError::NoRecord { .. }), "{e}");
    assert_eq!(e.exit_code(), 2);
}

/// Catches: a `/metrics` client that sends another path or host, or misreads a body;
/// and the command line not wired to the verbs (flags, defaults, exit codes, the
/// `FAIL`/`REFUSED:` prefixes).
#[test]
fn the_command_line_reads_metrics_over_http_and_exits_as_documented() {
    let server = crate::testkit::http_server(|_| crate::testkit::http("200 OK", ZEROS.as_bytes()));
    let url =
        HttpUrl::new(&format!("http://127.0.0.1:{}/metrics", server.addr.port())).expect("url");
    let (status, body) = metrics::http_get(&url).expect("get");
    assert_eq!((status, body.as_slice()), (200, ZEROS.as_bytes()));
    let seen = server.requests();
    assert_eq!(seen[0].path, "/metrics");
    assert_eq!(seen[0].header("host"), Some(url.authority()));

    let n = node("cli");
    let argv = |args: &[&str]| -> Vec<std::ffi::OsString> {
        ["kbf-deploy", "linux"]
            .iter()
            .chain(args)
            .map(Into::into)
            .collect()
    };
    let cli = |exec: &mut FakeExec, args: &[&str]| {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = crate::cli::run(argv(args), exec, &|_| None, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("out"),
            String::from_utf8(err).expect("err"),
        )
    };
    let metrics_flag = url.to_string();
    let (meta, hot, out) = (
        n.meta.to_str().expect("meta"),
        n.hot.to_str().expect("hot"),
        n.out.to_str().expect("out"),
    );
    let backup = [
        "meta-backup",
        "--meta-dir",
        meta,
        "--hot-dir",
        hot,
        "--out",
        out,
        "--metrics",
        &metrics_flag,
    ];
    let mut exec = FakeExec::new(0);
    let (code, stdout, _) = cli(&mut exec, &backup);
    assert_eq!(code, ExitCode::SUCCESS, "{stdout}");
    assert!(
        stdout.contains("DRY-RUN: run /usr/bin/systemctl stop kbf-server"),
        "{stdout}"
    );
    let readyz = format!("for http://127.0.0.1:{}/readyz", server.addr.port());
    assert!(stdout.contains(&readyz), "{stdout}");

    let applied: Vec<&str> = backup
        .iter()
        .copied()
        .chain(["--apply", "--ready-secs", "9"])
        .collect();
    let (code, _, err) = cli(&mut exec, &applied);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(
        matches!(exec.log.last(), Some(Step::WaitHttp { deadline, .. }) if deadline.as_secs() == 9)
    );

    let into = n.root.join("restored");
    let verify = [
        "meta-verify",
        "--archive",
        out,
        "--into",
        into.to_str().expect("into"),
    ];
    let (code, stdout, err) = cli(&mut exec, &verify);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(
        stdout.contains("verified: 2 files") && stdout.contains("log id: 0000000000000007"),
        "{stdout}"
    );

    let (code, _, err) = cli(&mut exec, &verify);
    assert_eq!(code, ExitCode::from(2));
    assert!(err.starts_with("REFUSED: "), "{err}");

    let mut bytes = fs::read(out).expect("archive");
    let at = bytes
        .windows(2)
        .position(|w| w == b"hs")
        .expect("hardstate bytes");
    bytes[at] ^= 1;
    fs::write(out, bytes).expect("flip");
    let other = n.root.join("other");
    let verify = [
        "meta-verify",
        "--archive",
        out,
        "--into",
        other.to_str().expect("other"),
    ];
    let (code, _, err) = cli(&mut exec, &verify);
    assert_eq!(code, ExitCode::FAILURE);
    assert!(err.starts_with("FAIL "), "{err}");
}
