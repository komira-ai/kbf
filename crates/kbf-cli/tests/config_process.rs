//! `kbf.toml` and `--check` in the `kbf` binary: a start from a file alone, the
//! command line over the file, the refusals of a file, and the exit status of `--check` in
//! every mode (0 fine, 2 refused, 75 not ready), with nothing started.

use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use kbf_cli::config::group_of;
use kbf_cli::names;
use kbf_deploy::linux::verify_cert::{CLIENT_AUTH, ROLE_URI_PREFIX, Row};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose as Eku, IsCa,
    KeyPair, KeyUsagePurpose, SanType, date_time_ymd,
};

const BIN: &str = env!("CARGO_BIN_EXE_kbf");
/// The stand-in `crun` the container driver's check runs `--version` on.
const FAKE_CRUN: &str = env!("CARGO_BIN_EXE_kbf-cli-fake-crun");

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("config-process-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn write(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode");
}

struct Run {
    code: Option<i32>,
    out: String,
    err: String,
}

fn run(args: &[String]) -> Run {
    run_in(args, &[])
}

/// `kbf` with these variables set in its environment, killed if it has not exited in 30
/// seconds (a start that was meant to be refused serves forever): then `code` is `None`.
fn run_in(args: &[String], env: &[(String, String)]) -> Run {
    let mut child = Command::new(BIN)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run kbf");
    let mut out = child.stdout.take().expect("stdout");
    let mut err = child.stderr.take().expect("stderr");
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut out, &mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut err, &mut text);
        text
    });
    let mut code = None;
    for _ in 0..600 {
        if let Some(status) = child.try_wait().expect("wait") {
            code = Some(status.code());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if code.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    Run {
        code: code.flatten(),
        out: out.join().expect("out"),
        err: err.join().expect("err"),
    }
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| (*s).to_owned()).collect()
}

/// The settings of one start: flag name and value.
type Settings = Vec<(&'static str, String)>;

/// `--flag=value` for each.
fn as_flags(settings: &Settings) -> Vec<String> {
    settings.iter().map(|(k, v)| format!("--{k}={v}")).collect()
}

/// The `kbf.toml` that stands for them: one table per group, each value a string.
fn as_toml(settings: &Settings) -> String {
    let mut groups: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for (k, v) in settings {
        let group = group_of(k).unwrap_or_else(|| panic!("--{k} has no group"));
        groups
            .entry(group)
            .or_default()
            .push(format!("{k} = {v:?}"));
    }
    groups
        .into_iter()
        .map(|(g, lines)| format!("[{g}]\n{}\n", lines.join("\n")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The farm's CA and the certificates it issues, as the farm's issuer makes them.
struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    fn new(dir: &Path) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key"))
            .expect("the CA");
        write(&dir.join("ca.pem"), &ca.pem(), 0o644);
        Self { ca }
    }

    /// `<stem>.pem` and `<stem>.key` in `dir`: a certificate of `row` naming `names`, valid
    /// now, or (`future`) from a date far ahead, which a clock that is behind sees.
    fn issue(&self, dir: &Path, stem: &str, row: Row, names: &[&str], future: bool) {
        let mut leaf = CertificateParams::new(Vec::<String>::new()).expect("params");
        leaf.subject_alt_names = names
            .iter()
            .map(|n| SanType::DnsName((*n).to_owned().try_into().expect("IA5")))
            .collect();
        leaf.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}{}", row.role())
                .try_into()
                .expect("IA5"),
        ));
        leaf.is_ca = IsCa::ExplicitNoCa;
        leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf.extended_key_usages = row
            .ekus()
            .iter()
            .map(|e| {
                if *e == CLIENT_AUTH {
                    Eku::ClientAuth
                } else {
                    Eku::ServerAuth
                }
            })
            .collect();
        leaf.use_authority_key_identifier_extension = true;
        if future {
            leaf.not_before = date_time_ymd(2999, 1, 1);
            leaf.not_after = date_time_ymd(3999, 1, 1);
        }
        let key = KeyPair::generate().expect("key");
        let cert = leaf.signed_by(&key, &self.ca).expect("sign");
        write(&dir.join(format!("{stem}.pem")), &cert.pem(), 0o644);
        write(
            &dir.join(format!("{stem}.key")),
            &key.serialize_pem(),
            0o600,
        );
    }
}

