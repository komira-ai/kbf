//! The VM driver for `kbf-daemon`: runs each `vm` lease in a macOS virtual machine of
//! its own, booted from a fresh clone of a golden image and destroyed with the clone
//! when the lease ends (`docs/design/macos-vms.md` section 6). The hypervisor side is
//! the [`VmHost`] trait; [`FakeHost`] implements it with no hypervisor, running the real
//! `kbf-guest` agent on the host, and the macOS host ([`MacVmHost`], which runs `kbf-vmm
//! vm-run` through the root helper) is built over a [`VmSession`]. The daemon does not
//! offer this driver yet.
//!
//! One lease, in order:
//! 1. **Prepare.** The Action and Command are fetched. An action that asks for the
//!    network (platform property `network` = `on` or `standard`) is refused with
//!    INVALID_ARGUMENT: network-on VM leases wait for the design that keeps a guest off
//!    the farm's own network. The input root is written to `root/` in the lease's
//!    inputs share, the working directory below it; each declared output's parent is
//!    made in the outputs share.
//! 2. **Size.** The VM gets every booked core (the last rounded up) and the booked
//!    memory less the 1 GiB booked for the helper process
//!    ([`config::HELPER_MEMORY_BYTES`]); a booking that leaves the guest nothing fails
//!    the lease as the farm's.
//! 3. **Free space.** The clones' volume must have [`VmConfig::min_free_bytes`] free, or
//!    the lease fails as the farm's before anything is cloned.
//! 4. **Clone and start.** A per-boot token from the OS's random source is written to
//!    [`TOKEN_FILE`] in the inputs share. The golden image is cloned
//!    ([`VmHost::clone_image`]) and the VM started from the clone with a new
//!    [`MachineId`], two virtiofs shares (inputs read-only, outputs read-write) and no
//!    network device ([`VmSpec`]).
//! 5. **Boot.** The driver connects to the guest's `kbf-guest` agent until it answers
//!    `Ready`, within [`VmConfig::boot_timeout`] (120 s by default), and refuses a guest
//!    whose agent is not in the [`GUI_SESSION`]. The boot is not charged to the action:
//!    the guest gets the action's whole timeout with the `Run`.
//! 6. **Run.** One `Run`: argv, the Command's environment, the working directory and the
//!    declared outputs. The guest writes stdout and stderr to the outputs share and
//!    enforces the timeout; the driver waits for its report, at most
//!    [`VmConfig::report_grace`] (30 s by default) past the timeout.
//! 7. **Collect.** The VM is stopped, then the outputs are read from the host side of the
//!    outputs share with `kbf-outputs`, which follows no symlinks, with stdout and
//!    stderr.
//! 8. **Destroy, always.** On every path (an exit, a failure, a kill, the daemon
//!    dropping the run) the VM is stopped and the clone and the lease directory removed;
//!    either one left fails the lease.
//!
//! How each failure is reported: a host's refusal to start another VM
//! ([`HostError::LimitExceeded`]), a boot past its timeout, a guest that crashes or goes
//! silent, too little free space and every other host or guest failure are the farm's
//! ([`kbf_daemon::RuntimeError::Failed`], which the server records as an
//! infrastructure failure), never the action's. The guest's timeout is
//! [`kbf_daemon::RuntimeError::TimedOut`]; a request the guest refuses as malformed, or
//! whose program it cannot start, is the client's
//! ([`kbf_daemon::RuntimeError::Invalid`]).
//!
//! At start ([`VmDriver::new`]) everything in the lease and clone directories is
//! removed: a daemon that crashed left it. Its VMs end with it: `kbf-vmm vm-run` serves a VM over
//! its stdin and stdout and force-stops the VM at the end of its stdin; the macOS host
//! also ends the runs it recorded ([`mac`]).
//!
//! Not yet: the relay from the macOS host to the guest agent, the daemon flags that turn the driver on, auxiliary storage
//! per clone, the guest's resource usage in the result, and how an action inside the
//! guest names the outputs share (`kbf-guest`'s request carries no path for it).

pub mod clock;
pub mod config;
mod driver;
mod fake;
pub mod host;
pub mod mac;
#[cfg(test)]
mod tests;

// The act steps the tests' commands run come from the test binary.
#[cfg(test)]
kbf_testkit::act_test_hook!();

pub use clock::{Clock, Wall};
pub use config::VmConfig;
pub use driver::{DRIVER, KIND, VmDriver};
pub use fake::{CloneGate, FakeHost, FakeStream, MACOS_VM_LIMIT};
pub use host::{HostError, MachineId, NetworkDevice, Share, VmHost, VmSpec};
pub use mac::{MacConfig, MacVmHost, VmSession};

/// The file in the inputs share holding the per-boot token, as hex: the name
/// `kbf-guest serve --token-file` is given.
pub const TOKEN_FILE: &str = "kbf-guest.token";

/// The launchd session the guest agent must run in: a logged-in GUI session, which UI
/// tests need.
pub const GUI_SESSION: &str = "Aqua";
