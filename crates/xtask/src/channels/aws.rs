//! The `aws` program, behind a trait so a test answers in place of it. The credentials are the
//! job's own (`AWS_*` of the role the workflow assumed); nothing here reads the environment.

use std::path::PathBuf;
use std::process::Command;

use super::Error;

/// What one run of `aws` came to.
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    /// Whether it exited with status 0.
    pub ok: bool,
    /// What it printed on standard output.
    pub stdout: String,
    /// What it printed on standard error.
    pub stderr: String,
}

/// What the channel verbs ask of AWS.
pub trait Aws {
    /// `aws <args>`. A refusal by AWS is an [`Answer`] that is not `ok`, not an error: the verbs
    /// read it (a `412` is an answer to the question "is this file there already").
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, args: &[String]) -> Result<Answer, Error>;
}

/// The `aws` program at a path.
pub struct RealAws(pub PathBuf);

impl Aws for RealAws {
    fn run(&self, args: &[String]) -> Result<Answer, Error> {
        let out = Command::new(&self.0)
            .args(args)
            .output()
            .map_err(Error::spawn(self.0.display().to_string()))?;
        Ok(Answer {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}