/// The environment of a host with what the container driver looks for: a private
/// runtime directory (`--oci-runtime` names the stand-in `crun`, in `settings`).
fn host_env(dir: &Path) -> Vec<(String, String)> {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let xdg = dir.join("xdg");
    std::fs::create_dir_all(&xdg).expect("xdg");
    std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700)).expect("mode");
    vec![
        (
            "PATH".to_owned(),
            format!("{}:/usr/bin:/bin", bin.display()),
        ),
        ("XDG_RUNTIME_DIR".to_owned(), xdg.display().to_string()),
    ]
}

/// How a start fails its checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Host {
    /// Everything it needs is there.
    Fine,
    /// A file it needs is missing.
    Missing,
    /// A certificate is not valid yet.
    Early,
}

/// The settings of `mode` on a host that is `host`, under `dir`.
fn settings(mode: &str, dir: &Path, pki: &Pki, host: Host) -> Settings {
    let p = |name: &str| dir.join(name).display().to_string();
    let early = host == Host::Early;
    let node = |s: &mut Settings| {
        pki.issue(dir, "node", Row::Daemon, &["n"], early);
        let ca = if host == Host::Missing {
            p("no-such-ca.pem")
        } else {
            p("ca.pem")
        };
        s.extend([
            ("driver", "container".to_owned()),
            ("oci-runtime", FAKE_CRUN.to_owned()),
            ("server", "https://127.0.0.1:1".to_owned()),
            ("ca-cert", ca),
            ("cert", p("node.pem")),
            ("key", p("node.key")),
            ("node-id", "n".to_owned()),
            ("data-dir", p("data")),
        ]);
    };
    let mut s: Settings = Vec::new();
    match mode {
        names::DAEMON | names::ALL_IN_ONE => node(&mut s),
        names::STORAGE_AND_DAEMON => {
            node(&mut s);
            s.extend([
                ("storage-listen", "127.0.0.1:0".to_owned()),
                ("meta-dir", p("meta")),
            ]);
        }
        names::STORAGE => {
            s.push(("storage-listen", "127.0.0.1:0".to_owned()));
            match host {
                Host::Fine => {}
                Host::Missing => s.extend([
                    ("storage-tls-cert", p("no-such.pem")),
                    ("storage-tls-key", p("no-such.key")),
                    ("storage-client-ca", p("no-such-ca.pem")),
                ]),
                Host::Early => {
                    pki.issue(dir, "storage", Row::Storage, &["127.0.0.1"], true);
                    s.extend([
                        ("storage-tls-cert", p("storage.pem")),
                        ("storage-tls-key", p("storage.key")),
                        ("storage-client-ca", p("ca.pem")),
                    ]);
                }
            }
        }
        names::FRONTEND => {
            s.push(("storage", "http://127.0.0.1:1".to_owned()));
            match host {
                Host::Fine => {}
                Host::Missing => s.push(("reapi-auth-policy", p("no-such-policy.json"))),
                Host::Early => {
                    pki.issue(dir, "frontend", Row::FrontendWorker, &["127.0.0.1"], true);
                    s.extend([
                        ("worker-tls-cert", p("frontend.pem")),
                        ("worker-tls-key", p("frontend.key")),
                        ("worker-client-ca", p("ca.pem")),
                    ]);
                }
            }
        }
        other => panic!("{other}"),
    }
    s
}

