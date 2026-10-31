//! The Mac side, on any machine: the tools it runs are a fake launchd, codesign and the
//! rest ([`fx::Sim`]) over real files under a scratch directory.

mod apply_tests;
mod dispatch;
pub(crate) mod fx;
mod github_tests;
mod install;
mod pure;
mod query;
mod real_output;
mod stage_tests;
mod tierstore_signer_tests;
