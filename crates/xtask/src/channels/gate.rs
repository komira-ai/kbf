//! `channels gate`: whether a channel job has the settings it needs.
//!
//! | settings of the job | `enabled` | the job |
//! |---|---|---|
//! | none set | `false` | green, a notice names the job and the settings, nothing is written |
//! | all set | `true` | runs |
//! | some set | `false` | red, naming the missing ones |
//!
//! `--setting NAME` and `--require NAME` name environment variables, which the verb reads
//! itself: a workflow step passes the variables, a secret among them, in its `env` and no value
//! ever reaches a command line, a shell expansion or the log. A setting is set when its variable
//! is not empty; the value is never printed.
//!
//! A job that follows another (the apt index after the apt read) names what it needs from it
//! with `--require NAME`, `NAME` holding the earlier job's output: unless every one is exactly
//! `true` the job is off, green, and its own settings are not judged (the earlier job said it is
//! off, and says why).
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

/// Is `name` an environment variable name of the form the gate takes (upper case letters, digits
/// and `_`)?
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

/// The `gate` verb: the lines to print.
///
/// # Errors
/// A label or name that is not of its form, a name given twice, no setting and no requirement,
/// or some but not all of the settings set.
pub fn gate(
    env: Env,
    label: &str,
    settings: &[String],
    requires: &[String],
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    if label.is_empty() || label.chars().any(char::is_control) || label.contains("::") {
        return Err(Error::Usage(format!(
            "--label {label:?} is not a plain name"
        )));
    }
    if settings.is_empty() && requires.is_empty() {
        return Err(Error::Usage(
            "give at least one --setting or --require".to_owned(),
        ));
    }
    let mut unmet = Vec::new();
    for name in requires {
        check_name("--require", name)?;
        if env(name).as_deref() != Some("true") {
            unmet.push(name.as_str());
        }
    }
    let mut set = Vec::new();
    let mut unset = Vec::new();
    for name in settings {
        check_name("--setting", name)?;
        if set.contains(&name.as_str()) || unset.contains(&name.as_str()) {
            return Err(Error::Usage(format!("--setting {name} is given twice")));
        }
        if env(name).is_some_and(|value| !value.is_empty()) {
            set.push(name.as_str());
        } else {
            unset.push(name.as_str());
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
