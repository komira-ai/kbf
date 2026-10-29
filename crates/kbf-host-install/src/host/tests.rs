use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

use super::{Environment, Host, SELF_EXE, dispatch, handle};
use crate::config::ChannelSource;
use crate::locks;
use crate::outcome::{FailureCode, Outcome, Refusal};
use crate::pace::fake::Fake;
use crate::plan;
use crate::request::{Channel, Version};
use crate::runner::Runner;
use crate::runner::fake::{FakeRunner, exec, ok};
use crate::trust::Trust;

pub(super) const INSTALL: &str = "install kbf=1.2.3-1 from candidate";

pub(super) struct Fx {
    pub(super) dir: TestDir,
    pub(super) host: Host,
    pub(super) source: PathBuf,
    /// The time of this host's waits: it moves only when they sleep.
    pub(super) clock: Fake,
}

pub(super) struct Answer {
    pub(super) code: u8,
    pub(super) out: String,
    pub(super) err: String,
}

impl Answer {
    pub(super) fn json(&self) -> Value {
        assert!(
            self.out.ends_with('\n') && self.out.matches('\n').count() == 1,
            "{:?}",
            self.out
        );
        serde_json::from_str(&self.out).unwrap()
    }
}

impl Fx {
    /// A host whose allowlist names `candidate`, signed by a key of the trusted user.
    pub(super) fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        let trust = Trust::owned_by(fs::metadata(&*dir).unwrap().uid(), &dir);
        let mut host = Host::for_test(&dir, trust);
        let clock = Fake::new();
        host.pace = std::rc::Rc::new(clock.pace());
        let key = dir.join("key.gpg");
        write(&key, 0o644, "key");
        let source = dir.join("candidate.sources");
        write(
            &source,
            0o644,
            &format!(
                "Types: deb\nURIs: https://apt.example.org/kbf\nSuites: candidate\nSigned-By: {}\n",
                key.display()
            ),
        );
        write(
            &host.config,
            0o644,
            &format!(
                "[channels.candidate]\nsuite = \"candidate\"\nsource_list = \"{}\"\n",
                source.display()
            ),
        );
        Self {
            dir,
            host,
            source,
            clock,
        }
    }

    pub(super) fn run(&self, runner: &dyn Runner, command: &str) -> Answer {
        self.run_os(runner, Some(OsStr::new(command)))
    }

    pub(super) fn run_os(&self, runner: &dyn Runner, command: Option<&OsStr>) -> Answer {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = handle(&self.host, runner, command, &mut out, &mut err);
        Answer {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    pub(super) fn audit_lines(&self) -> Vec<Value> {
        fs::read_to_string(&self.host.audit)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub(super) fn channel_source(&self) -> ChannelSource {
        ChannelSource {
            suite: "candidate".into(),
            source_list: self.source.clone(),
        }
    }
}

pub(super) fn write(path: &std::path::Path, mode: u32, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

pub(super) fn version(text: &str) -> Version {
    Version::new(text).unwrap()
}

/// The script of an install over an older package: read, update, install, read again.
pub(super) fn install_script(before: &str, after: &str) -> FakeRunner {
    FakeRunner::new(vec![ok(before), ok("updated"), ok("installed"), ok(after)])
}

/// A request outside the grammar, a channel the allowlist does not name, or no request at
/// all, is refused with exit 2 and no process starts. The input is not echoed.
#[test]
fn refusals_run_nothing() {
    let fx = Fx::new("host-refuse");
    for (command, reason) in [
        ("query x", "bad_command"),
        ("install kbf=1;id from candidate", "bad_command"),
        ("install kbf=1 from candidate ", "bad_command"),
        ("install kbf=1 from ../x", "bad_command"),
        ("install foo=1 from candidate", "bad_command"),
        ("Install kbf=1 from candidate", "bad_command"),
        ("install kbf=1:2 from candidate", "bad_command"),
        ("install kbf=1 from stable", "channel_not_allowed"),
    ] {
        let runner = FakeRunner::new(vec![]);
        let answer = fx.run(&runner, command);
        assert_eq!(answer.code, 2, "{command}");
        assert_eq!(
            answer.json(),
            json!({"result": "refused", "reason": reason}),
            "{command}"
        );
        assert!(runner.calls().is_empty(), "{command}: nothing may run");
    }
    let none = FakeRunner::new(vec![]);
    let answer = fx.run_os(&none, None);
    assert_eq!(
        (answer.code, answer.json()["reason"].as_str()),
        (2, Some("bad_command"))
    );
    let non_utf8 = FakeRunner::new(vec![]);
    let answer = fx.run_os(&non_utf8, Some(OsStr::from_bytes(b"query\xff")));
    assert_eq!(
        (answer.code, answer.json()["reason"].as_str()),
        (2, Some("bad_command"))
    );
    assert!(none.calls().is_empty() && non_utf8.calls().is_empty());
}

/// A refused request is audited, with the request as received, escaped and cut; a request
/// with a newline in it makes one line, not two.
#[test]
fn a_refusal_is_audited_without_forging_a_line() {
    let fx = Fx::new("host-audit-refuse");
    let runner = FakeRunner::new(vec![]);
    let nasty = format!("query\n{{\"outcome\":\"installed\"}}{}", "x".repeat(500));
    fx.run(&runner, &nasty);
    fx.run(&runner, "install kbf=1 from stable");
    let audit = fx.audit_lines();
    assert_eq!(audit.len(), 2, "one line per invocation");
    assert_eq!(audit[0]["outcome"], "refused");
    assert_eq!(audit[0]["reason"], "bad_command");
    assert_eq!(audit[0]["verb"], Value::Null);
    assert_eq!(audit[0]["command"].as_str().unwrap().chars().count(), 200);
    assert!(
        audit[0]["command"]
            .as_str()
            .unwrap()
            .starts_with("query\n{")
    );
    assert_eq!(audit[1]["verb"], "install");
    assert_eq!(audit[1]["channel"], "stable");
    assert_eq!(audit[1]["reason"], "channel_not_allowed");
    assert!(audit[1].get("command").is_none());
}

/// Config problems refuse an install and run nothing: no allowlist, one anyone can write,
/// a source that is not signed, a key that is not trusted.
#[test]
fn config_problems_refuse() {
    let runner = FakeRunner::new(vec![]);
    let fx = Fx::new("host-cfg-missing");
    fs::remove_file(&fx.host.config).unwrap();
    assert_eq!(fx.run(&runner, INSTALL).json()["reason"], "config_invalid");

    let fx = Fx::new("host-cfg-writable");
    fs::set_permissions(&fx.host.config, fs::Permissions::from_mode(0o666)).unwrap();
    let answer = fx.run(&runner, INSTALL);
    assert_eq!(
        (answer.code, answer.json()["reason"].as_str()),
        (2, Some("config_unsafe"))
    );

    let fx = Fx::new("host-cfg-owner");
    let stranger = Host {
        trust: Trust::owned_by(fs::metadata(&*fx.dir).unwrap().uid() + 1, &fx.dir),
        ..fx.host.clone()
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        handle(
            &stranger,
            &runner,
            Some(OsStr::new(INSTALL)),
            &mut out,
            &mut err
        ),
        2
    );
    assert!(String::from_utf8(out).unwrap().contains("config_unsafe"));

    let fx = Fx::new("host-cfg-unsigned");
    write(
        &fx.source,
        0o644,
        "Types: deb\nURIs: https://apt.example.org/kbf\nSuites: candidate\n",
    );
    assert_eq!(
        fx.run(&runner, INSTALL).json()["reason"],
        "channel_unsigned"
    );

    let fx = Fx::new("host-cfg-key");
    write(&fx.dir.join("key.gpg"), 0o666, "key");
    assert_eq!(fx.run(&runner, INSTALL).json()["reason"], "key_untrusted");
    assert!(runner.calls().is_empty(), "no refusal started a process");
}

/// No install goes unrecorded: when the log cannot be opened, an install is not started
/// (exit 3, no process), while a query and a refusal are answered with a warning on stderr.
#[test]
fn no_install_without_an_audit_log() {
    let fx = Fx::new("host-no-audit");
    write(&fx.host.audit, 0o644, "");
    let runner = FakeRunner::new(vec![]);
    let answer = fx.run(&runner, INSTALL);
    assert_eq!(answer.code, 3);
    assert_eq!(
        answer.json(),
        json!({"result": "failed", "reason": "audit_unavailable"})
    );
    assert!(answer.err.contains("audit"), "{}", answer.err);
    assert!(runner.calls().is_empty(), "nothing ran");

    let runner = FakeRunner::new(vec![ok("un  \n"), ok("")]);
    let answer = fx.run(&runner, "query");
    assert_eq!(answer.code, 0);
    assert!(answer.err.contains("audit"));
    let runner = FakeRunner::new(vec![]);
    let answer = fx.run(&runner, "query x");
    assert_eq!(answer.code, 2);
    assert!(answer.err.contains("audit"));
    assert_eq!(
        fs::read_to_string(&fx.host.audit).unwrap(),
        "",
        "the unsafe log was left alone"
    );
}

/// Nothing a request holds is echoed: stdout is the closed line, whatever came in.
#[test]
fn input_is_never_echoed() {
    let fx = Fx::new("host-echo");
    let runner = FakeRunner::new(vec![]);
    let canary = "CANARY-1;\"\\";
    let answer = fx.run(&runner, &format!("install kbf={canary} from candidate"));
    assert!(!answer.out.contains("CANARY") && !answer.err.contains("CANARY"));
}

/// The production paths are the ones the packaging installs.
#[test]
fn production_paths() {
    let host = Host::production();
    assert_eq!(host.config, PathBuf::from("/etc/kbf/host-install.toml"));
    assert_eq!(host.audit, PathBuf::from("/var/log/kbf-host-install.log"));
    assert_eq!(
        host.install_lock,
        PathBuf::from("/run/kbf-host-install.lock")
    );
    assert_eq!(host.proc_locks, PathBuf::from("/proc/locks"));
    assert_eq!(host.dpkg_lock, PathBuf::from("/var/lib/dpkg/lock-frontend"));
    // The unit runs this very binary again, by the path it was started from.
    assert!(host.self_exe.is_absolute(), "{:?}", host.self_exe);
    assert_eq!(
        host.self_exe,
        std::env::current_exe().unwrap(),
        "the test binary stands for the program here"
    );
    assert_eq!(SELF_EXE, "/usr/libexec/kbf/kbf-host-install");
    assert_eq!(
        Host::for_test(Path::new("/x"), Trust::root()).self_exe,
        PathBuf::from(SELF_EXE)
    );
    assert_eq!(
        host.trust.owns(&fs::metadata("/").unwrap()),
        fs::metadata("/").unwrap().uid() == 0
    );
}

// ---- the install, as the process sshd starts sees it: one unit, its answer relayed ----

pub(super) fn channel(text: &str) -> Channel {
    Channel::new(text).unwrap()
}

/// What the unit prints for `outcome`: its line and a newline.
fn unit_says(outcome: &Outcome) -> String {
    format!("{}\n", outcome.json_line())
}

fn installed(text: &str) -> Outcome {
    Outcome::Installed {
        version: text.into(),
    }
}

/// A normal install, as the process sshd starts sees it: exactly one command runs, the
/// install unit, and it is the plan's; its answer is the answer; the audit line says who,
/// what and the outcome. This process does not hold the install lock while it waits (the unit
/// does): a hold here would end with the ssh connection, with `dpkg` still running.
#[test]
fn an_install_starts_one_unit() {
    let fx = Fx::new("host-install");
    let lock = fx.host.install_lock.clone();
    let runner = FakeRunner::new(vec![ok(&unit_says(&installed("1.2.3-1")))])
        .before_each(move |_| assert!(!locks::flock_held(&lock), "the caller holds the lock"));
    let answer = fx.run(&runner, INSTALL);
    assert_eq!(answer.code, 0, "{}", answer.err);
    assert_eq!(
        answer.json(),
        json!({"result": "installed", "installed_version": "1.2.3-1", "unit_state": "finished"})
    );
    assert_eq!(
        runner.calls(),
        vec![plan::unit_install(
            &fx.host.self_exe,
            &version("1.2.3-1"),
            &channel("candidate")
        )]
    );
    let audit = fx.audit_lines();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["verb"], "install");
    assert_eq!(audit[0]["version"], "1.2.3-1");
    assert_eq!(audit[0]["channel"], "candidate");
    assert_eq!(audit[0]["outcome"], "installed");
    assert_eq!(audit[0]["reason"], Value::Null);
    assert!(audit[0]["time"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        fs::metadata(&fx.host.audit).unwrap().permissions().mode() & 0o777,
        0o600
    );
    locks::wait_lockable(&fx.host.install_lock);
    assert!(!locks::flock_held(&fx.host.install_lock), "none left");
}

/// Every answer the unit can give is the answer, with its exit status and its audit line:
/// done, already done, each refusal (exit 2) and each failure (exit 3), with the failure's
/// detail, from the unit's stderr, on stderr. Noise before the line (a unit may write
/// anything) is not the answer; the last line is.
#[test]
fn the_unit_answer_is_relayed() {
    let mut outcomes = vec![
        installed("1.2.3-1"),
        installed("1.2.3-01"),
        Outcome::AlreadyInstalled {
            version: "1.2.3-1".into(),
        },
    ];
    outcomes.extend(Refusal::ALL.map(Outcome::Refused));
    outcomes.extend(FailureCode::ALL.map(|code| {
        Outcome::Failed(crate::outcome::Failure {
            code,
            detail: String::new(),
        })
    }));
    for outcome in outcomes {
        for noise in ["", "apt noise\n", "\n\n"] {
            let fx = Fx::new("host-relay");
            let stdout = format!("{noise}{}", unit_says(&outcome));
            let runner =
                FakeRunner::new(vec![exec(outcome.exit_code().into(), &stdout, "E: why\n")]);
            let answer = fx.run(&runner, INSTALL);
            assert_eq!(answer.code, outcome.exit_code(), "{outcome:?}");
            let want: Value = serde_json::from_str(&outcome.json_line()).unwrap();
            assert_eq!(answer.json(), want, "{outcome:?}");
            let audit = fx.audit_lines();
            let (word, reason) = outcome.audit_fields();
            assert_eq!(audit[0]["outcome"], word);
            assert_eq!(audit[0]["reason"], json!(reason));
            let failed = matches!(outcome, Outcome::Failed(_));
            assert_eq!(
                answer.err.contains("E: why"),
                failed,
                "{outcome:?}: {}",
                answer.err
            );
        }
    }
}

/// The unit's answer is not believed past what was asked: a version other than the target
/// (under dpkg's ordering) is a failed verification, and a unit that did not answer, or
/// answered with something else, is an install failure, whatever its exit status.
#[test]
fn an_unintelligible_unit_is_an_install_failure() {
    let wrong_version = [
        installed("1.2.4-1"),
        Outcome::AlreadyInstalled {
            version: "1.2.2-1".into(),
        },
        installed("1.2.3-1+b1"),
    ];
    for outcome in wrong_version {
        let fx = Fx::new("host-relay-wrong");
        let runner = FakeRunner::new(vec![ok(&unit_says(&outcome))]);
        let answer = fx.run(&runner, INSTALL);
        assert_eq!(answer.code, 3, "{outcome:?}");
        assert_eq!(answer.json()["reason"], "verify_failed", "{outcome:?}");
    }
    let query =
        r#"{"result":"query","installed_version":null,"running_commit":null,"in_progress":false}"#;
    let cases = [
        ("no output, exit 0", exec(0, "", "")),
        (
            "no output, exit 1",
            exec(1, "", "Failed to start transient service unit"),
        ),
        ("not json", exec(0, "done\n", "")),
        ("a query line", exec(0, &format!("{query}\n"), "")),
        (
            "killed by a signal",
            Ok(crate::runner::Exec {
                code: None,
                signal: None,
                stdout: vec![],
                stderr: b"killed".to_vec(),
            }),
        ),
        (
            "cannot start",
            Err(std::io::Error::other("systemd-run: not found")),
        ),
        (
            "a line cut short",
            exec(0, "{\"result\":\"installed\",\"installed_ver", ""),
        ),
    ];
    for (what, reply) in cases {
        let fx = Fx::new("host-relay-lost");
        let runner = FakeRunner::new(vec![reply]);
        let answer = fx.run(&runner, INSTALL);
        assert_eq!(answer.code, 3, "{what}");
        assert_eq!(
            answer.json(),
            json!({"result": "failed", "reason": "install_failed"}),
            "{what}"
        );
        assert!(!answer.err.is_empty(), "{what}: the cause is on stderr");
        assert_eq!(fx.audit_lines()[0]["reason"], "install_failed");
    }
}

/// Two installs cannot overlap: while the install lock is held (by the unit of another
/// install) a second request is answered `busy` and starts nothing, so a second, older
/// target cannot pass the same-version check against the same old version and then install over
/// the first; and a repeat of the running install is `busy` and not a unit that systemd
/// refuses by name. A lock file that cannot be asked about is busy too (idle is not
/// proved). Once the holder is gone the same install goes through. A query takes no lock.
#[test]
fn an_overlapping_install_is_busy() {
    let fx = Fx::new("host-busy");
    let holder = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&fx.host.install_lock)
        .unwrap();
    holder.lock().unwrap();
    let runner = FakeRunner::new(vec![]);
    let answer = fx.run(&runner, INSTALL);
    assert_eq!(answer.code, 3);
    assert_eq!(answer.json(), json!({"result": "failed", "reason": "busy"}));
    assert!(runner.calls().is_empty(), "nothing ran");
    assert_eq!(fx.audit_lines().len(), 1, "the attempt is audited");

    let runner = FakeRunner::new(vec![ok("un  \n"), ok("")]);
    assert_eq!(fx.run(&runner, "query").code, 0);

    drop(holder);
    locks::wait_lockable(&fx.host.install_lock);
    let runner = FakeRunner::new(vec![ok(&unit_says(&installed("1.2.3-1")))]);
    assert_eq!(fx.run(&runner, INSTALL).code, 0);
    assert_eq!(runner.calls().len(), 1);

    let mut fx = Fx::new("host-busy-link");
    let target = fx.dir.join("elsewhere");
    fs::write(&target, "").unwrap();
    let link = fx.dir.join("lock-link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    fx.host.install_lock = link;
    let runner = FakeRunner::new(vec![]);
    assert_eq!(fx.run(&runner, INSTALL).json()["reason"], "busy");
    assert!(runner.calls().is_empty());
}

/// Every invocation adds exactly one audit line: a query, an install, a refusal.
#[test]
fn every_invocation_adds_exactly_one_line() {
    let fx = Fx::new("host-lines");
    let runner = FakeRunner::new(vec![
        ok("un  \n"),
        ok(""),
        ok(&unit_says(&installed("1.2.3-1"))),
    ]);
    fx.run(&runner, "query");
    fx.run(&runner, INSTALL);
    fx.run(&runner, "query x");
    assert_eq!(fx.audit_lines().len(), 3);
}

// ---- query ----

/// `query` does not need the allowlist: it answers on a host with none, and says what it is
/// (`result`: `query`) like every other answer.
#[test]
fn query_does_not_read_the_allowlist() {
    let fx = Fx::new("host-query-nocfg");
    fs::remove_file(&fx.host.config).unwrap();
    let runner = FakeRunner::new(vec![ok("un  \n"), ok("")]);
    let answer = fx.run(&runner, "query");
    assert_eq!(answer.code, 0);
    assert_eq!(
        answer.json(),
        json!({"result": "query", "installed_version": null, "running_commit": null, "node_id": null, "in_progress": false})
    );
    assert_eq!(
        runner.calls(),
        vec![plan::installed_version(), plan::units_running()]
    );
    assert_eq!(fx.audit_lines()[0]["verb"], "query");
    assert_eq!(fx.audit_lines()[0]["outcome"], "query");
}

fn kbf_json(extra: &str) -> String {
    format!("{{\"profile\":\"release\",\"version\":\"0.1.0\"{extra}}}\n")
}

const UNIT_LISTING: &str =
    "kbf-host-install-1.2.3-1.service loaded activating start start kbf-host-install\n";

/// `query` on an installed host: the version, the commit the binary reports, and whether
/// an install is in progress.
#[test]
fn query_reports_version_commit_and_progress() {
    let fx = Fx::new("host-query");
    let full = "0123456789abcdef0123456789abcdef01234567";
    let runner = FakeRunner::new(vec![
        ok("ii  1.2.3-1\n"),
        ok(&kbf_json(&format!(
            ",\"commit\":\"0123456789ab\",\"commit_full\":\"{full}\""
        ))),
        ok(UNIT_LISTING),
    ]);
    let answer = fx.run(&runner, "query");
    assert_eq!(
        answer.json(),
        json!({"result": "query", "installed_version": "1.2.3-1", "running_commit": full, "node_id": null, "in_progress": true})
    );
    assert_eq!(
        runner.calls(),
        vec![
            plan::installed_version(),
            plan::running_build(),
            plan::units_running()
        ]
    );
}

/// `query` reports the node id from the node's configuration, on one line beside the
/// running commit, and `null` when the host has no configuration to read it from.
#[test]
fn query_reports_the_node_id() {
    let fx = Fx::new("host-query-node-id");
    write(&fx.host.node_config, 0o640, "[node]\nid = \"build-7\"\n");
    let runner = FakeRunner::new(vec![ok("un  \n"), ok("")]);
    let answer = fx.run(&runner, "query");
    assert_eq!(answer.code, 0);
    assert_eq!(answer.json()["node_id"], "build-7");
    assert_eq!(answer.out.matches('\n').count(), 1, "one line");

    fs::remove_file(&fx.host.node_config).unwrap();
    let runner = FakeRunner::new(vec![ok("un  \n"), ok("")]);
    assert_eq!(fx.run(&runner, "query").json()["node_id"], Value::Null);
}

/// The running commit falls back to the short commit, and is null when the build did not
/// know it, when the binary fails, hangs (the runner kills it at its limit) or prints
/// something else, or when it is not text.
#[test]
fn the_running_commit_is_best_effort() {
    for (what, reply, want) in [
        (
            "short only",
            ok(&kbf_json(",\"commit\":\"0123456789ab\"")),
            json!("0123456789ab"),
        ),
        (
            "unknown build",
            ok(&kbf_json(",\"commit\":\"unknown\"")),
            Value::Null,
        ),
        (
            "full unknown, short known",
            ok(&kbf_json(",\"commit\":\"abc\",\"commit_full\":\"\"")),
            json!("abc"),
        ),
        ("no commit field", ok(&kbf_json("")), Value::Null),
        (
            "commit is a number",
            ok(&kbf_json(",\"commit\":7")),
            Value::Null,
        ),
        ("not json", ok("kbf 0.1.0\n"), Value::Null),
        ("json but not an object", ok("[1]\n"), Value::Null),
        (
            "exit 1",
            exec(1, &kbf_json(",\"commit\":\"abc\""), "usage"),
            Value::Null,
        ),
        (
            "cannot start",
            Err(std::io::Error::other("no such file")),
            Value::Null,
        ),
        (
            "hung and killed at the limit",
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "ran past its limit",
            )),
            Value::Null,
        ),
    ] {
        let fx = Fx::new("host-commit");
        let runner = FakeRunner::new(vec![ok("ii  1.2.3-1\n"), reply, ok("")]);
        let answer = fx.run(&runner, "query");
        assert_eq!(answer.code, 0, "{what}");
        assert_eq!(answer.json()["running_commit"], want, "{what}");
        assert_eq!(answer.json()["installed_version"], "1.2.3-1", "{what}");
        assert_eq!(runner.calls().len(), 3, "{what}: the rest was still asked");
    }
}

/// The running build is asked under a limit: the command the query runs is the plan's, whose
/// limit the runner keeps (a binary that hangs as root must not hang the query).
#[test]
fn the_running_build_has_a_limit() {
    let fx = Fx::new("host-commit-limit");
    let runner = FakeRunner::new(vec![ok("ii  1.2.3-1\n"), ok("{}"), ok("")]);
    fx.run(&runner, "query");
    let asked = runner.calls()[1].clone();
    assert_eq!(asked.program, "/usr/bin/kbf");
    assert_eq!(asked.timeout, Some(Duration::from_secs(10)));
}

/// An install is in progress when ANY of three things holds, and each alone is enough:
/// the install flock is held (by the unit, the process doing the work), a `kbf-host-install-*`
/// unit exists that is not over (the listing is not empty: `activating` and `deactivating` are
/// in it, not only `active`), or some process holds the dpkg frontend lock (read from the lock
/// table, so no lock is taken). With none of them it is false.
#[test]
fn in_progress_is_any_of_three_signals() {
    let fx = Fx::new("host-progress");
    write(&fx.host.dpkg_lock, 0o644, "");
    let stat = rustix::fs::stat(&fx.host.dpkg_lock).unwrap();
    let line = format!(
        "1: POSIX  ADVISORY  WRITE 812 {:02x}:{:02x}:{} 0 EOF\n",
        rustix::fs::major(stat.st_dev),
        rustix::fs::minor(stat.st_dev),
        stat.st_ino
    );
    let ask = |units: &str| {
        let runner = FakeRunner::new(vec![ok("ii  1.2.3-1\n"), exec(1, "", ""), ok(units)]);
        fx.run(&runner, "query").json()["in_progress"]
            .as_bool()
            .unwrap()
    };
    let flock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&fx.host.install_lock)
        .unwrap();
    write(&fx.host.proc_locks, 0o644, "");

    assert!(!ask(""), "none of the three");
    assert!(!ask("\n  \n"), "a listing of blanks is empty");
    assert!(ask(UNIT_LISTING), "an activating unit alone");
    assert!(
        ask("kbf-host-install-1.2.3-1.service loaded active running x\n"),
        "an active unit alone"
    );
    assert!(
        ask("kbf-host-install-1.2.3-1.service loaded deactivating stop-sigterm x\n"),
        "a deactivating unit alone"
    );

    flock.lock().unwrap();
    assert!(
        ask(""),
        "the held flock alone: the unit that holds it is not listed"
    );
    assert!(ask(UNIT_LISTING), "flock and unit");
    flock.unlock().unwrap();
    assert!(!ask(""), "the flock released");

    write(&fx.host.proc_locks, 0o644, &line);
    assert!(ask(""), "the dpkg lock alone");
    write(&fx.host.proc_locks, 0o644, "");
    assert!(!ask(""), "and gone");
}

