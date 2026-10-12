//! `pilot server-install`: one kbf-server as a capped systemd user service, run as the
//! operator's own user (never root, never with sudo).
//!
//! It refuses, before changing anything:
//! - a host that is not Linux, and running as root;
//! - a bind address the configured interface (`KBF_TAILNET_IFACE`) does not carry
//!   (`ip -o -4 addr show dev <iface>`): the REAPI listener this unit starts is plain
//!   text, so the operator names the one network it serves;
//! - a REAPI or worker port another process holds (`ss -Hltn`), unless the service
//!   itself is active (a re-run);
//! - `pki/ca.pem`, `pki/server.pem` or `pki/server.key` missing; a `server.pem` that
//!   does not chain to `ca.pem` for serverAuth with `KBF_TLS_SERVER_NAME`, or a key that
//!   is not its own ([`super::certs`]); a key that is not mode 0600;
//! - an S3 environment file that is not mode 0600 or lacks `AWS_ACCESS_KEY_ID=` or
//!   `AWS_SECRET_ACCESS_KEY=` with a value (a quoted empty `""` or `''` is none); the PKI
//!   directory or that file inside a git work tree (a `.git` at or above it);
//! - with `--apply`, a store whose `/minio/health/live` does not answer 200 (a dry run
//!   only warns);
//! - a `--bin` that is not an executable ELF file for this host's CPU, whose
//!   `--version` fails, or whose `--help` lacks a flag the unit passes;
//! - a path or value the unit cannot carry as it is ([`super::carried`]).
//!
//! It warns when the user does not linger (`/var/lib/systemd/linger/<user>`): the
//! service would stop with the user's last session.
//!
//! The plan has each of these only when the host differs:
//! 1. the state directory and its `bin` and `log` directories (0700), the user unit
//!    directory (0755), and any directory missing above them (0755);
//! 2. `--bin` copied to `<state>/bin/kbf-server` (0755), the binary it replaces kept as
//!    `kbf-server.previous` (its mode);
//! 3. `kbf-pilot.slice` (the caps) and `kbf-pilot-server.service` (0644) in the user
//!    unit directory, rendered from compiled-in templates;
//! 4. when anything changed or the service is not active: `daemon-reload` (when
//!    something changed), `enable` and `restart`. After an apply that restarted it, the
//!    verb waits up to 30 s for kbf-server's start line (`kbf-server <version>
//!    reapi=...`) in `<state>/log/kbf-server.out.log` past where the file ended before,
//!    and fails (exit 1) with the last lines of the error log when none comes.
//!
//! `--uninstall` stops and disables the service (when systemd knows it), moves both
//! units to `<state>/removed/<unit>.<stamp>` and runs `daemon-reload` when it moved
//! one. The binary and the logs stay.

use std::io::Write;
use std::net::Ipv4Addr;

use super::config::ServerSettings;
use super::{
    ID, IP, PilotError, SS, SYSTEMCTL, carried, certs, command, inspect, note, query, stat, tool,
    under,
};
use crate::template::{self, Value, Values};
use kbf_host::exec::{Entry, Exec, succeeded};
use kbf_host::path::HostPath;
use kbf_host::plan::{Mode, Plan, Planner, RunAs, execute};
use kbf_host::step::{Cmd, HttpUrl, Perms};

/// The units, as systemd names them.
pub const SERVICE: &str = "kbf-pilot-server.service";
pub const SLICE: &str = "kbf-pilot.slice";

const SLICE_TEMPLATE: &str = include_str!("../../templates/pilot/kbf-pilot.slice");
const SERVICE_TEMPLATE: &str = include_str!("../../templates/pilot/kbf-pilot-server.service");
const LINGER: &str = "/var/lib/systemd/linger";
/// How many seconds the verb waits for the start line, one look a second.
pub const READY_SECS: u64 = 30;
/// The lines of the error log printed when the start line never comes.
const ERR_TAIL: usize = 20;

/// The OS and CPU the verb runs on (`std::env::consts`), a value so tests can pick.
#[derive(Clone, Copy, Debug)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Platform {
    pub const HOST: Platform = Platform {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    };
}

