//! The updater's end of the helper's socket: the requests it writes, the answers it reads, and
//! the contract with the real answer types of `kbf-host-install`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use kbf_host_install::contract::local::{Answer, Code, LastView, PendingView, QueryView};
use kbf_testkit::dir::TestDir;

use super::local::{
    Ended, HostState, Limits, Local, LocalError, LocalInstaller, SocketLocal, Trial,
};
use super::testing::{COMMIT_A, COMMIT_B, Events, FakeLocal, VA, VB, host, version};
use crate::inventory::Role;
use crate::ports::{Installed, Installer};
use crate::wire::Outcome;

/// A helper that answers each connection from `answer`, which sees the request line and gives
/// the bytes to write back (none: close without an answer). The lines it was asked, at the end.
fn serve(
    dir: &TestDir,
    connections: usize,
    answer: impl Fn(&str) -> Option<String> + Send + 'static,
) -> (PathBuf, JoinHandle<Vec<String>>) {
    let socket = dir.join("h.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let handle = std::thread::spawn(move || {
        let mut asked = Vec::new();
        for _ in 0..connections {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let line = line.trim_end_matches('\n').to_owned();
            if let Some(text) = answer(&line) {
                (&stream).write_all(text.as_bytes()).unwrap();
            }
            asked.push(line);
        }
        asked
    });
    (socket, handle)
}

fn limits() -> Limits {
    Limits {
        install: Duration::from_secs(5),
        switch: None,
        revert: Duration::from_secs(5),
        other: Duration::from_secs(5),
    }
}

