//! The kit's systemd units and Caddyfile, rendered from the templates compiled in from
//! `templates/mdm/` with the settings ([`crate::template`]'s strict `@KEY@` rules).

use super::config::MdmConfig;
use crate::template::{self, TemplateError, Value, Values};

pub const SLICE: &str = "kbf-mdm.slice";
pub const SCEP: &str = "kbf-mdm-scep.service";
pub const NANOHUB: &str = "kbf-mdm-nanohub.service";
pub const DEPSERVER: &str = "kbf-mdm-depserver.service";
pub const DEPSYNCER: &str = "kbf-mdm-depsyncer.service";
pub const PROXY: &str = "kbf-mdm-proxy.service";
/// The certificate job and the proxy's reload on renewal: installed by the TLS step,
/// stopped and moved aside by `install --uninstall`.
pub const CERT_SERVICE: &str = "kbf-mdm-cert.service";
pub const CERT_TIMER: &str = "kbf-mdm-cert.timer";
pub const RELOAD_PATH: &str = "kbf-mdm-proxy-reload.path";
pub const RELOAD_SERVICE: &str = "kbf-mdm-proxy-reload.service";

/// The units `mdm install` renders, in the order it installs them.
pub const UNITS: [(&str, &str); 6] = [
    (SLICE, include_str!("../../templates/mdm/kbf-mdm.slice")),
    (
        SCEP,
        include_str!("../../templates/mdm/kbf-mdm-scep.service"),
    ),
    (
        NANOHUB,
        include_str!("../../templates/mdm/kbf-mdm-nanohub.service"),
    ),
    (
        DEPSERVER,
        include_str!("../../templates/mdm/kbf-mdm-depserver.service"),
    ),
    (
        DEPSYNCER,
        include_str!("../../templates/mdm/kbf-mdm-depsyncer.service"),
    ),
    (
        PROXY,
        include_str!("../../templates/mdm/kbf-mdm-proxy.service"),
    ),
];
pub const CADDYFILE: &str = include_str!("../../templates/mdm/Caddyfile");

/// The rendered files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// `(unit name, text)`, in [`UNITS`] order.
    pub units: Vec<(&'static str, String)>,
    pub caddyfile: String,
}

pub fn render(c: &MdmConfig) -> Result<Rendered, TemplateError> {
    let values = values(c);
    let mut units = Vec::new();
    for (name, text) in UNITS {
        units.push((name, template::render(name, text, &values)?));
    }
    let caddyfile = template::render("Caddyfile", CADDYFILE, &values)?;
    Ok(Rendered { units, caddyfile })
}

fn values(c: &MdmConfig) -> Values {
    let pairs = [
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
    pairs
        .into_iter()
        .map(|(k, v)| (k, Value::Text(v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdm::pins::Tool;
    use crate::mdm::tests::config;
    use crate::unit::UnitFile;

    /// Catches a template that gains a placeholder the kit does not set, a unit that
    /// starts a binary `mdm install` does not install (or from outside MDM_BIN), and a
    /// service outside the capped slice.
    #[test]
    fn every_template_renders_and_runs_an_installed_binary_in_the_slice() {
        let c = config();
        let r = render(&c).expect("renders");
        let installed: Vec<&str> = Tool::ALL
            .into_iter()
            .flat_map(|t| t.members("1"))
            .map(|(_, name)| name)
            .collect();
        for (name, text) in &r.units[1..] {
            let unit = UnitFile::parse(text).expect("parses");
            let program = unit.exec_start().first().copied().unwrap_or_default();
            let bin = program
                .strip_prefix("/opt/kbf-mdm/bin/")
                .unwrap_or_default();
            assert!(installed.contains(&bin), "{name}: {program}");
            assert_eq!(unit.get_in("Service", "Slice"), Some(SLICE), "{name}");
            assert_eq!(unit.get_in("Service", "User"), Some("kbf-mdm"), "{name}");
        }
        let slice = UnitFile::parse(&r.units[0].1).expect("parses");
        assert_eq!(slice.get_in("Slice", "MemoryMax"), Some("1G"));
        let proxy = UnitFile::parse(&r.units[5].1).expect("parses");
        let allow = format!("localhost {}", c.lan);
        assert_eq!(
            proxy.get_in("Service", "IPAddressAllow"),
            Some(allow.as_str())
        );
        let bind = format!("\tdefault_bind {}\n", c.bind);
        assert!(r.caddyfile.contains(&bind), "{}", r.caddyfile);
        assert!(r.caddyfile.contains("https://mdm.example.com:443 {"));
    }
}
