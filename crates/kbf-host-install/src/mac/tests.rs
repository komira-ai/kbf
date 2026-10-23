//! The Mac side, on any machine: the tools it runs are a fake launchd, codesign and the
//! rest ([`fx::Sim`]) over real files under a scratch directory.

mod apply_tests;
mod dispatch;
mod fx;
mod install;
mod pure;
mod query;
mod stage_tests;
