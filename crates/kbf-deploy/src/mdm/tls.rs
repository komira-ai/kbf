//! `mdm tls`: the MDM's TLS certificate from an ACME CA over DNS-01, renewed by a
//! timer, as root. lego, pinned and installed by `mdm install`, does the ACME work.
//!
//! It checks first, and refuses before anything changes: `MDM_ACME_EMAIL`,
//! `MDM_ACME_SERVER`, `MDM_ACME_DNS_PROVIDER`, `MDM_ACME_RESOLVERS` or
//! `MDM_ACME_PROPAGATION_WAIT` is unset or malformed ([`Acme`]);
//! `MDM_ETC/dns.env` is not a 0600 file of root's holding every variable the provider
//! reads (lego reads them from its environment); `MDM_BIN/lego` is missing, or its
//! `lego run --help` lists no flag the unit passes.
//!
//! The plan has each of these only when the host differs:
//! 1. `kbf-mdm-cert.service` (`lego run ... --dns <provider>` as `MDM_USER`, asking
//!    `MDM_ACME_RESOLVERS` and waiting `MDM_ACME_PROPAGATION_WAIT` for the record), its
//!    twice-a-day timer, and the path unit that restarts the proxy when the
//!    certificate changes, under `/etc/systemd/system`, then `daemon-reload`;
//! 2. the timer and the path unit enabled, and restarted when their unit changed, else
//!    started when not active;
//! 3. `kbf-mdm-cert.service` started once when there is no certificate yet (the first
//!    issuance; the timer renews it after);
//! 4. the proxy enabled and started when not active, once the certificate is there:
//!    when step 3 runs, in a second plan after it (a dry run shows no proxy step then).
//!
//! After an apply the certificate must be there, else the run fails before the proxy;
//! its subject, issuer and expiry are printed.
//!
//! `mdm tls --check-godaddy` only reads: it asks GoDaddy's DNS API, with the key in
//! `dns.env`, for the TXT records of `MDM_DNS_NAME`'s domain, and says whether the
//! account may use the API (GoDaddy refuses accounts below a size), so the operator
//! knows to keep `godaddy` or to point `_acme-challenge.<MDM_DNS_NAME>` by CNAME into a
//! zone another provider serves.

use std::collections::BTreeSet;
use std::fmt;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use super::MdmError;
use super::config::{KEYS, MdmConfig, MdmConfigError};
use super::install::{place, service, systemctl};
use super::render::{CERT_SERVICE, CERT_TIMER, PROXY, RELOAD_PATH, RELOAD_SERVICE};
use super::secret;
use crate::config::Config;
use crate::https::{Client, HttpsUrl};
use crate::linux::host::{self, note, under};
use crate::template::{self, Value, Values};
use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;
use kbf_host::plan::{Mode, Planner, RunAs, execute};
use kbf_host::step::{Cmd, Owner, Perms};

const UNITS: &str = "/etc/systemd/system";
const ROOT: Owner = Owner { uid: 0, gid: 0 };
/// GoDaddy's API, which `--check-godaddy` asks.
pub const GODADDY_API: &str = "https://api.godaddy.com";
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

/// The units `mdm tls` renders, in the order it installs them.
pub const TLS_UNITS: [(&str, &str); 4] = [
    (
        CERT_SERVICE,
        include_str!("../../templates/mdm/kbf-mdm-cert.service"),
    ),
    (
        CERT_TIMER,
        include_str!("../../templates/mdm/kbf-mdm-cert.timer"),
    ),
    (
        RELOAD_PATH,
        include_str!("../../templates/mdm/kbf-mdm-proxy-reload.path"),
    ),
    (
        RELOAD_SERVICE,
        include_str!("../../templates/mdm/kbf-mdm-proxy-reload.service"),
    ),
];

/// A DNS provider lego solves the DNS-01 challenge with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsProvider {
    GoDaddy,
    Route53,
    Cloudflare,
}

