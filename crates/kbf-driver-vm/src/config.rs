//! [`VmConfig`]: what a node sets for its VM driver.

use std::path::PathBuf;
use std::time::Duration;

use kbf_outputs::OutputLimits;

/// How long a guest may take from `start` to its agent's `Ready`: 120 s until boot is
/// measured (expected 20-60 s, `docs/design/macos-vms.md` section 6, step 3). Not
/// charged to the action's timeout.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(120);

/// The memory booked for the VM's helper process, out of the lease's booking: the guest
/// gets the rest (16 GiB of the default 17 GiB booking).
pub const HELPER_MEMORY_BYTES: u64 = 1 << 30;

/// The free space the clone's volume must have before a clone is made. A clone costs
/// nothing until written, so this is room for what a guest writes; the figure is an
/// assumption until guests are measured.
pub const MIN_FREE_BYTES: u64 = 32 << 30;

/// How long past the action's timeout the driver waits for the guest's report before it
/// ends the run itself: the guest enforces the timeout and drains the command's group
/// for up to 5 s ([`kbf_guest::run::DRAIN`]), so a report later than this means the
/// guest is not answering.
pub const REPORT_GRACE: Duration = Duration::from_secs(30);

/// The VM driver's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmConfig {
    /// The golden image's directory; every lease boots a clone of it.
    pub image: PathBuf,
    /// Where lease directories (`leases/`) and clones (`clones/`) go. On a Mac, on the
    /// image's volume, so a clone is copy-on-write.
    pub scratch: PathBuf,
    /// See [`BOOT_TIMEOUT`].
    pub boot_timeout: Duration,
    /// How often a booting guest is tried.
    pub boot_poll: Duration,
    /// The timeout of an action that names none.
    pub default_timeout: Duration,
    /// See [`MIN_FREE_BYTES`].
    pub min_free_bytes: u64,
    /// The output limits, as for the other drivers.
    pub outputs: OutputLimits,
}

impl VmConfig {
    /// The defaults above, a one hour default timeout and the default output limits.
    #[must_use]
    pub fn new(image: PathBuf, scratch: PathBuf) -> Self {
        Self {
            image,
            scratch,
            boot_timeout: BOOT_TIMEOUT,
            boot_poll: Duration::from_millis(250),
            default_timeout: Duration::from_secs(3600),
            min_free_bytes: MIN_FREE_BYTES,
            outputs: OutputLimits::DEFAULT,
        }
    }

    /// The directory lease directories go in.
    #[must_use]
    pub fn leases(&self) -> PathBuf {
        self.scratch.join("leases")
    }

    /// The directory clones go in.
    #[must_use]
    pub fn clones(&self) -> PathBuf {
        self.scratch.join("clones")
    }
}
