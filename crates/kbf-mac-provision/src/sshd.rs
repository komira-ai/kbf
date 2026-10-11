//! Key-only SSH as sshd uses it: the kbf drop-in, read first, and nothing before it.

use std::io;

use crate::items::{Ctx, Lines, Outcome, fail, pass};

const SSHD: &str = "/usr/sbin/sshd";
const SSHD_CONFIG: &str = "/etc/ssh/sshd_config";
const DROPIN_DIR: &str = "/etc/ssh/sshd_config.d";
/// `000-` sorts first, and sshd keeps the first value it reads for each keyword.
pub const DROPIN_NAME: &str = "000-kbf.conf";
pub const DROPIN: &str = "/etc/ssh/sshd_config.d/000-kbf.conf";
pub const INCLUDE: &str = "Include /etc/ssh/sshd_config.d/*";
const ROOT_FILE: &str = "-rw-r--r-- 0 0";

/// The drop-in the profile asks for.
pub fn dropin(password_auth: bool) -> String {
    let p = if password_auth { "yes" } else { "no" };
    format!("PasswordAuthentication {p}\nKbdInteractiveAuthentication {p}\nPermitRootLogin no\n")
}

/// The keywords sshd_config must not set before its `Include` line.
const EARLY: &[&str] = &[
    "passwordauthentication",
    "kbdinteractiveauthentication",
    "challengeresponseauthentication",
    "permitrootlogin",
];

/// The keywords `sshd -T` must report at the drop-in's values.
const EFFECTIVE: &[&str] = &[
    "passwordauthentication",
    "kbdinteractiveauthentication",
    "permitrootlogin",
];

/// The files in the drop-in directory that sshd reads before the drop-in.
fn early_dropins(c: &Ctx<'_>) -> String {
    c.mac
        .list(DROPIN_DIR)
        .into_iter()
        .take_while(|name| name != DROPIN_NAME)
        .filter(|name| c.mac.exists(&format!("{DROPIN_DIR}/{name}")))
        .map(|name| format!(" {name}"))
        .collect()
}

/// The first word of each line of sshd_config before its `Include` line whose
/// keyword (in any case, before any `=`) is one of [`EARLY`].
fn early_keywords(config: &str) -> String {
    config
        .split('\n')
        .take_while(|line| *line != INCLUDE)
        .filter_map(|line| line.split_whitespace().next())
        .filter(|word| {
            let lower = word.to_ascii_lowercase();
            let keyword = lower.split('=').next().unwrap_or_default();
            EARLY.contains(&keyword)
        })
        .map(|word| format!(" {word}"))
        .collect()
}

/// Lines as `sshd -T` prints them, lower case and sorted, joined by `;`.
fn sorted_lower(lines: impl Iterator<Item = String>) -> String {
    let mut lines: Vec<String> = lines.map(|l| l.to_ascii_lowercase()).collect();
    lines.sort();
    lines.join(";")
}

pub(crate) fn ssh_password_auth(c: &Ctx<'_>) -> Outcome {
    let want = dropin(c.value() == "1");
    let config = String::from_utf8_lossy(&c.mac.read(SSHD_CONFIG).unwrap_or_default()).into_owned();
    let early = early_dropins(c);
    let keywords = early_keywords(&config);
    let joined = want.lines().collect::<Vec<_>>().join(";");
    if !config.split('\n').any(|line| line == INCLUDE) {
        return fail(format!("{SSHD_CONFIG} lacks the line '{INCLUDE}'"));
    }
    if c.mac.read(DROPIN).as_deref() != Some(want.as_bytes()) {
        return fail(format!("{DROPIN} differs from the profile"));
    }
    let owner = c.mac.ls(DROPIN);
    if owner != ROOT_FILE {
        return fail(format!("{DROPIN} is {owner}, want {ROOT_FILE}"));
    }
    if !early.is_empty() {
        return fail(format!(
            "sshd reads{early} before {DROPIN_NAME} (apply does not remove them)"
        ));
    }
    if !keywords.is_empty() {
        return fail(format!(
            "{SSHD_CONFIG} sets{keywords} before its Include line (apply does not edit it)"
        ));
    }
    if c.mac.euid() != 0 {
        return pass(format!("{joined} (sshd -T not run: needs root)"));
    }
    let wanted = sorted_lower(want.lines().map(str::to_owned));
    let reported = sorted_lower(c.mac.out(SSHD, &["-T"]).lines().filter_map(|line| {
        let mut words = line.split_whitespace();
        let keyword = words.next()?;
        EFFECTIVE
            .contains(&keyword)
            .then(|| format!("{keyword} {}", words.next().unwrap_or_default()))
    }));
    if reported == wanted {
        pass(format!("{joined} (sshd -T agrees)"))
    } else {
        fail(format!("sshd -T reports '{reported}', want '{wanted}'"))
    }
}

/// Writes the drop-in, root's, mode 644, when sshd_config has the `Include` line and
/// the drop-in is not already that file. Drop-ins read before it and keywords before
/// the `Include` line are left: `check` reports them, and they are the operator's.
pub(crate) fn fix_ssh_password_auth(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let config = String::from_utf8_lossy(&c.mac.read(SSHD_CONFIG).unwrap_or_default()).into_owned();
    if !config.split('\n').any(|line| line == INCLUDE) {
        return Ok(());
    }
    let want = dropin(c.value() == "1");
    if c.mac.read(DROPIN).as_deref() == Some(want.as_bytes()) && c.mac.ls(DROPIN) == ROOT_FILE {
        return Ok(());
    }
    c.mac.mkdir(DROPIN_DIR)?;
    c.mac.replace(DROPIN, want.as_bytes(), 0, 0, 0o644)?;
    out.change(c.key, format!("wrote {DROPIN}"));
    Ok(())
}
