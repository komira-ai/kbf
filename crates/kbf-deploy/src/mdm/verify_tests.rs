use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::rc::Rc;
use std::time::Duration;

use rcgen::{CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair};

use super::*;
use crate::mdm::tests::{config, lan};
use crate::testkit::{TlsServer, http, tls_server};
use kbf_host::exec::Entry;
use kbf_host::step::Owner;
use kbf_host::testkit::{FakeExec, reply};

const UID: u32 = 990;
const ALL: usize = 28;

/// What the fake host's tools and the proxy answer.
struct World {
    active: BTreeSet<String>,
    listening: String,
    ss_fails: bool,
    resolves: Result<String, ()>,
    user: i32,
    /// The proxy's answer by path.
    answers: BTreeMap<&'static str, Vec<u8>>,
}

fn entry(kind: Kind, perms: u32, uid: u32, bytes: &[u8]) -> Entry {
    let owner = Owner { uid, gid: uid };
    let perms = Perms::new(perms);
    match kind {
        Kind::Dir => Entry::Dir { perms, owner },
        _ => Entry::File {
            perms,
            owner,
            bytes: bytes.to_vec(),
        },
    }
}

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

/// A certificate for the MDM's name whose issuer's organisation is `org`, valid until
/// 2030-01-01, as PEM.
fn lego_cert(org: &str) -> String {
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca.distinguished_name.push(DnType::OrganizationName, org);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    let mut leaf = CertificateParams::new(vec!["mdm.example.com".to_owned()]).expect("params");
    leaf.not_after = rcgen::date_time_ymd(2030, 1, 1);
    leaf.signed_by(&KeyPair::generate().expect("key"), &ca)
        .expect("signed")
        .pem()
}

/// 2029-12-01, a month before the certificate ends.
fn december() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_890_777_600)
}

/// A healthy MDM: every check passes.
fn host() -> (FakeExec, Rc<RefCell<World>>) {
    let mut exec = FakeExec::new(0);
    let units = [SCEP, NANOHUB, DEPSERVER, PROXY, CERT_TIMER, RELOAD_PATH];
    let bind = lan(10);
    let mut scep = CertificateParams::new(Vec::<String>::new()).expect("params");
    scep.distinguished_name = rcgen::DistinguishedName::new();
    scep.distinguished_name
        .push(DnType::CommonName, "kbf-mdm SCEP CA");
    let ca_der = scep
        .self_signed(&KeyPair::generate().expect("key"))
        .expect("CA")
        .der()
        .to_vec();
    let world = Rc::new(RefCell::new(World {
        active: units.iter().map(|u| (*u).to_owned()).collect(),
        listening: format!(
            "LISTEN 0 4096 127.0.0.1:9004 0.0.0.0:*\nLISTEN 0 4096 127.0.0.1:9005 0.0.0.0:*\n\
             LISTEN 0 4096 127.0.0.1:9001 0.0.0.0:*\nLISTEN 0 4096 {bind}:443 0.0.0.0:*\n\
             LISTEN 0 128 0.0.0.0:22 0.0.0.0:*\n"
        ),
        ss_fails: false,
        resolves: Ok(format!(
            "{bind}      STREAM mdm.example.com\n{bind}      DGRAM\n"
        )),
        user: 0,
        answers: BTreeMap::from([
            ("/version", http("200 OK", b"{\"version\":\"v0.2.0\"}\n")),
            ("/scep?operation=GetCACert", http("200 OK", &ca_der)),
            ("/api/v1/nanomdm/push/x", http("404 Not Found", b"")),
            ("/v1/tokenpki/x", http("404 Not Found", b"")),
            ("/enroll", http("200 OK", b"<plist/>")),
        ]),
    }));
    let w = Rc::clone(&world);
    exec.answer = Box::new(move |cmd| {
        let w = w.borrow();
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        match (cmd.program.as_str(), &args[..]) {
            (GETENT, ["passwd", "--", name]) => Ok(reply(
                w.user,
                &format!("{name}:x:{UID}:{UID}::/var/lib/kbf-mdm:/usr/sbin/nologin\n"),
                "",
            )),
            (GETENT, ["ahostsv4", ..]) => match &w.resolves {
                Ok(text) => Ok(reply(0, text, "")),
                Err(()) => Err(io::Error::from(io::ErrorKind::NotFound)),
            },
            (SS, _) => Ok(reply(i32::from(w.ss_fails), &w.listening, "no netlink")),
            (_, ["is-active", "--quiet", u]) => {
                Ok(reply(i32::from(!w.active.contains(*u)), "", ""))
            }
            // id -G
            _ => Ok(reply(0, &format!("{UID}\n"), "")),
        }
    });
    let root = |e: &mut FakeExec, path: &str, kind, perms, bytes: &[u8]| {
        e.entries.insert(p(path), entry(kind, perms, 0, bytes));
    };
    for dir in ["/etc", "/opt", "/var", "/var/lib"] {
        root(&mut exec, dir, Kind::Dir, 0o755, b"");
    }
    exec.entries
        .insert(p("/var/lib/kbf-mdm"), entry(Kind::Dir, 0o700, UID, b""));
    root(&mut exec, "/etc/kbf-mdm", Kind::Dir, 0o750, b"");
    for f in SECRET_FILES {
        root(
            &mut exec,
            &format!("/etc/kbf-mdm/{f}"),
            Kind::File,
            0o600,
            b"K=v\n",
        );
    }
    root(
        &mut exec,
        "/etc/kbf-mdm/erase_allowed_signers",
        Kind::File,
        0o644,
        b"op sk-ssh-ed25519 AAAA\n",
    );
    exec.entries.insert(
        p("/var/lib/kbf-mdm/push-topic"),
        entry(Kind::File, 0o600, UID, b"com.apple.mgmt.x\n"),
    );
    let crt = "/var/lib/kbf-mdm/lego/certificates/mdm.example.com.crt";
    let pem = lego_cert("Let's Encrypt");
    exec.entries
        .insert(p(crt), entry(Kind::File, 0o600, UID, pem.as_bytes()));
    (exec, world)
}

