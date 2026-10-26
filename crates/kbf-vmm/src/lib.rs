//! `kbf-vmm`: the VM helper for macOS VM leases (docs/design/macos-vm-guests.md).
//!
//! Today it holds the feasibility probes of the design's gates (`probe`, `probe-bench`) and
//! the base-image builder (`image base`). The logic that decides each verdict is plain
//! Rust over the [`system::System`] trait and runs in unit tests on any OS; the one place
//! that calls Virtualization.framework is `vz` (macOS only), reached through `real`.

pub mod app;
pub mod bench;
pub mod bundle;
pub mod cli;
pub mod e1;
pub mod e3;
pub mod gates;
pub mod guest;
pub mod guest_command;
pub mod harvest;
pub mod hostfs;
pub mod image;
pub mod matrix;
pub mod offline_put;
pub mod probe;
pub mod report;
pub mod sampler;
pub mod signature;
pub mod sink;
pub mod spec;
pub mod stage;
pub mod system;
pub mod volume;
mod xattr;

#[cfg(target_os = "macos")]
mod offline;
#[cfg(target_os = "macos")]
pub mod real;
#[cfg(target_os = "macos")]
mod vz;

#[cfg(test)]
mod app_tests;
#[cfg(test)]
mod bench_tests;
#[cfg(test)]
mod e3_tests;
#[cfg(test)]
mod fake;
#[cfg(test)]
mod gates_tests;
#[cfg(test)]
mod probe_tests;
#[cfg(test)]
mod stage_tests;
