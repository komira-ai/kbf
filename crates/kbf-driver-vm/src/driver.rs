//! [`VmDriver`]: one VM per lease, from a fresh clone, destroyed with its clone when
//! the lease ends however it ends. See the crate documentation for the steps.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use kbf_daemon::cas::{Cas, CasError};
use kbf_daemon::tree::{
    TreeError, check_relative, fetch_message, materialize, output_paths, real_dirs,
};
use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_driver_native::CasStore;
use kbf_driver_native::network::{Network, network_of};
use kbf_guest::client::{Client, ClientError};
use kbf_guest::wire::{End, Exit, Refusal, RunRequest, TOKEN_LEN};
use kbf_outputs::OutputsError;
use kbf_proto::reapi::{Action, ActionResult, Command, ExecutedActionMetadata};
use kbf_types::{LeaseId, Resources};
use prost_types::Timestamp;
use tokio::sync::oneshot;

use crate::config::{HELPER_MEMORY_BYTES, VmConfig};
use crate::host::{HostError, INPUTS_TAG, MachineId, OUTPUTS_TAG, Share, VmHost, VmSpec};
use crate::{GUI_SESSION, TOKEN_FILE};

/// The driver name in the node report.
pub const DRIVER: &str = "vm";
/// The lease kind this driver runs.
pub const KIND: &str = "vm";

/// The directory in the inputs share the input root is written to; the command's
/// working directory is below it.
const ROOT: &str = "root";

/// A killer waiting for a lease's VM and files to be gone.
type Stop = oneshot::Sender<()>;

/// Runs `vm` leases, each in a VM of its own; see the crate documentation.
#[derive(Debug)]
pub struct VmDriver<H, C> {
    config: VmConfig,
    host: Arc<H>,
    cas: Arc<C>,
    stops: Mutex<BTreeMap<LeaseId, oneshot::Sender<Stop>>>,
}

/// What the run needs from the Action and Command.
struct Prepared {
    request: RunRequest,
    outputs: Vec<String>,
    timeout: Duration,
}