/// Catches `--check` that does not report the three outcomes of a start, in any mode, or
/// that starts something: for every mode, on a host that is fine, lacks a file or has a
/// certificate that is not valid yet, the exit status is 0, 2 and 75, standard output
/// says `ok` only for the first, standard error names the problem for the others, and the
/// directories a start would make are not made. The same settings are given as flags and
/// as a `kbf.toml`, which exit the same and print the same.
#[test]
fn check_exits_0_2_or_75_in_every_mode_and_starts_nothing() {
    for mode in names::MODES {
        for (host, want) in [(Host::Fine, 0), (Host::Missing, 2), (Host::Early, 75)] {
            let dir = scratch(&format!("check-{}-{host:?}", mode.name));
            let pki = Pki::new(&dir);
            let s = settings(mode.name, &dir, &pki, host);
            let mut flags = vec![mode.name.to_owned(), "--check".to_owned()];
            flags.extend(as_flags(&s));
            let env = host_env(&dir);
            let by_flags = run_in(&flags, &env);
            let toml = dir.join("kbf.toml");
            write(&toml, &as_toml(&s), 0o600);
            let by_file = run_in(
                &[
                    mode.name.to_owned(),
                    "--check".to_owned(),
                    format!("--config={}", toml.display()),
                ],
                &env,
            );
            for (how, r) in [("flags", &by_flags), ("file", &by_file)] {
                let at = format!("{} on a {host:?} host, by {how}: {}", mode.name, r.err);
                assert_eq!(r.code, Some(want), "{at}");
                match host {
                    Host::Fine => {
                        assert_eq!(r.out, format!("kbf {}: ok\n", mode.name), "{at}");
                        assert!(
                            r.err.lines().all(|l| l.contains("warning:")),
                            "a problem on a fine host: {at}"
                        );
                    }
                    Host::Missing => {
                        assert!(r.out.is_empty(), "{at}");
                        assert!(
                            r.err
                                .lines()
                                .any(|l| l.starts_with(&format!("kbf {}: ", mode.name))
                                    && l.contains("does not exist")),
                            "{at}"
                        );
                    }
                    Host::Early => {
                        assert!(r.out.is_empty(), "{at}");
                        assert!(r.err.contains("is not valid yet"), "{at}");
                    }
                }
                for made in ["data", "meta"] {
                    assert!(!dir.join(made).exists(), "{at}: --check made {made}");
                }
            }
            assert_eq!(
                by_flags.err, by_file.err,
                "{} {host:?}: the file says what the flags say",
                mode.name
            );
        }
    }
}

/// Catches `--check` that exits 0 for a command line a start refuses (it must parse
/// first): a mode without its required flags and a flag of another mode exit 2 with
/// clap's message, and the problem of the file is the one printed.
#[test]
fn check_refuses_what_a_start_refuses() {
    let r = run(&strings(&["daemon", "--check"]));
    assert_eq!(r.code, Some(2), "{}", r.err);
    assert!(
        r.err.contains("required arguments were not provided"),
        "{}",
        r.err
    );
    assert!(r.out.is_empty());
    let r = run(&strings(&["daemon", "--check", "--cold-store=file:///x"]));
    assert_eq!(r.code, Some(2), "{}", r.err);
    assert!(
        r.err.contains("kbf daemon refuses --cold-store"),
        "{}",
        r.err
    );
    let dir = scratch("check-bad-file");
    let toml = dir.join("kbf.toml");
    write(&toml, "[listener]\nbogus = 1\n", 0o600);
    let r = run(&[
        "storage".to_owned(),
        "--check".to_owned(),
        format!("--config={}", toml.display()),
    ]);
    assert_eq!(r.code, Some(2), "{}", r.err);
    assert!(
        r.err.contains("`listener.bogus`") && r.err.contains(&toml.display().to_string()),
        "{}",
        r.err
    );
}

/// Catches `kbf check-args` changed from what `kbf-deploy` calls: it still parses the
/// flags and exits 0, with nothing on stdout and none of the host's checks (a certificate
/// that is not there is fine), and it says on stderr, in one line, that it is deprecated
/// and what to use. Exit 2 is kept for flags a start refuses.
#[test]
fn check_args_still_parses_and_says_it_is_deprecated() {
    let ok = run(&strings(&[
        "check-args",
        "daemon",
        "--driver=container",
        "--server=https://127.0.0.1:1",
        "--ca-cert=/nonexistent/ca.pem",
        "--cert=/nonexistent/node.pem",
        "--key=/nonexistent/node.key",
        "--node-id=n",
    ]));
    assert_eq!(ok.code, Some(0), "{}", ok.err);
    assert!(ok.out.is_empty());
    assert_eq!(ok.err.lines().count(), 1, "{}", ok.err);
    assert!(
        ok.err.contains("deprecated") && ok.err.contains("--check"),
        "{}",
        ok.err
    );
    let bad = run(&strings(&["check-args", "daemon", "--bogus-flag"]));
    assert_eq!(bad.code, Some(2));
    // The refusal is the first line, which `kbf-deploy` reports; the note is the last.
    assert!(
        bad.err
            .lines()
            .next()
            .is_some_and(|l| l.contains("unexpected argument '--bogus-flag'")),
        "{}",
        bad.err
    );
    assert!(
        bad.err
            .lines()
            .last()
            .is_some_and(|l| l.contains("deprecated")),
        "{}",
        bad.err
    );
}

