//! `mdm verify`: read-only checks that the MDM is up and exposed only as intended.
//!
//! Each check prints `ok    <check>` or `FAIL  <check>: <why>`, then a summary line; the
//! verb exits 0 when every check passes and 1 otherwise. It changes nothing. Run it
//! with sudo: as another user the checks of the 0700 state and the 0600 secret files
//! fail, since it cannot see them.
//!
//! The checks:
//! - the SCEP server, NanoHUB, depserver and the proxy are active, and so are the
//!   certificate renewal timer and the proxy's reload on renewal;
//! - NanoHUB, SCEP and depserver listen on `127.0.0.1` at their ports only, the proxy on
//!   `MDM_BIND:443` only, and nothing on port 80;
//! - through the proxy, at `MDM_BIND:443` with the real name (the server must prove it):
//!   `/version` answers; `/scep?operation=GetCACert` returns a DER certificate (the
//!   SCEP CA); NanoHUB's `/api/` and NanoDEP's `/v1/` answer 404; `/enroll` answers 200;
//! - the TLS certificate in `MDM_STATE/lego/certificates/` is issued by Let's Encrypt
//!   and valid for 21 more days;
//! - `MDM_DNS_NAME` resolves to `MDM_BIND` here (`getent ahostsv4`);
//! - `MDM_STATE` is a 0700 directory of `MDM_USER`'s; `MDM_ETC` a 0750 directory of
//!   root's; `nanohub.env`, `nanodep.env`, `scep.env` and `dns.env` under it are 0600
//!   files of root's;
//! - the push topic is recorded (`MDM_STATE/push-topic`) and the erase signer installed
//!   (`MDM_ETC/erase_allowed_signers`), both not empty; the signer file and every
//!   directory above it are root's and not writable by group or others.

use std::collections::BTreeSet;
use std::fmt;
use std::io::Write;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use super::MdmError;
use super::config::MdmConfig;
use super::render::{CERT_TIMER, DEPSERVER, NANOHUB, PROXY, RELOAD_PATH, SCEP};
use crate::https::{Client, HttpsUrl};
use crate::linux::host::{self, SS, tool, under, unit_is};
use kbf_host::exec::{Exec, Kind, Meta};
use kbf_host::path::HostPath;
use kbf_host::step::{Cmd, Perms};
use kbf_host::user::{self, GETENT};

/// How long the certificate must stay valid.
pub const VALID_FOR_DAYS: i64 = 21;
/// The secret files, all 0600 and root's.
pub const SECRET_FILES: [&str; 4] = ["nanohub.env", "nanodep.env", "scep.env", "dns.env"];

/// What `mdm verify` checks against.
pub struct Verify<'a> {
    pub config: &'a MdmConfig,
    pub client: &'a Client,
    /// Where the proxy is dialled: `MDM_BIND:443` on a host.
    pub proxy: SocketAddr,
    pub now: SystemTime,
}

/// One check and how it went: what it saw when it passed, or why it failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub result: Result<String, String>,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.result {
            Ok(seen) if seen.is_empty() => write!(f, "ok    {}", self.name),
            Ok(seen) => write!(f, "ok    {}: {seen}", self.name),
            Err(why) => write!(f, "FAIL  {}: {why}", self.name),
        }
    }
}

type Outcome = Result<String, String>;

