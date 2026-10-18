//! `kbf-installer envoy fetch|verify|path|list` as the command line has them
//! (docs/design/kbf-installer.md, section 2): the arguments after `envoy`, the text or
//! `--json` printed, and the exit code. The binary's own parser hands this the words after
//! `envoy`; nothing here reads the environment.
//!
//! ```text
//! envoy fetch --manifest FILE [--apply] [--json]   dry run unless --apply
//! envoy verify [--json]                            the pinned build is what the pin says
//! envoy path [--json]                              where the pinned binary is
//! envoy list [--json]                              the builds in envoy/, and the pins
//! ```
//!
//! Exit codes: 0 ok; 1 failed; 2 usage or a missing prerequisite (not root, a manifest that
//! cannot be read); 3 refused (the manifest, the hash, the binary, the version, a directory
//! with other bytes, an installed build that fails its checks); 4 busy (not used here: the
//! lock is waited for); 5 needs the operator (no pin, an unreadable `state.json`). `fetch`,
//! `verify` and `path` need root; `list` does not.

use serde_json::json;

use super::EnvoyError;
use super::install::{self, Done, Env};
use super::manifest;

pub const OK: u8 = 0;
pub const USAGE: u8 = 2;

/// What a verb printed and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub code: u8,
    pub stdout: String,
    pub stderr: String,
}

const USAGE_TEXT: &str = "usage: kbf-installer envoy fetch --manifest FILE [--apply] [--json]\n       \
kbf-installer envoy verify|path|list [--json]";

/// A verb and its flags.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Fetch { manifest: String, apply: bool },
    Verify,
    Path,
    List,
}

#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    command: Command,
    json: bool,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let (mut verb, mut json, mut apply, mut manifest) = (None, false, false, None);
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--json" => json = true,
            "--apply" => apply = true,
            "--manifest" => match words.next() {
                Some(file) if manifest.is_none() => manifest = Some(file.clone()),
                Some(_) => return Err("--manifest is given once".to_owned()),
                None => return Err("--manifest needs a file".to_owned()),
            },
            flag if flag.starts_with('-') => return Err(format!("unknown flag {flag}")),
            name if verb.is_none() => verb = Some(name),
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    let command = match verb {
        Some("fetch") => Command::Fetch {
            manifest: manifest.take().ok_or("fetch needs --manifest FILE")?,
            apply,
        },
        Some("verify") => Command::Verify,
        Some("path") => Command::Path,
        Some("list") => Command::List,
        Some(other) => return Err(format!("unknown verb {other}")),
        None => return Err("a verb is needed".to_owned()),
    };
    if manifest.is_some() {
        return Err("--manifest is for fetch".to_owned());
    }
    if apply && !matches!(command, Command::Fetch { .. }) {
        return Err("--apply is for fetch".to_owned());
    }
    Ok(Parsed { command, json })
}

/// What a verb did: the object `--json` prints and the text otherwise.
struct Report {
    value: serde_json::Value,
    text: String,
}

/// Run the verb in `args` (the words after `envoy`).
pub fn run(env: &Env, args: &[String]) -> Outcome {
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(why) => {
            return Outcome {
                code: USAGE,
                stdout: String::new(),
                stderr: format!("{why}\n{USAGE_TEXT}\n"),
            };
        }
    };
    match execute(env, &parsed.command) {
        Ok(report) if parsed.json => Outcome {
            code: OK,
            stdout: format!("{}\n", report.value),
            stderr: String::new(),
        },
        Ok(report) => Outcome {
            code: OK,
            stdout: report.text,
            stderr: String::new(),
        },
        Err(e) => failed(&e, parsed.json),
    }
}

fn failed(e: &EnvoyError, json: bool) -> Outcome {
    let code = e.exit();
    if json {
        let value = json!({"ok": false, "code": e.code(), "exit": code, "message": e.to_string()});
        return Outcome {
            code,
            stdout: format!("{value}\n"),
            stderr: String::new(),
        };
    }
    let word = if code == 3 { "REFUSED" } else { "FAILED" };
    Outcome {
        code,
        stdout: String::new(),
        stderr: format!("{word} {}: {e}\n", e.code()),
    }
}

fn execute(env: &Env, command: &Command) -> Result<Report, EnvoyError> {
    let root_only = !matches!(command, Command::List);
    if root_only && rustix::process::geteuid().as_raw() != env.owner_uid {
        return Err(EnvoyError::NotRoot);
    }
    match command {
        Command::Fetch { manifest, apply } => fetch(env, manifest, *apply),
        Command::Verify => {
            let v = install::verify(env)?;
            Ok(Report {
                text: format!("ok {} sha256 {}\n", v.id, v.sha256),
                value: json!({
                    "ok": true,
                    "id": v.id,
                    "sha256": v.sha256,
                    "path": v.binary,
                }),
            })
        }
        Command::Path => {
            let path = install::path(env)?;
            Ok(Report {
                text: format!("{}\n", path.display()),
                value: json!({"ok": true, "path": path}),
            })
        }
        Command::List => {
            let builds = install::list(env)?;
            let text = builds
                .iter()
                .map(|b| {
                    let pin = match (b.current, b.previous) {
                        (true, _) => "current",
                        (_, true) => "previous",
                        _ => "-",
                    };
                    format!("{} {pin}\n", b.id)
                })
                .collect();
            let builds: Vec<_> = builds
                .iter()
                .map(|b| json!({"id": b.id, "current": b.current, "previous": b.previous}))
                .collect();
            Ok(Report {
                text,
                value: json!({"ok": true, "builds": builds}),
            })
        }
    }
}

fn fetch(env: &Env, manifest_file: &str, apply: bool) -> Result<Report, EnvoyError> {
    let bytes = install::read_trusted(env, std::path::Path::new(manifest_file), MANIFEST_CAP)?;
    let pin = manifest::pin_for(&bytes, env.arch)?;
    if !apply {
        let plan = install::plan(env, &pin)?;
        let action = if plan.installed {
            format!("{} is already installed", plan.dir.display())
        } else {
            format!(
                "would fetch {} and install it as {}",
                pin.url,
                plan.dir.display()
            )
        };
        return Ok(Report {
            text: format!(
                "envoy {} sha256 {}\n{action}\ndry run: nothing changed; --apply does it\n",
                pin.version, pin.sha256
            ),
            value: json!({
                "ok": true,
                "apply": false,
                "id": plan.id,
                "version": pin.version,
                "url": pin.url,
                "sha256": pin.sha256,
                "dir": plan.dir,
                "installed": plan.installed,
            }),
        });
    }
    let done = install::fetch(env, &pin)?;
    let word = match done.done {
        Done::Installed => "installed",
        Done::Unchanged => "unchanged",
    };
    let none = || "none".to_owned();
    let (current, previous) = (done.pins.current.clone(), done.pins.previous.clone());
    Ok(Report {
        text: format!(
            "{word} {}\npinned: current {} previous {}\n",
            done.dir.display(),
            current.as_ref().map_or_else(none, ToString::to_string),
            previous.as_ref().map_or_else(none, ToString::to_string),
        ),
        value: json!({
            "ok": true,
            "apply": true,
            "id": done.id,
            "dir": done.dir,
            "done": word,
            "current": current,
            "previous": previous,
        }),
    })
}

/// The most of a manifest read.
const MANIFEST_CAP: u64 = 1024 * 1024;