/// A query does not take the install lock for good: after it, an install can.
#[test]
fn a_query_leaves_no_lock() {
    let fx = Fx::new("host-query-lock");
    write(&fx.host.install_lock, 0o600, "");
    let runner = FakeRunner::new(vec![ok("un  \n"), ok("")]);
    fx.run(&runner, "query");
    // Taking it is the proof: a probe's shared lock left behind would refuse this.
    locks::wait_lockable(&fx.host.install_lock);
    let file = fs::File::open(&fx.host.install_lock).unwrap();
    file.try_lock().expect("the query's probe was let go");
}

/// A package that dpkg flags as needing a reinstall (`iiR`) is not cleanly installed: `query`
/// reports no installed version for it (the updater then installs, and apt repairs it) and
/// still answers, with exit 0.
#[test]
fn query_does_not_report_a_package_in_error() {
    for status in ["iiR", "iiH", "iiX", "iiZ"] {
        let fx = Fx::new("host-query-flag");
        let runner = FakeRunner::new(vec![ok(&format!("{status} 1.2.3-1\n")), ok("")]);
        let answer = fx.run(&runner, "query");
        assert_eq!(answer.code, 0, "{status}");
        assert_eq!(
            answer.json(),
            json!({"result": "query", "installed_version": null, "running_commit": null, "node_id": null, "in_progress": false}),
            "{status}"
        );
        assert_eq!(
            runner.calls(),
            vec![plan::installed_version(), plan::units_running()],
            "{status}: no build is asked of a package that is not installed"
        );
    }
}