/// Every check, in the order they print.
pub fn checks<E: Exec>(exec: &E, v: &Verify<'_>) -> Result<Vec<Check>, MdmError> {
    let c = v.config;
    let name = &c.dns_name;
    let crt = under(&c.state, &format!("lego/certificates/{name}.crt"))?;
    let mut secrets = Vec::new();
    for file in SECRET_FILES {
        secrets.push(under(&c.etc, file)?);
    }
    let topic = under(&c.state, "push-topic")?;
    let signers = under(&c.etc, "erase_allowed_signers")?;
    let mut urls = Vec::new();
    for path in [
        "/version",
        "/scep?operation=GetCACert",
        "/api/v1/nanomdm/push/x",
        "/v1/tokenpki/x",
        "/enroll",
    ] {
        urls.push(HttpsUrl::new(&format!("https://{name}{path}"))?);
    }
    let mut rows = Vec::new();
    let mut row = |name: String, result: Outcome| rows.push(Check { name, result });
    for unit in [SCEP, NANOHUB, DEPSERVER, PROXY, CERT_TIMER, RELOAD_PATH] {
        row(format!("{unit} is active"), active(exec, unit));
    }
    let listening = host::checked(exec, &Cmd::new(tool(SS)).arg("-Hltn")).map_err(text);
    let bind = c.bind.to_string();
    for (what, port, addr) in [
        ("NanoHUB", c.nanohub_port, "127.0.0.1"),
        ("SCEP", c.scep_port, "127.0.0.1"),
        ("depserver", c.dep_port, "127.0.0.1"),
        ("the proxy", 443, bind.as_str()),
    ] {
        let only = listening.clone().and_then(|t| listens_only(&t, port, addr));
        row(format!("{what} listens on {addr}:{port} only"), only);
    }
    let none = listening.and_then(|t| match listeners(&t, 80) {
        l if l.is_empty() => Ok(String::new()),
        l => Err(format!("listening on {}", join(&l))),
    });
    row("nothing listens on port 80".to_owned(), none);

    let get = |i: usize| -> Result<(u16, Vec<u8>), String> {
        let mut body = Vec::new();
        let answer = v
            .client
            .get(&urls[i], Some(v.proxy), &mut body)
            .map_err(text)?;
        Ok((answer.status, body))
    };
    let version = get(0).and_then(|(status, body)| {
        let said = String::from_utf8_lossy(&body).trim().to_owned();
        match status {
            200 if said.contains("version") => Ok(said),
            _ => Err(format!("answered {status}: {said:?}")),
        }
    });
    row(format!("https://{name}/version answers"), version);
    let ca = get(1).and_then(|(status, der)| match status {
        200 => scep_ca(&der),
        _ => Err(format!("answered {status}")),
    });
    row(format!("https://{name}/scep returns the SCEP CA"), ca);
    for (i, what, path, want) in [
        (
            2,
            "NanoHUB's API is not served",
            "/api/v1/nanomdm/push/x",
            404,
        ),
        (3, "NanoDEP is not served", "/v1/tokenpki/x", 404),
        (4, "the enrollment profile is served", "/enroll", 200),
    ] {
        let got = get(i).and_then(|(status, _)| match status {
            s if s == want => Ok(String::new()),
            s => Err(format!("answered {s}, not {want}")),
        });
        row(format!("{what} (https://{name}{path}: {want})"), got);
    }

    let cert = file_bytes(exec, &crt).and_then(|pem| certificate(&pem, v.now));
    let (issuer, expiry) = match cert {
        Ok((issuer, expiry)) => (issuer, expiry),
        Err(e) => (Err(e.clone()), Err(e)),
    };
    row(
        "the certificate is issued by Let's Encrypt".to_owned(),
        issuer,
    );
    row(
        format!("the certificate is valid for {VALID_FOR_DAYS} more days"),
        expiry,
    );
    row(
        format!("{name} resolves to {} here", c.bind),
        resolves(exec, name, &bind),
    );

    let state = user::lookup(exec, &c.user)
        .map_err(text)
        .and_then(|u| u.ok_or(format!("no user {}", c.user)))
        .and_then(|u| mode_is(exec, &c.state, Kind::Dir, 0o700, u.uid));
    row(
        format!("{} is a 0700 directory of {}", c.state, c.user),
        state,
    );
    row(
        format!("{} is a 0750 directory of root", c.etc),
        mode_is(exec, &c.etc, Kind::Dir, 0o750, 0),
    );
    for (file, path) in SECRET_FILES.into_iter().zip(secrets) {
        let ok = mode_is(exec, &path, Kind::File, 0o600, 0);
        row(format!("{}/{file} is a 0600 file of root", c.etc), ok);
    }
    row(
        "the push topic is recorded".to_owned(),
        not_empty(exec, &topic),
    );
    row(
        "the erase signer is installed".to_owned(),
        not_empty(exec, &signers),
    );
    let trusted = root_only(exec, &signers);
    row(
        "the erase signer and every directory above it are root's alone".to_owned(),
        trusted,
    );
    Ok(rows)
}

/// Print every check and the summary; whether all passed.
pub fn run<E: Exec>(exec: &E, v: &Verify<'_>, out: &mut dyn Write) -> Result<bool, MdmError> {
    if exec.euid() != 0 {
        say(
            out,
            "NOTE: not root: the checks of root-only files fail; run it with sudo",
        )?;
    }
    let rows = checks(exec, v)?;
    let fails = rows.iter().filter(|r| r.result.is_err()).count();
    for r in &rows {
        say(out, &r.to_string())?;
    }
    if fails == 0 {
        say(out, "verify: all checks passed")?;
    } else {
        say(out, &format!("verify: {fails} check(s) failed"))?;
    }
    Ok(fails == 0)
}

/// An error, as the text of a failed check.
fn text(e: impl fmt::Display) -> String {
    e.to_string()
}

fn say(out: &mut dyn Write, line: &str) -> Result<(), MdmError> {
    writeln!(out, "{line}").map_err(MdmError::Write)
}

fn active<E: Exec>(exec: &E, unit: &str) -> Outcome {
    if unit_is(exec, "is-active", unit).map_err(text)? {
        return Ok(String::new());
    }
    Err("not active".to_owned())
}