fn dir(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

fn line(answer: &Answer) -> Option<String> {
    Some(format!("{}\n", answer.json_line()))
}

/// The requests the updater writes are the five the helper accepts, and nothing else: each is
/// accepted by the helper's own parser (its verbs: stage, switch, confirm, revert, query), and
/// carries only a version and a channel name.
#[test]
fn the_requests_are_the_helpers_five_and_carry_a_version_and_a_channel_only() {
    let d = dir("lc-requests");
    let (socket, asked) = serve(&d, 5, |request| {
        let verb = kbf_host_install::contract::local::request_verb(request);
        match verb {
            Some("query") => line(&Answer::Queried(QueryView {
                commit: None,
                version: None,
                node_id: None,
                pending: None,
                last: None,
                busy: false,
                releases: Vec::new(),
                releases_omitted: 0,
                releases_unavailable: false,
            })),
            Some("stage") => line(&Answer::Staged {
                version: VB.into(),
                commit: COMMIT_B.into(),
                current: false,
            }),
            Some("switch") => line(&Answer::Switched {
                version: VB.into(),
                commit: COMMIT_B.into(),
                deadline_unix: 77,
            }),
            Some("confirm") => line(&Answer::Confirmed { version: VB.into() }),
            Some("revert") => line(&Answer::Reverted { version: None }),
            other => panic!("the helper would not accept {request:?}: {other:?}"),
        }
    });
    let helper = SocketLocal::new(&socket, limits());
    helper.query().unwrap();
    helper.install(&version(VB), "stable").unwrap();
    assert_eq!(helper.switch(&version(VB)), Ok(77));
    helper.confirm(&version(VB)).unwrap();
    helper.revert().unwrap();
    assert_eq!(
        asked.join().unwrap(),
        [
            "query".to_owned(),
            format!("install kbf={VB} from stable"),
            format!("switch kbf={VB}"),
            format!("confirm kbf={VB}"),
            "revert".to_owned(),
        ]
    );
}

/// The channel the agent is configured with and the helper's channel grammar agree: whatever
/// the flags take, the helper takes, and whatever it would refuse the flags refuse.
#[test]
fn the_flags_take_the_channels_the_helper_takes() {
    let take = |name: &str| {
        let args: Vec<String> = [
            "--coordinator",
            "c.example:443",
            "--client-cert",
            "/c",
            "--client-key",
            "/k",
            "--ca",
            "/ca",
            "--channel",
            name,
            "--operator-api",
            "http://127.0.0.1:1",
            "--token-file",
            "/t",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        super::flags::parse_flags(&args).is_ok()
    };
    for name in [
        "stable",
        "a",
        "0",
        "candidate-2",
        &"a".repeat(32),
        "",
        "A",
        "-a",
        "a_b",
        "a/b",
        "a b",
        &"a".repeat(33),
        "é",
    ] {
        let helper = kbf_host_install::contract::local::request_verb(&format!(
            "install kbf=1.0-1 from {name}"
        ))
        .is_some();
        assert_eq!(take(name), helper, "{name:?}");
    }
}

/// The answers the helper really writes are read: every field the updater uses, and every refusal
/// and failure code, which come back as the closed code they carry.
#[test]
fn the_helpers_real_answers_are_read() {
    let d = dir("lc-answers");
    let trial = PendingView {
        version: Some(VB.into()),
        commit: COMMIT_B.into(),
        deadline_unix: Some(600),
    };
    let view = QueryView {
        commit: Some(COMMIT_A.into()),
        version: Some(VA.into()),
        node_id: Some("n1".into()),
        pending: Some(trial),
        last: Some(LastView {
            target: VB.into(),
            outcome: "failed",
            reason: Some("timeout"),
        }),
        busy: true,
        releases: Vec::new(),
        releases_omitted: 0,
        releases_unavailable: false,
    };
    let (socket, _) = serve(&d, 1, move |_| line(&Answer::Queried(view.clone())));
    let state = SocketLocal::new(&socket, limits()).query().unwrap();
    assert_eq!(
        state,
        HostState {
            commit: Some(COMMIT_A.into()),
            version: Some(version(VA)),
            node_id: Some("n1".into()),
            pending: Some(Trial {
                version: Some(version(VB)),
                commit: COMMIT_B.into(),
                deadline_unix: Some(600)
            }),
            last: Some(Ended {
                target: VB.into(),
                outcome: Outcome::Failed,
                reason: Some("timeout".into())
            }),
            busy: true,
        }
    );

    // The other way a last update can have ended.
    let d2 = dir("lc-succeeded");
    let (socket, _) = serve(&d2, 1, |_| {
        line(&Answer::Queried(QueryView {
            commit: None,
            version: None,
            node_id: None,
            pending: None,
            last: Some(LastView {
                target: VB.into(),
                outcome: "succeeded",
                reason: None,
            }),
            busy: false,
            releases: Vec::new(),
            releases_omitted: 0,
            releases_unavailable: false,
        }))
    });
    let state = SocketLocal::new(&socket, limits()).query().unwrap();
    assert_eq!(state.last.unwrap().outcome, Outcome::Succeeded);

    for code in Code::ALL {
        let name = code.code();
        let refused_dir = dir("lc-refused");
        let (socket, _) = serve(&refused_dir, 1, move |_| {
            line(&Answer::Refused(code, "detail".into()))
        });
        assert_eq!(
            SocketLocal::new(&socket, limits()).revert(),
            Err(LocalError::Refused(name.to_owned()))
        );
        let failed_dir = dir("lc-failed");
        let (socket, _) = serve(&failed_dir, 1, move |_| {
            line(&Answer::Failed(code, "detail".into()))
        });
        assert_eq!(
            SocketLocal::new(&socket, limits()).revert(),
            Err(LocalError::Failed(name.to_owned()))
        );
    }
}

/// An answer that is not an answer to the request is a protocol error, not a guess: another
/// result than the request's, text, a staged release that is not the one asked for, a switch to
/// another version, a confirm of another, a query without `busy`, a version that is not one, an
/// update that neither succeeded nor failed.
#[test]
fn an_answer_that_does_not_fit_the_request_is_a_protocol_error() {
    let v = version(VB);
    type Call = Box<dyn Fn(&SocketLocal) -> Result<(), LocalError>>;
    let cases: Vec<(&str, Call, &str)> = vec![
        ("not json", Box::new(|h| h.revert()), "not json\n"),
        (
            "another result",
            Box::new(|h| h.revert()),
            r#"{"result":"confirmed","version":"1"}"#,
        ),
        (
            "a stage of another version",
            Box::new({
                let v = v.clone();
                move |h| h.install(&v, "stable").map(drop)
            }),
            r#"{"result":"staged","version":"0.1.9+gdddddddddddd-5","commit":"bbbb","current":false}"#,
        ),
        (
            "a stage with no commit",
            Box::new({
                let v = v.clone();
                move |h| h.install(&v, "stable").map(drop)
            }),
            r#"{"result":"staged","version":"0.1.7+gbbbbbbbbbbbb-3","current":false}"#,
        ),
        (
            "a switch of another version",
            Box::new({
                let v = v.clone();
                move |h| h.switch(&v).map(drop)
            }),
            r#"{"result":"switched","version":"0.1.9+gdddddddddddd-5","commit":"b","deadline_unix":1}"#,
        ),
        (
            "a switch with no deadline",
            Box::new({
                let v = v.clone();
                move |h| h.switch(&v).map(drop)
            }),
            r#"{"result":"switched","version":"0.1.7+gbbbbbbbbbbbb-3","commit":"b"}"#,
        ),
        (
            "a confirm of another version",
            Box::new({
                let v = v.clone();
                move |h| h.confirm(&v)
            }),
            r#"{"result":"confirmed","version":"0.1.9+gdddddddddddd-5"}"#,
        ),
        (
            "a query with no busy",
            Box::new(|h| h.query().map(drop)),
            r#"{"result":"query","commit":null,"version":null,"node_id":null,"pending":null,"last":null}"#,
        ),
        (
            "a version that is not one",
            Box::new(|h| h.query().map(drop)),
            r#"{"result":"query","commit":null,"version":"/etc/passwd","node_id":null,"pending":null,"last":null,"busy":false}"#,
        ),
        (
            "a trial with a version that is not one",
            Box::new(|h| h.query().map(drop)),
            r#"{"result":"query","commit":null,"version":null,"node_id":null,"pending":{"version":"x y","commit":"b","deadline_unix":1},"last":null,"busy":false}"#,
        ),
        (
            "an update that neither succeeded nor failed",
            Box::new(|h| h.query().map(drop)),
            r#"{"result":"query","commit":null,"version":null,"node_id":null,"pending":null,"last":{"target":"1","outcome":"perhaps","reason":null},"busy":false}"#,
        ),
    ];
    for (name, call, text) in cases {
        let d = dir("lc-protocol");
        let text = format!("{}\n", text.trim_end());
        let (socket, _) = serve(&d, 1, move |_| Some(text.clone()));
        let err = call(&SocketLocal::new(&socket, limits())).unwrap_err();
        assert!(matches!(err, LocalError::Protocol(_)), "{name}: {err:?}");
    }
}

/// A helper that is not there, closes without an answer, answers with no end of line, or does not
/// answer in time is unreachable: nothing may have run, and the error says so.
#[test]
fn a_helper_that_is_gone_or_silent_is_unreachable() {
    let d = dir("lc-gone");
    let helper = SocketLocal::new(&d.join("no.sock"), limits());
    assert!(matches!(helper.query(), Err(LocalError::Unreachable(_))));

    let (socket, _) = serve(&d, 1, |_| None);
    assert!(matches!(
        SocketLocal::new(&socket, limits()).revert(),
        Err(LocalError::Unreachable(_))
    ));

    let d = dir("lc-noeol");
    let (socket, _) = serve(&d, 1, |_| {
        Some("{\"result\":\"reverted\",\"version\":null}".to_owned())
    });
    assert!(matches!(
        SocketLocal::new(&socket, limits()).revert(),
        Err(LocalError::Unreachable(_))
    ));

    let d = dir("lc-slow");
    let listener = UnixListener::bind(d.join("slow.sock")).unwrap();
    let hold = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // Say nothing, and hold the connection until the client hangs up on it (the read
        // timeout here is only a hang guard).
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut sink = [0_u8; 64];
        // Whether the client hung up (and the wait did not run out its guard).
        loop {
            match std::io::Read::read(&mut stream, &mut sink) {
                Ok(0) => break true,
                Ok(_) => {}
                Err(e) => {
                    break !matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    );
                }
            }
        }
    });
    let quick = Limits {
        other: Duration::from_millis(100),
        ..limits()
    };
    assert!(matches!(
        SocketLocal::new(&d.join("slow.sock"), quick).query(),
        Err(LocalError::Unreachable(_))
    ));
    assert!(
        hold.join().unwrap(),
        "the client gave up on the silent helper before the helper's guard ran out"
    );

    for error in [
        LocalError::Unreachable("x".into()),
        LocalError::Refused("c".into()),
        LocalError::Failed("c".into()),
        LocalError::Protocol("x".into()),
    ] {
        assert!(!error.to_string().is_empty());
    }
    assert_eq!(Limits::default().install, Duration::from_secs(1200));
}