/// A running `kbf storage`, killed when dropped.
struct Node {
    child: std::process::Child,
    line: String,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts `kbf storage` with `args` and waits for its start line.
fn start_storage(args: &[String]) -> Node {
    let mut child = Command::new(BIN)
        .arg("storage")
        .args(args)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn kbf storage");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("a start line");
    Node { child, line }
}

/// Catches a file that `kbf <mode>` does not apply at a real start: `kbf storage` given
/// nothing but `--config` serves on the file's listener and over the file's stores, which
/// it makes (the start line names the listener it bound: loopback port 0 in the file,
/// so a port that is not 0 shows the file was read).
#[test]
fn a_storage_node_starts_from_its_file_alone() {
    let dir = scratch("start-file-only");
    let toml = dir.join("kbf.toml");
    let s: Settings = vec![
        ("storage-listen", "127.0.0.1:0".to_owned()),
        (
            "cold-store",
            format!("file://{}", dir.join("cold").display()),
        ),
        ("meta-dir", dir.join("meta").display().to_string()),
    ];
    write(&toml, &as_toml(&s), 0o600);
    let node = start_storage(&[format!("--config={}", toml.display())]);
    assert!(node.line.starts_with("kbf-storage "), "{}", node.line);
    let addr = node
        .line
        .split_whitespace()
        .find_map(|w| w.strip_prefix("storage="))
        .unwrap_or_else(|| panic!("no storage= in {:?}", node.line));
    assert!(
        addr.starts_with("127.0.0.1:") && !addr.ends_with(":0"),
        "{addr}"
    );
    assert!(
        dir.join("meta").exists(),
        "the file's --meta-dir was not used"
    );
}

/// Catches the file's key winning at a real start: the file's listener is a privileged
/// port a test cannot bind, so only a command line that replaces it starts.
#[test]
fn a_flag_replaces_the_files_key_at_a_real_start() {
    let dir = scratch("start-override");
    let toml = dir.join("kbf.toml");
    let s: Settings = vec![
        ("storage-listen", "127.0.0.1:1".to_owned()),
        (
            "cold-store",
            format!("file://{}", dir.join("cold").display()),
        ),
        ("meta-dir", dir.join("meta").display().to_string()),
    ];
    write(&toml, &as_toml(&s), 0o600);
    let node = start_storage(&[
        format!("--config={}", toml.display()),
        "--storage-listen=127.0.0.1:0".to_owned(),
    ]);
    assert!(node.line.starts_with("kbf-storage "), "{}", node.line);
}

/// Catches a start that goes on past a file it should refuse: an unknown key and a file
/// writable by group each exit 2 naming the file, print nothing on stdout and make
/// nothing (the metadata directory stays absent).
#[test]
fn a_start_with_a_bad_file_exits_2_and_makes_nothing() {
    let dir = scratch("start-bad-file");
    let meta = dir.join("meta");
    let base = format!(
        "[stores]\ncold-store = \"file://{}\"\nmeta-dir = \"{}\"\n\n[listener]\nstorage-listen = \"127.0.0.1:0\"\n",
        dir.join("cold").display(),
        meta.display()
    );
    let toml = dir.join("kbf.toml");
    for (text, mode, want) in [
        (
            format!("{base}typo = 1\n"),
            0o600,
            "unknown key `listener.typo`",
        ),
        (base.clone(), 0o660, "writable by group or other"),
        (base.clone(), 0o602, "writable by group or other"),
    ] {
        write(&toml, &text, mode);
        let r = run(&["storage".to_owned(), format!("--config={}", toml.display())]);
        assert_eq!(r.code, Some(2), "{want}: {}", r.err);
        assert!(r.out.is_empty(), "{want}: {}", r.out);
        assert!(
            r.err.contains(want) && r.err.contains(&toml.display().to_string()),
            "{}",
            r.err
        );
        assert!(
            !meta.exists(),
            "{want}: a refused start made its metadata directory"
        );
    }
}
