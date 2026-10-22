//! The host inventory: `/etc/kbf-updater/inventory`, root-owned, one host per line.
//!
//! ```text
//! # alias   address               role      host key (type and base64, as in known_hosts)
//! fe1       fe1.farm.example:22   frontend  ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample
//! st1       st1.farm.example      storage   ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample
//! ```
//!
//! Blank lines and `#` lines are skipped. A line is exactly five words: the alias, the
//! address, the role, and the key as its type and its base64. The updater never
//! learns a host key by asking the host: the pinned key here is the one ssh must see.
//!
//! The file decides which machines a roll may touch and as whom ssh logs in, so it is read
//! only if it is owned by the expected uid (root) and writable by no one else, and any
//! malformed line refuses the whole file: a half-read inventory would roll a different set
//! of hosts than the operator wrote.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::api::{ApiError, ErrorCode, Hosts};

/// What a host runs, which decides its turn and its health bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The frontend (kbf-proxy and the active child): rolled first, no safe-to-stop.
    Frontend,
    /// A storage node, possibly with a daemon.
    Storage,
    /// A daemon-only host.
    Daemon,
}

impl Role {
    fn parse(s: &str) -> Option<Role> {
        match s {
            "frontend" => Some(Role::Frontend),
            "storage" => Some(Role::Storage),
            "daemon" => Some(Role::Daemon),
            _ => None,
        }
    }
}

/// One host of the inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub alias: String,
    /// `host` or `host:port`; never starts with `-`, so ssh cannot read it as an option.
    pub address: String,
    pub role: Role,
    /// The pinned host key, `<type> <base64>`.
    pub host_key: String,
}

/// Why an inventory was refused.
#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    #[error("cannot read the inventory: {0}")]
    Io(#[from] std::io::Error),
    #[error("the inventory is owned by uid {found}, not {expected}")]
    Owner { found: u32, expected: u32 },
    #[error("the inventory is writable by group or others (mode {mode:o})")]
    Mode { mode: u32 },
    #[error("line {line}: {reason}")]
    Line { line: usize, reason: String },
    #[error("the inventory has no hosts")]
    Empty,
}

const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "ssh-rsa",
];

/// The parsed inventory: hosts in file order, aliases unique, at most one frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inventory {
    hosts: Vec<Host>,
}

impl Inventory {
    /// Read the file at `path`, which must be owned by `owner_uid` and not writable by group
    /// or others.
    pub fn load(path: &Path, owner_uid: u32) -> Result<Inventory, InventoryError> {
        let meta = fs::metadata(path)?;
        if meta.uid() != owner_uid {
            return Err(InventoryError::Owner {
                found: meta.uid(),
                expected: owner_uid,
            });
        }
        let mode = meta.mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(InventoryError::Mode { mode });
        }
        Inventory::parse(&fs::read_to_string(path)?)
    }

    pub fn parse(text: &str) -> Result<Inventory, InventoryError> {
        let mut hosts: Vec<Host> = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = |reason: String| InventoryError::Line {
                line: n + 1,
                reason,
            };
            let host = parse_line(line).map_err(at)?;
            if hosts.iter().any(|h| h.alias == host.alias) {
                return Err(at(format!("alias {} is listed twice", host.alias)));
            }
            if host.role == Role::Frontend && hosts.iter().any(|h| h.role == Role::Frontend) {
                return Err(at("a second frontend".to_owned()));
            }
            hosts.push(host);
        }
        if hosts.is_empty() {
            return Err(InventoryError::Empty);
        }
        Ok(Inventory { hosts })
    }

    pub fn host(&self, alias: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.alias == alias)
    }

    /// The hosts of a roll in the order they are touched: the frontend first, then the rest
    /// one at a time, in inventory order for "all" and in the order given for a list. An
    /// alias that is not in the inventory, or is listed twice, is refused here, before
    /// anything runs.
    pub fn plan(&self, hosts: &Hosts) -> Result<Vec<&Host>, ApiError> {
        let mut chosen: Vec<&Host> = match hosts {
            Hosts::All(_) => self.hosts.iter().collect(),
            Hosts::Aliases(aliases) => {
                let mut out: Vec<&Host> = Vec::new();
                for alias in aliases {
                    let host = self.host(alias).ok_or_else(|| {
                        ApiError::new(
                            ErrorCode::UnknownAlias,
                            format!("no host with the alias {alias:?} in the inventory"),
                        )
                    })?;
                    if out.iter().any(|h| h.alias == host.alias) {
                        return Err(ApiError::new(
                            ErrorCode::BadRequest,
                            format!("the alias {alias:?} is listed twice"),
                        ));
                    }
                    out.push(host);
                }
                out
            }
        };
        // Stable: the frontend moves to the front, everything else keeps its order.
        chosen.sort_by_key(|h| h.role != Role::Frontend);
        Ok(chosen)
    }
}

