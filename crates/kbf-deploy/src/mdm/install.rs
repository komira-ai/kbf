//! `mdm install`: NanoHUB, the SCEP server, NanoDEP and the HTTPS proxy as capped
//! system services, as root.
//!
//! It checks the host first, read only, and refuses before anything changes: the
//! effective uid is not 0 (the plan reads root-only files, so a dry run needs sudo
//! too); no interface carries `MDM_BIND`; a port it uses is held by another process
//! (one held by the kit's own active unit is a re-run); the user `MDM_USER` does not
//! exist (the refusal prints the `useradd` line); an archive in `--bin-dir` is missing
//! or its sha256 is not its pin; a secret file is not mode 0600 or not root's.
//!
//! The plan has each of these only when the host differs:
//! 1. `MDM_STATE` and its subdirectories, `MDM_USER`'s, 0700; `MDM_ETC`, root and
//!    `MDM_USER`'s group, 0750; `MDM_BIN`, root's, 0755;
//! 2. each binary unpacked from its checked archive into `MDM_BIN`, root's, 0755, the
//!    one it replaces kept as `<name>.previous` (its mode, root's);
//! 3. the secrets, each made once from 32 random bytes as hex and never printed:
//!    `NANOHUB_API_KEY` (`nanohub.env`), `NANODEP_API` (`nanodep.env`) and
//!    `SCEP_CHALLENGE_PASSWORD` (`scep.env`), in 0600 root files under `MDM_ETC`;
//! 4. the SCEP CA (`scepserver ca -init`, as `MDM_USER`), when `ca.pem` is not there;
//! 5. the slice and the five service units under `/etc/systemd/system`, and the
//!    Caddyfile (root and `MDM_USER`'s group, 0640);
//! 6. `daemon-reload` when a unit changed; the SCEP server, NanoHUB and depserver
//!    enabled, and restarted when their unit, the slice, their binary or their secret
//!    changed, else started when not active; the proxy likewise, but only once the TLS
//!    certificate is there; depsyncer is left alone (it is enabled once an Apple token
//!    exists);
//! 7. after a start, a wait of up to 20 s each for NanoHUB's `/version` and the SCEP
//!    CA on loopback.
//!
//! `--uninstall` stops and disables every kbf-mdm unit there is and moves the unit
//! files to `MDM_STATE/removed/<unix seconds>/`. The state, the secrets, the binaries
//! and the user stay.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::MdmError;
use super::archive;
use super::config::MdmConfig;
use super::pins::Pins;
use super::render::{
    self, CERT_SERVICE, CERT_TIMER, DEPSERVER, NANOHUB, PROXY, RELOAD_PATH, RELOAD_SERVICE, SCEP,
    SLICE,
};
use crate::linux::host::{self, SS, SYSTEMCTL, binary, note, parents, tool, under, unit_is};
use kbf_host::exec::{Entry, Exec};
use kbf_host::path::HostPath;
use kbf_host::plan::{Mode, Plan, Planner, RunAs, execute};
use kbf_host::step::{Cmd, HttpUrl, Owner, Perms};
use kbf_host::user::{self, RunUser};

pub const IP: &str = "/usr/bin/ip";
const UNITS: &str = "/etc/systemd/system";
const ROOT: Owner = Owner { uid: 0, gid: 0 };
/// How long each service has to answer on loopback after a start.
pub const READY: Duration = Duration::from_secs(20);
/// The services `install` starts, with the binary each runs and its secret file.
const CORE: [(&str, &str, &str); 3] = [
    (SCEP, "scepserver", "scep.env"),
    (NANOHUB, "nanohub", "nanohub.env"),
    (DEPSERVER, "depserver", "nanodep.env"),
];
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

/// What `mdm install` was asked for.
pub struct Install {
    pub config: MdmConfig,
    /// Where `mdm fetch` put the archives.
    pub bin_dir: HostPath,
}

