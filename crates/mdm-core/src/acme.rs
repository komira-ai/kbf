//! The ACME settings, which only the TLS verb reads: the contact address, the ACME
//! directory, the DNS provider lego solves the DNS-01 challenge with, the nameservers
//! it asks and how long it waits for the record.

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use crate::config::{ConfigError, KEYS, Reader, bad, check};
use crate::https::HttpsUrl;
use crate::settings::Settings;

/// A DNS provider lego solves the DNS-01 challenge with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsProvider {
    /// GoDaddy.
    GoDaddy,
    /// Amazon Route 53.
    Route53,
    /// Cloudflare.
    Cloudflare,
}

impl DnsProvider {
    /// The provider called `name` in the settings, if there is one.
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

/// The ACME settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acme {
    /// The ACME account's contact address.
    pub email: String,
    /// The ACME directory.
    pub server: HttpsUrl,
    /// The DNS provider.
    pub provider: DnsProvider,
    /// The nameservers lego asks about the challenge name, not the host's resolver (a
    /// stub there caches the record's NXDOMAIN).
    pub resolvers: Vec<SocketAddr>,
    /// How long lego waits after setting the record before the CA checks it.
    pub propagation_wait: Duration,
}

impl Acme {
    /// Check the ACME settings in `text`, a settings file of the same keys as
    /// [`crate::config::MdmConfig::parse`].
    pub fn parse(text: &str) -> Result<Acme, ConfigError> {
        let settings = Settings::parse(text, KEYS)?;
        let r = Reader(&settings);
        // The unit's ExecStart carries each as a word: no space, quote or `%` (a
        // systemd specifier).
        let word = |s: &str, more: &str| {
            s.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "._-".contains(ch) || more.contains(ch))
        };
        let email = r.get("ACME_EMAIL")?;
        let at = email.split_once('@');
        let ok = word(&email, "@+")
            && !email.starts_with('-')
            && at.is_some_and(|(user, domain)| {
                !user.is_empty() && !domain.contains('@') && domain.contains('.')
            });
        check("ACME_EMAIL", &email, ok, "not an email address")?;
        let text = r.get("ACME_SERVER")?;
        let server = HttpsUrl::new(&text).ok().filter(|_| word(&text, ":/"));
        let server = server.ok_or(bad(
            "ACME_SERVER",
            &text,
            "not an https:// URL of letters, digits and . _ - : /",
        ))?;
        let text = r.get("ACME_DNS_PROVIDER")?;
        let provider = DnsProvider::named(&text).ok_or(bad(
            "ACME_DNS_PROVIDER",
            &text,
            "not one of godaddy, route53, cloudflare",
        ))?;
        let text = r.get("ACME_RESOLVERS")?;
        let mut resolvers = Vec::new();
        for w in text.split_whitespace() {
            // A scope id (`fe80::1%2`) prints a `%`, which systemd reads as a specifier.
            let addr = w.parse::<SocketAddr>().ok().filter(|a| a.port() != 0);
            let addr = addr.filter(|a| !a.to_string().contains('%'));
            resolvers.push(addr.ok_or(bad(
                "ACME_RESOLVERS",
                &text,
                "not nameservers as IP:PORT words, like 192.0.2.53:53",
            ))?);
        }
        check(
            "ACME_RESOLVERS",
            &text,
            !resolvers.is_empty(),
            "no nameserver",
        )?;
        let text = r.get("ACME_PROPAGATION_WAIT")?;
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

    /// The nameservers as lego's `--dns.resolvers` takes several: comma-separated.
    pub fn resolvers_arg(&self) -> String {
        let words: Vec<String> = self.resolvers.iter().map(SocketAddr::to_string).collect();
        words.join(",")
    }

    /// The wait as lego's `--dns.propagation.wait` takes it: whole seconds, `300s`.
    pub fn propagation_wait_arg(&self) -> String {
        format!("{}s", self.propagation_wait.as_secs())
    }
}

/// `MDM_ACME_PROPAGATION_WAIT`: whole seconds above 0, written `300s`.
fn wait(text: &str) -> Option<Duration> {
    let secs = text.strip_suffix('s')?;
    let all_digits = !secs.is_empty() && secs.bytes().all(|b| b.is_ascii_digit());
    let secs = secs.parse::<u64>().ok().filter(|s| all_digits && *s > 0)?;
    Some(Duration::from_secs(secs))
}
