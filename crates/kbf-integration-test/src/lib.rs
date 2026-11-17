//! Integration tests that run in hosted CI, including the workspace layering test
//! in `tests/layering.rs`, and the repository lints applied by `tests/repo_lints.rs`:
//! the workflow lint in `tests/workflows/` (a test-only module, so its YAML parser
//! stays a dev-dependency) and [`hygiene`] for public text, and the check of the
//! example systemd units in `docs/deploy/` against the binaries' flag parsers
//! ([`deploy_units`], `tests/deploy_units.rs`).
//!
//! It also holds `kbf-js-stand-in` ([`js_stand_in`], Linux only), the `crun`, `ip`, `nft`
//! and `nsenter` that `tests/executor_real_service_integration.rs` gives the pinned js-agent,
//! and the throwaway certificates of the proxy and object-store tests ([`pki`]).

pub mod deploy_units;
pub mod doc_commands;
pub mod hygiene;
#[cfg(target_os = "linux")]
pub mod js_stand_in;
pub mod pki;