/// The proxy, answering from `world`.
fn proxy(world: &Rc<RefCell<World>>) -> TlsServer {
    let answers = world.borrow().answers.clone();
    tls_server(&["mdm.example.com"], move |path| {
        answers
            .get(path)
            .cloned()
            .unwrap_or_else(|| http("500 Oops", b""))
    })
}

/// The names of the checks that failed.
fn failed(exec: &FakeExec, server: &TlsServer, now: SystemTime) -> Vec<String> {
    let c = config();
    let client = server.client();
    let v = Verify {
        config: &c,
        client: &client,
        proxy: server.addr,
        now,
    };
    let rows = checks(exec, &v).expect("checks run");
    assert_eq!(rows.len(), ALL);
    rows.into_iter()
        .filter(|r| r.result.is_err())
        .map(|r| r.name)
        .collect()
}

/// Catches a check that fails on a healthy MDM (an operator stops trusting `verify`),
/// and a summary that miscounts.
#[test]
fn a_healthy_mdm_passes_every_check() {
    let (exec, world) = host();
    let server = proxy(&world);
    assert_eq!(failed(&exec, &server, december()), Vec::<String>::new());
    let c = config();
    let client = server.client();
    let v = Verify {
        config: &c,
        client: &client,
        proxy: server.addr,
        now: december(),
    };
    let mut out = Vec::new();
    assert!(run(&exec, &v, &mut out).expect("runs"));
    let text = String::from_utf8_lossy(&out).into_owned();
    assert_eq!(text.lines().count(), ALL + 1, "{text}");
    assert!(
        text.contains("ok    https://mdm.example.com/version answers: {\"version\":\"v0.2.0\"}\n"),
        "{text}"
    );
    assert!(
        text.contains("ok    https://mdm.example.com/scep returns the SCEP CA: CN=kbf-mdm SCEP CA"),
        "{text}"
    );
    assert!(
        text.contains("ok    the certificate is valid for 21 more days: 31 days left"),
        "{text}"
    );
    assert!(text.ends_with("verify: all checks passed\n"), "{text}");
}

/// The planted mutant: a secret file others can read (0644) must fail its own check,
/// and only that one. A `verify` that compares the mode loosely (any of the owner's
/// bits, or "no more than") passes it.
#[test]
fn verify_fails_a_0644_secret_file() {
    let (mut exec, world) = host();
    let server = proxy(&world);
    exec.entries.insert(
        p("/etc/kbf-mdm/nanohub.env"),
        entry(Kind::File, 0o644, 0, b"K=v\n"),
    );
    assert_eq!(
        failed(&exec, &server, december()),
        ["/etc/kbf-mdm/nanohub.env is a 0600 file of root"]
    );
    let c = config();
    let client = server.client();
    let v = Verify {
        config: &c,
        client: &client,
        proxy: server.addr,
        now: december(),
    };
    let mut out = Vec::new();
    assert!(!run(&exec, &v, &mut out).expect("runs"));
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(
        text.contains("FAIL  /etc/kbf-mdm/nanohub.env is a 0600 file of root: is a File with mode 0644, owner uid 0\n"),
        "{text}"
    );
    assert!(text.ends_with("verify: 1 check(s) failed\n"), "{text}");
}

