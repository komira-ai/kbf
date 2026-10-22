//! The command line end to end: arguments, the token file, a fake server on a socket, the
//! day files it writes, the exit codes; and the guard that keeps the exporter read-only.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::Command;

use common::*;
use kbf_estimator_replay::cli::{Failure, run, write_days};
use kbf_estimator_replay::readonly::{Api, CallError, ReadOnly, Transport};
use kbf_estimator_replay::row::{Row, day_of};
use kbf_testkit::dir::TestDir;

/// A server that answers `routes` over HTTP/1.1 on a loopback port, and records the
/// request lines it saw. Any method but GET is answered 405 and counted as a failure of
/// the test through `seen`.
fn serve(routes: Routes) -> (SocketAddr, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let line = head.lines().next().unwrap_or_default().to_owned();
            log.lock().unwrap().push(line.clone());
            let mut words = line.split(' ');
            let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
            let (status, body) = if method == "GET" {
                routes.get(path).cloned().unwrap_or((404, "{}".into()))
            } else {
                (405, String::new())
            };
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (addr, seen)
}

fn args(addr: SocketAddr, token: Option<&Path>, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = [
        "kbf-estimator-replay",
        "export",
        "--api",
        &addr.to_string(),
        "--since",
        SINCE,
        "--until",
        UNTIL,
        "--out",
        &out.display().to_string(),
    ]
    .map(str::to_owned)
    .to_vec();
    if let Some(token) = token {
        a.extend(["--token-file".to_owned(), token.display().to_string()]);
    }
    a
}

/// Catches: an export that writes nothing, files not split by the UTC day the attempt was
/// queued on, a line a replay cannot read back, a summary that omits the counts, and a
/// credential that is not the one in the token file. Every request the server saw is a GET.
#[test]
fn the_export_verb_writes_one_file_per_day_and_asks_with_get_only() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "replay-cli");
    let token = tmp.join("token");
    std::fs::write(&token, "  sekrit\n").unwrap();
    let (addr, seen) = serve(farm());
    let out = tmp.join("out");
    let mut summary = Vec::new();
    run(args(addr, Some(&token), &out), &mut summary).unwrap();

    let summary = String::from_utf8(summary).unwrap();
    assert!(
        summary.starts_with("invocations 2 attempts 8 rows 4 "),
        "{summary}"
    );
    assert!(summary.contains("runs-2026-10-01.jsonl 2\n"), "{summary}");
    assert!(summary.contains("runs-2026-10-02.jsonl 2\n"), "{summary}");

    let day1 = std::fs::read_to_string(out.join("runs-2026-10-01.jsonl")).unwrap();
    let rows: Vec<Row> = day1.lines().map(|l| Row::parse(l).unwrap()).collect();
    assert_eq!(
        rows.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        ["r2", "r1"]
    );
    assert!(day1.ends_with('\n'));
    assert!(
        !day1.contains("null"),
        "an absent field is left out: {day1}"
    );
    let day2 = std::fs::read_to_string(out.join("runs-2026-10-02.jsonl")).unwrap();
    assert_eq!(day2.lines().count(), 2);

    let seen = seen.lock().unwrap();
    assert!(seen.len() > 10, "{seen:?}");
    assert!(seen.iter().all(|l| l.starts_with("GET ")), "{seen:?}");
}

/// Catches arguments that are accepted without what the verb needs, and each way the verb
/// can fail being reported as success.
#[test]
fn bad_arguments_and_failures_are_reported() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "replay-cli");
    let out = tmp.join("out");
    let mut sink = Vec::new();

    let err = run(["kbf-estimator-replay", "export"], &mut sink).unwrap_err();
    assert!(
        matches!(err, Failure::Usage(ref t) if t.contains("--api")),
        "{err:?}"
    );

    let (addr, _) = serve(farm());
    let mut bad = args(addr, None, &out);
    bad[5] = "soon".into();
    let err = run(bad, &mut sink).unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains("soon")),
        "{err:?}"
    );

    let err = run(
        args(addr, Some(&tmp.join("no-such-token")), &out),
        &mut sink,
    )
    .unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains("no-such-token")),
        "{err:?}"
    );

    // Nothing listens: the error names the address.
    let dead = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let err = run(args(dead, None, &out), &mut sink).unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains(&dead.to_string())),
        "{err:?}"
    );

    // An output path that is a file.
    let file = tmp.join("file");
    std::fs::write(&file, "x").unwrap();
    let err = run(args(addr, None, &file), &mut sink).unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains("file")),
        "{err:?}"
    );

    // A sink that cannot be written.
    struct Full;
    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let err = run(args(addr, None, &out), &mut Full).unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains("disk full")),
        "{err:?}"
    );
}