/// A read that cannot be trusted: what it is, dpkg's reply, and the code it is answered with.
pub(super) type UnreadableCase = (
    &'static str,
    fn() -> std::io::Result<crate::runner::Exec>,
    &'static str,
);

/// The reads that cannot be trusted, as `query` and as the install unit see them; the
/// cases are shared by both.
pub(super) fn unreadable_cases() -> Vec<UnreadableCase> {
    vec![
        (
            "dpkg-query exit 2",
            || exec(2, "", "dpkg-query: error"),
            "query_failed",
        ),
        (
            "dpkg-query cannot start",
            || Err(std::io::Error::other("gone")),
            "query_failed",
        ),
        ("empty output", || ok(""), "query_failed"),
        ("a status and no version", || ok("ii\n"), "query_failed"),
        ("short status", || ok("i 1.0\n"), "query_failed"),
        (
            "half-configured",
            || ok("iF  1.0-1\n"),
            "package_state_unclean",
        ),
        (
            "half-installed",
            || ok("ih  1.0-1\n"),
            "package_state_unclean",
        ),
        (
            "bad version",
            || ok("ii  not!a!version\n"),
            "installed_version_invalid",
        ),
        (
            "version with a quote",
            || ok("ii  1.0\"1\n"),
            "installed_version_invalid",
        ),
    ]
}