/// Each row plants one fault on a healthy MDM; exactly the named checks must fail, so
/// every check is shown to catch its fault and to pass on everything else.
#[test]
fn each_fault_fails_exactly_its_checks() {
    type Plant = fn(&mut FakeExec, &mut World);
    let cases: Vec<(Plant, &[&str])> = vec![
        (
            |_, w| {
                w.active.remove(SCEP);
            },
            &["kbf-mdm-scep.service is active"],
        ),
        (
            |_, w| {
                w.active.remove(CERT_TIMER);
            },
            &["kbf-mdm-cert.timer is active"],
        ),
        (
            |_, w| {
                w.listening
                    .push_str("LISTEN 0 4096 0.0.0.0:9004 0.0.0.0:*\n")
            },
            &["NanoHUB listens on 127.0.0.1:9004 only"],
        ),
        (
            |_, w| w.listening = w.listening.replace("127.0.0.1:9001", "127.0.0.1:9011"),
            &["depserver listens on 127.0.0.1:9001 only"],
        ),
        (
            |_, w| w.listening.push_str("LISTEN 0 4096 [::]:80 [::]:*\n"),
            &["nothing listens on port 80"],
        ),
        (
            |_, w| w.ss_fails = true,
            &[
                "NanoHUB listens on 127.0.0.1:9004 only",
                "SCEP listens on 127.0.0.1:9005 only",
                "depserver listens on 127.0.0.1:9001 only",
                "the proxy listens on PROXY only",
                "nothing listens on port 80",
            ],
        ),
        (
            |_, w| drop(w.answers.insert("/version", http("200 OK", b"hello"))),
            &["https://mdm.example.com/version answers"],
        ),
        (
            |_, w| {
                drop(
                    w.answers
                        .insert("/scep?operation=GetCACert", http("200 OK", b"not DER")),
                )
            },
            &["https://mdm.example.com/scep returns the SCEP CA"],
        ),
        (
            |_, w| {
                drop(
                    w.answers
                        .insert("/scep?operation=GetCACert", http("503 Down", b"")),
                )
            },
            &["https://mdm.example.com/scep returns the SCEP CA"],
        ),
        (
            |_, w| {
                drop(
                    w.answers
                        .insert("/api/v1/nanomdm/push/x", http("200 OK", b"{}")),
                )
            },
            &["NanoHUB's API is not served (https://mdm.example.com/api/v1/nanomdm/push/x: 404)"],
        ),
        (
            |_, w| drop(w.answers.remove("/enroll")),
            &["the enrollment profile is served (https://mdm.example.com/enroll: 200)"],
        ),
        (
            |e, _| {
                let pem = lego_cert("(STAGING) Pretend Pear X1");
                let crt = p("/var/lib/kbf-mdm/lego/certificates/mdm.example.com.crt");
                e.entries
                    .insert(crt, entry(Kind::File, 0o600, UID, pem.as_bytes()));
            },
            &["the certificate is issued by Let's Encrypt"],
        ),
        (
            |e, _| {
                drop(
                    e.entries
                        .remove(&p("/var/lib/kbf-mdm/lego/certificates/mdm.example.com.crt")),
                )
            },
            &[
                "the certificate is issued by Let's Encrypt",
                "the certificate is valid for 21 more days",
            ],
        ),
        (
            |e, _| {
                let crt = p("/var/lib/kbf-mdm/lego/certificates/mdm.example.com.crt");
                e.entries.insert(
                    crt,
                    entry(Kind::File, 0o600, UID, b"-----BEGIN JUNK-----\n"),
                );
            },
            &[
                "the certificate is issued by Let's Encrypt",
                "the certificate is valid for 21 more days",
            ],
        ),
        (
            |e, _| {
                let crt = p("/var/lib/kbf-mdm/lego/certificates/mdm.example.com.crt");
                let pem = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
                e.entries
                    .insert(crt, entry(Kind::File, 0o600, UID, pem.as_bytes()));
            },
            &[
                "the certificate is issued by Let's Encrypt",
                "the certificate is valid for 21 more days",
            ],
        ),
        (
            |_, w| w.resolves = Ok("192.0.2.7      STREAM mdm.example.com\n".to_owned()),
            &["mdm.example.com resolves to BIND here"],
        ),
        (
            |_, w| w.resolves = Ok(String::new()),
            &["mdm.example.com resolves to BIND here"],
        ),
        (
            |_, w| w.resolves = Err(()),
            &["mdm.example.com resolves to BIND here"],
        ),
        (
            |_, w| {
                w.listening = w
                    .listening
                    .replace(&format!("{}:443", lan(10)), "0.0.0.0:443")
            },
            &["the proxy listens on PROXY only"],
        ),
        (
            |e, _| {
                let state = entry(Kind::File, 0o700, UID, b"");
                e.entries.insert(p("/var/lib/kbf-mdm"), state);
            },
            &["/var/lib/kbf-mdm is a 0700 directory of kbf-mdm"],
        ),
        (
            |e, _| {
                drop(
                    e.entries
                        .insert(p("/var/lib/kbf-mdm"), entry(Kind::Dir, 0o755, UID, b"")),
                )
            },
            &["/var/lib/kbf-mdm is a 0700 directory of kbf-mdm"],
        ),
        (
            |_, w| w.user = 2,
            &["/var/lib/kbf-mdm is a 0700 directory of kbf-mdm"],
        ),
        (
            |_, w| w.user = 1,
            &["/var/lib/kbf-mdm is a 0700 directory of kbf-mdm"],
        ),
        (
            |e, _| {
                drop(
                    e.entries
                        .insert(p("/etc/kbf-mdm"), entry(Kind::Dir, 0o750, 501, b"")),
                )
            },
            &[
                "/etc/kbf-mdm is a 0750 directory of root",
                "the erase signer and every directory above it are root's alone",
            ],
        ),
        (
            |e, _| {
                drop(e.entries.insert(
                    p("/etc/kbf-mdm/scep.env"),
                    entry(Kind::File, 0o600, 501, b"K=v\n"),
                ))
            },
            &["/etc/kbf-mdm/scep.env is a 0600 file of root"],
        ),
        (
            |e, _| drop(e.entries.remove(&p("/etc/kbf-mdm/dns.env"))),
            &["/etc/kbf-mdm/dns.env is a 0600 file of root"],
        ),
        (
            |e, _| {
                drop(e.entries.insert(
                    p("/var/lib/kbf-mdm/push-topic"),
                    entry(Kind::File, 0o600, UID, b""),
                ))
            },
            &["the push topic is recorded"],
        ),
        (
            |e, _| drop(e.entries.remove(&p("/etc/kbf-mdm/erase_allowed_signers"))),
            &[
                "the erase signer is installed",
                "the erase signer and every directory above it are root's alone",
            ],
        ),
        (
            |e, _| drop(e.entries.insert(p("/etc"), entry(Kind::Dir, 0o775, 0, b""))),
            &["the erase signer and every directory above it are root's alone"],
        ),
        (
            |e, _| {
                drop(
                    e.entries
                        .insert(p("/etc/kbf-mdm/erase_allowed_signers"), Entry::Other),
                )
            },
            &[
                "the erase signer is installed",
                "the erase signer and every directory above it are root's alone",
            ],
        ),
    ];
    let bind = lan(10).to_string();
    for (plant, want) in cases {
        let (mut exec, world) = host();
        plant(&mut exec, &mut world.borrow_mut());
        let server = proxy(&world);
        let want: Vec<String> = want
            .iter()
            .map(|w| {
                w.replace("PROXY", &format!("{bind}:443"))
                    .replace("BIND", &bind)
            })
            .collect();
        assert_eq!(failed(&exec, &server, december()), want);
    }
}

