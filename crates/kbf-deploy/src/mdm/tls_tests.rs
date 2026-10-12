use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use super::*;
use crate::linux::host::SYSTEMCTL;
use crate::mdm::push_cert::tests::{host, pair};
use crate::mdm::tests::{config, config_text};
use crate::testkit::{http, tls_server};
use crate::unit::UnitFile;
use kbf_host::step::Step;
use kbf_host::testkit::{FakeExec, reply};

const CRT: &str = "/var/lib/kbf-mdm/lego/certificates/mdm.example.com.crt";
const LEGO: &str = "/opt/kbf-mdm/bin/lego";
const HELP: &str = "USAGE: lego run [options]\n  --accept-tos\n  --email value\n  --server value\n  --path value\n  --key-type value\n  --dns value\n  --domains value\n";

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

fn file(bytes: &[u8], perms: u32, uid: u32) -> Entry {
    Entry::File {
        perms: Perms::new(perms),
        owner: Owner { uid, gid: uid },
        bytes: bytes.to_vec(),
    }
}

fn acme_text(email: &str, server: &str, provider: &str) -> String {
    config_text()
        .replace("MDM_ACME_EMAIL=\n", &format!("MDM_ACME_EMAIL={email}\n"))
        .replace(
            "MDM_ACME_SERVER=https://acme-v02.api.letsencrypt.org/directory\n",
            &format!("MDM_ACME_SERVER={server}\n"),
        )
        .replace(
            "MDM_ACME_DNS_PROVIDER=\n",
            &format!("MDM_ACME_DNS_PROVIDER={provider}\n"),
        )
}

const DIRECTORY: &str = "https://acme-v02.api.letsencrypt.org/directory";

fn acme() -> Acme {
    Acme::parse(&acme_text("ops@example.com", DIRECTORY, "godaddy")).expect("valid settings")
}

/// An installed MDM host: `dns.env` with GoDaddy's key, lego in MDM_BIN, and
/// systemctl keeping which units are enabled and active.
fn tls_host(help: &'static str) -> (FakeExec, Rc<RefCell<BTreeSet<String>>>) {
    let mut exec = host();
    exec.entries.insert(
        p("/etc/kbf-mdm/dns.env"),
        file(b"GODADDY_API_KEY=k\nGODADDY_API_SECRET=s\n", 0o600, 0),
    );
    exec.entries.insert(p(LEGO), file(b"lego", 0o755, 0));
    let units = Rc::new(RefCell::new(BTreeSet::new()));
    let u = Rc::clone(&units);
    exec.answer = Box::new(move |cmd| {
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        let mut u = u.borrow_mut();
        Ok(match (cmd.program.as_str(), &args[..]) {
            (LEGO, ["run", "--help"]) => reply(0, help, ""),
            (SYSTEMCTL, [verb @ ("is-enabled" | "is-active"), "--quiet", unit]) => {
                reply(i32::from(!u.contains(&format!("{verb} {unit}"))), "", "")
            }
            (SYSTEMCTL, ["enable", unit]) => {
                u.insert(format!("is-enabled {unit}"));
                reply(0, "", "")
            }
            (SYSTEMCTL, ["start" | "restart", unit]) => {
                u.insert(format!("is-active {unit}"));
                reply(0, "", "")
            }
            _ => reply(0, "", ""),
        })
    });
    (exec, units)
}

fn run_it(exec: &mut FakeExec, a: &Acme, mode: Mode) -> (Result<(), MdmError>, String) {
    let mut out = Vec::new();
    let got = run(exec, &config(), a, mode, &mut out);
    (got, String::from_utf8_lossy(&out).into_owned())
}

fn runs(exec: &FakeExec) -> Vec<String> {
    exec.log
        .iter()
        .filter_map(|s| match s {
            Step::Run(cmd) => Some(cmd.args.join(" ")),
            _ => None,
        })
        .collect()
}

/// Catches ACME settings that would break the certificate unit's command line or send
/// the account elsewhere: an address without a domain or with a space or `%` (a systemd
/// specifier), a plain-HTTP or odd-character directory, a provider lego is not set up
/// for here, and a key left empty.
#[test]
fn acme_settings_are_checked() {
    let a = acme();
    assert_eq!(
        (a.email.as_str(), a.server.to_string(), a.provider),
        (
            "ops@example.com",
            DIRECTORY.to_owned(),
            DnsProvider::GoDaddy
        )
    );
    for provider in [DnsProvider::Route53, DnsProvider::Cloudflare] {
        let text = acme_text("ops+mdm@example.com", DIRECTORY, &provider.to_string());
        assert_eq!(Acme::parse(&text).map(|a| a.provider).ok(), Some(provider));
    }
    let bad = [
        ("opsexample.com", DIRECTORY, "godaddy", "MDM_ACME_EMAIL"),
        ("ops@localhost", DIRECTORY, "godaddy", "MDM_ACME_EMAIL"),
        ("@example.com", DIRECTORY, "godaddy", "MDM_ACME_EMAIL"),
        ("ops@a@example.com", DIRECTORY, "godaddy", "MDM_ACME_EMAIL"),
        ("o%s@example.com", DIRECTORY, "godaddy", "MDM_ACME_EMAIL"),
        (
            "ops@example.com",
            "http://acme.example.com/dir",
            "godaddy",
            "MDM_ACME_SERVER",
        ),
        (
            "ops@example.com",
            "https://acme.example.com/d%ir",
            "godaddy",
            "MDM_ACME_SERVER",
        ),
        (
            "ops@example.com",
            DIRECTORY,
            "gandi",
            "MDM_ACME_DNS_PROVIDER",
        ),
        ("", DIRECTORY, "godaddy", "MDM_ACME_EMAIL is not set"),
    ];
    for (email, server, provider, want) in bad {
        let got = Acme::parse(&acme_text(email, server, provider)).map_err(|e| e.to_string());
        assert!(
            got.as_ref().is_err_and(|e| e.contains(want)),
            "{want}: {got:?}"
        );
    }
}

