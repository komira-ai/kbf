//! `mdm-host install`: NanoHUB, the SCEP server, NanoDEP and the HTTPS proxy as capped system
//! services, as root.
//!
//! It checks the host first, read only, and refuses before anything changes: the effective uid
//! is not 0 (the plan reads root-only files, so a dry run needs root too); `MDM_STATE`,
//! `MDM_ETC` or `MDM_BIN` is a symbolic link; no interface carries `MDM_BIND`; a port it uses
//! is held by another process (one held by the kit's own active unit is a rerun); an archive in
//! `--bin-dir` is missing or its sha256 is not its pin; a secret file is not mode 0600 or not
//! root's.
//!
//! The plan has each of these only when the host differs:
//! 1. the user `MDM_USER` (a system user and group of its name, no login);
//! 2. `MDM_STATE` and its subdirectories, `MDM_USER`'s, 0700; `MDM_ETC`, root and `MDM_USER`'s
//!    group, 0750; `MDM_BIN`, root's, 0755;
//! 3. each binary unpacked from its checked archive into `MDM_BIN`, root's, 0755, the one it
//!    replaces kept as `<name>.previous`;
//! 4. the secrets, each made once from 32 random bytes as hex and never printed:
//!    `NANOHUB_API_KEY` (`nanohub.env`), `NANODEP_API` (`nanodep.env`) and
//!    `SCEP_CHALLENGE_PASSWORD` (`scep.env`), in 0600 root files under `MDM_ETC`;
//! 5. the SCEP CA (`scepserver ca -init`, as `MDM_USER`), unless `ca.pem` is there: an existing
//!    CA is kept, since replacing it re-enrolls every Mac;
//! 6. the slice and the service units under `/etc/systemd/system` (root's, 0644) and the
//!    Caddyfile (root and `MDM_USER`'s group, 0640), exactly as `mdm-core` renders them;
//! 7. `daemon-reload` when a unit changed; the SCEP server, NanoHUB and depserver enabled, and
//!    restarted when their unit, the slice, their binary or their secret changed, else started
//!    when not active; the proxy likewise, but only once the TLS certificate is there;
//!    depsyncer is left alone (it is enabled once an Apple token exists).
//!
//! `--uninstall` stops and disables every unit of the kit that is installed and moves the unit
//! files to `MDM_STATE/removed/<unix seconds>/` (a 0700 directory of `MDM_USER`'s). The state (database, SCEP CA, DEP token, TLS
//! key and certificate), the secrets, the binaries and the user stay.

use std::collections::BTreeSet;
use std::io::{self, Write};

use host_step::{Account, Cmd, Host, Kind, Plan, Planner, UnitVerb, execute};
use mdm_core::commands::scep_ca_init_args;
use mdm_core::config::MdmConfig;
use mdm_core::pins::Pins;
use mdm_core::render::{self, UnitNames};
use sha2::{Digest, Sha256};

use crate::{Env, MdmError, archive, secret};

/// Where systemd reads local units from.
pub const UNITS_DIR: &str = "/etc/systemd/system";
const IP: &str = "/usr/bin/ip";
/// The socket lister.
pub const SS_PATH: &str = "/usr/bin/ss";
/// The secret files and the key each holds.
pub const SECRETS: [(&str, &str); 3] = [
    ("nanohub.env", "NANOHUB_API_KEY"),
    ("nanodep.env", "NANODEP_API"),
    ("scep.env", "SCEP_CHALLENGE_PASSWORD"),
];
const STATE_DIRS: [&str; 7] = [
    "nanohub",
    "scep",
    "scep/depot",
    "dep",
    "lego",
    "caddy",
    "www",
];

/// What `install` was asked for.
pub struct Install {
    /// The settings.
    pub config: MdmConfig,
    /// Where `fetch` put the archives.
    pub bin_dir: String,
    /// The largest archive read, in bytes.
    pub max_archive: u64,
}

