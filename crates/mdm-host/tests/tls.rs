//! `tls` and the secret reader on the fake host.

mod common;

use common::{FakeNet, SVC, changes, config, kit, ready_host, with_env};
use host_step::fake::FakeHost;
use host_step::{Host, Mode, SYSTEMCTL};
use mdm_core::render::render_tls;
use mdm_host::tls::{LEGO_FLAGS, run};
use mdm_host::{MdmError, secret};

const DNS_ENV: &str = "/etc/js-mdm/dns.env";
const LEGO: &str = "/opt/js-mdm/bin/lego";
const CERT: &str = "/var/lib/js-mdm/lego/certificates/mdm.example.com.crt";
const TOKEN: &str = "tok-SECRET-1234";
const UNITS: &str = "/etc/systemd/system";

fn openssl_show() -> [&'static str; 8] {
    [
        "/usr/bin/openssl",
        "x509",
        "-noout",
        "-subject",
        "-issuer",
        "-enddate",
        "-in",
        CERT,
    ]
}

/// A host ready for `tls`: root, the DNS key file, lego with every flag, nothing issued.
fn tls_host() -> FakeHost {
    let host = ready_host(&kit());
    host.put_file(
        DNS_ENV,
        0o600,
        (0, 0),
        format!("CLOUDFLARE_DNS_API_TOKEN={TOKEN}\n").as_bytes(),
    );
    host.put_file(LEGO, 0o755, (0, 0), b"lego");
    host.reply(&[LEGO, "run", "--help"], 0, &LEGO_FLAGS.join("  "));
    host.reply(
        &openssl_show(),
        0,
        "subject=CN = mdm.example.com\nissuer=CN = Example CA\nnotAfter=Jan  1 00:00:00 2027 GMT\n",
    );
    host
}

fn go(host: &FakeHost, mode: Mode) -> (Result<(), MdmError>, String) {
    let net = FakeNet::new(Default::default());
    let mut out = Vec::new();
    let result = with_env(host, &net, |env| {
        run(env, &config(), &common::acme(), mode, &mut out)
    });
    (result, String::from_utf8(out).unwrap())
}

fn issue_on_start(host: &FakeHost) {
    host.effect(
        &[SYSTEMCTL, "start", "--", "js-mdm-cert.service"],
        CERT,
        0o644,
        (SVC, SVC),
        b"CERT",
    );
}

/// Catches a dry run that acts or shows the DNS key: it lists the unit writes, the timer and
/// the first issuance, says the proxy waits for the certificate, runs only `lego run --help`,
/// and the key never appears in output or on a command line.
#[test]
fn a_dry_run_lists_the_units_and_the_first_issuance_and_shows_no_key() {
    let host = tls_host();
    let before = host.paths();
    let (result, shown) = go(&host, Mode::DryRun);
    result.unwrap();
    assert_eq!(host.paths(), before);
    assert_eq!(changes(&host), [format!("{LEGO} run --help")]);
    for line in [
        "dns.env: holds CLOUDFLARE_DNS_API_TOKEN (values not shown)",
        "DRY-RUN: write /etc/systemd/system/js-mdm-cert.service mode 0644 owner root:root (",
        "DRY-RUN: write /etc/systemd/system/js-mdm-cert.timer mode 0644 owner root:root (",
        "DRY-RUN: write /etc/systemd/system/js-mdm-proxy-reload.path mode 0644 owner root:root (",
        "DRY-RUN: systemctl daemon-reload",
        "DRY-RUN: systemctl enable -- js-mdm-proxy-reload.path",
        "DRY-RUN: systemctl restart -- js-mdm-cert.timer",
        "DRY-RUN: systemctl start -- js-mdm-cert.service",
        &format!("proxy: started once {CERT} is there"),
    ] {
        assert!(shown.contains(line), "missing {line:?} in\n{shown}");
    }
    assert!(!shown.contains(TOKEN));
    assert!(host.calls().iter().all(|c| !c.join(" ").contains(TOKEN)));
}

