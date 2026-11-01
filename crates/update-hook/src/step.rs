//! The five steps of the hook contract and the command line a hook is run with.
//!
//! The updater runs `<executable> <hook_args> <step> [args]`. `hook_args` is declared (default
//! `["update-hook"]`, the convention of our own services); everything from the step on is what
//! [`Invocation`] parses and prints: `prepare <artifact> [--params <file>]` and
//! `<cutover|verify|rollback|cleanup> [--params <file>]`.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// One of the five steps, in the order a successful update runs them (`Rollback` runs instead of
/// the rest after a failure).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Prepare,
    Cutover,
    Verify,
    Rollback,
    Cleanup,
}

impl Step {
    /// Every step, in the order of the table of the contract.
    pub const ALL: [Step; 5] = [
        Step::Prepare,
        Step::Cutover,
        Step::Verify,
        Step::Rollback,
        Step::Cleanup,
    ];

    /// The word the updater puts on the command line.
    pub const fn name(self) -> &'static str {
        match self {
            Step::Prepare => "prepare",
            Step::Cutover => "cutover",
            Step::Verify => "verify",
            Step::Rollback => "rollback",
            Step::Cleanup => "cleanup",
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Step {
    type Err = ArgsError;

    fn from_str(s: &str) -> Result<Step, ArgsError> {
        Step::ALL
            .into_iter()
            .find(|step| step.name() == s)
            .ok_or_else(|| ArgsError::UnknownStep(s.to_owned()))
    }
}

/// Why a hook's command line was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    #[error("no step given")]
    MissingStep,
    #[error("unknown step {0:?}")]
    UnknownStep(String),
    #[error("prepare needs the staged release directory")]
    MissingArtifact,
    #[error("unexpected argument {0:?}")]
    Unexpected(OsString),
    #[error("--params needs a file")]
    MissingParamsFile,
    #[error("--params given twice")]
    DuplicateParams,
}

/// The step a hook was asked to run, with its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub step: Step,
    /// The staged release directory: given to `prepare` and to no other step.
    pub artifact: Option<PathBuf>,
    /// The file of the `[hook.params]` table (see [`crate::Params`]).
    pub params: Option<PathBuf>,
}

impl Invocation {
    /// Parses the arguments that follow `hook_args`, the step first. A hook that takes the
    /// default `hook_args` skips two arguments of its own command line (program name and
    /// `update-hook`) and passes the rest.
    pub fn parse<I>(args: I) -> Result<Invocation, ArgsError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut args = args.into_iter();
        let first = args.next().ok_or(ArgsError::MissingStep)?;
        let step: Step = first.to_string_lossy().parse()?;
        let mut artifact = None;
        let mut params = None;
        while let Some(arg) = args.next() {
            if arg == "--params" {
                let file = args.next().ok_or(ArgsError::MissingParamsFile)?;
                if params.replace(PathBuf::from(file)).is_some() {
                    return Err(ArgsError::DuplicateParams);
                }
            } else if step == Step::Prepare && artifact.is_none() {
                artifact = Some(PathBuf::from(arg));
            } else {
                return Err(ArgsError::Unexpected(arg));
            }
        }
        if step == Step::Prepare && artifact.is_none() {
            return Err(ArgsError::MissingArtifact);
        }
        Ok(Invocation {
            step,
            artifact,
            params,
        })
    }

    /// The arguments the updater passes after `hook_args`: the inverse of [`Invocation::parse`].
    pub fn to_args(&self) -> Vec<OsString> {
        let mut args = vec![OsString::from(self.step.name())];
        args.extend(self.artifact.iter().map(OsString::from));
        if let Some(file) = &self.params {
            args.push(OsString::from("--params"));
            args.push(OsString::from(file));
        }
        args
    }
}
