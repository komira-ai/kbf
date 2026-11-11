//! `channels gate`: whether a channel job has the settings it needs.
//!
//! | settings of the job | `enabled` | the job |
//! |---|---|---|
//! | none set | `false` | green, a notice names the job and the settings, nothing is written |
//! | all set | `true` | runs |
//! | some set | `false` | red, naming the missing ones |
//!
//! `--setting NAME=VALUE` is configuration, passed on the command line: it is set when `VALUE` is
//! not empty. `--secret NAME` is secret material (a signing key, a passphrase), which must not
//! be on a command line: the verb reads the environment variable `NAME` itself, only to test
//! whether it is not empty. No value is ever printed, only names.
//!
//! A job that follows another (the apt index after the apt read) names what it needs from it
//! with `--require NAME=VALUE`, `VALUE` holding the earlier job's output: unless every one is
//! exactly `true` the job is off, green, and its own settings are not judged (the earlier job said
//! it is off, and says why).
//!
//! The third row is a half-made setting, a typo in a variable name among them: it must not leave
//! a channel silently off.

use std::path::Path;

use super::Error;
use super::append_output;

/// The environment, as the verb reads it: the value of a variable, when it is set.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The value of the process's environment variable `name`; none when it is not set or is not
/// text.
#[must_use]
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Is `name` a name of the form the gate takes (upper case letters, digits and `_`)?
fn check_name(flag: &str, name: &str) -> Result<(), Error> {
    let ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(Error::Usage(format!(
            "{flag} name {name:?} is not upper case letters, digits and `_`"
        )))
    }
}

/// Splits a `NAME=VALUE` argument of `flag`; the value is not echoed when the form is wrong.
fn split_pair<'a>(flag: &str, arg: &'a str) -> Result<(&'a str, &'a str), Error> {
    let (name, value) = arg
        .split_once('=')
        .ok_or_else(|| Error::Usage(format!("{flag} is not NAME=VALUE (a name, `=`, a value)")))?;
    check_name(flag, name)?;
    Ok((name, value))
}

/// The `gate` verb: the lines to print.
///
/// `settings` and `requires` are `NAME=VALUE` arguments, `secrets` names of environment variables.
///
/// # Errors
/// A label or name that is not of its form, a name given twice, no setting, secret or
/// requirement, or some but not all of the settings and secrets set.
pub fn gate(
    env: Env,
    label: &str,
    settings: &[String],
    secrets: &[String],
    requires: &[String],
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    if label.is_empty() || label.chars().any(char::is_control) || label.contains("::") {
        return Err(Error::Usage(format!(
            "--label {label:?} is not a plain name"
        )));
    }
    if settings.is_empty() && secrets.is_empty() && requires.is_empty() {
        return Err(Error::Usage(
            "give at least one --setting, --secret or --require".to_owned(),
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    let mut unmet = Vec::new();
    for arg in requires {
        let (name, value) = split_pair("--require", arg)?;
        if seen.contains(&name) {
            return Err(Error::Usage(format!("--require {name} is given twice")));
        }
        seen.push(name);
        if value != "true" {
            unmet.push(name);
        }
    }
    let mut set = Vec::new();
    let mut unset = Vec::new();
    for arg in settings {
        let (name, value) = split_pair("--setting", arg)?;
        if seen.contains(&name) {
            return Err(Error::Usage(format!("--setting {name} is given twice")));
        }
        seen.push(name);
        if value.is_empty() {
            unset.push(name);
        } else {
            set.push(name);
        }
    }
    for name in secrets {
        check_name("--secret", name)?;
        if seen.contains(&name.as_str()) {
            return Err(Error::Usage(format!("--secret {name} is given twice")));
        }
        seen.push(name);
        if env(name).is_some_and(|value| !value.is_empty()) {
            set.push(name);
        } else {
            unset.push(name);
        }
    }
    let enabled = unset.is_empty() && unmet.is_empty();
    if let Some(output) = output {
        append_output(output, &[format!("enabled={enabled}")])?;
    }
    if enabled {
        return Ok(vec![format!("{label}: all its settings are set")]);
    }
    if !unmet.is_empty() {
        return Ok(vec![format!(
            "::notice title=The {label} job is off::{} is not true, so it writes nothing and ends green.",
            unmet.join(", ")
        )]);
    }
    if set.is_empty() {
        return Ok(vec![format!(
            "::notice title=The {label} job is off::None of its settings ({}) is set, so it writes nothing and ends green.",
            unset.join(", ")
        )]);
    }
    Err(Error::Failed(format!(
        "{label}: {} set but {} not: set all of them or none, so that a channel is never left half on",
        set.join(", "),
        unset.join(", ")
    )))
}

#[cfg(test)]
mod tests;