/// Catches an apply that writes units unlike the core's render, forgets the first issuance, or
/// starts the proxy before the certificate exists; the certificate it prints is public data.
#[test]
fn apply_writes_the_core_units_issues_once_then_starts_the_proxy() {
    let host = tls_host();
    issue_on_start(&host);
    let (result, shown) = go(&host, Mode::Apply);
    result.unwrap();
    for unit in render_tls(&config(), &common::acme()).unwrap() {
        let n = host.node(&format!("{UNITS}/{}", unit.name)).unwrap();
        assert_eq!((n.mode, n.uid, n.gid), (0o644, 0, 0), "{}", unit.name);
        assert_eq!(
            n.bytes,
            unit.text.as_bytes(),
            "{} differs from the render",
            unit.name
        );
    }
    let ran = changes(&host);
    let issue = ran
        .iter()
        .position(|c| c.ends_with("start -- js-mdm-cert.service"))
        .unwrap();
    let proxy = ran
        .iter()
        .position(|c| c.ends_with("start -- js-mdm-proxy.service"))
        .unwrap();
    assert!(issue < proxy, "{ran:?}");
    assert!(
        shown.contains("certificate: subject=CN = mdm.example.com\n"),
        "{shown}"
    );
    assert!(shown.contains("certificate: issuer=CN = Example CA\n"));
    assert!(!shown.contains(TOKEN));
    assert!(host.calls().iter().all(|c| !c.join(" ").contains(TOKEN)));
}

/// Catches a rerun that issues again or touches the units: with the certificate there and the
/// units, timer and proxy running, it finds nothing to do (twice, once per plan) and still
/// shows the certificate.
#[test]
fn a_rerun_with_a_certificate_finds_nothing_to_do() {
    let host = tls_host();
    issue_on_start(&host);
    go(&host, Mode::Apply).0.unwrap();
    for unit in [
        "js-mdm-proxy-reload.path",
        "js-mdm-cert.timer",
        "js-mdm-proxy.service",
    ] {
        host.reply(&[SYSTEMCTL, "is-enabled", unit], 0, "enabled\n");
        host.reply(&[SYSTEMCTL, "is-active", unit], 0, "active\n");
    }
    host.clear_calls();
    let (result, shown) = go(&host, Mode::Apply);
    result.unwrap();
    assert_eq!(shown.matches("nothing to do\n").count(), 2, "{shown}");
    assert!(shown.contains("certificate: subject="));
    assert_eq!(
        changes(&host),
        [format!("{LEGO} run --help"), openssl_show().join(" ")]
    );
}

/// Catches a dry run with a certificate that skips the proxy plan: it shows the proxy start
/// and stops (nothing is shown of the certificate, since nothing was applied).
#[test]
fn a_dry_run_with_a_certificate_plans_the_proxy() {
    let host = tls_host();
    host.put_file(CERT, 0o644, (SVC, SVC), b"CERT");
    let (result, shown) = go(&host, Mode::DryRun);
    result.unwrap();
    assert!(
        shown.contains("DRY-RUN: systemctl start -- js-mdm-proxy.service"),
        "{shown}"
    );
    assert!(!shown.contains("certificate:"));
    assert!(!shown.contains("started once"));
}

/// Catches an issuance that "succeeds" with no certificate (the run must fail before the
/// proxy), and a certificate openssl cannot read; both exit 1.
#[test]
fn a_missing_or_unreadable_certificate_fails_the_run() {
    let host = tls_host();
    let (result, _) = go(&host, Mode::Apply);
    let err = result.unwrap_err();
    assert!(matches!(err, MdmError::NoCertificate(_)), "{err}");
    assert_eq!(err.exit_code(), 1);
    assert!(
        !changes(&host)
            .iter()
            .any(|c| c.ends_with("js-mdm-proxy.service"))
    );

    let host = tls_host();
    issue_on_start(&host);
    host.reply(&openssl_show(), 1, "");
    let err = go(&host, Mode::Apply).0.unwrap_err();
    assert!(matches!(err, MdmError::NotCertificate(_)), "{err}");
    assert_eq!(err.exit_code(), 1);
}

