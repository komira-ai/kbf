//! `pilot buckconfig`: point one linked worktree's buck2 at the pilot kbf-server.
//!
//! It writes the compiled-in overlay `templates/pilot/buckconfig.local`, filled from
//! the settings, to `<worktree>/.buckconfig.local`. A value there wins over the
//! checkout's other config files, so every remote action of that worktree, and only
//! of it, goes to the pilot.
//!
//! **It never overwrites or removes a file it did not write.** Every overlay starts
//! with the line [`MARKER`]; an existing `.buckconfig.local` that does not (or a link,
//! or anything else that is not a regular file) is refused, and left alone, before
//! any step is planned. An overlay with the same bytes and mode is left as it is.
//!
//! It refuses, too:
//! - a property set kbf would not route to its OS ([`routes_to`]): one that names no
//!   `OSFamily`, names it twice, or names another OS; and a darwin `label.<k>=<v>` that
//!   no daemon carries (not in `KBF_DARWIN_LABELS`). The two sets must differ;
//! - a directory with no `.buckconfig` (not a buck2 checkout);
//! - the main checkout (`git rev-parse --git-dir` equals `--git-common-dir`): only a
//!   linked worktree gets the overlay, so the checkout others build from is untouched;
//! - a worktree where `.buckconfig.local` is not gitignored (`git check-ignore`);
//! - a value the overlay cannot carry as it is.
//!
//! `--remove` deletes the overlay, again only one that carries the marker.

use std::io::Write;

use super::config::OverlaySettings;
use super::{GIT, MARKER, PilotError, carried, command, inspect, note, query, stat, under};
use crate::template::{self, Value, Values};
use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;
use kbf_host::plan::{Mode, Plan, Planner, RunAs, execute};
use kbf_host::step::Perms;

const TEMPLATE: &str = include_str!("../../templates/pilot/buckconfig.local");
const NAME: &str = ".buckconfig.local";

/// What `buckconfig` was asked for.
pub struct Overlay {
    pub settings: OverlaySettings,
    pub worktree: HostPath,
}

/// `<worktree>/.buckconfig.local`, refused unless nothing is there or an overlay is.
fn target(exec: &dyn Exec, worktree: &HostPath) -> Result<HostPath, PilotError> {
    let target = under(worktree, NAME)?;
    match inspect(exec, &target)? {
        None => Ok(target),
        Some(Entry::File { bytes, .. }) if bytes.starts_with(MARKER.as_bytes()) => Ok(target),
        Some(_) => Err(PilotError::Foreign(target)),
    }
}

/// Check the settings and the worktree, then plan the overlay.
pub fn plan<E: Exec>(exec: &E, a: &Overlay, out: &mut dyn Write) -> Result<Plan, PilotError> {
    let wt = &a.worktree;
    let target = target(exec, wt)?;
    let mut planner = Planner::new(exec);
    let s = &a.settings;
    if s.linux_properties == s.darwin_properties {
        return Err(PilotError::SameProperties);
    }
    routes_to(Os::Linux, &s.linux_properties)?;
    routes_to(Os::Darwin, &s.darwin_properties)?;
    labels_carried(&s.darwin_properties, &s.darwin_labels)?;
    let values = [
        ("KBF_SERVER_HOST", s.server_host.clone()),
        ("KBF_REAPI_PORT", s.reapi_port.to_string()),
        ("KBF_INSTANCE_NAME", s.instance_name.clone()),
        ("KBF_CONCURRENCY", s.concurrency.clone()),
        ("KBF_LINUX_PROPERTIES", s.linux_properties.clone()),
        ("KBF_DARWIN_PROPERTIES", s.darwin_properties.clone()),
        ("KBF_DARWIN_MACOS_HOSTS", s.darwin_macos_hosts.clone()),
    ];
    for (key, v) in &values {
        carried(key, "overlay", v, true)?;
    }
    let values = Values::from(values.map(|(k, v)| (k, Value::Text(v))));
    let text = template::render(NAME, TEMPLATE, &values)?;

    if !stat(exec, &under(wt, ".buckconfig")?)?.is_some_and(|m| m.kind == Kind::File) {
        return Err(PilotError::NotCheckout(wt.clone()));
    }
    let dirs = query(
        exec,
        &command(
            GIT,
            &[
                "-C",
                wt.as_str(),
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
            ],
        ),
    )?;
    let dirs = String::from_utf8_lossy(&dirs.stdout).into_owned();
    let mut lines = dirs.lines();
    let (git_dir, common) = (lines.next(), lines.next());
    if git_dir.is_none() || git_dir == common {
        return Err(PilotError::MainCheckout(wt.clone()));
    }
    let ignored = command(GIT, &["-C", wt.as_str(), "check-ignore", "-q", NAME]);
    if !query(exec, &ignored)?.status.success() {
        return Err(PilotError::NotIgnored(wt.clone()));
    }

    if planner.file(
        target.clone(),
        Perms::new(0o644),
        None,
        text.clone().into_bytes(),
    )? {
        note(out, format_args!("{target}: writes"))?;
        for line in text.lines() {
            note(out, format_args!("    | {line}"))?;
        }
        note(
            out,
            format_args!(
                "WARN: the linux actions of this worktree go to kbf too; with only Mac runners in the cell they wait for a linux runner"
            ),
        )?;
    } else {
        note(out, format_args!("{target}: unchanged"))?;
    }
    Ok(planner.finish())
}