impl<H: VmHost, C: Cas> VmDriver<H, C> {
    /// A driver over `host` and `cas`. Makes the lease and clone directories and
    /// removes everything in them: what is there was left by a daemon that stopped
    /// without cleaning up (it crashed or was killed), and a clone left behind would
    /// hold its disk space for good.
    ///
    /// # Errors
    /// A directory could not be made, read or emptied.
    pub fn new(config: VmConfig, host: Arc<H>, cas: Arc<C>) -> std::io::Result<Self> {
        for dir in [config.leases(), config.clones()] {
            std::fs::create_dir_all(&dir)?;
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                tracing::warn!(path = %path.display(), "removing what an earlier daemon left");
                kbf_outputs::remove_tree(&path)?;
            }
        }
        Ok(Self {
            config,
            host,
            cas,
            stops: Mutex::new(BTreeMap::new()),
        })
    }

    fn stops(&self) -> MutexGuard<'_, BTreeMap<LeaseId, oneshot::Sender<Stop>>> {
        // Every update is one insert or removal: consistent under a panic elsewhere.
        self.stops.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fetches the Action and Command, refuses what a VM lease may not ask for, and
    /// writes the input tree into the inputs share and each output's parent into the
    /// outputs share.
    async fn prepare(&self, dir: &Path, work: &Work) -> Result<Prepared, RuntimeError> {
        let cas = &*self.cas;
        let action: Action = fetch_message(cas, &work.action_digest)
            .await
            .map_err(tree_error)?;
        let command_digest = action
            .command_digest
            .as_ref()
            .ok_or_else(|| RuntimeError::Invalid("the Action names no Command".to_owned()))?;
        let input_root = action
            .input_root_digest
            .as_ref()
            .ok_or_else(|| RuntimeError::Invalid("the Action names no input root".to_owned()))?;
        let command: Command = fetch_message(cas, command_digest)
            .await
            .map_err(tree_error)?;
        if command.arguments.is_empty() {
            return Err(RuntimeError::Invalid(
                "the Command has no arguments".to_owned(),
            ));
        }
        if network_of(&action, &command)? == Network::On {
            return Err(RuntimeError::Invalid(
                "a VM lease runs without the network: network-on VM leases are not offered yet"
                    .to_owned(),
            ));
        }
        check_relative("working directory", &command.working_directory).map_err(tree_error)?;
        let outputs = output_paths(&command).map_err(tree_error)?;
        let timeout = timeout_of(&action, self.config.default_timeout)?;

        let (inputs, outputs_dir) = (dir.join(INPUTS_TAG), dir.join(OUTPUTS_TAG));
        for made in [dir, &inputs, &outputs_dir] {
            tokio::fs::create_dir(made).await.map_err(failed(made))?;
        }
        let root = inputs.join(ROOT);
        tokio::fs::create_dir(&root).await.map_err(failed(&root))?;
        materialize(cas, input_root, &root)
            .await
            .map_err(tree_error)?;
        // REAPI: the worker makes the working directory and each output's parent. The
        // outputs are written to the outputs share, so their parents are made there.
        real_dirs(&root, &command.working_directory)
            .await
            .map_err(tree_error)?;
        for output in &outputs {
            let parent = output.rsplit_once('/').map_or("", |(parent, _)| parent);
            real_dirs(&outputs_dir, parent).await.map_err(tree_error)?;
        }
        let cwd = if command.working_directory.is_empty() {
            ROOT.to_owned()
        } else {
            format!("{ROOT}/{}", command.working_directory)
        };
        let request = RunRequest {
            argv: command.arguments.clone(),
            env: (command.environment_variables.iter())
                .map(|v| (v.name.clone(), v.value.clone()))
                .collect(),
            cwd,
            // The whole of the action's timeout: the boot is not charged to it.
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            outputs: outputs.clone(),
        };
        Ok(Prepared {
            request,
            outputs,
            timeout,
        })
    }

    /// Fails unless the clones' volume has [`VmConfig::min_free_bytes`] free.
    fn check_free_space(&self) -> Result<(), RuntimeError> {
        let clones = self.config.clones();
        let stat = rustix::fs::statvfs(&clones)
            .map_err(std::io::Error::from)
            .map_err(failed(&clones))?;
        let free = stat.f_bavail.saturating_mul(stat.f_frsize);
        if free < self.config.min_free_bytes {
            return Err(RuntimeError::Failed(format!(
                "{} has {free} bytes free, less than the {} a clone needs",
                clones.display(),
                self.config.min_free_bytes
            )));
        }
        Ok(())
    }

    /// Waits for the guest's `Ready`, at most [`VmConfig::boot_timeout`] from now.
    async fn boot(
        &self,
        name: &str,
        token: [u8; TOKEN_LEN],
    ) -> Result<Client<H::Stream>, RuntimeError> {
        let host = Arc::clone(&self.host);
        let name = name.to_owned();
        let (timeout, poll) = (self.config.boot_timeout, self.config.boot_poll);
        blocking(move || {
            let deadline = Instant::now() + timeout;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(RuntimeError::Failed(format!(
                        "the guest was not ready within the boot timeout of {timeout:?}"
                    )));
                }
                match host.connect(&name) {
                    Ok(stream) => return handshake(stream, token, left),
                    Err(HostError::NotReady) => std::thread::sleep(poll.min(left)),
                    Err(e) => return Err(host_error(e)),
                }
            }
        })
        .await
    }

    /// Runs the command and waits for its report, the stop or the guest's silence.
    async fn execute(
        &self,
        mut client: Client<H::Stream>,
        prepared: &Prepared,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<Exit, RuntimeError> {
        let request = prepared.request.clone();
        // On a stop or the guest's silence the thread is left blocked on the stream
        // until the VM is stopped, which ends the stream. A thread of its own, not the
        // runtime's blocking pool: a stream that never ends must not hold up the
        // runtime's shutdown.
        let grace = self.config.report_grace;
        let (report, reported) = oneshot::channel();
        std::thread::spawn(move || {
            let exit = (client.run(&request).map_err(client_error))
                .and_then(|_| client.wait().map_err(client_error));
            let _ = report.send(exit);
        });
        tokio::select! {
            // A reader that panicked sends nothing; the grace below ends the lease.
            Ok(exit) = reported => exit,
            () = tokio::time::sleep(prepared.timeout + grace) => Err(RuntimeError::Failed(
                format!("the guest sent no report within {grace:?} past the action's timeout"),
            )),
            Ok(by) = &mut *stop => {
                *killer = Some(by);
                Err(RuntimeError::Killed)
            }
        }
    }

    /// Every step but the destroy.
    async fn attempt(
        &self,
        work: &Work,
        lease: &Lease<H>,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<ActionResult, RuntimeError> {
        let mut clock = ExecutedActionMetadata {
            worker_start_timestamp: now(),
            input_fetch_start_timestamp: now(),
            ..ExecutedActionMetadata::default()
        };
        let parts = &lease.parts;
        let prepared = tokio::select! {
            prepared = self.prepare(&parts.dir, work) => prepared?,
            Ok(by) = &mut *stop => {
                *killer = Some(by);
                return Err(RuntimeError::Killed);
            }
        };
        clock.input_fetch_completed_timestamp = now();
        let (cpus, memory_bytes) = vm_size(work.resources)?;
        self.check_free_space()?;

        let inputs = parts.dir.join(INPUTS_TAG);
        let token: [u8; TOKEN_LEN] = random()?;
        let token_file = inputs.join(TOKEN_FILE);
        tokio::fs::write(&token_file, hex::encode(token))
            .await
            .map_err(failed(&token_file))?;
        let spec = VmSpec {
            name: parts.name.clone(),
            clone: parts.clone.clone(),
            machine_id: MachineId(random()?),
            cpus,
            memory_bytes,
            shares: vec![
                Share {
                    tag: INPUTS_TAG,
                    host: inputs,
                    read_only: true,
                },
                Share {
                    tag: OUTPUTS_TAG,
                    host: parts.dir.join(OUTPUTS_TAG),
                    read_only: false,
                },
            ],
            network: Vec::new(),
        };
        let (host, image) = (Arc::clone(&self.host), self.config.image.clone());
        let started = spec.clone();
        blocking(move || {
            host.clone_image(&image, &started.clone)
                .map_err(failed(&started.clone))?;
            host.start(&started).map_err(host_error)
        })
        .await?;

        // The boot is not charged to the action: its timeout starts with `Run`.
        let client = tokio::select! {
            client = self.boot(&spec.name, token) => client?,
            Ok(by) = &mut *stop => {
                *killer = Some(by);
                return Err(RuntimeError::Killed);
            }
        };
        clock.execution_start_timestamp = now();
        let exit = self.execute(client, &prepared, stop, killer).await?;
        clock.execution_completed_timestamp = now();
        // Stopped before the outputs are read, so nothing in the guest writes to the
        // share while it is read: a process of the command the guest could not end
        // (`Exit::stragglers`) ends with the VM.
        parts.stop_vm().await?;
        let exit_code = exit_code(exit.end)?;

        clock.output_upload_start_timestamp = now();
        let store = CasStore(Arc::clone(&self.cas));
        let mut result = ActionResult {
            exit_code,
            ..ActionResult::default()
        };
        let outputs_dir = parts.dir.join(OUTPUTS_TAG);
        kbf_outputs::collect(
            &store,
            &outputs_dir,
            "",
            &prepared.outputs,
            self.config.outputs,
            &mut result,
        )
        .await
        .map_err(outputs_error)?;
        let max = self.config.outputs.max_stdio_bytes;
        for (name, slot) in [
            (kbf_guest::run::STDOUT, &mut result.stdout_digest),
            (kbf_guest::run::STDERR, &mut result.stderr_digest),
        ] {
            let stored = kbf_outputs::store_file(&store, &outputs_dir.join(name), max).await;
            *slot = Some(stored.map_err(outputs_error)?);
        }
        clock.output_upload_completed_timestamp = now();
        clock.worker_completed_timestamp = now();
        result.execution_metadata = Some(clock);
        Ok(result)
    }
}

impl<H: VmHost, C: Cas> Runtime for VmDriver<H, C> {
    fn driver(&self) -> &'static str {
        DRIVER
    }

    fn serves(&self, kind: &str) -> bool {
        kind == KIND
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let (stop_tx, mut stop) = oneshot::channel();
        let _registered = Registered::new(self, work.lease_id, stop_tx);
        let name = lease_name(work.lease_id);
        let lease = Lease::new(Parts {
            host: Arc::clone(&self.host),
            dir: self.config.leases().join(&name),
            clone: self.config.clones().join(&name),
            name,
        });
        let mut killer = None;
        let outcome = self.attempt(&work, &lease, &mut stop, &mut killer).await;
        let destroyed = lease.destroy().await;
        // A kill that arrived after the work ended still waits for the destroy.
        if let Some(by) = killer.or_else(|| stop.try_recv().ok()) {
            let _ = by.send(());
        }
        match (outcome, destroyed) {
            (Ok(_), Err(failed)) => Err(failed),
            (outcome, Err(failed)) => {
                tracing::error!(lease = %work.lease_id, "{failed}");
                outcome
            }
            (outcome, Ok(())) => outcome,
        }
    }

    async fn kill(&self, lease_id: LeaseId) {
        let Some(stop) = self.stops().remove(&lease_id) else {
            return;
        };
        let (tx, rx) = oneshot::channel();
        // Answered once the VM and the lease's files are gone. If the run ended without
        // seeing the kill, `tx` is dropped with it and `rx` returns at once.
        let _ = stop.send(tx);
        let _ = rx.await;
    }
}

