//! Shared fixtures: the example settings, archives built in memory with their pins, and a
//! fake host that looks like a machine ready for `install`.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::Ipv4Addr;

use flate2::Compression;
use flate2::write::{DeflateEncoder, GzEncoder};
use host_step::fake::FakeHost;
use host_step::{GETENT, SYSTEMCTL};
use mdm_core::acme::Acme;
use mdm_core::config::MdmConfig;
use mdm_core::pins::{Packing, Pins, Tool};
use mdm_host::{Download, Env};
use sha2::{Digest, Sha256};

/// The documented example settings as committed: every key, with `MDM_BIND` and
/// `MDM_LAN_CIDR` left empty (a public file carries no private address).
pub const EXAMPLE_FILE: &str = include_str!("../../../mdm-core/config.example");

/// The example site's bind address (host 10 of the first /24 of 10/8), built at run time
/// so that no source file spells a private address.
pub fn bind() -> String {
    Ipv4Addr::new(10, 0, 0, 10).to_string()
}

/// `text` with every `@TEST_BIND@` replaced by [`bind`].
pub fn site_addresses(text: &str) -> String {
    text.replace("@TEST_BIND@", &bind())
}

/// The documented example settings, every key set: [`EXAMPLE_FILE`] with [`bind`] and its /24.
pub fn example() -> String {
    let lan = format!("{}/24", Ipv4Addr::new(10, 0, 0, 0));
    let text = EXAMPLE_FILE
        .replace("\nMDM_BIND=\n", &format!("\nMDM_BIND={}\n", bind()))
        .replace("\nMDM_LAN_CIDR=\n", &format!("\nMDM_LAN_CIDR={lan}\n"));
    assert_ne!(text, EXAMPLE_FILE, "the example leaves the addresses empty");
    text
}

/// The example, parsed.
pub fn config() -> MdmConfig {
    MdmConfig::parse(&example()).expect("the example is valid")
}

/// The example's ACME settings.
pub fn acme() -> Acme {
    Acme::parse(&example()).expect("the example is valid")
}

/// The service user's id on the fake host.
pub const SVC: u32 = 900;
/// What the fake random source fills with.
pub const RANDOM_BYTE: u8 = 0xAB;
/// The secret value the fake random source makes: 32 bytes as hex.
pub fn random_hex() -> String {
    "ab".repeat(32)
}

/// Pieces of a zip that a test can damage.
#[derive(Clone, Default)]
pub struct Tweak {
    pub method: Option<u16>,
    pub plain: Option<u32>,
    pub packed: Option<u32>,
    pub name_len: Option<u16>,
    pub central_sig: Option<u32>,
    pub local_sig: Option<u32>,
}

fn le(out: &mut Vec<u8>, v: u64, n: usize) {
    out.extend_from_slice(&v.to_le_bytes()[..n]);
}

/// A zip of `(name, bytes)` members, deflated or stored, with damage as `tweak` says.
pub fn zip_raw(members: &[(&str, &[u8])], deflate: bool, tweak: &Tweak) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, bytes) in members {
        let (method, data) = match deflate {
            true => {
                let mut e = DeflateEncoder::new(Vec::new(), Compression::fast());
                e.write_all(bytes).unwrap();
                (8u16, e.finish().unwrap())
            }
            false => (0u16, bytes.to_vec()),
        };
        let offset = out.len() as u64;
        le(
            &mut out,
            u64::from(tweak.local_sig.unwrap_or(0x0403_4b50)),
            4,
        );
        le(&mut out, 20, 2);
        le(&mut out, 0, 2);
        le(&mut out, u64::from(method), 2);
        le(&mut out, 0, 4);
        le(&mut out, 0, 4);
        le(&mut out, data.len() as u64, 4);
        le(&mut out, bytes.len() as u64, 4);
        le(&mut out, name.len() as u64, 2);
        le(&mut out, 0, 2);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&data);

        le(
            &mut central,
            u64::from(tweak.central_sig.unwrap_or(0x0201_4b50)),
            4,
        );
        le(&mut central, 20, 2);
        le(&mut central, 20, 2);
        le(&mut central, 0, 2);
        le(&mut central, u64::from(tweak.method.unwrap_or(method)), 2);
        le(&mut central, 0, 4);
        le(&mut central, 0, 4);
        le(
            &mut central,
            u64::from(tweak.packed.unwrap_or(data.len() as u32)),
            4,
        );
        le(
            &mut central,
            u64::from(tweak.plain.unwrap_or(bytes.len() as u32)),
            4,
        );
        le(
            &mut central,
            u64::from(tweak.name_len.unwrap_or(name.len() as u16)),
            2,
        );
        le(&mut central, 0, 2);
        le(&mut central, 0, 2);
        le(&mut central, 0, 2);
        le(&mut central, 0, 2);
        le(&mut central, 0, 4);
        le(&mut central, offset, 4);
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u64;
    out.extend_from_slice(&central);
    le(&mut out, 0x0605_4b50, 4);
    le(&mut out, 0, 2);
    le(&mut out, 0, 2);
    le(&mut out, members.len() as u64, 2);
    le(&mut out, members.len() as u64, 2);
    le(&mut out, central.len() as u64, 4);
    le(&mut out, cd_offset, 4);
    le(&mut out, 0, 2);
    out
}

/// A deflated zip of `(name, bytes)` members.
pub fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
    zip_raw(members, true, &Tweak::default())
}

