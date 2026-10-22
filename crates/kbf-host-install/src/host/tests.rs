use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use kbf_testkit::dir::TestDir;
use serde_json::{json, Value};

use super::{handle, Host};
use crate::config::ChannelSource;
use crate::plan;
use crate::request::Version;
use crate::runner::fake::{exec, ok, FakeRunner};
use crate::runner::Runner;
use crate::trust::Trust;

const INSTALL: &str = "install kbf=1.2.3-1 from candidate";

struct Fx {
    dir: TestDir,
    host: Host,
    source: PathBuf,
}

struct Answer {
    code: u8,
    out: String,
    err: String,
}

impl Answer {
    fn json(&self) -> Value {
        assert!(self.out.ends_with('\n') && self.out.matches('\n').count() == 1, "{:?}", self.out);
        serde_json::from_str(&self.out).unwrap()
    }
}

impl Fx {
    /// A host whose allowlist names `candidate`, signed by a key of the trusted user.
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let trust = Trust::owned_by(fs::metadata(&*dir).unwrap().uid());
        let host = Host::for_test(&dir, trust);
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
        Self { dir, host, source }
    }

    fn run(&self, runner: &dyn Runner, command: &str) -> Answer {
        self.run_os(runner, Some(OsStr::new(command)))
    }

    fn run_os(&self, runner: &dyn Runner, command: Option<&OsStr>) -> Answer {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = handle(&self.host, runner, command, &mut out, &mut err);
        Answer {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    fn audit_lines(&self) -> Vec<Value> {
        fs::read_to_string(&self.host.audit)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn channel_source(&self) -> ChannelSource {
        ChannelSource { suite: "candidate".into(), source_list: self.source.clone() }
    }
}

fn write(path: &std::path::Path, mode: u32, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn version(text: &str) -> Version {
    Version::new(text).unwrap()
}

/// The script of an install over an older package: read, update, install, read again.
fn install_script(before: &str, after: &str) -> FakeRunner {
    FakeRunner::new(vec![ok(before), ok("updated"), ok("installed"), ok(after)])
}

/// A normal install: the four commands, in order, are the plan's; the line says installed;
/// the audit line says who, what and the outcome.
#[test]
fn an_install_runs_the_plan() {
    let fx = Fx::new("host-install");
    let runner = install_script("ii  1.0-1\n", "ii  1.2.3-1\n");
    let answer = fx.run(&runner, INSTALL);
    assert_eq!(answer.code, 0, "{}", answer.err);
    assert_eq!(
        answer.json(),
        json!({"result": "installed", "installed_version": "1.2.3-1", "unit_state": "finished"})
    );
    let v = version("1.2.3-1");
    assert_eq!(
        runner.calls(),
        vec![
            plan::installed_version(),
            plan::apt_update(&fx.channel_source(), &v),
            plan::apt_install(&fx.channel_source(), &v),
            plan::installed_version(),
        ]
    );
    let audit = fx.audit_lines();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["verb"], "install");
    assert_eq!(audit[0]["version"], "1.2.3-1");
    assert_eq!(audit[0]["channel"], "candidate");
    assert_eq!(audit[0]["outcome"], "installed");
    assert_eq!(audit[0]["reason"], Value::Null);
    assert!(audit[0]["time"].as_str().unwrap().ends_with('Z'));
    assert_eq!(fs::metadata(&fx.host.audit).unwrap().permissions().mode() & 0o777, 0o600);
}

/// A host with no `kbf` yet installs: `dpkg-query` exits 1 with nothing for an unknown
/// package, and a package that was removed but not purged (`rc`) is the same.
#[test]
fn installing_over_nothing() {
    for before in [exec(1, "", "dpkg-query: no packages found matching kbf"), ok("rc  1.0-1\n"), ok("un  \n")] {
        let fx = Fx::new("host-fresh");
        let runner = FakeRunner::new(vec![before, ok(""), ok(""), ok("ii  1.2.3-1\n")]);
        let answer = fx.run(&runner, INSTALL);
        assert_eq!(answer.code, 0, "{}", answer.err);
        assert_eq!(answer.json()["result"], "installed");
        assert_eq!(runner.calls().len(), 4);
    }
}

/// Equal is a no-op that reports installed: exit 0, only the read ran. Equal means equal
/// under dpkg's ordering, so `1.2.3-01` is the version `1.2.3-1` is.
#[test]
fn the_same_version_is_a_no_op() {
    for installed in ["1.2.3-1", "1.2.3-01", "0:1.2.3-1"] {
        let fx = Fx::new("host-equal");
        let runner = FakeRunner::new(vec![ok(&format!("ii  {installed}\n"))]);
        let answer = fx.run(&runner, INSTALL);
        assert_eq!(answer.code, 0, "{installed}: {}", answer.err);
        assert_eq!(
            answer.json(),
            json!({"result": "already_installed", "installed_version": installed, "unit_state": "not_run"})
        );
        assert_eq!(runner.calls(), vec![plan::installed_version()], "nothing else ran");
        assert_eq!(fx.audit_lines()[0]["outcome"], "already_installed");
    }
}

/// A downgrade is refused with exit 2 and nothing but the read runs: an older upstream, an
/// older revision, a higher epoch installed, and a release candidate of the same version.
#[test]
fn a_downgrade_is_refused() {
    for installed in ["1.2.4-1", "1.2.3-2", "1:0.1-1", "2.0", "1.2.3-1+b1"] {
        let fx = Fx::new("host-downgrade");
        let runner = FakeRunner::new(vec![ok(&format!("ii  {installed}\n"))]);
        let answer = fx.run(&runner, INSTALL);
        assert_eq!(answer.code, 2, "{installed}");
        assert_eq!(answer.json(), json!({"result": "refused", "reason": "older_than_installed"}));
        assert_eq!(runner.calls().len(), 1, "{installed}: nothing installed");
        assert_eq!(fx.audit_lines()[0]["reason"], "older_than_installed");
    }
    let fx = Fx::new("host-rc");
    let runner = FakeRunner::new(vec![ok("ii  1.2.3-1\n")]);
    assert_eq!(fx.run(&runner, "install kbf=1.2.3~rc1 from candidate").code, 2, "an rc is older");
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
        assert_eq!(answer.json(), json!({"result": "refused", "reason": reason}), "{command}");
        assert!(runner.calls().is_empty(), "{command}: nothing may run");
    }
    let none = FakeRunner::new(vec![]);
    let answer = fx.run_os(&none, None);
    assert_eq!((answer.code, answer.json()["reason"].as_str()), (2, Some("bad_command")));
    let non_utf8 = FakeRunner::new(vec![]);
    let answer = fx.run_os(&non_utf8, Some(OsStr::from_bytes(b"query\xff")));
    assert_eq!((answer.code, answer.json()["reason"].as_str()), (2, Some("bad_command")));
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
    assert!(audit[0]["command"].as_str().unwrap().starts_with("query\n{"));
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
    assert_eq!((answer.code, answer.json()["reason"].as_str()), (2, Some("config_unsafe")));

    let fx = Fx::new("host-cfg-owner");
    let stranger = Host { trust: Trust::owned_by(fs::metadata(&*fx.dir).unwrap().uid() + 1), ..fx.host.clone() };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(handle(&stranger, &runner, Some(OsStr::new(INSTALL)), &mut out, &mut err), 2);
    assert!(String::from_utf8(out).unwrap().contains("config_unsafe"));

    let fx = Fx::new("host-cfg-unsigned");
    write(&fx.source, 0o644, "Types: deb\nURIs: https://apt.example.org/kbf\nSuites: candidate\n");
    assert_eq!(fx.run(&runner, INSTALL).json()["reason"], "channel_unsigned");

    let fx = Fx::new("host-cfg-key");
    write(&fx.dir.join("key.gpg"), 0o666, "key");
    assert_eq!(fx.run(&runner, INSTALL).json()["reason"], "key_untrusted");
    assert!(runner.calls().is_empty(), "no refusal started a process");
}

/// `query` does not need the allowlist: it answers on a host with none.
#[test]
fn query_does_not_read_the_allowlist() {
    let fx = Fx::new("host-query-nocfg");
    fs::remove_file(&fx.host.config).unwrap();
    let runner = FakeRunner::new(vec![ok("un  \n"), exec(3, "inactive\n", "")]);
    let answer = fx.run(&runner, "query");
    assert_eq!(answer.code, 0);
    assert_eq!(
        answer.json(),
        json!({"installed_version": null, "running_commit": null, "in_progress": false})
    );
    assert_eq!(runner.calls(), vec![plan::installed_version(), plan::units_active()]);
    assert_eq!(fx.audit_lines()[0]["verb"], "query");
    assert_eq!(fx.audit_lines()[0]["outcome"], "query");
}

fn kbf_json(extra: &str) -> String {
    format!("{{\"profile\":\"release\",\"version\":\"0.1.0\"{extra}}}\n")
}

/// `query` on an installed host: the version, the commit the binary reports, and whether
/// an install unit is active.
#[test]
fn query_reports_version_commit_and_progress() {
    let fx = Fx::new("host-query");
    let full = "0123456789abcdef0123456789abcdef01234567";
    let runner = FakeRunner::new(vec![
        ok("ii  1.2.3-1\n"),
        ok(&kbf_json(&format!(",\"commit\":\"0123456789ab\",\"commit_full\":\"{full}\""))),
        exec(0, "", ""),
    ]);
    let answer = fx.run(&runner, "query");
    assert_eq!(
        answer.json(),
        json!({"installed_version": "1.2.3-1", "running_commit": full, "in_progress": true})
    );
    assert_eq!(
        runner.calls(),
        vec![plan::installed_version(), plan::running_build(), plan::units_active()]
    );
}

/// The running commit falls back to the short commit, and is null when the build did not
/// know it, when the binary fails or prints something else, or when it is not text.
#[test]
fn the_running_commit_is_best_effort() {
    for (what, reply, want) in [
        ("short only", ok(&kbf_json(",\"commit\":\"0123456789ab\"")), json!("0123456789ab")),
        ("unknown build", ok(&kbf_json(",\"commit\":\"unknown\"")), Value::Null),
        ("full unknown, short known", ok(&kbf_json(",\"commit\":\"abc\",\"commit_full\":\"\"")), json!("abc")),
        ("no commit field", ok(&kbf_json("")), Value::Null),
        ("commit is a number", ok(&kbf_json(",\"commit\":7")), Value::Null),
        ("not json", ok("kbf 0.1.0\n"), Value::Null),
        ("json but not an object", ok("[1]\n"), Value::Null),
        ("exit 1", exec(1, &kbf_json(",\"commit\":\"abc\""), "usage"), Value::Null),
        ("cannot start", Err(std::io::Error::other("no such file")), Value::Null),
    ] {
        let fx = Fx::new("host-commit");
        let runner = FakeRunner::new(vec![ok("ii  1.2.3-1\n"), reply, exec(3, "", "")]);
        let answer = fx.run(&runner, "query");
        assert_eq!(answer.code, 0, "{what}");
        assert_eq!(answer.json()["running_commit"], want, "{what}");
    }
}

/// In progress is true when the unit is active, or when the dpkg lock is held, and false
/// when neither; the lock is read from the lock table, so no lock is taken.
#[test]
fn in_progress_is_the_unit_or_the_lock() {
    let fx = Fx::new("host-progress");
    write(&fx.host.dpkg_lock, 0o644, "");
    let meta = fs::metadata(&fx.host.dpkg_lock).unwrap();
    let dev = meta.dev();
    let line = format!(
        "1: POSIX  ADVISORY  WRITE 812 {:02x}:{:02x}:{} 0 EOF\n",
        rustix::fs::major(dev),
        rustix::fs::minor(dev),
        meta.ino()
    );
    let ask = |unit: i32| {
        let runner = FakeRunner::new(vec![ok("ii  1.2.3-1\n"), exec(1, "", ""), exec(unit, "", "")]);
        fx.run(&runner, "query").json()["in_progress"].as_bool().unwrap()
    };
    write(&fx.host.proc_locks, 0o644, "");
    assert!(!ask(3), "neither");
    assert!(ask(0), "the unit alone");
    write(&fx.host.proc_locks, 0o644, &line);
    assert!(ask(3), "the lock alone");
    assert!(ask(0), "both");
}

/// A read that cannot be trusted is a failure with exit 3, not a guess: `dpkg-query` failing
/// or printing nonsense, a package state that is neither installed nor absent, a version
/// dpkg would not accept, and `systemctl` that cannot start.
#[test]
fn unreadable_state_fails() {
    type Reply = fn() -> std::io::Result<crate::runner::Exec>;
    let cases: Vec<(&str, Reply, &str)> = vec![
        ("dpkg-query exit 2", || exec(2, "", "dpkg-query: error"), "query_failed"),
        ("dpkg-query cannot start", || Err(std::io::Error::other("gone")), "query_failed"),
        ("empty output", || ok(""), "query_failed"),
        ("a status and no version", || ok("ii\n"), "query_failed"),
        ("short status", || ok("i 1.0\n"), "query_failed"),
        ("half-configured", || ok("iF  1.0-1\n"), "package_state_unclean"),
        ("half-installed", || ok("ih  1.0-1\n"), "package_state_unclean"),
        ("bad version", || ok("ii  not!a!version\n"), "installed_version_invalid"),
        ("version with a quote", || ok("ii  1.0\"1\n"), "installed_version_invalid"),
    ];
    for (what, reply, reason) in cases {
        for command in ["query", INSTALL] {
            let fx = Fx::new("host-unreadable");
            let runner = FakeRunner::new(vec![reply()]);
            let answer = fx.run(&runner, command);
            assert_eq!(answer.code, 3, "{what} / {command}");
            assert_eq!(answer.json(), json!({"result": "failed", "reason": reason}), "{what}");
            assert_eq!(runner.calls().len(), 1, "{what}: nothing else ran");
            assert_eq!(fx.audit_lines()[0]["reason"], reason);
        }
    }
    let fx = Fx::new("host-systemctl");
    let runner = FakeRunner::new(vec![ok("ii  1.0-1\n"), exec(1, "", ""), Err(std::io::Error::other("gone"))]);
    let answer = fx.run(&runner, "query");
    assert_eq!((answer.code, answer.json()["reason"].as_str()), (3, Some("query_failed")));
}

/// A failing step stops the install: the update failing means no install ran, and the
/// detail (cleaned) is on stderr, not stdout.
#[test]
fn a_failed_update_stops_the_install() {
    for reply in [exec(100, "", "E: Failed to fetch\x1b[0m\n"), Err(std::io::Error::other("systemd-run: not found"))] {
        let fx = Fx::new("host-update-fails");
        let runner = FakeRunner::new(vec![ok("ii  1.0-1\n"), reply]);
        let answer = fx.run(&runner, INSTALL);
        assert_eq!(answer.code, 3);
        assert_eq!(answer.json(), json!({"result": "failed", "reason": "update_failed"}));
        assert!(answer.err.contains("Failed to fetch") || answer.err.contains("not found"), "{}", answer.err);
        assert!(!answer.err.contains('\x1b'));
        assert_eq!(runner.calls().len(), 2, "the install step did not run");
        assert_eq!(fx.audit_lines()[0]["outcome"], "failed");
        assert_eq!(fx.audit_lines()[0]["reason"], "update_failed");
    }
}

/// The install step failing is its own code.
#[test]
fn a_failed_install_step() {
    for reply in [exec(100, "", "E: Unable to locate package kbf\n"), Err(std::io::Error::other("gone"))] {
        let fx = Fx::new("host-install-fails");
        let runner = FakeRunner::new(vec![ok("ii  1.0-1\n"), ok(""), reply]);
        let answer = fx.run(&runner, INSTALL);
        assert_eq!(answer.code, 3);
        assert_eq!(answer.json()["reason"], "install_failed");
        assert_eq!(runner.calls().len(), 3);
        assert!(!answer.err.is_empty());
    }
}

/// An install that ends well but leaves another version (or cannot be read back) is not
/// reported installed.
#[test]
fn an_install_is_verified() {
    for after in [ok("ii  1.0-1\n"), ok("un  \n"), exec(2, "", "boom"), ok("ii  1.2.3-1+b1\n")] {
        let fx = Fx::new("host-verify");
        let runner = FakeRunner::new(vec![ok("ii  1.0-1\n"), ok(""), ok(""), after]);
        let answer = fx.run(&runner, INSTALL);
        assert_eq!(answer.code, 3);
        assert_eq!(answer.json()["reason"], "verify_failed");
    }
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
    assert_eq!(answer.json(), json!({"result": "failed", "reason": "audit_unavailable"}));
    assert!(answer.err.contains("audit"), "{}", answer.err);
    assert!(runner.calls().is_empty(), "nothing ran");

    let runner = FakeRunner::new(vec![ok("un  \n"), exec(3, "", "")]);
    let answer = fx.run(&runner, "query");
    assert_eq!(answer.code, 0);
    assert!(answer.err.contains("audit"));
    let runner = FakeRunner::new(vec![]);
    let answer = fx.run(&runner, "query x");
    assert_eq!(answer.code, 2);
    assert!(answer.err.contains("audit"));
    assert_eq!(fs::read_to_string(&fx.host.audit).unwrap(), "", "the unsafe log was left alone");
}

/// The audit line is the last thing written: when it cannot be written after a run the
/// outcome stands. (A log that is full cannot be made in a test; a log removed between the
/// open and the write still accepts the write, so this checks the line exists once.)
#[test]
fn every_invocation_adds_exactly_one_line() {
    let fx = Fx::new("host-lines");
    let runner = FakeRunner::new(vec![ok("un  \n"), exec(3, "", ""), ok("ii  1.2.3-1\n"), ok("un  \n")]);
    fx.run(&runner, "query");
    fx.run(&runner, INSTALL);
    fx.run(&runner, "query x");
    assert_eq!(fx.audit_lines().len(), 3);
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
    assert_eq!(host.proc_locks, PathBuf::from("/proc/locks"));
    assert_eq!(host.dpkg_lock, PathBuf::from("/var/lib/dpkg/lock-frontend"));
    assert_eq!(host.trust.owns(&fs::metadata("/").unwrap()), fs::metadata("/").unwrap().uid() == 0);
}