/// Catches each refusal that must precede any change: not root, a DNS key file that is missing,
/// too open, lacking a variable or holding a control character, lego missing, or a lego
/// without a flag the unit passes (a flag that is only a prefix of another is not found).
#[test]
fn the_checks_refuse_before_anything_changes() {
    let refused = |host: &FakeHost| {
        let before = host.paths();
        let err = go(host, Mode::Apply).0.unwrap_err();
        assert_eq!(host.paths(), before, "changed the host");
        err
    };

    let err = refused(&FakeHost::new(1000));
    assert!(
        matches!(err, MdmError::NeedsRoot { euid: 1000, .. }),
        "{err}"
    );
    assert_eq!(err.exit_code(), 2);

    let host = tls_host();
    Host::remove(&host, DNS_ENV).unwrap();
    let err = refused(&host);
    assert!(matches!(err, MdmError::NoSecret { .. }), "{err}");

    let host = tls_host();
    host.put_file(DNS_ENV, 0o644, (0, 0), b"CLOUDFLARE_DNS_API_TOKEN=x\n");
    assert!(matches!(
        refused(&host),
        MdmError::SecretMode { mode: 0o644, .. }
    ));

    let host = tls_host();
    host.put_file(
        DNS_ENV,
        0o600,
        (0, 0),
        b"OTHER=x\nCLOUDFLARE_DNS_API_TOKEN=\n",
    );
    assert!(matches!(refused(&host), MdmError::NoSecret { .. }));

    let host = tls_host();
    host.put_file(DNS_ENV, 0o600, (0, 0), b"CLOUDFLARE_DNS_API_TOKEN=a\x07b\n");
    let err = refused(&host);
    assert!(matches!(err, MdmError::SecretValue { .. }), "{err}");
    assert!(!err.to_string().contains('\u{7}'));

    let host = tls_host();
    Host::remove(&host, LEGO).unwrap();
    let err = refused(&host);
    assert_eq!(
        err.to_string(),
        format!("{LEGO} is missing: mdm-host install --apply installs it")
    );

    let host = tls_host();
    let without_dns: Vec<&str> = LEGO_FLAGS
        .iter()
        .copied()
        .filter(|f| *f != "--dns")
        .collect();
    host.reply(&[LEGO, "run", "--help"], 0, &without_dns.join(", "));
    let err = refused(&host);
    assert!(
        matches!(err, MdmError::LegoFlag { flag: "--dns", .. }),
        "{err}"
    );
    assert_eq!(err.exit_code(), 2);
}

/// Catches a secret reader that prints, or accepts, what it should not: the value is exposed
/// only by `expose`, `Debug` hides it, and a file that is not 0600 root's is refused.
#[test]
fn a_secret_is_read_from_a_root_only_file_and_never_shown() {
    let host = FakeHost::new(0);
    host.put_file("/s", 0o600, (0, 0), b"A=1\nKEY=value\n");
    let s = secret::read(&host, "/s", "KEY").unwrap();
    assert_eq!(s.expose(), "value");
    assert_eq!(format!("{s:?}"), "Secret(..)");
    assert!(matches!(
        secret::read(&host, "/s", "NOPE"),
        Err(MdmError::NoSecret { .. })
    ));
    host.put_file("/s", 0o600, (5, 0), b"KEY=value\n");
    assert!(matches!(
        secret::read(&host, "/s", "KEY"),
        Err(MdmError::SecretMode { uid: 5, .. })
    ));
    assert!(secret::has_value(b"KEY=v\n", "KEY"));
    assert!(!secret::has_value(b"KEY=\nOTHER=v\n", "KEY"));
}
