//! Where the frontend's REAPI listener is, read from the frontend's own configuration so a
//! host needs no second place that names it: `/etc/kbf/kbf.toml` (`[listener] listen`,
//! `[tls] reapi-tls-cert`), then the flags of the frontend's systemd unit, which override
//! the file as a flag overrides its key.
//!
//! The address to dial is the listener's own, with an unspecified address (`0.0.0.0`,
//! `[::]`) read as loopback. A listener bound to one address is dialled there. A listener
//! with a certificate is `https://`, verified under the certificate's first DNS name (its
//! common name when it has none), because the connection goes to an address and the
//! certificate is for the farm's name. No config and no unit mean nothing derived.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use x509_parser::extensions::GeneralName;

/// The default of the frontend's `--listen`.
const DEFAULT_LISTEN: &str = "127.0.0.1:8980";

/// Where the frontend's REAPI listener is, as its own config says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derived {
    pub url: String,
    /// The name the listener's certificate carries; `Some` exactly when `url` is https.
    pub server_name: Option<String>,
}

#[derive(Default)]
struct Settings {
    listen: Option<String>,
    cert: Option<PathBuf>,
}

/// # Errors
/// A file that cannot be read or parsed, a `listen` that is not an address, a certificate
/// that cannot be read or names nothing.
pub fn derive(toml: &Path, units: &[PathBuf]) -> Result<Option<Derived>, String> {
    let mut found = false;
    let mut settings = Settings::default();
    if let Some(text) = read(toml)? {
        found = true;
        settings = from_toml(&text, toml)?;
    }
    for unit in units {
        let Some(text) = read(unit)? else { continue };
        let Some(flags) = from_unit(&text) else {
            continue;
        };
        found = true;
        settings.listen = flags.listen.or(settings.listen);
        settings.cert = flags.cert.or(settings.cert);
        break;
    }
    if !found {
        return Ok(None);
    }
    let listen = settings.listen.as_deref().unwrap_or(DEFAULT_LISTEN);
    let addr: SocketAddr = listen
        .parse()
        .map_err(|_| format!("listen {listen:?} is not an address"))?;
    let host = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let dial = SocketAddr::new(host, addr.port());
    Ok(Some(match settings.cert {
        None => Derived {
            url: format!("http://{dial}"),
            server_name: None,
        },
        Some(cert) => Derived {
            url: format!("https://{dial}"),
            server_name: Some(certificate_name(&cert)?),
        },
    }))
}

fn read(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn from_toml(text: &str, path: &Path) -> Result<Settings, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let get = |t: &str, k: &str| {
        table
            .get(t)
            .and_then(|t| t.get(k))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    Ok(Settings {
        listen: get("listener", "listen"),
        cert: get("tls", "reapi-tls-cert").map(PathBuf::from),
    })
}

/// The flags of the first `ExecStart` that sets `--listen`; none for a unit that does not
/// (a storage node's).
fn from_unit(text: &str) -> Option<Settings> {
    let joined = text.replace("\\\n", " ");
    let line = joined
        .lines()
        .find(|l| l.trim_start().starts_with("ExecStart="))?;
    let mut settings = Settings::default();
    let mut words = line.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let (flag, value) = match word.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (word, None),
        };
        let mut take = || value.clone().or_else(|| words.next().map(str::to_owned));
        match flag {
            "--listen" => settings.listen = take(),
            "--reapi-tls-cert" => settings.cert = take().map(PathBuf::from),
            _ => {}
        }
    }
    settings.listen.is_some().then_some(settings)
}

/// The first DNS name of the leaf certificate in `path`, else its common name.
fn certificate_name(path: &Path) -> Result<String, String> {
    let shown = |why: &str| format!("reapi-tls-cert {}: {why}", path.display());
    let bytes = std::fs::read(path).map_err(|e| shown(&e.to_string()))?;
    let (_, pem) =
        x509_parser::pem::parse_x509_pem(&bytes).map_err(|_| shown("holds no PEM certificate"))?;
    let cert = pem
        .parse_x509()
        .map_err(|_| shown("is not an X.509 certificate"))?;
    let dns = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .and_then(|san| {
            san.value.general_names.iter().find_map(|n| match n {
                GeneralName::DNSName(name) => Some((*name).to_owned()),
                _ => None,
            })
        });
    let common = || {
        cert.subject()
            .iter_common_name()
            .next()
            .and_then(|cn| cn.as_str().ok())
            .map(str::to_owned)
    };
    dns.or_else(common).ok_or_else(|| shown("names no host"))
}
