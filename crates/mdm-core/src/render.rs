//! The kit's systemd units and Caddyfile, rendered from the templates compiled in from
//! `templates/` with the settings ([`crate::template`]'s strict `@KEY@` rules).
//!
//! Every unit is named `<MDM_UNIT_PREFIX>-<part>.<kind>` and the slice is `MDM_SLICE`
//! ([`UnitNames`]). The render is a pure function of the settings: the same settings
//! give the same bytes, which [`manifest`] hashes for comparison with an installed host.

use sha2::{Digest, Sha256};

use crate::acme::Acme;
use crate::config::MdmConfig;
use crate::template::{self, TemplateError, Values};

const SLICE: &str = include_str!("../templates/slice");
const SCEP: &str = include_str!("../templates/scep.service");
const NANOHUB: &str = include_str!("../templates/nanohub.service");
const DEPSERVER: &str = include_str!("../templates/depserver.service");
const DEPSYNCER: &str = include_str!("../templates/depsyncer.service");
const PROXY: &str = include_str!("../templates/proxy.service");
const CERT_SERVICE: &str = include_str!("../templates/cert.service");
const CERT_TIMER: &str = include_str!("../templates/cert.timer");
const RELOAD_PATH: &str = include_str!("../templates/proxy-reload.path");
const RELOAD_SERVICE: &str = include_str!("../templates/proxy-reload.service");
const CADDYFILE: &str = include_str!("../templates/Caddyfile");

/// The name the Caddyfile is installed under, in `MDM_ETC`.
pub const CADDYFILE_NAME: &str = "Caddyfile";

/// The unit names of one kit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitNames {
    /// The slice that caps the services (`MDM_SLICE`).
    pub slice: String,
    /// The SCEP server.
    pub scep: String,
    /// NanoHUB.
    pub nanohub: String,
    /// NanoDEP's depserver.
    pub depserver: String,
    /// NanoDEP's depsyncer.
    pub depsyncer: String,
    /// The HTTPS proxy.
    pub proxy: String,
    /// The certificate job.
    pub cert_service: String,
    /// The certificate job's timer.
    pub cert_timer: String,
    /// The path unit that restarts the proxy when the certificate changes.
    pub reload_path: String,
    /// The service that restart runs.
    pub reload_service: String,
}

impl UnitNames {
    /// The names for the settings `c`.
    pub fn new(c: &MdmConfig) -> UnitNames {
        let p = &c.unit_prefix;
        UnitNames {
            slice: c.slice.clone(),
            scep: format!("{p}-scep.service"),
            nanohub: format!("{p}-nanohub.service"),
            depserver: format!("{p}-depserver.service"),
            depsyncer: format!("{p}-depsyncer.service"),
            proxy: format!("{p}-proxy.service"),
            cert_service: format!("{p}-cert.service"),
            cert_timer: format!("{p}-cert.timer"),
            reload_path: format!("{p}-proxy-reload.path"),
            reload_service: format!("{p}-proxy-reload.service"),
        }
    }
}

/// A file to install: its name and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// The file name.
    pub name: String,
    /// The text.
    pub text: String,
}

/// The files the install verb renders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// The slice and the five services, in the order they are installed.
    pub units: Vec<Unit>,
    /// The Caddyfile, installed as [`CADDYFILE_NAME`].
    pub caddyfile: String,
}

impl Rendered {
    /// The units and then the Caddyfile (as [`CADDYFILE_NAME`]), for [`manifest`].
    pub fn files(&self) -> Vec<Unit> {
        let mut files = self.units.clone();
        files.push(Unit {
            name: CADDYFILE_NAME.to_owned(),
            text: self.caddyfile.clone(),
        });
        files
    }
}

/// The slice, the five services and the Caddyfile for the settings `c`.
pub fn render(c: &MdmConfig) -> Result<Rendered, TemplateError> {
    let n = UnitNames::new(c);
    let values = values(c);
    let parts = [
        (n.slice, SLICE),
        (n.scep, SCEP),
        (n.nanohub, NANOHUB),
        (n.depserver, DEPSERVER),
        (n.depsyncer, DEPSYNCER),
        (n.proxy, PROXY),
    ];
    let mut units = Vec::new();
    for (name, template) in parts {
        let text = template::render(&name, template, &values)?;
        units.push(Unit { name, text });
    }
    let caddyfile = template::render(CADDYFILE_NAME, CADDYFILE, &values)?;
    Ok(Rendered { units, caddyfile })
}

/// The certificate job, its timer and the proxy's reload on renewal, in the order they
/// are installed.
pub fn render_tls(c: &MdmConfig, acme: &Acme) -> Result<Vec<Unit>, TemplateError> {
    let n = UnitNames::new(c);
    let mut values = values(c);
    values.insert("ACME_EMAIL", acme.email.clone());
    values.insert("ACME_SERVER", acme.server.to_string());
    values.insert("DNS_PROVIDER", acme.provider.to_string());
    values.insert("DNS_RESOLVERS", acme.resolvers_arg());
    values.insert("PROPAGATION_WAIT", acme.propagation_wait_arg());
    let parts = [
        (n.cert_service, CERT_SERVICE),
        (n.cert_timer, CERT_TIMER),
        (n.reload_path, RELOAD_PATH),
        (n.reload_service, RELOAD_SERVICE),
    ];
    let mut units = Vec::new();
    for (name, template) in parts {
        let text = template::render(&name, template, &values)?;
        units.push(Unit { name, text });
    }
    Ok(units)
}

fn values(c: &MdmConfig) -> Values {
    let pairs = [
        ("PREFIX", c.unit_prefix.clone()),
        ("SLICE", c.slice.clone()),
        ("USER", c.user.clone()),
        ("STATE", c.state.to_string()),
        ("ETC", c.etc.to_string()),
        ("BIN", c.bin.to_string()),
        ("NANOHUB_PORT", c.nanohub_port.to_string()),
        ("SCEP_PORT", c.scep_port.to_string()),
        ("DEP_PORT", c.dep_port.to_string()),
        ("DEP_NAME", c.dep_name.clone()),
        ("LAN_CIDR", c.lan.to_string()),
        ("BIND", c.bind.to_string()),
        ("DNS_NAME", c.dns_name.clone()),
        ("MEMORY_MAX", c.memory_max.clone()),
        ("CPU_QUOTA", c.cpu_quota.clone()),
        ("TASKS_MAX", c.tasks_max.clone()),
    ];
    pairs.into_iter().collect()
}

/// Whether [`manifest`] hashes a file as it is or without its comment lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comments {
    /// The text as rendered.
    Keep,
    /// Without the lines whose first non-blank character is `#`. The comments are the
    /// only wording that differs between this crate's templates and the kbf-deploy
    /// templates they were ported from, so this is the form to compare with a host that
    /// kbf-deploy installed.
    Strip,
}

/// `text` without the lines whose first non-blank character is `#`.
pub fn strip_comments(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// One line per file, `<sha256 hex>  <name>`, in the order given: what `sha256sum`
/// prints for the same bytes, so an installed host can be compared with a render.
pub fn manifest(files: &[Unit], comments: Comments) -> String {
    let mut out = String::new();
    for file in files {
        let text = match comments {
            Comments::Keep => file.text.clone(),
            Comments::Strip => strip_comments(&file.text),
        };
        let sum = hex::encode(Sha256::digest(text.as_bytes()));
        out.push_str(&format!("{sum}  {}\n", file.name));
    }
    out
}