fn parse_line(line: &str) -> Result<Host, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let [alias, address, role, key_type, key] = words[..] else {
        return Err(format!(
            "expected alias, address, role, key type and key; found {} words",
            words.len()
        ));
    };
    if !valid_alias(alias) {
        return Err(format!(
            "alias {alias:?} must be 1..=63 of letters, digits, _ and -, starting with a letter or digit"
        ));
    }
    if !valid_address(address) {
        return Err(format!("address {address:?} is not a host or host:port"));
    }
    let role = Role::parse(role)
        .ok_or_else(|| format!("role {role:?} is not frontend, storage or daemon"))?;
    if !KEY_TYPES.contains(&key_type) {
        return Err(format!(
            "host key type {key_type:?} is not a supported type"
        ));
    }
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        return Err("the host key is not base64".to_owned());
    }
    Ok(Host {
        alias: alias.to_owned(),
        address: address.to_owned(),
        role,
        host_key: format!("{key_type} {key}"),
    })
}

fn valid_alias(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.len() <= 63
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn valid_address(s: &str) -> bool {
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (s, None),
    };
    let host_ok = host
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric())
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    let port_ok = port.is_none_or(|p| {
        p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n > 0)
    });
    host_ok && port_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::All;
    use std::os::unix::fs::PermissionsExt;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample";

    fn file(lines: &[&str]) -> String {
        lines.join("\n")
    }

    fn sample() -> Inventory {
        Inventory::parse(&file(&[
            "# the farm",
            "",
            &format!("st1 st1.farm.example storage {KEY}"),
            &format!("fe1 fe1.farm.example:2222 frontend {KEY}"),
            &format!("  dm1\tdm1.farm.example  daemon  {KEY}  "),
            &format!("st2 st2.farm.example storage {KEY}"),
        ]))
        .expect("inventory")
    }

    fn aliases(plan: &[&Host]) -> Vec<String> {
        plan.iter().map(|h| h.alias.clone()).collect()
    }

    fn line_error(text: &str) -> String {
        match Inventory::parse(text).expect_err("refused") {
            InventoryError::Line { line, reason } => format!("{line}: {reason}"),
            other => panic!("not a line error: {other}"),
        }
    }

    /// Catches a reader that drops a field or reads the wrong column: every field of every
    /// host, with comments, blank lines and mixed whitespace skipped.
    #[test]
    fn a_well_formed_inventory_reads_every_field() {
        let inv = sample();
        let fe = inv.host("fe1").expect("fe1");
        assert_eq!(fe.address, "fe1.farm.example:2222");
        assert_eq!(fe.role, Role::Frontend);
        assert_eq!(fe.host_key, KEY);
        assert_eq!(inv.host("dm1").expect("dm1").role, Role::Daemon);
        assert_eq!(inv.host("st1").expect("st1").role, Role::Storage);
        assert!(inv.host("nope").is_none());
    }

    /// Catches an unknown alias slipping past as a skipped host: it must be an error before
    /// anything runs, whether the list is all unknown, mixed, or the unknown one is last.
    #[test]
    fn an_unknown_alias_is_refused_before_anything_runs() {
        let inv = sample();
        for list in [vec!["ghost"], vec!["st1", "ghost"], vec!["ghost", "st1"]] {
            let hosts = Hosts::Aliases(list.iter().map(|s| (*s).to_owned()).collect());
            let err = inv.plan(&hosts).expect_err("refused");
            assert_eq!(err.code, ErrorCode::UnknownAlias, "{list:?}");
            assert!(err.message.contains("ghost"));
        }
    }

    /// Catches an order that is not frontend first, then one host at a time: "all" is the
    /// inventory order with the frontend moved up, a list keeps the caller's order after it.
    #[test]
    fn the_plan_puts_the_frontend_first_and_keeps_the_rest_in_order() {
        let inv = sample();
        let all = inv.plan(&Hosts::All(All::All)).expect("all");
        assert_eq!(aliases(&all), ["fe1", "st1", "dm1", "st2"]);
        let list = Hosts::Aliases(vec!["st2".into(), "fe1".into(), "st1".into()]);
        assert_eq!(
            aliases(&inv.plan(&list).expect("list")),
            ["fe1", "st2", "st1"]
        );
        let nodes = Hosts::Aliases(vec!["dm1".into(), "st2".into()]);
        assert_eq!(aliases(&inv.plan(&nodes).expect("nodes")), ["dm1", "st2"]);
    }

    /// Catches a host rolled twice by a repeated alias.
    #[test]
    fn a_repeated_alias_in_a_list_is_refused() {
        let hosts = Hosts::Aliases(vec!["st1".into(), "st1".into()]);
        let err = sample().plan(&hosts).expect_err("refused");
        assert_eq!(err.code, ErrorCode::BadRequest);
    }

    /// Catches a lenient reader: each malformed line is a refusal of the whole file, with the
    /// line number, and none is half-read.
    #[test]
    fn a_malformed_line_refuses_the_file() {
        let ok = format!("fe1 fe1.example frontend {KEY}");
        let cases: [(&str, &str); 14] = [
            ("fe1 fe1.example frontend", "found 3 words"),
            (&format!("{ok} extra"), "found 6 words"),
            (&format!("-x fe1.example storage {KEY}"), "alias"),
            (&format!("a/b fe1.example storage {KEY}"), "alias"),
            (
                &format!("{} fe1.example storage {KEY}", "a".repeat(64)),
                "alias",
            ),
            (&format!("a -oProxyCommand=x storage {KEY}"), "address"),
            (&format!("a host:0 storage {KEY}"), "address"),
            (&format!("a host:99999x storage {KEY}"), "address"),
            (&format!("a host: storage {KEY}"), "address"),
            (&format!("a h;st storage {KEY}"), "address"),
            (&format!("a h.example admin {KEY}"), "role"),
            ("a h.example storage ssh-dss AAAA", "key type"),
            ("a h.example storage ssh-ed25519 AA!A", "base64"),
            ("a h.example storage ssh-ed25519 AA\u{e9}A", "base64"),
        ];
        for (text, want) in cases {
            let got = line_error(&format!("# header\n{text}"));
            assert!(got.starts_with("2: "), "{text}: {got}");
            assert!(got.contains(want), "{text}: {got}");
        }
    }

    /// Catches a duplicate alias (the second would shadow or double-roll a host), a second
    /// frontend (two hosts both claiming the swap), and an inventory with nothing in it.
    #[test]
    fn duplicates_a_second_frontend_and_an_empty_file_are_refused() {
        let a = format!("a a.example storage {KEY}");
        assert!(line_error(&format!("{a}\n{a}")).contains("listed twice"));
        let f1 = format!("f1 f1.example frontend {KEY}");
        let f2 = format!("f2 f2.example frontend {KEY}");
        assert!(line_error(&format!("{f1}\n{f2}")).contains("second frontend"));
        assert!(matches!(
            Inventory::parse("# nothing\n\n"),
            Err(InventoryError::Empty)
        ));
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/updater-scratch")
            .join(format!("inv-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// Catches an inventory read from a file anyone could have edited: another owner, group
    /// write and world write are each refused; a root-style 0644 owned by the expected uid
    /// is read.
    #[test]
    fn load_checks_owner_and_mode() {
        let path = scratch("load").join("inventory");
        fs::write(&path, format!("a a.example storage {KEY}\n")).expect("write");
        let mine = fs::metadata(&path).expect("meta").uid();
        for (mode, ok) in [(0o644, true), (0o600, true), (0o664, false), (0o646, false)] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
            let got = Inventory::load(&path, mine);
            assert_eq!(got.is_ok(), ok, "mode {mode:o}: {got:?}");
            if !ok {
                assert!(matches!(got, Err(InventoryError::Mode { mode: m }) if m == mode));
            }
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let other = Inventory::load(&path, mine + 1).expect_err("owner");
        assert!(
            matches!(other, InventoryError::Owner { found, expected } if found == mine && expected == mine + 1)
        );
        assert!(other.to_string().contains("owned by uid"));
        let missing = Inventory::load(&path.with_extension("none"), mine).expect_err("missing");
        assert!(matches!(missing, InventoryError::Io(_)));
    }
}