/// Connects to the guest over `stream` with `token`, its reads bounded by `left` until
/// `Ready`, and checks the session it runs in.
fn handshake<S: kbf_guest::server::Stream>(
    stream: S,
    token: [u8; TOKEN_LEN],
    left: Duration,
) -> Result<Client<S>, RuntimeError> {
    let io = |e: std::io::Error| RuntimeError::Failed(format!("the guest's stream: {e}"));
    // A second handle on the socket, whose read timeout is the socket's own.
    let control = stream.try_clone().map_err(io)?;
    control.set_read_timeout(Some(left)).map_err(io)?;
    let client = Client::connect(stream, token).map_err(client_error)?;
    // The command may run for hours without a message: no read bound from here on.
    control.set_read_timeout(None).map_err(io)?;
    if client.session() != GUI_SESSION {
        return Err(RuntimeError::Failed(format!(
            "the guest agent runs in the session {:?}, not {GUI_SESSION}",
            client.session()
        )));
    }
    Ok(client)
}

/// The cores and guest memory a lease's booking gives its VM: every booked core, the
/// last one rounded up, and the booked memory less the helper's.
///
/// # Errors
/// The booking leaves the guest no memory: the farm's error, not the action's.
fn vm_size(resources: Resources) -> Result<(u32, u64), RuntimeError> {
    // A booking past u32::MAX cores is not one a host can start; it refuses it.
    let cpus = u32::try_from(resources.cpu_millis.div_ceil(1000).max(1)).unwrap_or(u32::MAX);
    match resources.memory_bytes.checked_sub(HELPER_MEMORY_BYTES) {
        Some(memory) if memory > 0 => Ok((cpus, memory)),
        _ => Err(RuntimeError::Failed(format!(
            "the lease booked {} bytes of memory, which leaves its VM none past the helper's {HELPER_MEMORY_BYTES}",
            resources.memory_bytes
        ))),
    }
}