/// A read that cannot be trusted is a failure with exit 3, not a guess: `dpkg-query` failing
/// or printing nonsense, a package state that is neither installed nor absent, a version
/// dpkg would not accept; and the systemd listing failing, or exiting non-zero.
#[test]
fn unreadable_state_fails_the_query() {
    for (what, reply, reason) in unreadable_cases() {
        let fx = Fx::new("host-unreadable");
        let runner = FakeRunner::new(vec![reply()]);
        let answer = fx.run(&runner, "query");
        assert_eq!(answer.code, 3, "{what}");
        assert_eq!(
            answer.json(),
            json!({"result": "failed", "reason": reason}),
            "{what}"
        );
        assert_eq!(runner.calls().len(), 1, "{what}: nothing else ran");
        assert_eq!(fx.audit_lines()[0]["reason"], reason);
    }
    for (what, reply) in [
        ("systemctl cannot start", Err(std::io::Error::other("gone"))),
        ("systemctl exits 1", exec(1, "", "Failed to connect to bus")),
        (
            "systemctl is killed",
            Ok(crate::runner::Exec {
                code: None,
                signal: None,
                stdout: vec![],
                stderr: vec![],
            }),
        ),
    ] {
        let fx = Fx::new("host-systemctl");
        let runner = FakeRunner::new(vec![ok("ii  1.0-1\n"), exec(1, "", ""), reply]);
        let answer = fx.run(&runner, "query");
        assert_eq!(
            (answer.code, answer.json()["reason"].as_str()),
            (3, Some("query_failed")),
            "{what}"
        );
    }
}