/// The user's directories, from `HOME` and `XDG_CONFIG_HOME` (systemd's user manager
/// reads its units from `$XDG_CONFIG_HOME/systemd/user`, `~/.config` when unset).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    pub home: HostPath,
    pub xdg_config: Option<HostPath>,
}

impl Home {
    /// `<state>`: `KBF_SERVER_STATE`, else `~/.local/share/kbf-pilot`.
    fn state(&self, configured: Option<&HostPath>) -> Result<HostPath, PilotError> {
        match configured {
            Some(s) => Ok(s.clone()),
            None => under(&self.home, ".local/share/kbf-pilot"),
        }
    }

    fn units(&self) -> Result<HostPath, PilotError> {
        match &self.xdg_config {
            Some(c) => under(c, "systemd/user"),
            None => under(&self.home, ".config/systemd/user"),
        }
    }
}

/// What `server-install` was asked for.
pub struct Install {
    pub settings: ServerSettings,
    /// The kbf-server binary to install.
    pub bin: HostPath,
    /// The directory holding `ca.pem`, `server.pem` and `server.key`; default
    /// `~/.config/kbf-pilot/pki`.
    pub pki: Option<HostPath>,
    /// The S3 key pair file; default `~/.config/kbf-pilot/s3.env`.
    pub s3_env: Option<HostPath>,
    pub home: Home,
}

/// A plan, and where to look for the start line after it is applied.
pub struct Planned {
    pub plan: Plan,
    pub ready: Option<Ready>,
}

/// The start line is looked for in `log` past `offset`, its length before the restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ready {
    pub log: HostPath,
    pub err_log: HostPath,
    pub offset: usize,
}