/// Check the host, then plan the directories, binaries, secrets, CA, units and services.
pub fn plan<E: Exec>(
    exec: &E,
    a: &Install,
    pins: &Pins,
    out: &mut dyn Write,
) -> Result<Plan, MdmError> {
    let c = &a.config;
    let user = preflight(exec, c, out)?;
    let binaries = binaries(exec, &a.bin_dir, pins)?;
    note(out, format_args!("archives: every one matches its pin"))?;

    let mut planner = Planner::new(exec);
    let mine = Owner {
        uid: user.uid,
        gid: user.gid,
    };
    parents(exec, &mut planner, &c.state)?;
    planner.dir(c.state.clone(), Perms::new(0o700), Some(mine))?;
    for d in STATE_DIRS {
        planner.dir(under(&c.state, d)?, Perms::new(0o700), Some(mine))?;
    }
    parents(exec, &mut planner, &c.etc)?;
    let etc_owner = Owner {
        uid: 0,
        gid: user.gid,
    };
    planner.dir(c.etc.clone(), Perms::new(0o750), Some(etc_owner))?;
    parents(exec, &mut planner, &c.bin)?;
    planner.dir(c.bin.clone(), Perms::new(0o755), Some(ROOT))?;

    let mut changed: BTreeSet<&str> = BTreeSet::new();
    for (name, bytes) in binaries {
        let path = under(&c.bin, name)?;
        if binary(exec, &mut planner, &path, bytes, ROOT, out)? {
            changed.insert(name);
        }
    }
    for (file, key) in SECRETS {
        if secret(exec, &mut planner, &under(&c.etc, file)?, key, out)? {
            changed.insert(file);
        }
    }
    let ca = under(&c.state, "scep/depot/ca.pem")?;
    if host::stat(exec, &ca)?.is_some() {
        note(out, format_args!("SCEP CA: kept ({ca})"))?;
    } else {
        let depot = under(&c.state, "scep/depot")?;
        let init = Cmd::new(under(&c.bin, "scepserver")?)
            .arg("ca")
            .arg("-init")
            .arg("-depot")
            .arg(depot.as_str())
            .arg("-years")
            .arg("10")
            .arg("-common_name")
            .arg("kbf-mdm SCEP CA")
            .arg("-organization")
            .arg(c.org.as_str())
            .arg("-organizational_unit")
            .arg("kbf-mdm")
            .user(user.clone());
        planner.run(init);
    }

    let rendered = render::render(c)?;
    let dir = HostPath::new(UNITS)?;
    let mut units_changed = false;
    for (name, text) in &rendered.units {
        let path = under(&dir, name)?;
        if place(
            &mut planner,
            &path,
            Some(ROOT),
            Perms::new(0o644),
            text,
            out,
        )? {
            units_changed = true;
            changed.insert(name);
        }
    }
    let caddyfile = under(&c.etc, "Caddyfile")?;
    if place(
        &mut planner,
        &caddyfile,
        Some(etc_owner),
        Perms::new(0o640),
        &rendered.caddyfile,
        out,
    )? {
        changed.insert("Caddyfile");
    }

    if units_changed {
        planner.run(systemctl(&["daemon-reload"]));
    }
    let slice = changed.contains(SLICE);
    let mut started = false;
    for (unit, bin, env) in CORE {
        let again =
            slice || changed.contains(unit) || changed.contains(bin) || changed.contains(env);
        started |= service(exec, &mut planner, unit, again)?;
    }
    let cert = under(&c.state, &format!("lego/certificates/{}.crt", c.dns_name))?;
    if host::stat(exec, &cert)?.is_some() {
        let again = slice
            || changed.contains(PROXY)
            || changed.contains("caddy")
            || changed.contains("Caddyfile");
        service(exec, &mut planner, PROXY, again)?;
    } else {
        note(
            out,
            format_args!("proxy: not started; it waits for the TLS certificate {cert}"),
        )?;
    }
    if started {
        let version = HttpUrl::new(&format!("http://127.0.0.1:{}/version", c.nanohub_port))?;
        planner.wait_http(version, READY);
        let ca = format!("http://127.0.0.1:{}/scep?operation=GetCACert", c.scep_port);
        planner.wait_http(HttpUrl::new(&ca)?, READY);
    }
    Ok(planner.finish())
}

