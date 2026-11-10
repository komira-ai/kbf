//! The `kbf-coordinator` binary: its flags, its start line, its refusals and its stop.

mod support;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, ChildStdout, Command, Stdio};

use kbf_testkit::dir::TestDir;
use rustix::process::{Pid, Signal, kill_process};
use support::{OPERATOR, Pki, STATE, client_config, exchange};

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
    assert_eq!(
        words.len(),
        4,
        "no health listener without the flag: {line}"
    );
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

/// The addresses a start line names.
struct StartLine {
    heartbeat: SocketAddr,
    control: SocketAddr,
    health: SocketAddr,
}

/// Reads the start line of a coordinator started with `--health-listen`.
fn start_line(stdout: &mut BufReader<ChildStdout>) -> StartLine {
    let mut line = String::new();
    stdout.read_line(&mut line).expect("a start line");
    let addr = |key: &str| -> SocketAddr {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key} in {line:?}"))
            .parse()
            .expect("an address")
    };
    StartLine {
        heartbeat: addr("heartbeat="),
        control: addr("control="),
        health: addr("health="),
    }
}

/// Every file of `dir`, by name, with its bytes.
fn snapshot(dir: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .expect("listed")
        .map(|entry| {
            let path = entry.expect("an entry").path();
            let name = path
                .file_name()
                .expect("a name")
                .to_string_lossy()
                .into_owned();
            (name, std::fs::read(&path).expect("read"))
        })
        .collect()
}

/// The answer of a plain-HTTP `GET path` to `addr`, whole.
fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = std::net::TcpStream::connect(addr).expect("connected");
    let request = format!("GET {path} HTTP/1.1\r\nHost: health\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("sent");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("read");
    answer
}

fn pid_of(child: &Child) -> Pid {
    Pid::from_raw(i32::try_from(child.id()).expect("a pid")).expect("not zero")
}

