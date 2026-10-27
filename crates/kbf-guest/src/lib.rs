//! `kbf-guest`: the agent inside a macOS VM guest (`docs/design/macos-vms.md` section 6,
//! step 4). It is started as a LaunchAgent of the guest's non-admin auto-login user,
//! refuses to run as root, and runs exactly one command per boot for the host.
//!
//! - [`wire`]: the framed, versioned messages, byte for byte.
//! - [`server`]: the handshake (version and per-boot token), the one-command rule,
//!   `Kill`, the timeout, and the exit report.
//! - [`run`]: the working directory checked against the inputs share, stdout and stderr
//!   written to the outputs share, the command's process group, `wait4` usage, and the
//!   report on requested outputs.
//! - [`client`]: the host side.
//! - [`session`]: the launchd session type `Ready` reports, and the root refusal.
//!
//! - [`mount`]: `mount-share`, the guest's own bounded virtiofs mount.
//! - [`frame`]: the frames probe results and ready markers travel in over virtio-socket.
//! - [`shutdown`]: the root daemon that listens on a virtio-socket port for the host's
//!   `shutdown` command and shuts the guest down cleanly.
//! - [`sim`]: the iOS simulator probe (`kbf-guest sim`), its steps, frames and deadlines;
//!   [`sim_run`] runs one command under a deadline.
//! - [`mkuser`]: the guest's user (`kbf-guest mkuser`), created once by `image prime`.
//! - [`simpaths`]: where `kbf-vmm image setup` stages Xcode and the simulator runtimes.
//! - [`probe`]: the probe modes `kbf-vmm probe g2` and `g3` read, which run as any user.
//!
//! It serves a Unix socket. The virtio socket a VM gives it, the relay through
//! `kbf-vmm`, the image that installs it and the VM driver that calls it are planned and
//! not built.

pub mod client;
pub mod diag;
pub mod frame;
pub mod mkuser;
pub mod mkuser_audit;
#[cfg(test)]
mod mkuser_audit_tests;
#[cfg(target_os = "macos")]
pub mod mkuser_host;
#[cfg(test)]
mod mkuser_tests;
pub mod mount;
pub mod probe;
#[cfg(target_os = "macos")]
pub mod probe_host;
pub mod run;
pub mod server;
pub mod session;
pub mod shutdown;
#[cfg(target_os = "macos")]
pub mod shutdown_host;
pub mod sim;
pub mod sim_choose;
#[cfg(target_os = "macos")]
pub mod sim_host;
pub mod sim_parse;
pub mod sim_run;
#[cfg(test)]
mod sim_tests;
pub mod sim_user;
pub mod sim_xctest;
pub mod simpaths;
pub mod vsock;
pub mod wire;