/// Catches a certificate unit that runs lego as root, passes the DNS key on argv
/// instead of through the 0600 environment file, or names another email, directory,
/// provider or name than the settings; and a reload unit that watches another file
/// than the certificate the proxy serves.
#[test]
fn the_rendered_units_run_lego_as_the_mdm_user_with_the_settings() {
    let c = config();
    let values = values(&c, &acme());
    let cert =
        UnitFile::parse(&template::render(CERT_SERVICE, TLS_UNITS[0].1, &values).expect("renders"))
            .expect("parses");
    assert_eq!(cert.get_in("Service", "User"), Some("kbf-mdm"));
    assert_eq!(
        cert.get_in("Service", "EnvironmentFile"),
        Some("/etc/kbf-mdm/dns.env")
    );
    let argv = cert.exec_start();
    let after = |flag: &str| argv.iter().position(|w| *w == flag).map(|i| argv[i + 1]);
    assert_eq!(argv[0], LEGO);
    assert_eq!(after("--email"), Some("ops@example.com"));
    assert_eq!(after("--server"), Some(DIRECTORY));
    assert_eq!(after("--dns"), Some("godaddy"));
    assert_eq!(after("--domains"), Some("mdm.example.com"));
    assert!(!argv.iter().any(|w| w.contains("GODADDY")));
    let watch =
        UnitFile::parse(&template::render(RELOAD_PATH, TLS_UNITS[2].1, &values).expect("renders"))
            .expect("parses");
    assert_eq!(watch.get_in("Path", "PathChanged"), Some(CRT));
    for (name, text) in TLS_UNITS {
        assert!(template::render(name, text, &values).is_ok(), "{name}");
    }
}

/// Catches a dry run that changes anything, and a plan that misses a step a first run
/// needs: the four units, daemon-reload, the timer and the reload path on, the first
/// issuance, then the proxy, in that order (the proxy cannot start without the
/// certificate).
#[test]
fn a_first_run_installs_the_units_issues_the_certificate_then_starts_the_proxy() {
    let (mut exec, _) = tls_host(HELP);
    let (got, out) = run_it(&mut exec, &acme(), Mode::DryRun);
    got.expect("planned");
    assert!(exec.log.is_empty());
    assert!(out.contains("dns.env: holds GODADDY_API_KEY GODADDY_API_SECRET (not shown)"));
    assert!(!out.contains("GODADDY_API_KEY=k"), "{out}");
    for unit in [CERT_SERVICE, CERT_TIMER, RELOAD_PATH, RELOAD_SERVICE] {
        assert!(
            out.contains(&format!(
                "DRY-RUN: write /etc/systemd/system/{unit} (0644, owner 0:0"
            )),
            "{unit}: {out}"
        );
    }

    let (got, out) = run_it(&mut exec, &acme(), Mode::Apply);
    let err = got.err().map(|e| (e.exit_code(), e.to_string()));
    assert!(
        err.as_ref()
            .is_some_and(|(code, e)| *code == 1 && e.contains(&format!("no certificate at {CRT}"))),
        "{err:?} {out}"
    );
    assert_eq!(
        runs(&exec),
        [
            "daemon-reload",
            "enable kbf-mdm-proxy-reload.path",
            "restart kbf-mdm-proxy-reload.path",
            "enable kbf-mdm-cert.timer",
            "restart kbf-mdm-cert.timer",
            "start kbf-mdm-cert.service",
            "enable kbf-mdm-proxy.service",
            "start kbf-mdm-proxy.service",
        ]
    );
}