/// Catches: two coordinators on one state directory, each ordering hosts from its own memory.
/// With one coordinator serving, a second start on the same state file, given the very
/// addresses the first is bound to, exits 1 with the lock's refusal naming the first's pid
/// (by the whole message: not "address in use", so the lock was taken before any bind), prints
/// no start line, and leaves every file of the state directory byte for byte as it was. The
/// first goes on serving its state. Once the first is killed (SIGKILL: no clean-up of its own),
/// the next start takes the lock, serves, and the lock file names it.
#[test]
fn a_second_coordinator_on_the_state_directory_is_refused_and_writes_nothing() {
    let files = Files::new("bin-lock");
    std::fs::write(files.path("desired.json"), STATE).expect("a desired state");
    let mut flags = files.flags();
    flags.extend(["--health-listen".to_owned(), "127.0.0.1:0".to_owned()]);
    let mut first = spawn(&flags);
    let mut stdout = BufReader::new(first.stdout.take().expect("stdout"));
    let bound = start_line(&mut stdout);
    let lock = files.path("desired.json.lock");
    let first_note = format!(
        "pid={} heartbeat=127.0.0.1:0 control=127.0.0.1:0\n",
        first.id()
    );
    assert_eq!(
        std::fs::read_to_string(&lock).expect("the lock file"),
        first_note
    );
    let before = snapshot(files.dir.path());

    let mut second = with(&flags, "--heartbeat-listen", &bound.heartbeat.to_string());
    second = with(&second, "--control-listen", &bound.control.to_string());
    second = with(&second, "--health-listen", &bound.health.to_string());
    let (code, out, err) = run(&second);
    assert_eq!(code, 1, "{err}");
    assert_eq!(out, "", "a refused start prints no start line");
    assert_eq!(
        err,
        format!(
            "kbf-coordinator: {lock}: another kbf-coordinator holds the lock of this state file \
             ({}); a second coordinator on one state file is refused\n",
            first_note.trim_end()
        )
    );
    assert_eq!(
        snapshot(files.dir.path()),
        before,
        "the refused start wrote nothing"
    );

    assert!(get(bound.health, "/healthz").starts_with("HTTP/1.1 200 OK\r\n"));
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let config = client_config(&files.pki, None);
    let (code, body) = runtime
        .block_on(exchange(
            bound.control,
            &config,
            "GET",
            "/v1/status",
            Some(OPERATOR),
            "",
        ))
        .expect("answered");
    assert_eq!(code, 200, "{body}");
    assert!(
        body.contains(r#""version":"2.0""#),
        "the first serves its state: {body}"
    );

    kill_process(pid_of(&first), Signal::KILL).expect("killed");
    first.wait().expect("reaped");
    let mut next = spawn(&flags);
    let mut stdout = BufReader::new(next.stdout.take().expect("stdout"));
    let bound = start_line(&mut stdout);
    assert!(get(bound.health, "/healthz").ends_with("\r\n\r\nok\n"));
    assert_eq!(
        std::fs::read_to_string(&lock).expect("the lock file"),
        format!(
            "pid={} heartbeat=127.0.0.1:0 control=127.0.0.1:0\n",
            next.id()
        )
    );
    kill_process(pid_of(&next), Signal::TERM).expect("signalled");
    assert_eq!(next.wait().expect("exited").code(), Some(0));
}

/// Catches: a health listener on an address others reach, or one the start line does not
/// name: `--health-listen` with the unspecified or a non-loopback address is a usage error
/// (exit 2, the reason on stderr) before anything is read or locked; on loopback it is named
/// as `health=` after the other two and answers `GET /healthz` with 200 `ok` and any other
/// path with 404.
#[test]
fn the_health_listener_is_loopback_only_and_answers_healthz() {
    let files = Files::new("bin-health");
    for addr in ["0.0.0.0:0", "192.0.2.1:0", "[::]:0"] {
        let mut flags = files.flags();
        flags.extend(["--health-listen".to_owned(), addr.to_owned()]);
        let (code, _, err) = run(&flags);
        assert_eq!(code, 2, "{addr}: {err}");
        assert!(err.contains("is not a loopback address"), "{err}");
    }
    assert!(
        !files.dir.path().join("desired.json.lock").exists(),
        "a usage error takes no lock"
    );

    let mut flags = files.flags();
    flags.extend(["--health-listen".to_owned(), "127.0.0.1:0".to_owned()]);
    let mut child = spawn(&flags);
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("a start line");
    let words: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(words.len(), 5, "{line}");
    let health: SocketAddr = words[4]
        .strip_prefix("health=")
        .expect("health= last")
        .parse()
        .expect("an address");
    assert!(health.ip().is_loopback() && health.port() != 0, "{health}");
    let ok = get(health, "/healthz");
    assert!(
        ok.starts_with("HTTP/1.1 200 OK\r\n") && ok.ends_with("\r\n\r\nok\n"),
        "{ok}"
    );
    let missing = get(health, "/v1/status");
    assert!(
        missing.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{missing}"
    );
    kill_process(pid_of(&child), Signal::TERM).expect("signalled");
    assert_eq!(child.wait().expect("exited").code(), Some(0));
}

/// Catches: a start that reads its files, or goes on, without the state file's lock: a state
/// file in a directory that does not exist stops the start with exit 1 and the lock file's
/// path, before anything else is looked at (the operator token file it names is missing too,
/// and the message is the lock's).
#[test]
fn a_start_whose_lock_file_cannot_be_made_is_refused_naming_it() {
    let files = Files::new("bin-nolock");
    let state = files.path("missing/desired.json");
    let flags = with(&files.flags(), "--state-file", &state);
    let flags = with(
        &flags,
        "--operator-token-file",
        &files.path("missing.token"),
    );
    let (code, out, err) = run(&flags);
    assert_eq!(code, 1, "{err}");
    assert_eq!(out, "");
    assert_eq!(
        err,
        format!("kbf-coordinator: {state}.lock: No such file or directory (os error 2)\n")
    );
}

/// Catches: a start that keeps the state file's lock, or goes on serving, when its health
/// address cannot be bound: with the address already taken it exits 1, prints no start line,
/// and the lock is free at once for the next start, which serves.
#[test]
fn a_start_whose_health_address_is_taken_exits_and_frees_the_lock() {
    let files = Files::new("bin-health-taken");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bound");
    let addr = taken.local_addr().expect("addr").to_string();
    let mut flags = files.flags();
    flags.extend(["--health-listen".to_owned(), addr]);
    let (code, out, err) = run(&flags);
    assert_eq!(code, 1, "{err}");
    assert_eq!(out, "");
    assert!(err.starts_with("kbf-coordinator: "), "{err}");
    drop(taken);

    let flags = with(&flags, "--health-listen", "127.0.0.1:0");
    let mut next = spawn(&flags);
    let mut stdout = BufReader::new(next.stdout.take().expect("stdout"));
    let bound = start_line(&mut stdout);
    assert!(get(bound.health, "/healthz").starts_with("HTTP/1.1 200 OK\r\n"));
    kill_process(pid_of(&next), Signal::TERM).expect("signalled");
    assert_eq!(next.wait().expect("exited").code(), Some(0));
}