/// The host checks, read only; the services' user.
fn preflight<E: Exec>(exec: &E, c: &MdmConfig, out: &mut dyn Write) -> Result<RunUser, MdmError> {
    let euid = exec.euid();
    if euid != 0 {
        return Err(MdmError::InstallNeedsRoot { euid });
    }
    // `ip -o -4 addr show`: `<n>: <if>    inet <addr>/<bits> brd ...`, one per line.
    let addrs = host::checked(
        exec,
        &Cmd::new(tool(IP))
            .arg("-o")
            .arg("-4")
            .arg("addr")
            .arg("show"),
    )?;
    let bind = c.bind.to_string();
    let carried = addrs.lines().any(|l| {
        let mut words = l.split_whitespace();
        words.any(|w| w == "inet")
            && words.next().and_then(|a| a.split('/').next()) == Some(bind.as_str())
    });
    if !carried {
        return Err(MdmError::NotCarried(c.bind));
    }
    // `ss -Hltn`: `<state> <recv-q> <send-q> <local address:port> <peer>`.
    let listening = host::checked(exec, &Cmd::new(tool(SS)).arg("-Hltn"))?;
    for (port, unit) in [
        (c.nanohub_port, NANOHUB),
        (c.scep_port, SCEP),
        (c.dep_port, DEPSERVER),
        (443, PROXY),
    ] {
        let tail = format!(":{port}");
        let held = listening.lines().any(|l| {
            l.split_whitespace()
                .nth(3)
                .is_some_and(|local| local.ends_with(&tail))
        });
        if held {
            if !unit_is(exec, "is-active", unit)? {
                return Err(MdmError::PortHeld(port));
            }
            note(out, format_args!("port {port}: held by {unit} (a re-run)"))?;
        }
    }
    user::lookup(exec, &c.user)?.ok_or(MdmError::NoUser {
        user: c.user.clone(),
        home: c.state.clone(),
    })
}

