//! `mdm-host verify`: read-only checks that the MDM is up and exposed only as intended, and that
//! the installed files are what `mdm-core` renders for the settings.
//!
//! Each check prints `ok    <check>` or `FAIL  <check>: <why>`. Then one line per file the
//! kit installs (the slice, the services, the Caddyfile, and the certificate job's units when
//! the ACME settings are given): `same`, `differs` (with the sha256 of the installed and of
//! the rendered text), `missing`, `wrong` (not a regular file, or not the mode and owner the
//! install plan gives it: a link is never followed) and `unreadable` for a file this user may
//! not read. The comparison leaves out comment lines, so a host installed by a tool whose
//! templates are worded differently compares equal when the settings are the same. Each
//! `--ignore LINE` leaves out of both texts the lines equal to LINE, whole (a line that holds
//! LINE and more is kept), and every line left out is printed under its file; a LINE that is
//! empty or holds a line break is refused. A summary line ends the output. The verb exits 0
//! when everything passes and 1 otherwise. It changes nothing, and needs no root for
//! world-readable files; the checks of the 0700 state and the 0600 secrets fail for another
//! user, since they cannot be seen.
//!
//! The checks:
//! - the SCEP server, NanoHUB, depserver and the proxy are active, and so are the certificate
//!   renewal timer and the proxy's reload on renewal;
//! - NanoHUB, SCEP and depserver listen on `127.0.0.1` at their ports only, the proxy on
//!   `MDM_BIND:443` only, and nothing on port 80;
//! - the TLS certificate in `MDM_STATE/lego/certificates/` exists and is valid for 21 more days;
//! - `MDM_STATE` is a 0700 directory of `MDM_USER`'s; `MDM_ETC` a 0750 directory of root's;
//!   `nanohub.env`, `nanodep.env`, `scep.env` and `dns.env` under it are 0600 files of root's.

use std::fmt;
use std::io::Write;

use host_step::{Account, Cmd, Host, Kind, SYSTEMCTL, ids};
use mdm_core::acme::Acme;
use mdm_core::config::MdmConfig;
use mdm_core::render::{self, UnitNames, render_tls, strip_comments};
use sha2::{Digest, Sha256};

use crate::install::{SS_PATH, UNITS_DIR};
use crate::{Env, MdmError};

/// How long the certificate must stay valid.
pub const VALID_FOR_DAYS: u64 = 21;
/// The secret files, all 0600 and root's.
pub const SECRET_FILES: [&str; 4] = ["nanohub.env", "nanodep.env", "scep.env", "dns.env"];
const OPENSSL: &str = "/usr/bin/openssl";
const NOT_ROOT: &str = "NOTE: not root: the checks of root-only files fail; run it as root";
const NO_ACME: &str = "skipped  the certificate job's units: the settings have no ACME keys";

/// What `verify` checks against.
pub struct Verify<'a> {
    /// The settings.
    pub config: &'a MdmConfig,
    /// The ACME settings, when the file has them: the certificate job's units are compared.
    pub acme: Option<&'a Acme>,
    /// Whole lines left out of both texts of every file before they are compared.
    pub ignore: &'a [String],
}

/// One check and how it went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// What was checked.
    pub name: String,
    /// What it saw when it passed, or why it failed.
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

fn text(e: impl fmt::Display) -> String {
    e.to_string()
}

/// Every health check, in the order they print.
pub fn checks(host: &dyn Host, v: &Verify<'_>) -> Vec<Check> {
    let c = v.config;
    let n = UnitNames::new(c);
    let mut rows = Vec::new();
    let mut row = |name: String, result: Outcome| rows.push(Check { name, result });
    for unit in [
        &n.scep,
        &n.nanohub,
        &n.depserver,
        &n.proxy,
        &n.cert_timer,
        &n.reload_path,
    ] {
        row(format!("{unit} is active"), active(host, unit));
    }
    let listening = host.run(&Cmd::new(SS_PATH).arg("-Hltn")).map_err(text);
    let listening = listening.and_then(|o| match o.code {
        0 => Ok(o.stdout),
        code => Err(format!("ss exited with status {code}")),
    });
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
    let none = listening.and_then(|t| match listeners(&t, 80).as_slice() {
        [] => Ok(String::new()),
        some => Err(format!("listening on {}", some.join(", "))),
    });
    row("nothing listens on port 80".to_owned(), none);
    let crt = format!("{}/lego/certificates/{}.crt", c.state, c.dns_name);
    row(
        format!("the certificate is valid for {VALID_FOR_DAYS} more days"),
        certificate(host, &crt),
    );
    row(
        format!("{} is a 0700 directory of {}", c.state, c.user),
        state_dir(host, c),
    );
    row(
        format!("{} is a 0750 directory of root", c.etc),
        mode_is(host, c.etc.as_str(), Kind::Dir, 0o750, 0),
    );
    for file in SECRET_FILES {
        let path = format!("{}/{file}", c.etc);
        row(
            format!("{path} is a 0600 file of root"),
            mode_is(host, &path, Kind::File, 0o600, 0),
        );
    }
    rows
}