/// Run `cmd` and return its standard output, or why it failed.
pub fn checked(host: &dyn Host, cmd: &Cmd) -> Result<String, MdmError> {
    let out = host.run(cmd)?;
    match out.code {
        0 => Ok(out.stdout),
        code => Err(MdmError::Command {
            line: cmd.line(),
            code,
        }),
    }
}

/// [`plan`], then print or carry the plan out.
pub fn run(
    env: &Env<'_>,
    a: &Install,
    pins: &Pins,
    mode: host_step::Mode,
    out: &mut dyn Write,
) -> Result<(), MdmError> {
    let p = plan(env, a, pins, out)?;
    Ok(execute(&p, mode, env.host, out)?)
}

/// [`plan_uninstall`], then print or carry the plan out.
pub fn run_uninstall(
    env: &Env<'_>,
    c: &MdmConfig,
    mode: host_step::Mode,
    out: &mut dyn Write,
) -> Result<(), MdmError> {
    let p = plan_uninstall(env, c, out)?;
    Ok(execute(&p, mode, env.host, out)?)
}

fn root_only(host: &dyn Host, verb: &'static str) -> Result<(), MdmError> {
    match host.euid() {
        0 => Ok(()),
        euid => Err(MdmError::NeedsRoot { verb, euid }),
    }
}

/// Check the host, then plan the user, directories, binaries, secrets, CA, units and services.
pub fn plan(
    env: &Env<'_>,
    a: &Install,
    pins: &Pins,
    out: &mut dyn Write,
) -> Result<Plan, MdmError> {
    let (c, host) = (&a.config, env.host);
    preflight(host, c, out)?;
    let binaries = binaries(host, &a.bin_dir, pins, a.max_archive)?;
    writeln!(out, "archives: every one matches its pin")?;

    let (state, etc, bin) = (c.state.as_str(), c.etc.as_str(), c.bin.as_str());
    let (root, mine) = (Account::root(), Account::new(&c.user, &c.user));
    let etc_owner = Account::new("root", &c.user);
    let mut p = Planner::new(host);
    p.user(&c.user, state)?;
    p.dir(state, 0o700, &mine)?;
    for d in STATE_DIRS {
        p.dir(&format!("{state}/{d}"), 0o700, &mine)?;
    }
    p.dir(etc, 0o750, &etc_owner)?;
    p.dir(bin, 0o755, &root)?;

    let mut changed: BTreeSet<String> = BTreeSet::new();
    for (name, bytes) in binaries {
        let path = format!("{bin}/{name}");
        // The binary a new one replaces is kept beside it.
        if let Some(old) = host.read(&path)?.filter(|old| *old != bytes) {
            p.file(&format!("{path}.previous"), 0o755, &root, &old)?;
        }
        if p.file(&path, 0o755, &root, &bytes)? {
            changed.insert(name.to_owned());
        }
    }
    for (file, key) in SECRETS {
        if secret_file(env, &mut p, &format!("{etc}/{file}"), key, out)? {
            changed.insert(file.to_owned());
        }
    }
    let ca = format!("{state}/scep/depot/ca.pem");
    if host.stat(&ca)?.is_some() {
        writeln!(out, "SCEP CA: kept ({ca})")?;
    } else {
        let init = Cmd::new(&format!("{bin}/scepserver")).args(&scep_ca_init_args(c));
        p.run(init.as_user(&c.user));
    }

    let rendered = render::render(c)?;
    let mut units_changed = false;
    for unit in &rendered.units {
        let path = format!("{UNITS_DIR}/{}", unit.name);
        if p.file(&path, 0o644, &root, unit.text.as_bytes())? {
            units_changed = true;
            changed.insert(unit.name.clone());
        }
    }
    let caddyfile = format!("{etc}/{}", render::CADDYFILE_NAME);
    if p.file(&caddyfile, 0o640, &etc_owner, rendered.caddyfile.as_bytes())? {
        changed.insert(render::CADDYFILE_NAME.to_owned());
    }

    if units_changed {
        p.unit(UnitVerb::DaemonReload, &[]);
    }
    let n = UnitNames::new(c);
    let core = [
        (&n.scep, "scepserver", "scep.env"),
        (&n.nanohub, "nanohub", "nanohub.env"),
        (&n.depserver, "depserver", "nanodep.env"),
    ];
    for (unit, binary, env_file) in core {
        let again = [n.slice.as_str(), unit.as_str(), binary, env_file]
            .iter()
            .any(|k| changed.contains(*k));
        p.service(unit, again)?;
    }
    let cert = format!("{state}/lego/certificates/{}.crt", c.dns_name);
    if host.stat(&cert)?.is_some() {
        let again = [
            n.slice.as_str(),
            n.proxy.as_str(),
            "caddy",
            render::CADDYFILE_NAME,
        ]
        .iter()
        .any(|k| changed.contains(*k));
        p.service(&n.proxy, again)?;
    } else {
        let waits = format!("proxy: not started; it waits for the TLS certificate {cert}");
        writeln!(out, "{waits}")?;
    }
    Ok(p.finish())
}

