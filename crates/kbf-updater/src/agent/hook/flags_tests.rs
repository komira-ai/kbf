//! The flags before the step: the same settings as the agent's, in the same words, and the same
//! defaults.

use std::path::PathBuf;
use std::time::Duration;

use super::flags::split;

fn args(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_owned).collect()
}

const BASE: &str =
    "--record-dir /var/lib/hook --channel stable --client-cert /c --client-key /k --ca /ca";

fn refusal(line: &str) -> String {
    split(&args(line)).map(|_| ()).unwrap_err()
}

/// The shortest command line takes the agent's defaults: the helper socket, the 300 s health
/// timeout, 5 s polls, a 900 s retry, no drain deadline, the marks of the cordons in the record
/// directory; the step and what follows it are handed back.
///
/// Catches: a default that is not today's.
#[test]
fn the_defaults_are_the_agents() {
    let line = args(&format!("{BASE} prepare /artifact --params /p"));
    let (flags, rest) = split(&line).unwrap();
    assert_eq!(rest, ["prepare", "/artifact", "--params", "/p"]);
    assert_eq!(flags.record_dir, PathBuf::from("/var/lib/hook"));
    assert_eq!(flags.channel, "stable");
    assert_eq!(
        flags.helper_socket,
        PathBuf::from("/run/kbf-host-install/local.sock")
    );
    assert_eq!(flags.access.cordon_dir, PathBuf::from("/var/lib/hook"));
    assert_eq!(flags.access.files.cert, PathBuf::from("/c"));
    assert_eq!(flags.access.files.key, PathBuf::from("/k"));
    assert_eq!(flags.access.files.ca, PathBuf::from("/ca"));
    assert!(flags.access.frontend.is_none() && flags.access.node_api.is_none());
    assert!(flags.access.daemon_node.is_none());
    let t = flags.timing;
    assert_eq!(
        (
            t.drain_deadline,
            t.drain_poll,
            t.health_timeout,
            t.health_poll,
            t.retry,
            t.margin
        ),
        (
            None,
            Duration::from_secs(5),
            Duration::from_secs(300),
            Duration::from_secs(5),
            Duration::from_secs(900),
            Duration::from_secs(10)
        )
    );
}

/// Every flag, in both forms, and the enrollment directory in place of the three files.
///
/// Catches: a flag read and dropped.
#[test]
fn every_flag_is_read() {
    let line = args(
        "--record-dir=/r --channel=beta --enrollment-dir /e --frontend fe.example:18001 \
         --operator-api http://127.0.0.1:18002 --token-file /t --daemon-node d1 \
         --cordon-dir /cordons --helper-socket /run/h.sock --drain-deadline-secs 3600 \
         --health-timeout-secs 120 --health-poll-secs 2 --retry-secs 60 cutover",
    );
    let (flags, rest) = split(&line).unwrap();
    assert_eq!(rest, ["cutover"]);
    assert_eq!(flags.channel, "beta");
    assert_eq!(flags.access.cordon_dir, PathBuf::from("/cordons"));
    assert_eq!(flags.helper_socket, PathBuf::from("/run/h.sock"));
    assert_eq!(flags.access.daemon_node.as_deref(), Some("d1"));
    assert_eq!(flags.access.frontend.as_ref().map(|e| e.port), Some(18001));
    let api = flags.access.node_api.as_ref().unwrap();
    assert_eq!(
        (api.addr.port(), api.token_file.as_path()),
        (18002, std::path::Path::new("/t"))
    );
    assert!(flags.access.files.cert.starts_with("/e"));
    let t = flags.timing;
    assert_eq!(
        (t.drain_deadline, t.health_timeout, t.health_poll, t.retry),
        (
            Some(Duration::from_secs(3600)),
            Duration::from_secs(120),
            Duration::from_secs(2),
            Duration::from_secs(60)
        )
    );
}

/// Each way the flags can be wrong is refused in a line that names the flag.
///
/// Catches: a setting that is taken without its check.
#[test]
fn flags_that_are_wrong_are_refused_by_name() {
    assert!(
        refusal("--channel stable --client-cert /c --client-key /k --ca /ca prepare")
            .contains("--record-dir is required")
    );
    assert!(
        refusal("--record-dir /r --client-cert /c --client-key /k --ca /ca prepare")
            .contains("--channel is required")
    );
    assert!(
        refusal(
            "--record-dir /r --channel Stable --client-cert /c --client-key /k --ca /ca prepare"
        )
        .contains("--channel")
    );
    assert!(refusal("--record-dir /r --channel stable prepare").contains("--enrollment-dir"));
    assert!(
        refusal("--record-dir /r --channel stable --enrollment-dir /e --ca /ca prepare")
            .contains("--enrollment-dir")
    );
    assert!(refusal(&format!("{BASE} --frontend nonsense prepare")).contains("--frontend"));
    assert!(
        refusal(&format!("{BASE} --operator-api http://127.0.0.1:1 prepare"))
            .contains("given together")
    );
    assert!(
        refusal(&format!(
            "{BASE} --operator-api http://example.com:1 --token-file /t prepare"
        ))
        .contains("--operator-api")
    );
    assert!(
        refusal(&format!("{BASE} --daemon-node Not_A_Label prepare")).contains("--daemon-node")
    );
    assert!(
        refusal(&format!("{BASE} --drain-deadline-secs 0 prepare"))
            .contains("--drain-deadline-secs")
    );
    assert!(refusal(&format!("{BASE} --retry-secs 0 prepare")).contains("--retry-secs"));
    assert!(
        refusal(&format!("{BASE} --health-timeout-secs 90000 prepare"))
            .contains("--health-timeout-secs")
    );
    assert!(
        refusal(&format!("{BASE} --retry-secs 5 --retry-secs 6 prepare")).contains("given twice")
    );
    assert!(refusal(&format!("{BASE} --retry-secs")).contains("needs a value"));
    assert!(refusal(&format!("{BASE} --unknown 1 prepare")).contains("unknown argument --unknown"));
}
