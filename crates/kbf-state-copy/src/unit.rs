//! Whether the frontend's systemd unit is stopped.
//!
//! The lock beside the state file says that no process has the file open; the unit says that
//! systemd is not about to start one (a unit that is restarting between two runs holds no
//! lock at that moment). A cutover given a unit name asks both.

use std::io;
use std::process::Command;

use crate::error::Error;
use crate::seams::Seams;

/// The properties asked of systemd: whether the unit exists, and whether it runs.
const PROPERTIES: &str = "--property=LoadState,ActiveState";

/// What `<program> show --property=LoadState,ActiveState -- <name>` prints on its standard
/// output, without the end of the last line. The exit status is not read: the text is the
/// answer.
///
/// # Errors
/// The program cannot be started.
pub fn shown(program: &str, name: &str) -> io::Result<String> {
    let output = Command::new(program)
        .args(["show", PROPERTIES, "--", name])
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The state of the unit `name` according to `<program>`: the word [`state_of`] makes of what
/// [`shown`] prints.
///
/// # Errors
/// The program cannot be started.
pub fn query(program: &str, name: &str) -> io::Result<String> {
    shown(program, name).map(|text| state_of(&text))
}

/// The state a `Property=value` listing from `systemctl show` stands for: the unit's
/// `LoadState` when that is not `loaded` (`not-found` for a name no unit has, and the empty
/// word when the listing has no `LoadState`), and its `ActiveState` otherwise. Asking
/// `systemctl is-active` instead would say `inactive` for a mistyped name.
#[must_use]
pub fn state_of(listing: &str) -> String {
    let load = property(listing, "LoadState");
    if load == "loaded" {
        return property(listing, "ActiveState");
    }
    load
}

fn property(listing: &str, key: &str) -> String {
    let value = listing.lines().find_map(|line| value_of(line, key));
    value.unwrap_or_default().trim().to_owned()
}

/// What follows `key=` on `line`, if the line starts with it.
fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key)?.strip_prefix('=')
}

/// Refuses a unit that is not stopped. `inactive` and `failed` are stopped; `active`,
/// `activating`, `deactivating`, `reloading` and any word this tool does not know are not,
/// and neither is a name that systemd has no unit for.
///
/// # Errors
/// [`Error::Refused`] when the unit is not stopped or does not exist; [`Error::Failed`] when
/// systemd cannot be asked.
pub fn require_stopped(seams: &Seams, name: &str) -> Result<(), Error> {
    let state = (seams.unit_state)(name).map_err(|e| asking(name, &e))?;
    match state.as_str() {
        "inactive" | "failed" => Ok(()),
        "not-found" => Err(Error::Refused(format!(
            "systemd has no unit {name}: check the name given with --unit"
        ))),
        other => Err(Error::Refused(format!(
            "systemd says {other:?} of the unit {name}, and it must be inactive or failed: \
             stop it first"
        ))),
    }
}

fn asking(name: &str, err: &io::Error) -> Error {
    Error::Failed(format!("could not ask systemd about {name}: {err}"))
}