/// Catches an expiry check that counts from the wrong end, and a proxy that cannot
/// be reached passing any of the HTTPS checks.
#[test]
fn a_late_certificate_and_an_unreachable_proxy_fail() {
    let (exec, world) = host();
    let server = proxy(&world);
    // 2029-12-20: 12 days left.
    let late = december() + Duration::from_secs(19 * 86_400);
    assert_eq!(
        failed(&exec, &server, late),
        ["the certificate is valid for 21 more days"]
    );
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let gone = TlsServer {
        addr: closed.local_addr().expect("addr"),
        ca: server.ca.clone(),
        paths: std::sync::Arc::default(),
        heads: std::sync::Arc::default(),
    };
    drop(closed);
    let got = failed(&exec, &gone, december());
    assert_eq!(got.len(), 5, "{got:#?}");
    assert!(got.iter().all(|n| n.contains("https://")), "{got:#?}");
}

/// Catches checks of the state and secrets reported as passing when the host cannot be
/// read, and the note that `verify` needs root left out.
#[test]
fn unreadable_paths_fail_and_a_user_is_told_to_use_sudo() {
    let (mut exec, world) = host();
    let server = proxy(&world);
    let mut c = config();
    c.etc = p("/unreadable");
    let client = server.client();
    let v = Verify {
        config: &c,
        client: &client,
        proxy: server.addr,
        now: december(),
    };
    let rows = checks(&exec, &v).expect("checks run");
    let etc = rows
        .iter()
        .find(|r| r.name == "/unreadable is a 0750 directory of root");
    let why = etc.and_then(|r| r.result.clone().err()).unwrap_or_default();
    assert!(why.starts_with("cannot read /unreadable"), "{why}");
    exec.euid = 501;
    let mut out = Vec::new();
    assert!(!run(&exec, &v, &mut out).expect("runs"));
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(text.starts_with("NOTE: not root"), "{text}");
}
