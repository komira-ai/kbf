//! The `kbf-coordinator` binary: its flags, its start line, its refusals and its stop.

mod support;

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};

use kbf_testkit::dir::TestDir;
use rustix::process::{Pid, Signal, kill_process};
use support::{OPERATOR, Pki, client_config, exchange};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-coordinator");

/// The files the flags name, in a directory of their own.
struct Files {
    dir: TestDir,
    pki: Pki,
}

impl Files {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let pki = Pki::new();
        let leaf = pki.coordinator();
        let put = |file: &str, text: &str, mode: u32| {
            let path = dir.path().join(file);
            std::fs::write(&path, text).expect("written");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
        };
        put("tls.pem", &leaf.cert_pem(), 0o644);
        put("tls.key", &leaf.key_pem(), 0o600);
        put("ca.pem", &pki.root_pem(), 0o644);
        put(
            "oidc.toml",
            &kbf_oidc::testkit::config_text("https://token.example"),
            0o644,
        );
        put("operator.token", OPERATOR, 0o600);
        put(
            "services.toml",
            "[[service]]\nname = \"other\"\n[service.roles.daemon]\n",
            0o600,
        );
        Self { dir, pki }
    }

    fn path(&self, file: &str) -> String {
        self.dir.path().join(file).display().to_string()
    }

    fn flags(&self) -> Vec<String> {
        let mut flags = vec!["--heartbeat-listen".to_owned(), "127.0.0.1:0".to_owned()];
        for (flag, value) in [
            ("--control-listen", "127.0.0.1:0".to_owned()),
            ("--tls-cert", self.path("tls.pem")),
            ("--tls-key", self.path("tls.key")),
            ("--client-ca", self.path("ca.pem")),
            ("--state-file", self.path("desired.json")),
            ("--oidc-config", self.path("oidc.toml")),
            ("--operator-token-file", self.path("operator.token")),
        ] {
            flags.push(flag.to_owned());
            flags.push(value);
        }
        flags
    }
}

fn spawn(flags: &[String]) -> Child {
    Command::new(BIN)
        .args(flags)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawned")
}

/// How long a run that should end at once may take before it is taken to have started
/// serving instead: a bound on a hang, not a measure of speed.
const RUN_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