/// Catches the guard letting a write through: a POST, PUT or DELETE is refused before the
/// transport sees it, and so is one sent straight to the adapter over a real client (no
/// connection is made: the address is one nothing listens at).
#[test]
fn only_get_leaves_the_exporter() {
    let fake = Fake::new(farm());
    let api = ReadOnly::new(&fake);
    for method in ["POST", "PUT", "DELETE", "PATCH", "get", ""] {
        let err = api.request(method, "/v1/nodes:drain").unwrap_err();
        assert_eq!(
            err,
            CallError::NotGet {
                method: method.into(),
                path: "/v1/nodes:drain".into()
            }
        );
    }
    assert!(fake.seen.borrow().is_empty(), "nothing was sent");
    assert_eq!(api.request("GET", "/v1/nodes").unwrap().status, 200);
    assert_eq!(fake.seen.borrow().len(), 1);

    let dead = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let adapter = Api::new(dead, Some("t".into()));
    let err = adapter.call("POST", "/v1/nodes/n1:drain").unwrap_err();
    assert!(matches!(err, CallError::NotGet { .. }), "{err:?}");
    assert!(err.to_string().contains("reads only"), "{err}");
}

/// Catches rows grouped by the wrong day (local time, or the end time), a file per row, and
/// a line that does not read back.
#[test]
fn rows_are_grouped_by_the_utc_day_they_were_queued_on() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "replay-days");
    let mk = |id: &str, queued_us: u64| {
        Row {
        queued_us,
        ..Row::parse(
            r#"{"run_id":"x","instance":"i","platform":"linux","node":"n","shape":"-","cfg":"-","digest":"-","outcome":"ok","memory_kill":"none","queued_us":0}"#,
        )
        .map(|r| Row { run_id: id.into(), ..r })
        .unwrap()
    }
    };
    let rows = [
        mk("a", OCT_1 + DAY - 1),
        mk("b", OCT_1 + DAY),
        mk("c", OCT_1 + DAY + 1),
    ];
    let counts = write_days(&rows, &tmp.join("nested/out")).unwrap();
    assert_eq!(
        counts.into_iter().collect::<Vec<_>>(),
        [("2026-10-01".to_owned(), 1), ("2026-10-02".to_owned(), 2)]
    );
    let day2 = std::fs::read_to_string(tmp.join("nested/out/runs-2026-10-02.jsonl")).unwrap();
    let ids: Vec<String> = day2
        .lines()
        .map(|l| Row::parse(l).unwrap().run_id)
        .collect();
    assert_eq!(ids, ["b", "c"]);
    assert!(Row::parse("{").is_err());
    let future = rows[0].line().replace('}', ",\"future\":1}");
    assert!(Row::parse(&future).is_ok(), "an unknown key is read");
    assert_eq!(day_of(u64::MAX), "9999-12-31");
    assert_eq!(day_of(0), "1970-01-01");
}

/// Catches `main` not mapping success, a failed run and refused arguments to exit codes
/// 0, 1 and 2, and not saying why on stderr.
#[test]
fn the_binary_exits_0_1_and_2() {
    let exe = env!("CARGO_BIN_EXE_kbf-estimator-replay");
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "replay-bin");
    let (addr, _) = serve(farm());
    let ok = Command::new(exe)
        .args(&args(addr, None, &tmp.join("out"))[1..])
        .output()
        .unwrap();
    assert_eq!(ok.status.code(), Some(0), "{ok:?}");
    assert!(String::from_utf8_lossy(&ok.stdout).contains("rows 4"));

    let dead = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let failed = Command::new(exe)
        .args(&args(dead, None, &tmp.join("out2"))[1..])
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&failed.stderr).starts_with("kbf-estimator-replay: "));

    let usage = Command::new(exe).arg("export").output().unwrap();
    assert_eq!(usage.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&usage.stderr).contains("--api"));
}
