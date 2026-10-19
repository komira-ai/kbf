//! The root half of the pull-based updates (`docs/design/pull-updates.md`, section 7), run
//! as `kbf-installer run` and `kbf-installer boot-check`.
//!
//! It has no socket. The unprivileged `kbf` leaves one kind of file in the order spool: a
//! small order record per order ([`order`]), and no release bytes. Each pass the helper
//!
//! 1. finishes a restart it owes and reverts a swap whose confirm window ran out;
//! 2. takes the records in the spool, at most [`kbf_update::MAX_RECORDS`], none older than
//!    [`kbf_update::MAX_AGE_SECS`] since root first saw it, each opened without following a
//!    link and read once, and acts on them in (epoch, id) order, once each: `activate`
//!    checks the order, has the [`fetcher::Fetcher`] pull and verify the release if its
//!    directory is not there, swaps it in on trial and restarts the one configured
//!    service; `confirm` keeps it; `revert` goes back; on a frontend host (`kbf.toml` mode `frontend` or `all-in-one`)
//!    the frontend proxy's `frontend_update` stages a release without moving a link, and its
//!    `confirm` and `revert` flip to the staged release or drop it ([`cycle`], `frontend.rs`);
//! 3. collects the releases nothing needs once a swap was confirmed (`gc` keeps current,
//!    previous, staged, the swap on trial and any release a process executes from, and
//!    takes the lock);
//! 4. writes `status.json` (root's, world-readable) for the node to report.
//!
//! The loop ([`serve`]) makes a pass every five seconds (fixed) and, between passes,
//! starts the new program in place of itself if its own file was replaced. A confirm is
//! kept only if the new release is what runs ([`service::accept_confirm`]); one activation
//! is on trial at a time and activations are `min_activation_interval_s` apart. The
//! operator's `confirm`, `revert`, `rollback`, `gc` and `quiesce` ([`operator`]) run inline
//! as root under the same lock.
//!
//! `kbf-installer boot-check` runs as the service's `ExecStartPre` on Linux: it counts the
//! starts of a release on trial and reverts one that keeps failing before it starts again,
//! which needs no timer. It also starts the confirm window at the first start.
//!
//! Nothing here takes a path, a command or a unit from a record: the unit and the
//! directories are the helper's own flags.

pub mod cycle;
pub mod fetcher;
pub mod host_mode;
pub mod leases;
pub mod memory;
pub mod operator;
pub mod order;
pub mod restart;
pub mod serve;
pub mod service;
pub mod status;

#[cfg(test)]
mod crash_tests;
#[cfg(test)]
mod tests;