/// A switch is not limited by default (the helper waits for the frontend's drain as long as it
/// takes, and answers when the switch is done), so a helper that answers late is still heard:
/// this one holds its answer for a moment, a limit of the old 10 minutes' kind would have
/// given up, and the switch is read as made. The other requests keep their limits.
#[test]
fn a_switch_is_waited_for_as_long_as_the_helper_takes() {
    assert_eq!(Limits::default().switch, None);
    assert_eq!(Limits::default().revert, Duration::from_secs(600));
    let d = dir("lc-long-switch");
    let (socket, asked) = serve(&d, 1, |_| {
        std::thread::sleep(Duration::from_millis(600));
        line(&Answer::Switched {
            version: VB.into(),
            commit: COMMIT_B.into(),
            deadline_unix: 77,
        })
    });
    let quick = Limits {
        other: Duration::from_millis(100),
        revert: Duration::from_millis(100),
        ..limits()
    };
    assert_eq!(
        SocketLocal::new(&socket, quick).switch(&version(VB)),
        Ok(77)
    );
    assert_eq!(asked.join().unwrap(), [format!("switch kbf={VB}")]);
}

/// A switch with no limit still notices a helper that is gone: one that takes the request and
/// closes the socket (it died) ends the wait at once as unreachable, which the update reports.
#[test]
fn a_switch_with_no_limit_still_notices_a_helper_that_dies() {
    let d = dir("lc-dead-switch");
    let (socket, _) = serve(&d, 1, |_| None);
    assert_eq!(Limits::default().switch, None);
    assert!(matches!(
        SocketLocal::new(&socket, Limits::default()).switch(&version(VB)),
        Err(LocalError::Unreachable(_))
    ));
}

