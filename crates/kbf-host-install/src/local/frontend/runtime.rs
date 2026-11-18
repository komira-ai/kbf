//! The container runtime, behind a port: what an update of the frontend container asks of
//! it, and the one implementation for Docker.
//!
//! The port is small on purpose. Docker is the only runtime built; a host that runs the
//! frontend another way (k3s) would implement [`Runtime`] and change nothing else.
//!
//! A [`Docker`] runs the `docker` client by absolute path with no shell, through the same
//! [`Runner`] as the other programs of this crate, with a cleared environment: `DOCKER_CONFIG`
//! names a directory that is not there, so no credential helper or plugin of the account is
//! ever read.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::config::RunSpec;
use crate::runner::{Cmd, Exec, Runner};

/// What a container is, as far as an update looks at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// The id of the image it was made from.
    pub image_id: String,
    /// The image reference it was made with (`kbf-frontend:<commit>`).
    pub image_ref: String,
    pub running: bool,
    /// Docker's restart policy name (`no` or empty: none).
    pub restart: String,
}

/// A runtime call that did not work, in a line for the log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RuntimeError(pub String);

/// How a release's own configuration check ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// Whether it exited with status 0.
    pub passed: bool,
    /// How it ended, in words (`exit status 2`).
    pub ended: String,
    /// What it printed, on one line, cleaned and cut.
    pub output: String,
}

/// What the update needs of the container runtime.
pub trait Runtime {
    /// Load the image file.
    ///
    /// # Errors
    /// The runtime could not load it.
    fn load(&self, file: &Path) -> Result<(), RuntimeError>;

    /// The id of the image `reference` names, if the runtime has one.
    ///
    /// # Errors
    /// The runtime could not be asked.
    fn image_id(&self, reference: &str) -> Result<Option<String>, RuntimeError>;

    /// Forget the image `id`. Whether it was there does not matter.
    ///
    /// # Errors
    /// The runtime could not be asked.
    fn remove_image(&self, id: &str) -> Result<(), RuntimeError>;

    /// The container `name`, if there is one.
    ///
    /// # Errors
    /// The runtime could not be asked.
    fn container(&self, name: &str) -> Result<Option<Container>, RuntimeError>;

    /// Set the restart policy of the container `name`.
    ///
    /// # Errors
    /// The runtime could not change it.
    fn set_restart(&self, name: &str, policy: &str) -> Result<(), RuntimeError>;

    /// Run `argv` inside the running container `name`; it has to succeed.
    ///
    /// # Errors
    /// The command could not be run or ended with a failure.
    fn exec(&self, name: &str, argv: &[String]) -> Result<(), RuntimeError>;

    /// Stop the container `name`, giving its process `grace_secs` to finish after the
    /// signal. A container that is not there or not running is stopped already.
    ///
    /// # Errors
    /// The runtime could not stop it.
    fn stop(&self, name: &str, grace_secs: u64) -> Result<(), RuntimeError>;

    /// Remove the container `name`, running or not. One that is not there is removed already.
    ///
    /// # Errors
    /// The runtime could not remove it.
    fn remove(&self, name: &str) -> Result<(), RuntimeError>;

    /// Make and start a container from the image `image_id` as `spec` says.
    ///
    /// # Errors
    /// The runtime could not make or start it.
    fn run(&self, spec: &RunSpec, image_id: &str) -> Result<(), RuntimeError>;

    /// Run the configuration check of the image `image_id` once, in a throwaway container made
    /// as `spec` says (see [`RunSpec::check_args`]).
    ///
    /// # Errors
    /// The runtime could not run it at all. A check that ran and said no is a [`Checked`].
    fn check(&self, spec: &RunSpec, image_id: &str) -> Result<Checked, RuntimeError>;
}

/// The directory `docker` is told is its configuration: not there, so nothing is read from it.
const NO_CONFIG: &str = "/nonexistent/docker";

/// How long a `docker` call may take besides the load of an image.
const CALL: Duration = Duration::from_secs(120);

/// How long the configuration check of a release may take: it reads files and certificates and
/// waits for no network.
const CHECK: Duration = Duration::from_secs(60);

/// What `docker run` exits with when docker itself, not the command, failed.
const DOCKER_FAILED: i32 = 125;

/// The most of a check's output that is kept.
const CHECK_OUTPUT_MAX: usize = 1000;

/// How long the load of an image may take.
const LOAD: Duration = Duration::from_secs(900);

/// Docker, run as the `docker` client.
pub struct Docker {
    runner: Box<dyn Runner>,
    docker: PathBuf,
}

impl Docker {
    pub fn new(runner: Box<dyn Runner>, docker: PathBuf) -> Self {
        Self { runner, docker }
    }