impl DnsProvider {
    pub fn named(name: &str) -> Option<DnsProvider> {
        match name {
            "godaddy" => Some(DnsProvider::GoDaddy),
            "route53" => Some(DnsProvider::Route53),
            "cloudflare" => Some(DnsProvider::Cloudflare),
            _ => None,
        }
    }

    /// The variables lego reads for the provider, which `dns.env` must hold.
    pub fn vars(self) -> &'static [&'static str] {
        match self {
            DnsProvider::GoDaddy => &["GODADDY_API_KEY", "GODADDY_API_SECRET"],
            DnsProvider::Route53 => &[
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_REGION",
                "AWS_HOSTED_ZONE_ID",
            ],
            DnsProvider::Cloudflare => &["CLOUDFLARE_DNS_API_TOKEN"],
        }
    }
}

impl fmt::Display for DnsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DnsProvider::GoDaddy => "godaddy",
            DnsProvider::Route53 => "route53",
            DnsProvider::Cloudflare => "cloudflare",
        })
    }
}

/// The ACME settings, which only `mdm tls` reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acme {
    /// The ACME account's contact address.
    pub email: String,
    /// The ACME directory.
    pub server: HttpsUrl,
    pub provider: DnsProvider,
    /// The nameservers lego asks about the challenge name (its zone, a CNAME), not the
    /// host's resolver (a stub there caches the record's NXDOMAIN).
    pub resolvers: Vec<SocketAddr>,
    /// How long lego waits after setting the record before the CA checks it; lego's
    /// own propagation check is off (it passed on one of the zone's servers while the
    /// CA still saw NXDOMAIN).
    pub propagation_wait: Duration,
}

impl Acme {
    pub fn load(path: &Path) -> Result<Acme, MdmError> {
        Acme::from_config(&Config::load(path, KEYS).map_err(MdmConfigError::from)?)
    }

    pub fn parse(text: &str) -> Result<Acme, MdmError> {
        Acme::from_config(&Config::parse(text, KEYS).map_err(MdmConfigError::from)?)
    }

    fn from_config(c: &Config) -> Result<Acme, MdmError> {
        let get = |key: &str| -> Result<String, MdmError> {
            Ok(c.require(&format!("MDM_{key}"))
                .map_err(MdmConfigError::from)?
                .to_owned())
        };
        let bad = |key, value: &str, why| MdmError::Acme {
            key,
            value: value.to_owned(),
            why,
        };
        // The unit's ExecStart carries both as words: no space, quote or `%` (a
        // systemd specifier). `require` has refused an empty value already.
        let word = |s: &str, more: &str| {
            s.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "._-".contains(ch) || more.contains(ch))
        };
        let email = get("ACME_EMAIL")?;
        let at = email.split_once('@');
        let ok = word(&email, "@+")
            && at.is_some_and(|(user, domain)| {
                !user.is_empty() && !domain.contains('@') && domain.contains('.')
            });
        if !ok {
            return Err(bad("ACME_EMAIL", &email, "not an email address"));
        }
        let text = get("ACME_SERVER")?;
        let server = HttpsUrl::new(&text)
            .ok()
            .filter(|_| word(&text, ":/"))
            .ok_or(bad(
                "ACME_SERVER",
                &text,
                "not an https:// URL of letters, digits and . _ - : /",
            ))?;
        let text = get("ACME_DNS_PROVIDER")?;
        let provider = DnsProvider::named(&text).ok_or(bad(
            "ACME_DNS_PROVIDER",
            &text,
            "not one of godaddy, route53, cloudflare",
        ))?;
        let text = get("ACME_RESOLVERS")?;
        let mut resolvers = Vec::new();
        for word in text.split_whitespace() {
            let addr = word.parse::<SocketAddr>().ok().filter(|a| a.port() != 0);
            let addr = addr.ok_or(bad(
                "ACME_RESOLVERS",
                &text,
                "not nameservers as IP:PORT words, like 192.0.2.53:53",
            ))?;
            resolvers.push(addr);
        }
        if resolvers.is_empty() {
            return Err(bad("ACME_RESOLVERS", &text, "no nameserver"));
        }
        let text = get("ACME_PROPAGATION_WAIT")?;
        let propagation_wait = wait(&text).ok_or(bad(
            "ACME_PROPAGATION_WAIT",
            &text,
            "not a number of seconds above 0 with an s, like 300s",
        ))?;
        Ok(Acme {
            email,
            server,
            provider,
            resolvers,
            propagation_wait,
        })
    }
}

