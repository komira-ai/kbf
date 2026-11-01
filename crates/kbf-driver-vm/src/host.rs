//! [`VmHost`]: what the driver asks of the hypervisor side, and the [`VmSpec`] it boots.
//!
//! The driver decides everything a VM gets (its clone, identity, size, shares and
//! devices) and writes it into a [`VmSpec`]; a host only carries it out. So the rules
//! of `docs/design/macos-vms.md` section 6 are the driver's, and a test of the driver
//! against [`crate::FakeHost`] checks them by reading the specs the host was given.

use std::io;
use std::path::{Path, PathBuf};

use kbf_guest::server::Stream;

/// The tag of the share holding the input tree and the per-boot token.
pub const INPUTS_TAG: &str = "inputs";
/// The tag of the share holding the command's stdout, stderr and outputs.
pub const OUTPUTS_TAG: &str = "outputs";

/// The machine identifier a VM gets, new for every lease: two VMs of one image must not
/// share one ("each should have a unique auxiliaryStorage and machineIdentifier",
/// `VZMacPlatformConfiguration`). The driver draws it from the OS's random source; the
/// host turns it into the platform's identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineId(pub [u8; 16]);

/// A host directory shared into the guest over virtiofs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    /// The name the guest mounts it by ([`INPUTS_TAG`], [`OUTPUTS_TAG`]).
    pub tag: &'static str,
    /// The directory on the host.
    pub host: PathBuf,
    /// Whether the guest may only read it (`VZSharedDirectory`'s `readOnly`).
    pub read_only: bool,
}

/// A network device in a VM's configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkDevice {
    /// A virtio network device behind the host's network address translation. No lease
    /// gets one yet: an action that asks for the network is refused.
    Nat,
}

/// One VM, as the driver decided it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmSpec {
    /// The VM's name on the host, unique among running VMs: the lease's name.
    pub name: String,
    /// The clone of the golden image the VM boots from; [`VmHost::clone_image`] made it.
    pub clone: PathBuf,
    /// This VM's machine identifier.
    pub machine_id: MachineId,
    /// Virtual CPUs: the booked cores.
    pub cpus: u32,
    /// The guest's memory: the booking less what the helper process is booked.
    pub memory_bytes: u64,
    /// The inputs share (read-only) and the outputs share (read-write), in that order.
    pub shares: Vec<Share>,
    /// Empty unless the lease asked for the network, which no lease may yet.
    pub network: Vec<NetworkDevice>,
}

/// What a host could not do.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The host already runs as many VMs as it may (the macOS licence allows two
    /// macOS guests per Mac, and Virtualization.framework refuses a third).
    #[error("the host already runs its limit of {0} VMs")]
    LimitExceeded(usize),
    /// The VM has started but its guest agent cannot be reached yet.
    #[error("the guest is not reachable yet")]
    NotReady,
    /// Anything else, the VM not running among it.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// The hypervisor side of the VM driver. Calls block; the driver makes them off the
/// async threads. Implementations: [`crate::FakeHost`], which runs the real `kbf-guest`
/// agent on the host for tests, and [`crate::MacVmHost`], which runs `kbf-vmm vm-run` through the root helper on macOS.
pub trait VmHost: Send + Sync + 'static {
    /// The relayed byte stream to a guest's `kbf-guest` agent.
    type Stream: Stream;

    /// Clones the golden image's directory `image` to `clone`, which does not exist,
    /// copy-on-write where the file system allows (`clonefile` on APFS).
    ///
    /// # Errors
    /// The clone could not be made.
    fn clone_image(&self, image: &Path, clone: &Path) -> io::Result<()>;

    /// Starts the VM `spec` describes, from its clone. Returns once it is starting;
    /// its guest is reached with [`VmHost::connect`].
    ///
    /// # Errors
    /// [`HostError::LimitExceeded`] when no further VM may run; anything else that
    /// stops it starting.
    fn start(&self, spec: &VmSpec) -> Result<(), HostError>;

    /// A new stream to the guest agent of the running VM `name`.
    ///
    /// # Errors
    /// [`HostError::NotReady`] while the guest is booting; anything else once it cannot
    /// be reached at all (the VM stopped or crashed).
    fn connect(&self, name: &str) -> Result<Self::Stream, HostError>;

    /// Stops the VM `name` at once (no clean shutdown) and ends whatever serves it, so
    /// every stream to its guest ends. Stopping a VM that is not running does nothing.
    ///
    /// # Errors
    /// The VM may still be running.
    fn stop(&self, name: &str) -> io::Result<()>;
}
