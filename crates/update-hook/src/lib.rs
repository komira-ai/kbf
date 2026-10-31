#![allow(unused)]
//! The contract between a host updater and the update hook a service ships, and the harness that
//! proves a hook keeps it. See the README of this crate, and docs/design/host-updates.md for why.
//!
//! The hook is **any executable**: a binary or a script. The updater runs
//! `<executable> <hook_args> <step> [args]`; `update-hook`, the default of `hook_args`, is the
//! convention our own services follow (`tierstore update-hook cutover`). Nothing here requires
//! Rust of a hook; a Rust hook uses [`Invocation`], [`Outcome`] and [`Report`] to speak the
//! contract, and every hook is tested by [`conformance`].
//!
//! The crate depends on no other crate of this workspace, so a service outside it can depend on
//! the contract; a test in kbf-integration-test keeps it so.

pub mod conformance;
mod exit;
mod line;
mod next;
mod params;
mod step;

pub use exit::{EXIT_DEFERRED, EXIT_FAILURE, EXIT_HALT, EXIT_OK, Outcome};
pub use line::{Class, LineError, MAX_REASON_LEN, Report, is_reason_token};
pub use next::{Context, Next, Phase, Verdict, next};
pub use params::{PROMOTION_CEILING_KEY, Params, ParamsError, Scalar};
pub use step::{ArgsError, Invocation, Step};