    fn cmd(&self, args: &[&str], timeout: Duration) -> Cmd {
        Cmd {
            program: self.docker.display().to_string(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            timeout: Some(timeout),
            env: vec![("DOCKER_CONFIG".to_owned(), NO_CONFIG.to_owned())],
            stdout_max: Some(64 * 1024),
            ..Cmd::default()
        }
    }

    fn run_cmd(&self, cmd: &Cmd) -> Result<Exec, RuntimeError> {
        let verb = cmd.args.first().cloned().unwrap_or_default();
        self.runner
            .run(cmd)
            .map_err(|e| RuntimeError(format!("docker {verb}: {e}")))
    }

    /// Run `cmd` and require success.
    fn ok(&self, cmd: &Cmd) -> Result<Exec, RuntimeError> {
        let exec = self.run_cmd(cmd)?;
        if exec.success() {
            return Ok(exec);
        }
        Err(failure(cmd, &exec))
    }

    /// Run `cmd`; its failure is `None` when docker says what it was asked about is not there.
    fn ok_or_missing(&self, cmd: &Cmd) -> Result<Option<Exec>, RuntimeError> {
        let exec = self.run_cmd(cmd)?;
        if exec.success() {
            return Ok(Some(exec));
        }
        if exec.stderr_text().contains("No such") {
            return Ok(None);
        }
        Err(failure(cmd, &exec))
    }
}

/// What a check printed (standard error, then standard output) as one line: the lines apart by
/// ` / `, a control character a `?`, cut at [`CHECK_OUTPUT_MAX`].
fn one_line(exec: &Exec) -> String {
    let text = format!("{}\n{}", exec.stderr_text(), exec.stdout_text());
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines
        .join(" / ")
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(CHECK_OUTPUT_MAX)
        .collect()
}

fn failure(cmd: &Cmd, exec: &Exec) -> RuntimeError {
    let verb = cmd.args.first().map_or("", String::as_str);
    let said: String = exec
        .stderr_text()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    RuntimeError(format!(
        "docker {verb}: exit status {:?}: {said}",
        exec.code
    ))
}

impl Runtime for Docker {
    fn load(&self, file: &Path) -> Result<(), RuntimeError> {
        let file = file.display().to_string();
        self.ok(&self.cmd(&["image", "load", "--input", &file], LOAD))
            .map(drop)
    }

    fn image_id(&self, reference: &str) -> Result<Option<String>, RuntimeError> {
        let cmd = self.cmd(
            &["image", "inspect", "--format", "{{.Id}}", reference],
            CALL,
        );
        Ok(self
            .ok_or_missing(&cmd)?
            .map(|exec| exec.stdout_text().trim().to_owned()))
    }

    fn remove_image(&self, id: &str) -> Result<(), RuntimeError> {
        self.ok_or_missing(&self.cmd(&["image", "rm", id], CALL))
            .map(drop)
    }

    fn container(&self, name: &str) -> Result<Option<Container>, RuntimeError> {
        let cmd = self.cmd(
            &[
                "container",
                "inspect",
                "--format",
                "{{.Image}} {{.Config.Image}} {{.State.Running}} {{.HostConfig.RestartPolicy.Name}}",
                name,
            ],
            CALL,
        );
        let Some(exec) = self.ok_or_missing(&cmd)? else {
            return Ok(None);
        };
        let text = exec.stdout_text();
        let mut fields = text.split_whitespace();
        let seen = (fields.next(), fields.next(), fields.next());
        let (restart, rest) = (fields.next(), fields.next());
        match seen {
            (Some(id), Some(image_ref), Some(running @ ("true" | "false"))) if rest.is_none() => {
                Ok(Some(Container {
                    image_id: id.to_owned(),
                    image_ref: image_ref.to_owned(),
                    running: running == "true",
                    restart: restart.unwrap_or_default().to_owned(),
                }))
            }
            _ => Err(RuntimeError(
                "docker container inspect: the answer is not what was asked for".to_owned(),
            )),
        }
    }

    fn set_restart(&self, name: &str, policy: &str) -> Result<(), RuntimeError> {
        self.ok(&self.cmd(&["update", "--restart", policy, name], CALL))
            .map(drop)
    }

    fn exec(&self, name: &str, argv: &[String]) -> Result<(), RuntimeError> {
        let mut args = vec!["exec", name];
        args.extend(argv.iter().map(String::as_str));
        self.ok(&self.cmd(&args, CALL)).map(drop)
    }

    fn stop(&self, name: &str, grace_secs: u64) -> Result<(), RuntimeError> {
        let grace = grace_secs.to_string();
        // The client waits for the grace and then for the kill.
        let timeout = CALL + Duration::from_secs(grace_secs);
        self.ok_or_missing(&self.cmd(&["stop", "--time", &grace, name], timeout))
            .map(drop)
    }

    fn remove(&self, name: &str) -> Result<(), RuntimeError> {
        self.ok_or_missing(&self.cmd(&["rm", "--force", name], CALL))
            .map(drop)
    }

    fn run(&self, spec: &RunSpec, image_id: &str) -> Result<(), RuntimeError> {
        let args = spec.run_args(image_id);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.ok(&self.cmd(&args, CALL)).map(drop)
    }

    fn check(&self, spec: &RunSpec, image_id: &str) -> Result<Checked, RuntimeError> {
        let args = spec.check_args(image_id);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let cmd = self.cmd(&args, CHECK);
        let exec = self.run_cmd(&cmd)?;
        if exec.code == Some(DOCKER_FAILED) {
            return Err(failure(&cmd, &exec));
        }
        Ok(Checked {
            passed: exec.success(),
            ended: exec.ended(),
            output: one_line(&exec),
        })
    }
}