/// A gzip-compressed tar of `(name, bytes)` members.
pub fn tar_gz_of(members: &[(&str, &[u8])]) -> Vec<u8> {
    let gz = GzEncoder::new(Vec::new(), Compression::fast());
    let mut b = tar::Builder::new(gz);
    for (name, bytes) in members {
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o755);
        h.set_cksum();
        b.append_data(&mut h, name, *bytes).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

/// The version every test pin has.
pub const VERSION: &str = "1.0.0";

/// What a test binary holds: the tool's installed name and its bytes.
pub fn binary_bytes(name: &str) -> Vec<u8> {
    format!("binary {name} \u{1}\u{2}").into_bytes()
}

/// An archive per tool holding its members, the pins text for them, and the download map.
pub struct Kit {
    /// `archive file name -> bytes`.
    pub archives: BTreeMap<String, Vec<u8>>,
    /// `url -> bytes`.
    pub urls: BTreeMap<String, Vec<u8>>,
    pub pins: Pins,
}

pub fn kit() -> Kit {
    kit_with(|_, bytes| bytes)
}

/// A kit whose binaries are changed by `f(name, bytes)`.
pub fn kit_with(f: impl Fn(&str, Vec<u8>) -> Vec<u8>) -> Kit {
    let mut archives = BTreeMap::new();
    let mut urls = BTreeMap::new();
    let mut text = String::new();
    for tool in Tool::ALL {
        let members: Vec<(String, Vec<u8>)> = tool
            .members(VERSION)
            .into_iter()
            .map(|(member, name)| (member, f(name, binary_bytes(name))))
            .collect();
        let refs: Vec<(&str, &[u8])> = members
            .iter()
            .map(|(m, b)| (m.as_str(), b.as_slice()))
            .collect();
        let (file, bytes) = match tool {
            Tool::Caddy | Tool::Lego => (
                format!("{}-v{VERSION}.tar.gz", tool.name()),
                tar_gz_of(&refs),
            ),
            _ => (format!("{}-v{VERSION}.zip", tool.name()), zip_of(&refs)),
        };
        let url = format!(
            "https://downloads.example.org/{}/v{VERSION}/{file}",
            tool.name()
        );
        let sha = hex::encode(Sha256::digest(&bytes));
        text.push_str(&format!("{} {VERSION} {url} {sha}\n", tool.name()));
        urls.insert(url, bytes.clone());
        archives.insert(file, bytes);
    }
    Kit {
        archives,
        urls,
        pins: Pins::parse(&text).expect("test pins"),
    }
}

/// Which packing the pin of `tool` has (so a test can name the archive).
pub fn packing(kit: &Kit, tool: Tool) -> Packing {
    kit.pins.get(tool).packing
}

/// Downloads from a map; any other URL is a 404.
pub struct FakeNet {
    pub urls: BTreeMap<String, Vec<u8>>,
    pub asked: RefCell<Vec<String>>,
}

impl FakeNet {
    pub fn new(urls: BTreeMap<String, Vec<u8>>) -> FakeNet {
        FakeNet {
            urls,
            asked: RefCell::new(Vec::new()),
        }
    }
}

impl Download for FakeNet {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, String> {
        self.asked.borrow_mut().push(url.to_owned());
        let body = self.urls.get(url).ok_or("answered 404".to_owned())?;
        match (body.len() as u64) <= limit {
            true => Ok(body.clone()),
            false => Err("too large".to_owned()),
        }
    }
}

pub fn fill(buf: &mut [u8]) -> std::io::Result<()> {
    buf.fill(RANDOM_BYTE);
    Ok(())
}

/// Runs `f` with an [`Env`] over `host` and `net`.
pub fn with_env<T>(host: &FakeHost, net: &FakeNet, f: impl FnOnce(&Env<'_>) -> T) -> T {
    let env = Env {
        host,
        net,
        random: &fill,
        now: 1_700_000_000,
    };
    f(&env)
}

/// What `ip -o -4 addr show` prints for an interface that carries [`bind`].
pub fn bind_line() -> String {
    format!(
        "2: eth0    inet {}/24 brd {} scope global eth0\\       valid_lft forever\n",
        bind(),
        Ipv4Addr::new(10, 0, 0, 255)
    )
}

/// A machine ready for `install`: root, the service user, the bind address on an interface,
/// nothing listening, every unit disabled and inactive, and the archives in `/srv/fetch`.
pub fn ready_host(kit: &Kit) -> FakeHost {
    let host = FakeHost::new(0);
    host.add_user("js-mdm", SVC);
    host.reply(
        &["/usr/bin/ip", "-o", "-4", "addr", "show"],
        0,
        &bind_line(),
    );
    host.reply(&["/usr/bin/ss", "-Hltn"], 0, "");
    host.reply_prefix(&[SYSTEMCTL, "is-enabled"], 1, "disabled\n");
    host.reply_prefix(&[SYSTEMCTL, "is-active"], 3, "inactive\n");
    host.reply_prefix(&[SYSTEMCTL, "daemon-reload"], 0, "");
    host.reply_prefix(&[SYSTEMCTL, "enable"], 0, "");
    host.reply_prefix(&[SYSTEMCTL, "start"], 0, "");
    host.reply_prefix(&[SYSTEMCTL, "restart"], 0, "");
    host.reply_prefix(&[SYSTEMCTL, "disable"], 0, "");
    host.reply_prefix(&["/usr/sbin/runuser"], 0, "");
    for (file, bytes) in &kit.archives {
        host.put_file(&format!("/srv/fetch/{file}"), 0o644, (1000, 1000), bytes);
    }
    host
}

/// The commands recorded that are not account lookups or queries.
pub fn changes(host: &FakeHost) -> Vec<String> {
    host.calls()
        .iter()
        .filter(|c| !(c[0] == GETENT || (c[0] == SYSTEMCTL && c[1].starts_with("is-"))))
        .map(|c| c.join(" "))
        .collect()
}