/// Plan the overlay's removal: nothing when there is none.
pub fn plan_remove<E: Exec>(
    exec: &E,
    worktree: &HostPath,
    out: &mut dyn Write,
) -> Result<Plan, PilotError> {
    let target = target(exec, worktree)?;
    let mut planner = Planner::new(exec);
    if !planner.absent(target.clone())? {
        note(out, format_args!("no {target}"))?;
    }
    Ok(planner.finish())
}

/// Plan, then print or carry the plan out.
pub fn run<E: Exec>(
    exec: &mut E,
    a: &Overlay,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), PilotError> {
    let p = plan(&*exec, a, &mut *out)?;
    Ok(execute(&p, RunAs::User, mode, exec, &mut &mut *out)?)
}

/// [`plan_remove`], then print or carry the plan out.
pub fn run_remove<E: Exec>(
    exec: &mut E,
    worktree: &HostPath,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), PilotError> {
    let p = plan_remove(&*exec, worktree, &mut *out)?;
    Ok(execute(&p, RunAs::User, mode, exec, &mut &mut *out)?)
}

/// The OS a property set must route to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    Darwin,
}

impl Os {
    fn key(self) -> &'static str {
        match self {
            Os::Linux => "KBF_LINUX_PROPERTIES",
            Os::Darwin => "KBF_DARWIN_PROPERTIES",
        }
    }
}

/// Refuse a property set kbf would not route to `os`. kbf acts only on the property
/// names it knows and ignores the rest (docs/platform-properties.md), so a set must
/// name `OSFamily` exactly once (name and value in any case, as kbf reads them), with
/// a value of that OS: `linux`; `darwin`, `macos`, `macosx` or `osx`.
pub fn routes_to(os: Os, props: &str) -> Result<(), PilotError> {
    let key = os.key();
    let mut values = Vec::new();
    for pair in props.split(',') {
        let (name, value) = pair_of(key, pair)?;
        if name.eq_ignore_ascii_case("osfamily") {
            values.push(value);
        }
    }
    let [value] = values[..] else {
        if values.is_empty() {
            return Err(PilotError::NoOsFamily {
                key,
                props: props.to_owned(),
                want: match os {
                    Os::Linux => "Linux",
                    Os::Darwin => "Darwin",
                },
            });
        }
        return Err(PilotError::OsFamilyTwice {
            key,
            n: values.len(),
            props: props.to_owned(),
        });
    };
    let value_lower = value.to_ascii_lowercase();
    let names: &[&str] = match os {
        Os::Linux => &["linux"],
        Os::Darwin => &["darwin", "macos", "macosx", "osx"],
    };
    if names.contains(&value_lower.as_str()) {
        return Ok(());
    }
    Err(PilotError::WrongOsFamily {
        key,
        value: value.to_owned(),
        os: match os {
            Os::Linux => "linux",
            Os::Darwin => "darwin",
        },
    })
}

/// Refuse a `label.<k>=<v>` in `props` that is not a `<k>=<v>` of `labels` (the
/// daemons' `--label` set, comma separated): kbf matches it exactly against a
/// daemon's labels, so no daemon would serve it.
pub fn labels_carried(props: &str, labels: &str) -> Result<(), PilotError> {
    for pair in props.split(',') {
        let (name, _) = pair_of(Os::Darwin.key(), pair)?;
        if !name.to_ascii_lowercase().starts_with("label.") {
            continue;
        }
        let want = &pair["label.".len()..];
        if !labels.split(',').any(|l| l == want) {
            return Err(PilotError::LabelNotCarried {
                pair: pair.to_owned(),
                want: want.to_owned(),
                labels: labels.to_owned(),
            });
        }
    }
    Ok(())
}

/// `KEY=VALUE`, each side letters, digits and `_.-`, neither empty.
fn pair_of<'a>(key: &'static str, pair: &'a str) -> Result<(&'a str, &'a str), PilotError> {
    let word = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    };
    match pair.split_once('=') {
        Some((n, v)) if word(n) && word(v) => Ok((n, v)),
        _ => Err(PilotError::NotPair {
            key,
            pair: pair.to_owned(),
        }),
    }
}

#[cfg(test)]
#[path = "buckconfig_tests.rs"]
mod tests;
