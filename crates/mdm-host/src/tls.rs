//! `mdm-host tls`: the MDM's TLS certificate from an ACME CA over DNS-01, renewed by a timer, as
//! root. lego, pinned and installed by `mdm-host install`, does the ACME work.
//!
//! It checks first, and refuses before anything changes: an ACME setting is unset or
//! malformed (`mdm_core::acme::Acme`); `MDM_ETC/dns.env` is not a 0600 file of root's holding
//! every variable the provider reads; `MDM_BIN/lego` is missing, or its `lego run --help`
//! lists no flag the unit passes. The DNS credentials are never read into this program's
//! output or any command line: the certificate unit loads `dns.env` itself
//! (`EnvironmentFile=`), so lego gets them in its environment.
//!
//! The plan has each of these only when the host differs:
//! 1. the certificate unit, its twice-a-day timer, and the path unit that restarts the proxy
//!    when the certificate changes, under `/etc/systemd/system` (root's, 0644), then
//!    `daemon-reload`;
//! 2. the timer and the path unit enabled, and restarted when their unit changed, else started
//!    when not active;
//! 3. the certificate unit started once when there is no certificate yet (the first issuance;
//!    the timer renews it after);
//! 4. in a second plan, once the certificate is there (after step 3 when it runs; a dry run
//!    that would issue shows no proxy step), the proxy enabled and started when not active.
//!
//! After an apply the certificate must be there, else the run fails (exit 1) before the proxy;
//! its subject, issuer and expiry are printed (public data, read by `openssl`).
//!
//! A refusal changes nothing only for the checks above, which run before the first plan. The
//! plan runs in two parts, because the proxy can be planned only once the certificate is
//! there, so a run can end halfway: when the certificate is missing after the issuing unit
//! ran, the units, the timer and the first issuance attempt stay in place and the proxy is not
//! started. Fix the cause (`journalctl -u` the certificate unit) and run `tls --apply` again;
//! the second run finds the rest to do.

use std::collections::BTreeSet;
use std::io::Write;

use host_step::{Account, Cmd, Kind, Mode, Planner, UnitVerb, execute};
use mdm_core::acme::Acme;
use mdm_core::config::MdmConfig;
use mdm_core::render::{UnitNames, render_tls};

use crate::install::UNITS_DIR;
use crate::{Env, MdmError, secret};

const OPENSSL: &str = "/usr/bin/openssl";
/// The flags of `lego run` the certificate unit passes.
pub const LEGO_FLAGS: [&str; 9] = [
    "--accept-tos",
    "--email",
    "--server",
    "--path",
    "--key-type",
    "--dns",
    "--domains",
    "--dns.resolvers",
    "--dns.propagation.wait",
];

/// Check, then print or carry out the units, the first issuance and the proxy's start.
pub fn run(
    env: &Env<'_>,
    c: &MdmConfig,
    acme: &Acme,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), MdmError> {
    let host = env.host;
    let euid = host.euid();
    if euid != 0 {
        return Err(MdmError::NeedsRoot { verb: "tls", euid });
    }
    let dns_env = format!("{}/dns.env", c.etc);
    let vars = acme.provider.vars();
    for var in vars {
        secret::read(host, &dns_env, var)?;
    }
    writeln!(out, "dns.env: holds {} (values not shown)", vars.join(" "))?;
    let lego = format!("{}/lego", c.bin);
    if host.stat(&lego)?.map(|m| m.kind) != Some(Kind::File) {
        return Err(MdmError::NoLego(lego));
    }
    let help = host.run(&Cmd::new(&lego).arg("run").arg("--help"))?;
    let help = help.stdout + &help.stderr;
    // Whole words: `--dns` is not there because `--dns.resolvers` is.
    let words: BTreeSet<&str> = help
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '=')
        .collect();
    if let Some(flag) = LEGO_FLAGS.into_iter().find(|f| !words.contains(f)) {
        return Err(MdmError::LegoFlag { lego, flag });
    }

    let n = UnitNames::new(c);
    let root = Account::root();
    let mut p = Planner::new(host);
    let mut changed = BTreeSet::new();
    for unit in render_tls(c, acme)? {
        let path = format!("{UNITS_DIR}/{}", unit.name);
        if p.file(&path, 0o644, &root, unit.text.as_bytes())? {
            changed.insert(unit.name);
        }
    }
    if !changed.is_empty() {
        p.unit(UnitVerb::DaemonReload, &[]);
    }
    for unit in [&n.reload_path, &n.cert_timer] {
        p.service(unit, changed.contains(unit))?;
    }
    // The proxy is planned only with its certificate there: started without it, Caddy fails
    // and restarts every 5 s. A first issuance runs in this plan, so the proxy has a plan of
    // its own after it, planned once the certificate is there.
    let crt = format!("{}/lego/certificates/{}.crt", c.state, c.dns_name);
    let issue = host.stat(&crt)?.is_none();
    if issue {
        p.unit(UnitVerb::Start, &[n.cert_service.as_str()]);
    }
    execute(&p.finish(), mode, host, out)?;
    if issue && mode == Mode::DryRun {
        writeln!(out, "proxy: started once {crt} is there")?;
        return Ok(());
    }
    if host.stat(&crt)?.is_none() {
        return Err(MdmError::NoCertificate(crt));
    }
    let mut p = Planner::new(host);
    p.service(&n.proxy, false)?;
    execute(&p.finish(), mode, host, out)?;
    if mode == Mode::DryRun {
        return Ok(());
    }
    let shown = host.run(&Cmd::new(OPENSSL).args(&[
        "x509", "-noout", "-subject", "-issuer", "-enddate", "-in", &crt,
    ]))?;
    if shown.code != 0 {
        return Err(MdmError::NotCertificate(crt));
    }
    for line in shown.stdout.lines() {
        writeln!(out, "certificate: {line}")?;
    }
    Ok(())
}