/// The host checks, read only.
fn preflight(host: &dyn Host, c: &MdmConfig, out: &mut dyn Write) -> Result<(), MdmError> {
    root_only(host, "install")?;
    for path in [&c.state, &c.etc, &c.bin] {
        let link = host.stat(path.as_str())?.map(|m| m.kind) == Some(Kind::Other);
        if link {
            return Err(MdmError::IsLink(path.to_string()));
        }
    }
    // `ip -o -4 addr show`: `<n>: <if>    inet <addr>/<bits> brd ...`, one per line.
    let addrs = checked(host, &Cmd::new(IP).args(&["-o", "-4", "addr", "show"]))?;
    let bind = c.bind.to_string();
    let carried = addrs.lines().any(|l| {
        let mut words = l.split_whitespace();
        let after_inet = words.by_ref().find(|w| *w == "inet").and(words.next());
        after_inet.and_then(|a| a.split('/').next()) == Some(bind.as_str())
    });
    if !carried {
        return Err(MdmError::NotCarried(bind));
    }
    // `ss -Hltn`: `<state> <recv-q> <send-q> <local address:port> <peer>`.
    let listening = checked(host, &Cmd::new(SS_PATH).arg("-Hltn"))?;
    let n = UnitNames::new(c);
    for (port, unit) in [
        (c.nanohub_port, &n.nanohub),
        (c.scep_port, &n.scep),
        (c.dep_port, &n.depserver),
        (443, &n.proxy),
    ] {
        let tail = format!(":{port}");
        let held = listening.lines().any(|l| {
            l.split_whitespace()
                .nth(3)
                .is_some_and(|local| local.ends_with(&tail))
        });
        if held {
            let active = host.run(&Cmd::new(host_step::SYSTEMCTL).arg("is-active").arg(unit))?;
            if active.code != 0 {
                return Err(MdmError::PortHeld(port));
            }
            writeln!(out, "port {port}: held by {unit} (a rerun)")?;
        }
    }
    Ok(())
}

/// Every pinned archive in `dir` checked against its pin, and the binaries unpacked from them:
/// `(installed name, bytes)`.
fn binaries(
    host: &dyn Host,
    dir: &str,
    pins: &Pins,
    max: u64,
) -> Result<Vec<(&'static str, Vec<u8>)>, MdmError> {
    let mut out = Vec::new();
    for pin in pins.all() {
        let path = format!("{dir}/{}", pin.archive());
        // One bounded read of one open file: a name swapped for a link cannot get past the cap.
        let read = host.read_limited(&path, max).map_err(|e| match e.kind() {
            io::ErrorKind::FileTooLarge => MdmError::ArchiveTooBig {
                path: path.clone(),
                max,
            },
            _ => MdmError::Io(e),
        });
        let bytes = read?.ok_or_else(|| MdmError::NoArchive(path.clone()))?;
        let got = hex::encode(Sha256::digest(&bytes));
        if got != pin.sha256_hex() {
            return Err(MdmError::ArchiveDiffers {
                path,
                got,
                want: pin.sha256_hex(),
            });
        }
        for (member, name) in pin.members() {
            out.push((
                name,
                archive::member(pin.archive(), pin.packing, &bytes, &member)?,
            ));
        }
    }
    Ok(out)
}