// ---- which mode the process is in ----

fn env(vars: &[(&str, &str)]) -> Environment {
    Environment::from_vars(|name| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| OsString::from(value))
    })
}

/// The environment is read from four sshd variables and the one unit variable, by name.
#[test]
fn the_environment_is_read_by_name() {
    assert_eq!(env(&[]), Environment::default());
    assert!(!env(&[]).ssh_session);
    assert_eq!(
        env(&[("SSH_ORIGINAL_COMMAND", "query")]),
        Environment {
            ssh_original_command: Some("query".into()),
            unit_request: None,
            ssh_session: true,
        }
    );
    for name in [
        "SSH_ORIGINAL_COMMAND",
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "SSH_TTY",
    ] {
        assert!(env(&[(name, "x")]).ssh_session, "{name}");
    }
    assert!(!env(&[("SSH_AUTH_SOCK", "x"), ("HOME", "/root")]).ssh_session);
    assert_eq!(
        env(&[("KBF_HOST_INSTALL_UNIT", "install kbf=1 from c")]),
        Environment {
            ssh_original_command: None,
            unit_request: Some("install kbf=1 from c".into()),
            ssh_session: false,
        }
    );
    assert_eq!(plan::UNIT_ENV, "KBF_HOST_INSTALL_UNIT");
    // The real environment of this process is the same reading (cargo's variables are
    // neither).
    let process = Environment::from_process();
    assert_eq!(
        process.unit_request.is_some(),
        std::env::var_os("KBF_HOST_INSTALL_UNIT").is_some()
    );
}