/// The health check asks the helper what the host runs through the installer port: the commit
/// the host reports is the helper's, a host whose helper is busy has an install in progress, a
/// host with no version text has none installed, and the install the roll mode asks for is not
/// this mode's.
#[test]
fn the_helper_is_the_installer_port_of_the_health_check() {
    let events = Events::default();
    let local = FakeLocal::new(&events);
    let installer = LocalInstaller(Arc::new(local.clone()));
    let host = host(Role::Daemon);
    let report = installer.report(&host).unwrap();
    assert_eq!(report.installed, Installed::Version(VA.to_owned()));
    assert_eq!(report.running_commit.as_deref(), Some(COMMIT_A));
    assert_eq!(report.node_id.as_deref(), Some("n1"));
    assert_eq!(report.apply, None);

    local.world().state.version = None;
    assert_eq!(
        installer.report(&host).unwrap().installed,
        Installed::NotInstalled
    );
    local.world().state.busy = true;
    assert_eq!(
        installer.report(&host).unwrap().installed,
        Installed::InProgress
    );

    local.world().query = Err(LocalError::Unreachable("gone".into()));
    assert!(installer.report(&host).unwrap_err().is_unreachable());
    local.world().query = Err(LocalError::Failed("state_unavailable".into()));
    assert!(!installer.report(&host).unwrap_err().is_unreachable());
    assert!(installer.install(&host, &version(VB)).is_err());
}

/// Catches this agent failing to read a newer helper's query, which lists the releases it keeps
/// (and how many it left out of the list): the agent reads the host as it did before, the list
/// is passed over, and the line of a full list is read whole.
#[test]
fn a_query_that_lists_the_kept_releases_reads_as_before() {
    let d = dir("lc-kept");
    let kept: Vec<String> = (0..64)
        .map(|n| {
            format!(
                "{{\"commit\":\"{n:040x}\",\"version\":\"0.1.{n}+g{n:012x}-1\",\"why\":[\"pinned\"]}}"
            )
        })
        .collect();
    let text = format!(
        "{{\"result\":\"query\",\"commit\":\"{COMMIT_A}\",\"version\":\"{VA}\",\"node_id\":\"n1\",\
         \"pending\":null,\"last\":null,\"busy\":false,\"releases\":[{}],\"releases_omitted\":3}}\n",
        kept.join(",")
    );
    let (socket, _) = serve(&d, 1, move |_| Some(text.clone()));
    let state = SocketLocal::new(&socket, limits()).query().unwrap();
    assert_eq!(state.commit.as_deref(), Some(COMMIT_A));
    assert_eq!(state.version, Some(version(VA)));
    assert_eq!(state.node_id.as_deref(), Some("n1"));
    assert!(!state.busy);
}

/// Catches a helper's query that this agent cannot read whole: 70 releases that must stay, each
/// with a version text as long as a binary may declare, cut as the helper cuts them and answered
/// on the socket, are read by the agent's reader within its limit, the host as before.
#[test]
fn a_full_listing_of_long_versions_is_read_within_the_answer_limit() {
    use kbf_host_install::contract::local::{LISTED_BYTES, MAX_LISTED};
    let d = dir("lc-long");
    let kept: Vec<kbf_retain::Kept> = (0..70_u32)
        .map(|n| kbf_retain::Kept {
            commit: kbf_retain::CommitId::new(&format!("{n:040x}")).unwrap(),
            version: Some(format!("0.1.{n}+g{n:012x}-1{}", "x".repeat(4000))),
            why: vec!["pinned", "last_good"],
            version_left_out: false,
        })
        .collect();
    let (releases, releases_omitted) = kbf_retain::cut(kept, MAX_LISTED, LISTED_BYTES);
    assert!(releases_omitted > 0 && releases.iter().all(|k| k.version_left_out));
    let view = QueryView {
        commit: Some(COMMIT_A.into()),
        version: Some(VA.into()),
        node_id: Some("n1".into()),
        pending: None,
        last: None,
        busy: false,
        releases,
        releases_omitted,
        releases_unavailable: false,
    };
    let text = line(&Answer::Queried(view)).unwrap();
    assert!(text.len() < 16 * 1024, "{} bytes", text.len());
    let (socket, _) = serve(&d, 1, move |_| Some(text.clone()));
    let state = SocketLocal::new(&socket, limits()).query().unwrap();
    assert_eq!(state.commit.as_deref(), Some(COMMIT_A));
    assert_eq!(state.version, Some(version(VA)));
}