/// Check, then print or carry out the units, the first issuance and the proxy's start.
pub fn run<E: Exec>(
    exec: &mut E,
    c: &MdmConfig,
    acme: &Acme,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), MdmError> {
    let euid = exec.euid();
    if euid != 0 {
        return Err(MdmError::NeedsRoot {
            verb: "mdm tls",
            euid,
        });
    }
    let dns_env = under(&c.etc, "dns.env")?;
    let vars = acme.provider.vars();
    for var in vars {
        secret::read(&*exec, &dns_env, var)?;
    }
    note(
        out,
        format_args!("dns.env: holds {} (not shown)", vars.join(" ")),
    )?;
    let lego = under(&c.bin, "lego")?;
    if host::stat(&*exec, &lego)?.is_none_or(|m| m.kind != Kind::File) {
        return Err(MdmError::NoLego(lego));
    }
    let help = host::query(&*exec, &Cmd::new(lego.clone()).arg("run").arg("--help"))?;
    let help =
        String::from_utf8_lossy(&help.stdout).into_owned() + &String::from_utf8_lossy(&help.stderr);
    // Whole words: `--dns` is not there because `--dns.resolvers` is.
    let words: BTreeSet<&str> = help
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '=')
        .collect();
    if let Some(flag) = LEGO_FLAGS.into_iter().find(|f| !words.contains(f)) {
        return Err(MdmError::LegoFlag { lego, flag });
    }

    let mut planner = Planner::new(&*exec);
    let dir = HostPath::new(UNITS)?;
    let values = values(c, acme);
    let mut changed = BTreeSet::new();
    for (name, text) in TLS_UNITS {
        let text = template::render(name, text, &values)?;
        let path = under(&dir, name)?;
        let placed = place(
            &mut planner,
            &path,
            Some(ROOT),
            Perms::new(0o644),
            &text,
            out,
        );
        if placed? {
            changed.insert(name);
        }
    }
    if !changed.is_empty() {
        planner.run(systemctl(&["daemon-reload"]));
    }
    for unit in [RELOAD_PATH, CERT_TIMER] {
        service(&*exec, &mut planner, unit, changed.contains(unit))?;
    }
    // The proxy is planned only with its certificate there: started without it, Caddy
    // fails and restarts every 5 s. A first issuance runs in this plan, so the proxy
    // goes in a second one, planned once the issuance has written the certificate.
    let crt = under(&c.state, &format!("lego/certificates/{}.crt", c.dns_name))?;
    let issue = host::stat(&*exec, &crt)?.is_none();
    if issue {
        planner.run(systemctl(&["start", CERT_SERVICE]));
    } else {
        service(&*exec, &mut planner, PROXY, false)?;
    }
    let plan = planner.finish();
    execute(&plan, RunAs::Root, mode, exec, &mut out)?;
    if mode == Mode::DryRun {
        if issue {
            note(out, format_args!("proxy: started once {crt} is there"))?;
        }
        return Ok(());
    }
    let Some(Entry::File { bytes, .. }) = host::inspect(&*exec, &crt)? else {
        return Err(MdmError::NoCertificate(crt));
    };
    if issue {
        let mut planner = Planner::new(&*exec);
        service(&*exec, &mut planner, PROXY, false)?;
        let plan = planner.finish();
        execute(&plan, RunAs::Root, mode, exec, &mut out)?;
    }
    let shown = x509_parser::pem::parse_x509_pem(&bytes)
        .ok()
        .and_then(|(_, pem)| {
            let cert = pem.parse_x509().ok()?;
            let v = cert.validity();
            Some(format!(
                "certificate: {crt}: subject {}, issuer {}, valid until {}",
                cert.subject(),
                cert.issuer(),
                v.not_after
            ))
        })
        .ok_or(MdmError::NotCertificate(crt.clone()))?;
    note(out, format_args!("{shown}"))?;
    Ok(())
}

