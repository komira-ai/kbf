//! The profile canaries on a Mac ([`crate::slots::canary`] says what each is): each
//! probe is started the way an action is ([`Slot::launch`]), as the slot's user under
//! the profile generated for the lease, and its answer is judged by
//! [`canary::check`]. A slot whose canaries are not all refused is set aside
//! ([`Slot::set_aside`]) and the lease takes another.

use std::io::{self, Read as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kbf_daemon::RuntimeError;
use kbf_daemon::uid;

use super::Slot;
use crate::network::{Isolation, Network};
use crate::slots::canary::{self, CANARIES, Canary, Ran, Report};

/// How long the processes of a probe that timed out get to end.
const END_WITHIN: Duration = Duration::from_secs(2);

impl Slot {
    /// Runs every canary as the slot's user under the profile of a lease in `lease`
    /// with `network`, and logs how long they took. Every canary runs; the report says
    /// which were not refused (the slot is then to be set aside, [`Slot::set_aside`]).
    ///
    /// # Errors
    /// A canary command could not be built (the slot's uid is 0).
    pub async fn canary(
        &self,
        isolation: &Isolation,
        network: Network,
        lease: &Path,
    ) -> Result<Report, RuntimeError> {
        let uid = self.ready().user.uid;
        let mut commands = Vec::new();
        for canary in CANARIES {
            let args = [
                canary::ARG.to_owned(),
                canary.name.to_owned(),
                lease.to_string_lossy().into_owned(),
            ];
            let launch = self.launch(isolation, network, lease, self.program.clone(), &args)?;
            commands.push((canary.name, launch));
        }
        let started = Instant::now();
        let report = tokio::task::spawn_blocking(move || {
            canary::check(CANARIES, &mut |c: &Canary| {
                let (_, (program, args)) = commands
                    .iter()
                    .find(|(name, _)| *name == c.name)
                    .ok_or_else(|| io::Error::other("no command for the canary"))?;
                run(program, args, uid)
            })
        })
        .await
        .map_err(|e| RuntimeError::Failed(format!("profile canaries did not finish: {e}")))?;
        if report.all_refused() {
            tracing::info!(
                uid,
                total = ?started.elapsed(),
                canaries = ?report.passed.iter().map(|p| (p.name, p.took)).collect::<Vec<_>>(),
                "profile canaries passed"
            );
        }
        Ok(report)
    }
}

/// Starts `program` with `args`, waits for it at most [`canary::WITHIN`] (ending every
/// process of `uid` if it takes longer) and returns its exit code and output.
fn run(program: &Path, args: &[String], uid: u32) -> io::Result<Ran> {
    let started = Instant::now();
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > canary::WITHIN {
            let _ = child.kill();
            let _ = uid::end(uid, END_WITHIN);
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    Ok(Ran {
        code: status.code(),
        stdout,
        took: started.elapsed(),
    })
}