/// Plan the secret file `path` holding `key`, unless it holds a value already (then it must be
/// root's, 0600); whether one is made. The value is never printed.
fn secret_file(
    env: &Env<'_>,
    p: &mut Planner<'_>,
    path: &str,
    key: &str,
    out: &mut dyn Write,
) -> Result<bool, MdmError> {
    let host = env.host;
    let existing = host.read(path)?.unwrap_or_default();
    if secret::has_value(&existing, key) {
        let (mode, uid) = host.stat(path)?.map_or((0, 0), |m| (m.mode, m.uid));
        secret::check_mode(path, mode, uid)?;
        writeln!(out, "secret {key}: kept ({path})")?;
        return Ok(false);
    }
    // A file with the key empty is made again; one that holds anything else would lose it.
    if secret::holds_other_lines(&existing, key) {
        return Err(MdmError::SecretOther {
            path: path.to_owned(),
            key: key.to_owned(),
        });
    }
    let mut value = [0u8; 32];
    (env.random)(&mut value)?;
    let line = format!("{key}={}\n", hex::encode(value));
    p.file(path, 0o600, &Account::root(), line.as_bytes())?;
    writeln!(out, "secret {key}: makes {path} (0600 root; never shown)")?;
    Ok(true)
}

/// Plan the rollback: units stopped and disabled, their files moved to
/// `MDM_STATE/removed/<now>/`.
pub fn plan_uninstall(env: &Env<'_>, c: &MdmConfig, out: &mut dyn Write) -> Result<Plan, MdmError> {
    let host = env.host;
    root_only(host, "install --uninstall")?;
    let n = UnitNames::new(c);
    let all = [
        &n.slice,
        &n.scep,
        &n.nanohub,
        &n.depserver,
        &n.depsyncer,
        &n.proxy,
        &n.cert_service,
        &n.cert_timer,
        &n.reload_path,
        &n.reload_service,
    ];
    let mut there = Vec::new();
    for unit in all {
        if host.stat(&format!("{UNITS_DIR}/{unit}"))?.is_some() {
            there.push(unit);
        }
    }
    // The slice stops with its services; the two jobs are started by the timer and the path
    // unit, which are stopped.
    let idle = [&n.slice, &n.cert_service, &n.reload_service];
    let stop: Vec<&str> = there
        .iter()
        .filter(|u| !idle.contains(u))
        .map(|u| u.as_str())
        .collect();
    let mut p = Planner::new(host);
    if !stop.is_empty() {
        p.unit(UnitVerb::DisableNow, &stop);
    }
    let to = format!("{}/removed/{}", c.state, env.now);
    // The copies are the service user's, in a 0700 directory of its own: every directory on
    // the way is then root's or the service user's, which the host layer insists on.
    let mine = Account::new(&c.user, &c.user);
    if !there.is_empty() {
        p.dir(&to, 0o700, &mine)?;
    }
    for unit in &there {
        let path = format!("{UNITS_DIR}/{unit}");
        let bytes = host.read(&path)?.unwrap_or_default();
        p.file(&format!("{to}/{unit}"), 0o644, &mine, &bytes)?;
        p.absent(&path)?;
    }
    if there.is_empty() {
        writeln!(out, "no unit of the kit is installed")?;
    } else {
        p.unit(UnitVerb::DaemonReload, &[]);
    }
    let kept = format!(
        "kept: {} (database, SCEP CA, DEP token, TLS key and certificate), {} (secrets), {}, the user {}",
        c.state, c.etc, c.bin, c.user
    );
    writeln!(out, "{kept}")?;
    Ok(p.finish())
}