/// Every pinned archive in `dir` checked against its pin, and the binaries unpacked from
/// them: `(installed name, bytes)`.
fn binaries<E: Exec>(
    exec: &E,
    dir: &HostPath,
    pins: &Pins,
) -> Result<Vec<(&'static str, Vec<u8>)>, MdmError> {
    let mut out = Vec::new();
    for pin in pins.all() {
        let path = under(dir, pin.archive())?;
        let Some(Entry::File { bytes, .. }) = host::inspect(exec, &path)? else {
            return Err(MdmError::NoArchive(path));
        };
        let got = hex::encode(Sha256::digest(&bytes));
        if got != pin.sha256_hex() {
            return Err(MdmError::ArchiveDiffers {
                path,
                tool: pin.tool,
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

/// Plan the secret file `path` holding `key`, unless it holds a value already (then it
/// must be root's, 0600); whether one is made.
fn secret<E: Exec>(
    exec: &E,
    planner: &mut Planner<'_, E>,
    path: &HostPath,
    key: &str,
    out: &mut dyn Write,
) -> Result<bool, MdmError> {
    if let Some(Entry::File {
        perms,
        owner,
        bytes,
    }) = host::inspect(exec, path)?
        && has_value(&bytes, key)
    {
        super::secret::check_mode(path, perms, owner)?;
        note(out, format_args!("secret {key}: kept ({path})"))?;
        return Ok(false);
    }
    let value = random_hex()?;
    planner.file(
        path.clone(),
        Perms::new(0o600),
        Some(ROOT),
        format!("{key}={value}\n").into_bytes(),
    )?;
    note(
        out,
        format_args!("secret {key}: makes {path} (0600 root; never shown)"),
    )?;
    Ok(true)
}

/// A `KEY=<value>` line with a value is in `bytes`.
pub fn has_value(bytes: &[u8], key: &str) -> bool {
    let prefix = format!("{key}=");
    String::from_utf8_lossy(bytes)
        .lines()
        .any(|l| l.strip_prefix(&prefix).is_some_and(|v| !v.is_empty()))
}

/// 32 bytes from the kernel's random source, as 64 hex digits.
fn random_hex() -> Result<String, MdmError> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(MdmError::Random)?;
    Ok(hex::encode(bytes))
}

/// Plan `text` at `path`; when it changes, print it. Whether it changes.
pub(crate) fn place<E: Exec>(
    planner: &mut Planner<'_, E>,
    path: &HostPath,
    owner: Option<Owner>,
    perms: Perms,
    text: &str,
    out: &mut dyn Write,
) -> Result<bool, MdmError> {
    if !planner.file(path.clone(), perms, owner, text.as_bytes().to_vec())? {
        note(out, format_args!("{path}: unchanged"))?;
        return Ok(false);
    }
    note(out, format_args!("{path}: writes"))?;
    for line in text.lines() {
        note(out, format_args!("    | {line}"))?;
    }
    Ok(true)
}

/// Plan `unit` enabled, and restarted when `again`, else started when not active;
/// whether it is (re)started.
pub(crate) fn service<E: Exec>(
    exec: &E,
    planner: &mut Planner<'_, E>,
    unit: &str,
    again: bool,
) -> Result<bool, MdmError> {
    if !unit_is(exec, "is-enabled", unit)? {
        planner.run(systemctl(&["enable", unit]));
    }
    if again {
        planner.run(systemctl(&["restart", unit]));
        return Ok(true);
    }
    if !unit_is(exec, "is-active", unit)? {
        planner.run(systemctl(&["start", unit]));
        return Ok(true);
    }
    Ok(false)
}

/// Plan the rollback; `stamp` names the directory the unit files move to.
pub fn plan_uninstall<E: Exec>(
    exec: &E,
    c: &MdmConfig,
    stamp: &str,
    out: &mut dyn Write,
) -> Result<Plan, MdmError> {
    let euid = exec.euid();
    if euid != 0 {
        return Err(MdmError::InstallNeedsRoot { euid });
    }
    let mut planner = Planner::new(exec);
    let dir = HostPath::new(UNITS)?;
    let mut units: Vec<&str> = render::UNITS.iter().map(|(name, _)| *name).collect();
    units.extend([CERT_SERVICE, CERT_TIMER, RELOAD_PATH, RELOAD_SERVICE]);
    let mut there = Vec::new();
    for unit in units {
        if host::stat(exec, &under(&dir, unit)?)?.is_some() {
            there.push(unit);
        }
    }
    // The slice stops with its services; the two jobs are started by the timer and the
    // path unit, which are stopped.
    let idle = [SLICE, CERT_SERVICE, RELOAD_SERVICE];
    let stop: Vec<&str> = there
        .iter()
        .copied()
        .filter(|u| !idle.contains(u))
        .collect();
    if !stop.is_empty() {
        let mut args = vec!["disable", "--now"];
        args.extend(&stop);
        planner.run(systemctl(&args));
    }
    let to = under(&c.state, &format!("removed/{stamp}"))?;
    for name in &there {
        let path = under(&dir, name)?;
        if let Some(Entry::File {
            perms,
            owner,
            bytes,
        }) = host::inspect(exec, &path)?
        {
            let kept = under(&to, name)?;
            parents(exec, &mut planner, &kept)?;
            planner.file(kept, perms, Some(owner), bytes)?;
        }
        planner.absent(path)?;
    }
    if there.is_empty() {
        note(out, format_args!("no kbf-mdm unit is installed"))?;
    } else {
        planner.run(systemctl(&["daemon-reload"]));
    }
    note(
        out,
        format_args!(
            "kept: {} (database, SCEP CA, DEP token, TLS key), {} (secrets), {}, the user {}",
            c.state, c.etc, c.bin, c.user
        ),
    )?;
    Ok(planner.finish())
}

/// [`plan`], then print or carry the plan out.
pub fn run<E: Exec>(
    exec: &mut E,
    a: &Install,
    pins: &Pins,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), MdmError> {
    let p = plan(&*exec, a, pins, &mut *out)?;
    Ok(execute(&p, RunAs::Root, mode, exec, &mut out)?)
}

/// [`plan_uninstall`], then print or carry the plan out.
pub fn run_uninstall<E: Exec>(
    exec: &mut E,
    c: &MdmConfig,
    stamp: &str,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), MdmError> {
    let p = plan_uninstall(&*exec, c, stamp, &mut *out)?;
    Ok(execute(&p, RunAs::Root, mode, exec, &mut out)?)
}

pub(crate) fn systemctl(args: &[&str]) -> Cmd {
    args.iter()
        .fold(Cmd::new(tool(SYSTEMCTL)), |cmd, arg| cmd.arg(*arg))
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
