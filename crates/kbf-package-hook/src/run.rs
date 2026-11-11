//! One run of the hook: the arguments, the params file, the record directory, the host.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use update_hook::{Class, Invocation, Outcome, Params, Scalar};

use crate::host::Host;
use crate::record::Store;
use crate::steps::{Ctx, Done, step};
use crate::systemd::Systemd;

/// What the manifest's `[params]` say, and what the test harness adds.
pub struct Settings<'a> {
    pub host: &'a dyn Host,
    /// Called before every change of the record; a real run has none.
    pub before_write: Option<fn()>,
}

fn bad(reason: &str, why: impl std::fmt::Display) -> Done {
    Done::failed(reason, Class::Release, why)
}

/// A manifest param: text, present.
fn param<'p>(params: &'p Params, key: &str) -> Result<&'p str, Done> {
    match params.get(key) {
        Some(Scalar::Text(text)) if !text.is_empty() => Ok(text),
        _ => Err(bad(
            "bad_params",
            format!("param {key:?} must be non-empty text"),
        )),
    }
}

/// A unit name is passed to systemctl: no option, no space, no path.
fn is_unit_name(unit: &str) -> bool {
    !unit.starts_with('-')
        && unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@' | b':'))
}

fn is_mode_name(mode: &str) -> bool {
    !mode.starts_with('-')
        && mode
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn attempt(args: &[OsString], settings: &Settings<'_>) -> Done {
    let inv = match Invocation::parse(args.iter().cloned()) {
        Ok(inv) => inv,
        Err(e) => return bad("bad_arguments", e),
    };
    let Some(file) = &inv.params else {
        return bad(
            "bad_arguments",
            "no --params file: the updater always gives one",
        );
    };
    let params = match Params::read(file) {
        Ok(params) => params,
        Err(e) => return bad("bad_params", e),
    };
    let (unit, mode, state_dir) = match (
        param(&params, "unit"),
        param(&params, "mode"),
        param(&params, "state_dir"),
    ) {
        (Ok(unit), Ok(mode), Ok(state_dir)) => (unit, mode, state_dir),
        (Err(done), _, _) | (_, Err(done), _) | (_, _, Err(done)) => return done,
    };
    if !is_unit_name(unit) {
        return bad(
            "bad_params",
            format!("param \"unit\" {unit:?} is not a unit name"),
        );
    }
    if !is_mode_name(mode) {
        return bad(
            "bad_params",
            format!("param \"mode\" {mode:?} is not a mode name"),
        );
    }
    let store = Store::new(state_dir.into(), settings.before_write);
    if let Err(e) = store.ensure() {
        return e.into();
    }
    // Locked before anything else is touched: a second copy of a step refuses and changes nothing.
    let _lock = match store.lock() {
        Ok(lock) => lock,
        Err(e) => return e.into(),
    };
    step(
        &Ctx {
            host: settings.host,
            store: &store,
            params: &params,
            unit,
            mode,
        },
        &inv,
    )
}

/// One run: `args` are everything after `package-hook`. The last line goes to `out`; what comes
/// back is the exit code.
pub fn run(args: &[OsString], settings: &Settings<'_>, out: &mut dyn Write) -> Outcome {
    let done = attempt(args, settings);
    // A standard output nobody reads is no reason to change the exit code.
    let _ = done.report.write_to(out);
    done.outcome
}

/// The `kbf package-hook` entry: the real host, standard output.
#[must_use]
pub fn main(args: Vec<OsString>) -> ExitCode {
    let host = Systemd::new();
    let settings = Settings {
        host: &host,
        before_write: None,
    };
    run(&args, &settings, &mut std::io::stdout().lock()).into()
}