/// Catches a re-run that issues again, rewrites a unit or restarts a service when
/// nothing changed, a changed unit that does not reach systemd (no daemon-reload, the
/// timer not restarted), and an apply that does not show the certificate it got.
#[test]
fn a_rerun_changes_nothing_and_a_changed_timer_is_reloaded() {
    let (mut exec, _) = tls_host(HELP);
    let _ = run_it(&mut exec, &acme(), Mode::Apply);
    let (cert, _) = pair("mdm.example.com", false);
    exec.entries
        .insert(p(CRT), file(cert.as_bytes(), 0o644, 990));
    let steps = exec.log.len();
    let (got, out) = run_it(&mut exec, &acme(), Mode::Apply);
    got.expect("re-run");
    assert_eq!(exec.log.len(), steps, "{out}");
    assert!(out.contains("nothing to do\n"), "{out}");
    assert!(
        out.contains(&format!("certificate: {CRT}: subject ")),
        "{out}"
    );

    let timer = p("/etc/systemd/system/kbf-mdm-cert.timer");
    exec.entries
        .insert(timer, file(b"[Timer]\nOnCalendar=daily\n", 0o644, 0));
    exec.log.clear();
    run_it(&mut exec, &acme(), Mode::Apply).0.expect("timer");
    assert_eq!(runs(&exec), ["daemon-reload", "restart kbf-mdm-cert.timer"]);
}

/// Catches a run that goes ahead without what lego needs: the provider's key missing
/// from `dns.env` (here route53's), a `dns.env` others can read, lego missing, a lego
/// whose `run` lacks a flag the unit passes (another major), or a user instead of
/// root.
#[test]
fn a_run_without_what_lego_needs_is_refused() {
    let route53 = Acme::parse(&acme_text("ops@example.com", DIRECTORY, "route53")).expect("valid");
    let (mut exec, _) = tls_host(HELP);
    let err = run_it(&mut exec, &route53, Mode::DryRun)
        .0
        .err()
        .map(|e| e.to_string());
    assert!(
        err.as_ref()
            .is_some_and(|e| e.contains("has no AWS_ACCESS_KEY_ID=")),
        "{err:?}"
    );

    let (mut exec, _) = tls_host(HELP);
    exec.entries.insert(
        p("/etc/kbf-mdm/dns.env"),
        file(b"GODADDY_API_KEY=k\nGODADDY_API_SECRET=s\n", 0o644, 0),
    );
    let err = run_it(&mut exec, &acme(), Mode::DryRun)
        .0
        .err()
        .map(|e| e.to_string());
    assert!(
        err.as_ref().is_some_and(|e| e.contains("not 0600")),
        "{err:?}"
    );

    let (mut exec, _) = tls_host(HELP);
    exec.entries.remove(&p(LEGO));
    let err = run_it(&mut exec, &acme(), Mode::DryRun).0.err();
    assert!(matches!(err, Some(MdmError::NoLego(_))), "{err:?}");

    let (mut exec, _) =
        tls_host("USAGE: lego run\n  --accept-tos --email --server --path --key-type --domains\n");
    let err = run_it(&mut exec, &acme(), Mode::DryRun).0.err();
    assert!(
        matches!(err, Some(MdmError::LegoFlag { flag: "--dns", .. })),
        "{err:?}"
    );

    let (mut exec, _) = tls_host(HELP);
    exec.euid = 501;
    let err = run_it(&mut exec, &acme(), Mode::DryRun).0.err();
    assert!(matches!(err, Some(MdmError::NeedsRoot { .. })), "{err:?}");
    assert!(exec.log.is_empty());
}

/// Catches a check that asks for another domain than the name's apex, does not send the
/// key and secret as GoDaddy's `sso-key` header, or reads a refusal as usable; and a
/// verdict that does not say what the operator should do for each answer.
#[test]
fn check_godaddy_asks_for_the_apex_with_the_key_and_reads_the_answer() {
    assert_eq!(apex("mdm.example.com"), "example.com");
    assert_eq!(apex("example.com"), "example.com");
    let server = tls_server(&["api.godaddy.com"], |path| {
        if path.contains("example.com") {
            http("403 Forbidden", b"{}")
        } else {
            http("200 OK", b"[]")
        }
    });
    let (exec, _) = tls_host(HELP);
    let mut out = Vec::new();
    let usable = check_godaddy(
        &exec,
        &config(),
        &server.client(),
        Some(server.addr),
        &mut out,
    );
    assert_eq!(usable.ok(), Some(false));
    assert_eq!(
        server.paths.lock().expect("lock").as_slice(),
        ["/v1/domains/example.com/records/TXT"]
    );
    let head = server.heads.lock().expect("lock")[0].clone();
    assert!(
        head.contains("\r\nAuthorization: sso-key k:s\r\n"),
        "{head}"
    );
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("may not use it (HTTP 403"), "{out}");

    let mut c = config();
    c.dns_name = "mdm.example.org".to_owned();
    let usable = check_godaddy(
        &exec,
        &c,
        &server.client(),
        Some(server.addr),
        &mut Vec::new(),
    );
    assert_eq!(usable.ok(), Some(true));
    for (status, want) in [
        (200, "usable for example.com"),
        (401, "the key or the secret is wrong"),
        (404, "example.com is not in this account"),
        (500, "HTTP 500, which this check does not know"),
    ] {
        assert!(
            godaddy_verdict(status, "example.com").contains(want),
            "{status}"
        );
    }
    let mut user = exec;
    user.euid = 501;
    let err = check_godaddy(
        &user,
        &c,
        &server.client(),
        Some(server.addr),
        &mut Vec::new(),
    );
    assert!(matches!(err, Err(MdmError::NeedsRoot { .. })));
}