fn values(c: &MdmConfig, acme: &Acme) -> Values {
    let pairs = [
        ("USER", c.user.clone()),
        ("STATE", c.state.to_string()),
        ("ETC", c.etc.to_string()),
        ("BIN", c.bin.to_string()),
        ("ACME_EMAIL", acme.email.clone()),
        ("ACME_SERVER", acme.server.to_string()),
        ("DNS_PROVIDER", acme.provider.to_string()),
        ("DNS_NAME", c.dns_name.clone()),
        ("DNS_RESOLVERS", resolvers(&acme.resolvers)),
        (
            "PROPAGATION_WAIT",
            format!("{}s", acme.propagation_wait.as_secs()),
        ),
    ];
    pairs
        .into_iter()
        .map(|(k, v)| (k, Value::Text(v)))
        .collect()
}

/// `MDM_ACME_PROPAGATION_WAIT`: whole seconds above 0, written `300s`.
fn wait(text: &str) -> Option<Duration> {
    let secs = text.strip_suffix('s')?;
    let all_digits = !secs.is_empty() && secs.bytes().all(|b| b.is_ascii_digit());
    let secs = secs.parse::<u64>().ok().filter(|s| all_digits && *s > 0)?;
    Some(Duration::from_secs(secs))
}

/// The nameservers as lego's `--dns.resolvers` takes several: comma-separated.
fn resolvers(addrs: &[SocketAddr]) -> String {
    let words: Vec<String> = addrs.iter().map(SocketAddr::to_string).collect();
    words.join(",")
}

/// The domain GoDaddy holds `name` under: its last two labels.
pub fn apex(name: &str) -> &str {
    let mut dots = name.rmatch_indices('.');
    match (dots.next(), dots.next()) {
        (Some(_), Some((i, _))) => &name[i + 1..],
        _ => name,
    }
}

/// What a status of GoDaddy's API says about the account.
pub fn godaddy_verdict(status: u16, apex: &str) -> String {
    match status {
        200 => format!(
            "GoDaddy DNS API: usable for {apex} (HTTP 200); keep MDM_ACME_DNS_PROVIDER=godaddy"
        ),
        401 => "GoDaddy DNS API: the key or the secret is wrong (HTTP 401)".to_owned(),
        403 => "GoDaddy DNS API: this account may not use it (HTTP 403; GoDaddy limits the API to larger accounts); point _acme-challenge by CNAME into a zone route53 or cloudflare serves".to_owned(),
        404 => format!("GoDaddy DNS API: {apex} is not in this account (HTTP 404)"),
        other => format!("GoDaddy DNS API: HTTP {other}, which this check does not know"),
    }
}

/// Ask GoDaddy's API (at `addr` when given) whether the key in `dns.env` may use it;
/// whether it may.
pub fn check_godaddy<E: Exec>(
    exec: &E,
    c: &MdmConfig,
    client: &Client,
    addr: Option<SocketAddr>,
    out: &mut dyn Write,
) -> Result<bool, MdmError> {
    let euid = exec.euid();
    if euid != 0 {
        return Err(MdmError::NeedsRoot {
            verb: "mdm tls --check-godaddy",
            euid,
        });
    }
    let dns_env = under(&c.etc, "dns.env")?;
    let key = secret::read(exec, &dns_env, "GODADDY_API_KEY")?;
    let secret = secret::read(exec, &dns_env, "GODADDY_API_SECRET")?;
    let apex = apex(&c.dns_name);
    let url = HttpsUrl::new(&format!("{GODADDY_API}/v1/domains/{apex}/records/TXT"))?;
    let auth = format!("sso-key {}:{}", key.expose(), secret.expose());
    let answer = client.get_with(&url, addr, &[("Authorization", &auth)], &mut Vec::new())?;
    note(
        out,
        format_args!("{}", godaddy_verdict(answer.status, apex)),
    )?;
    Ok(answer.status == 200)
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tests;