fn active(host: &dyn Host, unit: &str) -> Outcome {
    let out = host
        .run(&Cmd::new(SYSTEMCTL).arg("is-active").arg(unit))
        .map_err(text)?;
    match out.code {
        0 => Ok(String::new()),
        _ => Err(format!("is {}", out.stdout.trim())),
    }
}

/// The local addresses (`addr:port`) of the listeners on `port` in `ss -Hltn` output.
fn listeners(ss: &str, port: u16) -> Vec<&str> {
    let tail = format!(":{port}");
    ss.lines()
        .filter_map(|l| l.split_whitespace().nth(3))
        .filter(|local| local.ends_with(&tail))
        .collect()
}

fn listens_only(ss: &str, port: u16, addr: &str) -> Outcome {
    let want = format!("{addr}:{port}");
    match listeners(ss, port).as_slice() {
        [one] if *one == want => Ok(String::new()),
        [] => Err("nothing listens there".to_owned()),
        some => Err(format!("listening on {}", some.join(", "))),
    }
}

fn certificate(host: &dyn Host, crt: &str) -> Outcome {
    host.stat(crt)
        .map_err(text)?
        .ok_or(format!("{crt} is not there"))?;
    let secs = VALID_FOR_DAYS * 24 * 3600;
    let cmd =
        Cmd::new(OPENSSL).args(&["x509", "-noout", "-checkend", &secs.to_string(), "-in", crt]);
    match host.run(&cmd).map_err(text)?.code {
        0 => Ok(String::new()),
        _ => Err("expires sooner, or is not a certificate".to_owned()),
    }
}

fn state_dir(host: &dyn Host, c: &MdmConfig) -> Outcome {
    let ids = ids(host, &Account::new(&c.user, &c.user)).map_err(text)?;
    let (uid, _) = ids.ok_or(format!("no user {}", c.user))?;
    mode_is(host, c.state.as_str(), Kind::Dir, 0o700, uid)
}

fn mode_is(host: &dyn Host, path: &str, kind: Kind, mode: u32, uid: u32) -> Outcome {
    let meta = host
        .stat(path)
        .map_err(text)?
        .ok_or(format!("{path} is not there"))?;
    let got = (meta.kind, meta.mode, meta.uid);
    match got == (kind, mode, uid) {
        true => Ok(String::new()),
        false => Err(format!(
            "is {:?} {:04o} uid {}, not {kind:?} {mode:04o} uid {uid}",
            got.0, got.1, got.2
        )),
    }
}

/// How an installed file compares with the render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Compared {
    /// The same, comments aside.
    Same,
    /// Different: the sha256 of the installed text and of the rendered text.
    Differs {
        /// The installed text's.
        installed: String,
        /// The rendered text's.
        rendered: String,
    },
    /// Not installed.
    Missing,
    /// Not a regular file, or not the mode and owner `install` gives it (a link whose target
    /// holds the same text, a 0666 file, a file of another user).
    Wrong(String),
    /// Not readable by this user.
    Unreadable(String),
}

impl Compared {
    /// Whether the file is as rendered.
    pub fn is_same(&self) -> bool {
        *self == Compared::Same
    }
}

/// A file `install` or `tls` makes: where, what text, and the mode and owner it is given.
#[derive(Clone, Debug)]
pub struct Want {
    /// Where.
    pub path: String,
    /// The rendered text.
    pub text: String,
    /// The permission bits.
    pub mode: u32,
    /// The owner.
    pub owner: Account,
}

fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// `text` without comment lines and without the lines equal to one of `ignore`; the lines
/// left out (the second part).
fn plain(text: &str, side: &str, ignore: &[String]) -> (String, Vec<String>) {
    let mut kept = String::new();
    let mut dropped = Vec::new();
    for line in strip_comments(text).lines() {
        match ignore.iter().any(|i| i == line) {
            true => dropped.push(format!("{side}: {line}")),
            false => kept.push_str(&format!("{line}\n")),
        }
    }
    (kept, dropped)
}