/// Runs the binary to its end: the exit code, stdout and stderr. A binary still running
/// after [`RUN_BOUND`] (one that started instead of refusing) is stopped and the test
/// fails saying so, rather than waiting for ever.
fn run(flags: &[String]) -> (i32, String, String) {
    let child = spawn(flags);
    let pid = Pid::from_raw(i32::try_from(child.id()).expect("a pid")).expect("not zero");
    let (sent, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || sent.send(child.wait_with_output()));
    let Ok(out) = ended.recv_timeout(RUN_BOUND) else {
        kill_process(pid, Signal::KILL).expect("stopped");
        panic!("the coordinator did not exit with {flags:?}: it started instead of refusing");
    };
    let out = out.expect("ran");
    (
        out.status.code().expect("an exit code"),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn with(flags: &[String], flag: &str, value: &str) -> Vec<String> {
    let mut flags = flags.to_vec();
    let at = flags.iter().position(|f| f == flag).expect("a flag");
    flags[at + 1] = value.to_owned();
    flags
}

/// Catches: a binary that does not serve what it announces: it prints one start line with
/// both bound addresses, answers the operator's status over TLS, and a SIGTERM stops it
/// with exit 0.
#[test]
fn it_announces_its_addresses_serves_and_stops_on_sigterm() {
    let files = Files::new("bin-serve");
    let mut flags = files.flags();
    flags.extend(["--services-file".to_owned(), files.path("services.toml")]);
    let mut child = spawn(&flags);
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("a start line");
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words[0], "kbf-coordinator", "{line}");
    assert_eq!(words[1], env!("CARGO_PKG_VERSION"));
    let control = words[3]
        .strip_prefix("control=")
        .expect("control=")
        .parse()
        .expect("addr");
    assert!(words[2].starts_with("heartbeat=127.0.0.1:"));

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let config = client_config(&files.pki, None);
    let (code, body) = runtime
        .block_on(exchange(
            control,
            &config,
            "GET",
            "/v1/status",
            Some(OPERATOR),
            "",
        ))
        .expect("answered");
    assert_eq!(code, 200, "{body}");
    assert!(body.contains(r#""hosts":[]"#), "{body}");

    let pid = Pid::from_raw(i32::try_from(child.id()).expect("a pid")).expect("not zero");
    kill_process(pid, Signal::TERM).expect("signalled");
    assert_eq!(child.wait().expect("exited").code(), Some(0));
}

/// Catches: the flags not being the interface: `--version` and `--help` exit 0, no flags and
/// an unknown flag exit 2 with a usage text, a tls reload of zero seconds is refused.
#[test]
fn the_flags_are_parsed_and_refused_as_the_usage_says() {
    let (code, out, _) = run(&["--version".to_owned()]);
    assert_eq!(code, 0);
    assert!(out.starts_with("kbf-coordinator "), "{out}");
    let (code, out, _) = run(&["--help".to_owned()]);
    assert_eq!(code, 0);
    assert!(
        out.contains("--heartbeat-listen") && out.contains("--oidc-config"),
        "{out}"
    );
    let (code, _, err) = run(&[]);
    assert_eq!(code, 2);
    assert!(err.contains("--heartbeat-listen"), "{err}");
    let files = Files::new("bin-flags");
    let mut flags = files.flags();
    flags.push("--no-such-flag".to_owned());
    assert_eq!(run(&flags).0, 2);
    let mut flags = files.flags();
    flags.extend(["--tls-reload-secs".to_owned(), "0".to_owned()]);
    assert_eq!(run(&flags).0, 2);
}

/// Catches: `--version` not naming the commit the binary was built from, which is what an
/// image carrying the binary is checked against: the line is `kbf-coordinator
/// <package version>+<12 hex digits of the commit>` (or `unknown` for a source archive).
#[test]
fn the_version_names_the_commit_it_was_built_from() {
    let (code, out, _) = run(&["--version".to_owned()]);
    assert_eq!(code, 0);
    let line = out.trim_end();
    let rest = line
        .strip_prefix("kbf-coordinator ")
        .unwrap_or_else(|| panic!("not `kbf-coordinator <version>`: {line:?}"));
    let (package, stamp) = rest
        .split_once('+')
        .unwrap_or_else(|| panic!("no `+<commit>` after the package version: {line:?}"));
    assert_eq!(package, env!("CARGO_PKG_VERSION"), "{line:?}");
    let hex = stamp.len() == 12
        && stamp
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    assert!(hex || stamp == "unknown", "the stamp is {stamp:?}");
}

/// Catches: a start that goes on with a file it cannot use: a missing certificate, an
/// operator token file others can read, a bad allow-list, an allow-list entry naming a
/// service that is not defined, a state file that is not a state, and an address in use each
/// stop it with exit 1 and a reason that names the file or flag.
#[test]
fn a_start_with_a_bad_file_or_address_is_refused_with_a_reason() {
    let files = Files::new("bin-refused");
    let flags = files.flags();
    let missing = files.path("nope.pem");
    let (code, _, err) = run(&with(&flags, "--tls-cert", &missing));
    assert_eq!(code, 1);
    assert!(err.contains("nope.pem"), "{err}");

    let open = files.path("open.token");
    std::fs::write(&open, OPERATOR).expect("written");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).expect("mode");
    let (code, _, err) = run(&with(&flags, "--operator-token-file", &open));
    assert_eq!(code, 1);
    assert!(err.contains("--operator-token-file"), "{err}");

    let bad = files.path("bad.toml");
    std::fs::write(&bad, "[remote_operator]\n").expect("written");
    let (code, _, err) = run(&with(&flags, "--oidc-config", &bad));
    assert_eq!(code, 1);
    assert!(err.contains("remote_operator"), "{err}");

    let mut with_services = flags.clone();
    with_services.extend(["--services-file".to_owned(), files.path("nope.toml")]);
    let (code, _, err) = run(&with_services);
    assert_eq!(code, 1);
    assert!(
        err.contains("--services-file") && err.contains("nope.toml"),
        "{err}"
    );

    let unknown = files.path("unknown.toml");
    let text = kbf_oidc::testkit::config_text("https://token.example");
    std::fs::write(&unknown, format!("{text}services = [\"kbf\", \"nope\"]\n")).expect("written");
    let (code, _, err) = run(&with(&flags, "--oidc-config", &unknown));
    assert_eq!(code, 1);
    assert!(
        err.contains("--oidc-config") && err.contains("nope"),
        "{err}"
    );

    let state = files.path("state.json");
    std::fs::write(&state, "{}").expect("written");
    let (code, _, err) = run(&with(&flags, "--state-file", &state));
    assert_eq!(code, 1);
    assert!(err.contains("state.json"), "{err}");

    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bound");
    let addr = taken.local_addr().expect("addr").to_string();
    let (code, _, err) = run(&with(&flags, "--heartbeat-listen", &addr));
    assert_eq!(code, 1);
    assert!(err.starts_with("kbf-coordinator: "), "{err}");
    drop(taken);
}

/// Catches: a start over an ops file this build cannot honour: a file with a key it does not
/// know, or with a newer schema, at the path `--ops-file` names or, with no flag, at the state
/// file's name with `.ops` added, stops it with exit 1 and a reason that names the flag and
/// the file; and the file is left as it was.
#[test]
fn an_ops_file_this_build_cannot_read_stops_the_start() {
    let files = Files::new("bin-ops");
    let flags = files.flags();
    let given = files.path("given.ops");
    std::fs::write(&given, r#"{"schema":1,"future":true}"#).expect("written");
    let mut explicit = flags.clone();
    explicit.extend(["--ops-file".to_owned(), given.clone()]);
    let (code, _, err) = run(&explicit);
    assert_eq!(code, 1);
    assert!(
        err.contains("--ops-file") && err.contains("given.ops"),
        "{err}"
    );

    let newer = r#"{"schema":2,"services":{}}"#;
    let default = files.path("desired.json.ops");
    std::fs::write(&default, newer).expect("written");
    let (code, _, err) = run(&flags);
    assert_eq!(code, 1);
    assert!(
        err.contains("--ops-file") && err.contains("desired.json.ops"),
        "{err}"
    );
    assert!(err.contains("newer"), "{err}");
    assert_eq!(std::fs::read_to_string(&default).expect("read"), newer);
}