/// The unit mode is reachable only from systemd: the variable alone starts it, and any sign
/// of an ssh session (the request, the connection, the client, a terminal) makes the
/// variable count for nothing and the process an ordinary request handler. Catches an
/// ssh client that sets the variable (sshd passes variables the host's `AcceptEnv` names)
/// and reaches the install body without the audit line and the checks of the front.
#[test]
fn the_unit_mode_is_not_reachable_over_ssh() {
    let request = "install kbf=1.2.3-1 from candidate";
    // Alone: the unit body runs (its first command is the package read under the lock).
    let fx = Fx::new("host-dispatch-unit");
    let runner = FakeRunner::new(vec![
        exec(1, "", "no packages"),
        ok(""),
        ok(""),
        ok("ii  1.2.3-1\n"),
    ]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = dispatch(
        &fx.host,
        &runner,
        &env(&[("KBF_HOST_INSTALL_UNIT", request)]),
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    assert_eq!(runner.calls().len(), 4);
    assert!(
        fx.audit_lines().is_empty(),
        "the unit does not audit; its starter did"
    );

    // With any one ssh variable: the unit variable is ignored, the process is an ordinary
    // request handler, and the request (none, or not one) is refused and audited.
    for name in [
        "SSH_ORIGINAL_COMMAND",
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "SSH_TTY",
    ] {
        let fx = Fx::new("host-dispatch-ssh");
        let runner = FakeRunner::new(vec![]);
        let vars = [("KBF_HOST_INSTALL_UNIT", request), (name, "x")];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = dispatch(&fx.host, &runner, &env(&vars), &mut out, &mut err);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 2, "{name}: {text}");
        assert!(text.contains("bad_command"), "{name}: {text}");
        assert!(
            runner.calls().is_empty(),
            "{name}: the install body did not run"
        );
        assert_eq!(fx.audit_lines().len(), 1, "{name}: audited as a request");
    }

    // No variable: the ordinary handler, whatever the session.
    let fx = Fx::new("host-dispatch-plain");
    let runner = FakeRunner::new(vec![ok("un  \n"), ok("")]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = dispatch(
        &fx.host,
        &runner,
        &env(&[("SSH_ORIGINAL_COMMAND", "query")]),
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0);
}

/// A lock is let go when it is dropped, even while another copy of its descriptor lives on.
/// A process another thread starts has such a copy from the fork until it execs, and a plain
/// `flock` lives as long as any copy does, so a drop could be followed by a refusal as busy
/// for as long as that child had not yet exec'd. The `try_clone` is that copy.
#[test]
fn a_lock_is_let_go_at_drop_while_a_copy_of_its_descriptor_lives_on() {
    let dir = TestDir::under(std::env::temp_dir(), "hi-lock-copy");
    let path = dir.join("install.lock");
    let held = super::take_lock(&path).unwrap();
    let copy = held.file().try_clone().unwrap();
    drop(held);
    let again = super::take_lock(&path);
    assert!(again.is_ok(), "busy after the drop: {:?}", again.err());
    drop(copy);
}
