//! What the host updater lets a hook do: the declaration file, the rules it is loaded under, the
//! comparison that finds a declaration asking for more, the record of approvals, and the
//! transient systemd unit that runs one step with exactly the declared rights. The reasons are
//! in docs/design/host-updates.md, sections 2.4, 3.3 and 3.4, and this crate's README.
//!
//! The crate names no product and depends on no crate of this workspace but the hook contract
//! (`update-hook`); a layering test of the workspace keeps both true.

pub mod accounts;
pub mod approval;
pub mod cli;
pub mod declaration;
pub mod digest;
pub mod gate;
pub mod launch;
pub mod load;
pub mod names;
pub mod reserved;
pub mod trust;
pub mod widening;

pub use accounts::{Accounts, EtcAccounts, UserIds};
pub use approval::{APPROVALS_FILE, ApprovalError, Approvals, Approved, approve};
pub use declaration::{Declaration, Hook, Refusal, Signer, StepCapabilities, Timeouts};
pub use digest::{BadDigest, Digest};
pub use gate::{Deferral, FIRST_INSTALL, Gate, RIGHTS_CHANGE, gate};
pub use launch::{
    Finished, Launch, LaunchError, RunError, Spawner, StepRun, Systemd, SystemdRun, command,
    properties, run_step, unit_name,
};
pub use load::{DECLARATION_DIR, Declared, Loaded, Policy, Refused, check_state_dir, load_dir};
pub use widening::widening;