/// How the guest's report ends the lease: an exit, or a signal as 128 + its number, is
/// the action's result; the guest's timeout is the action's; a kill nobody asked for
/// (the guest's connection dropped) is the farm's.
///
/// # Errors
/// [`RuntimeError::TimedOut`] or [`RuntimeError::Failed`], as above.
fn exit_code(end: End) -> Result<i32, RuntimeError> {
    match end {
        End::Exited(code) => Ok(code),
        End::Signaled(signal) => Ok(128 + signal),
        End::TimedOut => Err(RuntimeError::TimedOut),
        End::Killed => Err(RuntimeError::Failed(
            "the guest killed the command, unasked".to_owned(),
        )),
    }
}

/// A host's error as the lease reports it: every one is the farm's, to run elsewhere.
/// The limit of running VMs among them: the scheduler booked a VM slot this node did
/// not have.
fn host_error(error: HostError) -> RuntimeError {
    RuntimeError::Failed(format!("VM host: {error}"))
}

/// A guest's error as the lease reports it. A request the guest refuses as malformed,
/// or whose program it cannot start, is the action's; anything else (a wrong token, a
/// dropped connection) is the farm's.
fn client_error(error: ClientError) -> RuntimeError {
    match error {
        ClientError::Refused {
            reason: Refusal::BadRequest | Refusal::StartFailed,
            detail,
        } => RuntimeError::Invalid(detail),
        other => RuntimeError::Failed(format!("guest: {other}")),
    }
}