/// The local addresses (without the port) that listen on `port` in `ss -Hltn` output.
fn listeners(text: &str, port: u16) -> BTreeSet<String> {
    let tail = format!(":{port}");
    text.lines()
        .filter_map(|l| l.split_whitespace().nth(3))
        .filter_map(|local| local.strip_suffix(&tail))
        .map(str::to_owned)
        .collect()
}

fn listens_only(text: &str, port: u16, addr: &str) -> Outcome {
    let got = listeners(text, port);
    if got.len() == 1 && got.contains(addr) {
        return Ok(String::new());
    }
    if got.is_empty() {
        return Err("nothing listens there".to_owned());
    }
    Err(format!("listening on {}", join(&got)))
}

fn join(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// The SCEP CA's DER certificate; its subject.
fn scep_ca(der: &[u8]) -> Outcome {
    match x509_parser::parse_x509_certificate(der) {
        Ok(([], cert)) => Ok(cert.subject().to_string()),
        _ => Err(format!(
            "{} bytes that are not one DER certificate",
            der.len()
        )),
    }
}

/// Of the PEM certificate in `pem`: whether its issuer is Let's Encrypt, and whether it
/// is valid for [`VALID_FOR_DAYS`] more days from `now`.
fn certificate(pem: &[u8], now: SystemTime) -> Result<(Outcome, Outcome), String> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(pem).map_err(|e| format!("not PEM: {e}"))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| format!("not a certificate: {e}"))?;
    let issuer = cert.issuer().to_string();
    let by_le = if cert
        .issuer()
        .iter_organization()
        .any(|o| o.as_str() == Ok("Let's Encrypt"))
    {
        Ok(issuer)
    } else {
        Err(format!(
            "issued by {issuer} (a staging certificate, or none made yet)"
        ))
    };
    let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let left = cert.validity().not_after.timestamp() - i64::try_from(now).unwrap_or(i64::MAX);
    let days = left.div_euclid(86_400);
    let lasts = if days >= VALID_FOR_DAYS {
        Ok(format!("{days} days left"))
    } else {
        Err(format!("{days} days left"))
    };
    Ok((by_le, lasts))
}

fn resolves<E: Exec>(exec: &E, name: &str, bind: &str) -> Outcome {
    let cmd = Cmd::new(tool(GETENT)).arg("ahostsv4").arg("--").arg(name);
    let out = host::query(exec, &cmd).map_err(text)?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // `<addr> STREAM <name>`, first line.
    let first = text.split_whitespace().next().unwrap_or("nothing");
    if first == bind {
        return Ok(String::new());
    }
    Err(format!(
        "resolves to {first}: the A record, or a resolver that drops private answers"
    ))
}

fn meta<E: Exec>(exec: &E, path: &HostPath) -> Result<Meta, String> {
    host::stat(exec, path)
        .map_err(text)?
        .ok_or(format!("{path} does not exist"))
}

/// `path` is a `kind` with exactly `perms`, owned by `uid`.
fn mode_is<E: Exec>(exec: &E, path: &HostPath, kind: Kind, perms: u32, uid: u32) -> Outcome {
    let m = meta(exec, path)?;
    if m.kind == kind && m.perms == Perms::new(perms) && m.owner.uid == uid {
        return Ok(String::new());
    }
    Err(format!(
        "is a {:?} with mode {}, owner uid {}",
        m.kind, m.perms, m.owner.uid
    ))
}

fn not_empty<E: Exec>(exec: &E, path: &HostPath) -> Outcome {
    match meta(exec, path)? {
        Meta {
            kind: Kind::File,
            len,
            ..
        } if len > 0 => Ok(String::new()),
        _ => Err(format!("{path} is empty or not a file")),
    }
}

/// The file at `path` and every directory above it are root's and not writable by
/// group or others; a link anywhere fails (it is not followed).
fn root_only<E: Exec>(exec: &E, path: &HostPath) -> Outcome {
    let s = path.as_str();
    // Every directory above the file but `/`, which a `HostPath` cannot name.
    let dirs = s
        .match_indices('/')
        .skip(1)
        .map(|(i, _)| (&s[..i], Kind::Dir));
    for (p, kind) in dirs.chain([(s, Kind::File)]) {
        let m = meta(
            exec,
            &HostPath::new(p).expect("a part of a HostPath is one"),
        )?;
        if m.kind != kind || m.owner.uid != 0 || m.perms.bits() & 0o022 != 0 {
            return Err(format!(
                "{p} is a {:?} with mode {}, owner uid {}",
                m.kind, m.perms, m.owner.uid
            ));
        }
    }
    Ok(String::new())
}

fn file_bytes<E: Exec>(exec: &E, path: &HostPath) -> Result<Vec<u8>, String> {
    match host::inspect(exec, path).map_err(text)? {
        Some(kbf_host::exec::Entry::File { bytes, .. }) => Ok(bytes),
        _ => Err(format!("{path} is not a file")),
    }
}

#[cfg(test)]
#[path = "verify_tests.rs"]
mod tests;