/// Check the host, then plan the directories, binary, units and service steps.
pub fn plan<E: Exec>(
    exec: &E,
    a: &Install,
    platform: Platform,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<Planned, PilotError> {
    user_on_linux(exec, platform)?;
    let s = &a.settings;
    let state = a.home.state(s.state.as_ref())?;
    let units = a.home.units()?;
    let pki = match &a.pki {
        Some(p) => p.clone(),
        None => under(&a.home.home, ".config/kbf-pilot/pki")?,
    };
    let s3_env = match &a.s3_env {
        Some(p) => p.clone(),
        None => under(&a.home.home, ".config/kbf-pilot/s3.env")?,
    };
    for (what, path) in [
        ("the state directory", &state),
        ("--pki", &pki),
        ("--s3-env-file", &s3_env),
    ] {
        carried(what, "unit", path.as_str(), false)?;
    }

    bound(exec, s.bind, &s.iface)?;
    note(out, format_args!("bind: {} on {}", s.bind, s.iface))?;
    let active = unit_is(exec, "is-active")?;
    ports_free(exec, &[s.reapi_port, s.worker_port], active, out)?;
    certificates(exec, &pki, &s.tls_server_name, out)?;
    s3_pair(exec, &s3_env)?;
    store_live(exec, &s.minio_endpoint, mode, out)?;
    linger(exec, out)?;

    let dest = under(&under(&state, "bin")?, "kbf-server")?;
    let args = exec_start(s, &dest, &pki)?;
    let bytes = supports(exec, &a.bin, &args, platform.arch, out)?;
    let log_dir = under(&state, "log")?;
    let slice = render(
        SLICE,
        SLICE_TEMPLATE,
        [
            ("MEMORY_MAX", Value::Text(s.memory_max.clone())),
            ("CPU_QUOTA", Value::Text(s.cpu_quota.clone())),
            ("TASKS_MAX", Value::Text(s.tasks_max.clone())),
        ],
    )?;
    let service = render(
        SERVICE,
        SERVICE_TEMPLATE,
        [
            ("S3_ENV", Value::Text(s3_env.to_string())),
            ("EXEC_START", Value::Text(args.join(" "))),
            ("LOG_DIR", Value::Text(log_dir.to_string())),
        ],
    )?;
    note(
        out,
        format_args!("service {SERVICE} runs: {}", args.join(" ")),
    )?;

    let mut planner = Planner::new(exec);
    parents(exec, &mut planner, &state)?;
    for dir in [&state, &under(&state, "bin")?, &log_dir] {
        planner.dir(dir.clone(), Perms::new(0o700), None)?;
    }
    parents(exec, &mut planner, &units)?;
    planner.dir(units.clone(), Perms::new(0o755), None)?;
    let mut changed = false;
    if let Some(Entry::File {
        perms, bytes: old, ..
    }) = inspect(exec, &dest)?
        && old != bytes
    {
        planner.file(under(&state, "bin/kbf-server.previous")?, perms, None, old)?;
    }
    changed |= planner.file(dest.clone(), Perms::new(0o755), None, bytes)?;
    for (name, text) in [(SLICE, slice), (SERVICE, service)] {
        let path = under(&units, name)?;
        if planner.file(
            path.clone(),
            Perms::new(0o644),
            None,
            text.clone().into_bytes(),
        )? {
            changed = true;
            note(out, format_args!("{path}: writes"))?;
            for line in text.lines() {
                note(out, format_args!("    | {line}"))?;
            }
        } else {
            note(out, format_args!("{path}: unchanged"))?;
        }
    }

    if !changed && active {
        note(out, format_args!("{SERVICE} is active and nothing changed"))?;
        return Ok(Planned {
            plan: planner.finish(),
            ready: None,
        });
    }
    if changed {
        planner.run(systemctl(exec, &["daemon-reload"]));
    }
    planner.run(systemctl(exec, &["enable", SERVICE]));
    planner.run(systemctl(exec, &["restart", SERVICE]));
    let log = under(&log_dir, "kbf-server.out.log")?;
    let offset = match stat(exec, &log)? {
        Some(m) => usize::try_from(m.len).unwrap_or(usize::MAX),
        None => 0,
    };
    Ok(Planned {
        plan: planner.finish(),
        ready: Some(Ready {
            log,
            err_log: under(&log_dir, "kbf-server.err.log")?,
            offset,
        }),
    })
}

/// Plan, print or carry the plan out, and after an apply that restarted kbf-server
/// wait for its start line, calling `pause` between looks (a second on a host).
pub fn run<E: Exec>(
    exec: &mut E,
    a: &Install,
    platform: Platform,
    mode: Mode,
    out: &mut dyn Write,
    pause: &dyn Fn(),
) -> Result<(), PilotError> {
    let planned = plan(&*exec, a, platform, mode, &mut *out)?;
    execute(&planned.plan, RunAs::User, mode, exec, &mut &mut *out)?;
    match planned.ready {
        Some(r) if mode == Mode::Apply => wait_ready(&*exec, &r, pause, out),
        _ => Ok(()),
    }
}

/// Look for the start line once a second for [`READY_SECS`]; without it, print the
/// error log's last lines and fail.
pub fn wait_ready(
    exec: &dyn Exec,
    r: &Ready,
    pause: &dyn Fn(),
    out: &mut dyn Write,
) -> Result<(), PilotError> {
    for _ in 0..READY_SECS {
        if let Some(Entry::File { bytes, .. }) = inspect(exec, &r.log)? {
            // A log shorter than before was rotated: all of it is new.
            let new = bytes.get(r.offset..).unwrap_or(&bytes);
            let text = String::from_utf8_lossy(new);
            if let Some(line) = text.lines().rfind(|l| l.contains(" reapi=")) {
                return note(out, format_args!("READY: {line}"));
            }
        }
        pause();
    }
    note(
        out,
        format_args!(
            "NOT READY after {READY_SECS} s. Last lines of {}:",
            r.err_log
        ),
    )?;
    if let Some(Entry::File { bytes, .. }) = inspect(exec, &r.err_log)? {
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines[lines.len().saturating_sub(ERR_TAIL)..] {
            note(out, format_args!("    {line}"))?;
        }
    }
    Err(PilotError::NotReady { secs: READY_SECS })
}

/// What `server-install --uninstall` was asked for.
pub struct Uninstall {
    /// `KBF_SERVER_STATE`; `None`: the default under `$HOME`.
    pub state: Option<HostPath>,
    pub home: Home,
}

/// Plan the rollback; `stamp` ends the names the units are moved to.
pub fn plan_uninstall<E: Exec>(
    exec: &E,
    a: &Uninstall,
    platform: Platform,
    stamp: &str,
    out: &mut dyn Write,
) -> Result<Plan, PilotError> {
    user_on_linux(exec, platform)?;
    let state = a.home.state(a.state.as_ref())?;
    let units = a.home.units()?;
    let mut planner = Planner::new(exec);
    if query(exec, &systemctl(exec, &["cat", SERVICE]))?
        .status
        .success()
    {
        planner.run(systemctl(exec, &["disable", "--now", SERVICE]));
    } else {
        note(out, format_args!("{SERVICE} is not installed"))?;
    }
    let removed = under(&state, "removed")?;
    let mut moved = false;
    for name in [SERVICE, SLICE] {
        let path = under(&units, name)?;
        if let Some(Entry::File { perms, bytes, .. }) = inspect(exec, &path)? {
            if !moved {
                parents(exec, &mut planner, &removed)?;
                planner.dir(removed.clone(), Perms::new(0o700), None)?;
            }
            let kept = under(&removed, &format!("{name}.{stamp}"))?;
            planner.file(kept, perms, None, bytes)?;
            planner.absent(path)?;
            moved = true;
        }
    }
    if moved {
        planner.run(systemctl(exec, &["daemon-reload"]));
    }
    note(
        out,
        format_args!("kept: {state} (the binary and the logs) and the bucket"),
    )?;
    Ok(planner.finish())
}

/// [`plan_uninstall`], then print or carry the plan out.
pub fn run_uninstall<E: Exec>(
    exec: &mut E,
    a: &Uninstall,
    platform: Platform,
    stamp: &str,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), PilotError> {
    let p = plan_uninstall(&*exec, a, platform, stamp, &mut *out)?;
    Ok(execute(&p, RunAs::User, mode, exec, &mut &mut *out)?)
}

fn user_on_linux(exec: &dyn Exec, platform: Platform) -> Result<(), PilotError> {
    if platform.os != "linux" {
        return Err(PilotError::NotLinux(platform.os.to_owned()));
    }
    if exec.euid() == 0 {
        return Err(PilotError::IsRoot);
    }
    Ok(())
}

/// `systemctl --user <args>`, reaching the user's manager through its runtime
/// directory (the command starts with an empty environment).
fn systemctl(exec: &dyn Exec, args: &[&str]) -> Cmd {
    let all: Vec<&str> = std::iter::once("--user")
        .chain(args.iter().copied())
        .collect();
    command(SYSTEMCTL, &all).env("XDG_RUNTIME_DIR", format!("/run/user/{}", exec.euid()))
}

/// `systemctl --user <verb> --quiet` of the service exited 0.
fn unit_is(exec: &dyn Exec, verb: &str) -> Result<bool, PilotError> {
    let cmd = systemctl(exec, &[verb, "--quiet", SERVICE]);
    Ok(query(exec, &cmd)?.status.success())
}

/// A query's standard output; a non-zero exit is an error naming its stderr.
fn checked(exec: &dyn Exec, cmd: &Cmd) -> Result<String, PilotError> {
    let out = succeeded(cmd, query(exec, cmd)?).map_err(|source| PilotError::Query {
        cmd: cmd.to_string(),
        source,
    })?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn bound(exec: &dyn Exec, bind: Ipv4Addr, iface: &str) -> Result<(), PilotError> {
    let cmd = command(IP, &["-o", "-4", "addr", "show", "dev", iface]);
    let out = query(exec, &cmd)?;
    let want = format!(" inet {bind}/");
    if out.status.success() && String::from_utf8_lossy(&out.stdout).contains(&want) {
        return Ok(());
    }
    Err(PilotError::NotCarried {
        iface: iface.to_owned(),
        bind: bind.to_string(),
    })
}

/// Each port is free, or the service is active (it holds its own ports on a re-run).
fn ports_free(
    exec: &dyn Exec,
    ports: &[u16],
    active: bool,
    out: &mut dyn Write,
) -> Result<(), PilotError> {
    // `ss -Hltn`: `<state> <recv-q> <send-q> <local address:port> <peer>`.
    let listening = checked(exec, &Cmd::new(tool(SS)).arg("-Hltn"))?;
    for &port in ports {
        let tail = format!(":{port}");
        let held = listening.lines().any(|l| {
            l.split_whitespace()
                .nth(3)
                .is_some_and(|local| local.ends_with(&tail))
        });
        if held && !active {
            return Err(PilotError::PortHeld(port));
        }
        if held {
            note(
                out,
                format_args!("port {port}: held by {SERVICE} (a re-run)"),
            )?;
        }
    }
    Ok(())
}

/// Refuse `dir` when it or a directory above it holds a `.git`.
fn outside_git(exec: &dyn Exec, dir: &str, named: &HostPath) -> Result<(), PilotError> {
    let mut d = dir;
    loop {
        let git = HostPath::new(&format!("{d}/.git"))?;
        if stat(exec, &git)?.is_some() {
            return Err(PilotError::InGitTree {
                path: named.clone(),
                git,
            });
        }
        match d.rfind('/') {
            Some(i) => d = &d[..i],
            None => return Ok(()),
        }
    }
}

/// A regular file's bytes, or [`PilotError::Missing`].
fn file(exec: &dyn Exec, path: &HostPath) -> Result<(Perms, Vec<u8>), PilotError> {
    match inspect(exec, path)? {
        Some(Entry::File { perms, bytes, .. }) => Ok((perms, bytes)),
        _ => Err(PilotError::Missing(path.clone())),
    }
}

fn private(path: &HostPath, perms: Perms) -> Result<(), PilotError> {
    if perms.bits() == 0o600 {
        Ok(())
    } else {
        Err(PilotError::NotPrivate {
            path: path.clone(),
            perms,
        })
    }
}

fn certificates(
    exec: &dyn Exec,
    pki: &HostPath,
    name: &str,
    out: &mut dyn Write,
) -> Result<(), PilotError> {
    outside_git(exec, pki.as_str(), pki)?;
    let (_, ca) = file(exec, &under(pki, "ca.pem")?)?;
    let server_pem = under(pki, "server.pem")?;
    let (_, server) = file(exec, &server_pem)?;
    let key_path = under(pki, "server.key")?;
    let (perms, key) = file(exec, &key_path)?;
    private(&key_path, perms)?;
    certs::check(&ca, &server, &key, name).map_err(|e| PilotError::Cert {
        path: server_pem,
        why: e.to_string(),
    })?;
    note(
        out,
        format_args!("certificates: server.pem chains to ca.pem (serverAuth, {name})"),
    )
}

fn s3_pair(exec: &dyn Exec, path: &HostPath) -> Result<(), PilotError> {
    let s = path.as_str();
    outside_git(exec, &s[..s.rfind('/').unwrap_or(0)], path)?;
    let (perms, bytes) = file(exec, path)?;
    private(path, perms)?;
    let text = String::from_utf8_lossy(&bytes);
    let pair = ["AWS_ACCESS_KEY_ID=", "AWS_SECRET_ACCESS_KEY="]
        .iter()
        .all(|key| {
            text.lines()
                .any(|l| l.strip_prefix(key).is_some_and(|v| !unquoted(v).is_empty()))
        });
    if pair {
        Ok(())
    } else {
        Err(PilotError::S3Env(path.clone()))
    }
}

/// `v` without one pair of enclosing double or single quotes, as systemd reads an
/// `EnvironmentFile` value: `""` and `''` are empty.
fn unquoted(v: &str) -> &str {
    for q in ["\"", "'"] {
        if let Some(inner) = v.strip_prefix(q).and_then(|v| v.strip_suffix(q)) {
            return inner;
        }
    }
    v
}

/// The store answers its liveness probe; a dry run only warns when it does not.
fn store_live(
    exec: &dyn Exec,
    endpoint: &str,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), PilotError> {
    let url = format!("{endpoint}/minio/health/live");
    let Ok(parsed) = HttpUrl::new(&url) else {
        return note(
            out,
            format_args!("store: {url} not checked (not an http:// URL)"),
        );
    };
    let got = match exec.http_status(&parsed) {
        Ok(200) => return note(out, format_args!("store: {url} answers")),
        Ok(code) => code.to_string(),
        Err(e) => e.to_string(),
    };
    if mode == Mode::Apply {
        return Err(PilotError::StoreDown { url, got });
    }
    note(
        out,
        format_args!("WARN: {url} answered {got}; --apply refuses until it answers 200"),
    )
}

fn linger(exec: &dyn Exec, out: &mut dyn Write) -> Result<(), PilotError> {
    let name = checked(exec, &Cmd::new(tool(ID)).arg("-un"))?;
    let name = name.trim();
    if stat(exec, &under(&tool(LINGER), name)?)?.is_none() {
        note(
            out,
            format_args!(
                "WARN: {name} does not linger: the service stops when the user's last session ends (loginctl enable-linger {name})"
            ),
        )?;
    }
    Ok(())
}

/// The unit's `ExecStart=` words, each one a unit line carries as it is.
fn exec_start(
    s: &ServerSettings,
    dest: &HostPath,
    pki: &HostPath,
) -> Result<Vec<String>, PilotError> {
    let mut args = vec![
        dest.to_string(),
        "--role".into(),
        "all".into(),
        "--store".into(),
        "s3".into(),
        "--s3-endpoint".into(),
        s.minio_endpoint.clone(),
        "--s3-bucket".into(),
        s.s3_bucket.clone(),
        "--s3-region".into(),
        s.s3_region.clone(),
        "--s3-prefix".into(),
        s.s3_prefix.clone(),
    ];
    if s.conditional_put {
        args.push("--s3-conditional-put".into());
    }
    args.extend([
        "--listen".into(),
        format!("{}:{}", s.bind, s.reapi_port),
        "--worker-listen".into(),
        format!("{}:{}", s.bind, s.worker_port),
        "--worker-tls-cert".into(),
        under(pki, "server.pem")?.to_string(),
        "--worker-tls-key".into(),
        under(pki, "server.key")?.to_string(),
        "--worker-client-ca".into(),
        under(pki, "ca.pem")?.to_string(),
    ]);
    for a in &args {
        carried("a kbf-server argument", "unit", a, false)?;
    }
    Ok(args)
}

/// The ELF `e_machine` of this host's CPU.
fn machine(arch: &str) -> Result<u16, PilotError> {
    match arch {
        "x86_64" => Ok(62),
        "aarch64" => Ok(183),
        other => Err(PilotError::UnknownArch(other.to_owned())),
    }
}

/// `bin` is an executable 64-bit little-endian ELF file for `arch` whose `--version`
/// succeeds and whose `--help` lists every `--flag` of `args`; its bytes.
fn supports(
    exec: &dyn Exec,
    bin: &HostPath,
    args: &[String],
    arch: &str,
    out: &mut dyn Write,
) -> Result<Vec<u8>, PilotError> {
    let bytes = match inspect(exec, bin)? {
        Some(Entry::File { perms, bytes, .. }) if perms.bits() & 0o111 != 0 => bytes,
        _ => return Err(PilotError::NotExecutable(bin.clone())),
    };
    let want = machine(arch)?.to_le_bytes();
    let elf = bytes.len() >= 20
        && bytes[..4] == *b"\x7fELF"
        && bytes[4..6] == [2, 1]
        && bytes[18..20] == want;
    if !elf {
        return Err(PilotError::WrongBinary {
            bin: bin.clone(),
            arch: arch.to_owned(),
        });
    }
    let version = checked(exec, &Cmd::new(bin.clone()).arg("--version"))?;
    let help = checked(exec, &Cmd::new(bin.clone()).arg("--help"))?;
    let listed: Vec<&str> = help
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .collect();
    if let Some(flag) = args
        .iter()
        .find(|a| a.starts_with("--") && !listed.contains(&a.as_str()))
    {
        return Err(PilotError::NoFlag {
            bin: bin.clone(),
            flag: flag.clone(),
        });
    }
    note(out, format_args!("binary: {}", version.trim()))?;
    Ok(bytes)
}

fn render<const N: usize>(
    name: &str,
    text: &str,
    values: [(&'static str, Value); N],
) -> Result<String, PilotError> {
    Ok(template::render(name, text, &Values::from(values))?)
}

/// Plan a directory (0755) for every missing directory above `path` not planned yet.
fn parents<E: Exec>(
    exec: &E,
    planner: &mut Planner<'_, E>,
    path: &HostPath,
) -> Result<(), PilotError> {
    let s = path.as_str();
    for (i, _) in s.match_indices('/').skip(1) {
        let dir = HostPath::new(&s[..i])?;
        let planned = planner
            .steps()
            .iter()
            .any(|step| matches!(step, kbf_host::Step::Mkdir { path, .. } if *path == dir));
        if !planned && stat(exec, &dir)?.is_none() {
            planner.dir(dir, Perms::new(0o755), None)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