/// Compare the file `want` names with what is installed: it must be a regular file with its
/// mode and owner (a link is not followed), and its text must equal the rendered text. Comment
/// lines are left out of both, and so is every line equal to one of `ignore`; the lines left
/// out are the second part.
pub fn compare(host: &dyn Host, want: &Want, ignore: &[String]) -> (Compared, Vec<String>) {
    let meta = match host.stat(&want.path) {
        Ok(Some(meta)) => meta,
        Ok(None) => return (Compared::Missing, Vec::new()),
        Err(e) => return (Compared::Unreadable(e.to_string()), Vec::new()),
    };
    let known = ids(host, &want.owner).ok().flatten();
    let owner = known.unwrap_or((u32::MAX, u32::MAX));
    let have = (meta.kind, meta.mode, (meta.uid, meta.gid));
    if have != (Kind::File, want.mode, owner) {
        let why = format!(
            "is {:?} {:04o} owner {}:{}, not File {:04o} owner {}:{}",
            have.0, have.1, meta.uid, meta.gid, want.mode, want.owner.user, want.owner.group
        );
        return (Compared::Wrong(why), Vec::new());
    }
    match host.read(&want.path) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes.unwrap_or_default()).into_owned();
            let (installed, mut dropped) = plain(&text, "installed", ignore);
            let (rendered, more) = plain(&want.text, "rendered", ignore);
            dropped.extend(more);
            let (installed, rendered) = (sha(&installed), sha(&rendered));
            let how = match installed == rendered {
                true => Compared::Same,
                false => Compared::Differs {
                    installed,
                    rendered,
                },
            };
            (how, dropped)
        }
        Err(e) => (Compared::Unreadable(e.to_string()), Vec::new()),
    }
}

/// Every installed file compared with the render, in install order: the path, how it
/// compares and the lines left out of the comparison.
pub fn files(
    host: &dyn Host,
    v: &Verify<'_>,
) -> Result<Vec<(String, Compared, Vec<String>)>, MdmError> {
    let c = v.config;
    let rendered = render::render(c)?;
    let root = Account::root();
    let mut wanted: Vec<Want> = Vec::new();
    let mut add = |path: String, text: String, mode: u32, owner: &Account| {
        wanted.push(Want {
            path,
            text,
            mode,
            owner: owner.clone(),
        });
    };
    for unit in &rendered.units {
        add(
            format!("{UNITS_DIR}/{}", unit.name),
            unit.text.clone(),
            0o644,
            &root,
        );
    }
    let caddy_owner = Account::new("root", &c.user);
    add(
        format!("{}/{}", c.etc, render::CADDYFILE_NAME),
        rendered.caddyfile,
        0o640,
        &caddy_owner,
    );
    let tls = match v.acme {
        Some(a) => render_tls(c, a)?,
        None => Vec::new(),
    };
    for unit in tls {
        add(
            format!("{UNITS_DIR}/{}", unit.name),
            unit.text,
            0o644,
            &root,
        );
    }
    Ok(wanted
        .iter()
        .map(|w| {
            let (how, dropped) = compare(host, w, v.ignore);
            (w.path.clone(), how, dropped)
        })
        .collect())
}

/// Print every check, the file comparison and the summary; whether all passed.
///
/// # Errors
/// An `--ignore` value that is empty or holds a line break; the output could not be written.
pub fn run(env: &Env<'_>, v: &Verify<'_>, out: &mut dyn Write) -> Result<bool, MdmError> {
    let host = env.host;
    for line in v.ignore {
        if line.is_empty() || line.contains(['\n', '\r']) {
            return Err(MdmError::BadIgnore(line.clone()));
        }
        let note = format!("ignoring   every line equal to {line:?}, on both sides");
        writeln!(out, "{note}")?;
    }
    if host.euid() != 0 {
        writeln!(out, "{NOT_ROOT}")?;
    }
    let mut fails = 0;
    for row in checks(host, v) {
        fails += usize::from(row.result.is_err());
        writeln!(out, "{row}")?;
    }
    if v.acme.is_none() {
        writeln!(out, "{NO_ACME}")?;
    }
    for (path, how, dropped) in files(host, v)? {
        fails += usize::from(!how.is_same());
        let line = match how {
            Compared::Same => format!("same       {path}"),
            Compared::Differs {
                installed,
                rendered,
            } => format!(
                "differs    {path}: installed sha256 {installed}, rendered sha256 {rendered}"
            ),
            Compared::Missing => format!("missing    {path}"),
            Compared::Wrong(why) => format!("wrong      {path}: {why}"),
            Compared::Unreadable(why) => format!("unreadable {path}: {why}"),
        };
        writeln!(out, "{line}")?;
        for left_out in dropped {
            writeln!(out, "ignored    {path}: {left_out}")?;
        }
    }
    match fails {
        0 => writeln!(out, "verify: all checks passed")?,
        n => writeln!(out, "verify: {n} check(s) failed")?,
    }
    Ok(fails == 0)
}