/// `N` bytes from the OS's random source.
fn random<const N: usize>() -> Result<[u8; N], RuntimeError> {
    let mut bytes = [0; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| RuntimeError::Failed(format!("/dev/urandom: {e}")))?;
    Ok(bytes)
}

/// Runs `f` on the blocking pool.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, RuntimeError> + Send + 'static,
) -> Result<T, RuntimeError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| RuntimeError::Failed(format!("blocking task: {e}")))?
}

fn lease_name(id: LeaseId) -> String {
    format!("lease-{}-{}", id.term, id.seq)
}

/// What a lease has on the host: its VM, its clone and its lease directory.
struct Parts<H> {
    host: Arc<H>,
    name: String,
    dir: PathBuf,
    clone: PathBuf,
}

impl<H: VmHost> Parts<H> {
    /// Stops the VM, off the async threads.
    async fn stop_vm(&self) -> Result<(), RuntimeError> {
        let (host, name) = (Arc::clone(&self.host), self.name.clone());
        blocking(move || {
            host.stop(&name)
                .map_err(|e| RuntimeError::Failed(format!("stopping the VM {name}: {e}")))
        })
        .await
    }

    /// Stops the VM, then removes the clone and the lease directory (a removal that
    /// returns has left nothing). Every step is tried whatever an earlier one did.
    fn destroy(&self) -> Result<(), RuntimeError> {
        let mut errors = Vec::new();
        if let Err(e) = self.host.stop(&self.name) {
            errors.push(format!("stop the VM {}: {e}", self.name));
        }
        for path in [&self.clone, &self.dir] {
            if let Err(e) = kbf_outputs::remove_tree(path) {
                errors.push(format!("remove {}: {e}", path.display()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::Failed(format!(
                "destroy: {}",
                errors.join("; ")
            )))
        }
    }
}

/// A lease's [`Parts`], destroyed when the run ends, or when the daemon drops it
/// (blocking, then).
struct Lease<H: VmHost> {
    parts: Arc<Parts<H>>,
    armed: bool,
}

impl<H: VmHost> Lease<H> {
    fn new(parts: Parts<H>) -> Self {
        Self {
            parts: Arc::new(parts),
            armed: true,
        }
    }

    /// Destroys the lease's parts, off the async threads.
    async fn destroy(mut self) -> Result<(), RuntimeError> {
        self.armed = false;
        let parts = Arc::clone(&self.parts);
        blocking(move || parts.destroy()).await
    }
}

impl<H: VmHost> Drop for Lease<H> {
    fn drop(&mut self) {
        if self.armed {
            let destroyed = self.parts.destroy();
            tracing::warn!(vm = %self.parts.name, ?destroyed, "run dropped; VM destroyed");
        }
    }
}

/// A lease's entry in the kill table, removed when its run ends or is dropped.
struct Registered<'a, H, C> {
    driver: &'a VmDriver<H, C>,
    id: LeaseId,
}

impl<'a, H: VmHost, C: Cas> Registered<'a, H, C> {
    fn new(driver: &'a VmDriver<H, C>, id: LeaseId, stop: oneshot::Sender<Stop>) -> Self {
        driver.stops().insert(id, stop);
        Self { driver, id }
    }
}

impl<H, C> Drop for Registered<'_, H, C> {
    fn drop(&mut self) {
        self.driver
            .stops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

fn now() -> Option<Timestamp> {
    Some(Timestamp::from(SystemTime::now()))
}

/// The action's timeout, or `default` when it names none (or zero).
fn timeout_of(action: &Action, default: Duration) -> Result<Duration, RuntimeError> {
    let Some(timeout) = &action.timeout else {
        return Ok(default);
    };
    let (Ok(seconds), Ok(nanos)) = (u64::try_from(timeout.seconds), u32::try_from(timeout.nanos))
    else {
        return Err(RuntimeError::Invalid(
            "the action's timeout is negative".to_owned(),
        ));
    };
    let timeout = Duration::new(seconds, nanos);
    Ok(if timeout.is_zero() { default } else { timeout })
}

/// A [`TreeError`] as the lease reports it, as the native driver does.
fn tree_error(error: TreeError) -> RuntimeError {
    match error {
        TreeError::Cas(CasError::Missing(blob)) => RuntimeError::MissingBlob(blob),
        TreeError::Invalid(why) => RuntimeError::Invalid(why),
        other => RuntimeError::Failed(other.to_string()),
    }
}

/// An [`OutputsError`] as the lease reports it, as the native driver does.
fn outputs_error(error: OutputsError) -> RuntimeError {
    match error {
        OutputsError::Invalid(why) => RuntimeError::Invalid(why),
        other => RuntimeError::Failed(other.to_string()),
    }
}

/// An I/O failure at `path` as the lease reports it: the farm's.
fn failed(path: &Path) -> impl FnOnce(std::io::Error) -> RuntimeError + '_ {
    move |error| RuntimeError::Failed(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// Catches a guest given the whole booking (the helper's 1 GiB counted twice on
    /// the node), cores rounded down (a 1.5 core booking given one), and a booking
    /// that leaves the guest no memory started anyway.
    #[test]
    fn a_vm_gets_the_booked_cores_and_the_memory_past_the_helper() {
        let size = |millis, memory| vm_size(Resources::new(millis, memory));
        assert_eq!(size(6000, 17 * GIB).ok(), Some((6, 16 * GIB)));
        assert_eq!(size(1500, 2 * GIB).ok(), Some((2, GIB)));
        assert_eq!(size(0, 2 * GIB).ok(), Some((1, GIB)));
        for memory in [0, GIB] {
            let refused = size(6000, memory);
            assert!(
                matches!(refused, Err(RuntimeError::Failed(ref why)) if why.contains("none past")),
                "{refused:?}"
            );
        }
    }

    /// Catches a signal reported as its bare number (a SIGKILL read as exit 9), the
    /// guest's timeout reported as an exit, and a kill nobody asked for counted as the
    /// action's result.
    #[test]
    fn the_guest_report_is_the_action_result_or_whose_failure_it_is() {
        assert_eq!(exit_code(End::Exited(3)).ok(), Some(3));
        assert_eq!(exit_code(End::Signaled(9)).ok(), Some(137));
        assert!(matches!(
            exit_code(End::TimedOut),
            Err(RuntimeError::TimedOut)
        ));
        assert!(matches!(
            exit_code(End::Killed),
            Err(RuntimeError::Failed(_))
        ));
    }

    /// Catches a refusal of the action's own request (a reserved output name, a
    /// program that does not start) reported as the farm's, which the server would
    /// treat as an infrastructure failure, and a broken handshake reported as the
    /// client's.
    #[test]
    fn a_guest_refusal_is_the_clients_only_for_the_request() {
        for reason in [Refusal::BadRequest, Refusal::StartFailed] {
            let error = client_error(ClientError::Refused {
                reason,
                detail: "no".to_owned(),
            });
            assert!(matches!(error, RuntimeError::Invalid(_)), "{reason:?}");
        }
        for reason in [
            Refusal::Token,
            Refusal::Version,
            Refusal::AlreadyRan,
            Refusal::NoHello,
        ] {
            let error = client_error(ClientError::Refused {
                reason,
                detail: "no".to_owned(),
            });
            assert!(matches!(error, RuntimeError::Failed(_)), "{reason:?}");
        }
    }

    /// Catches an action's own timeout ignored for the driver's default, and a zero
    /// timeout (REAPI: none given) taken as "end at once".
    #[test]
    fn the_timeout_is_the_actions_or_the_default() {
        let default = Duration::from_secs(60);
        let with = |seconds, nanos| Action {
            timeout: Some(prost_types::Duration { seconds, nanos }),
            ..Action::default()
        };
        assert_eq!(timeout_of(&Action::default(), default).ok(), Some(default));
        assert_eq!(timeout_of(&with(0, 0), default).ok(), Some(default));
        assert_eq!(
            timeout_of(&with(2, 5), default).ok(),
            Some(Duration::new(2, 5))
        );
        assert!(matches!(
            timeout_of(&with(-1, 0), default),
            Err(RuntimeError::Invalid(_))
        ));
    }
}
