//! `kbf-server`: the farm server, single node. It wires the front, the executor, the
//! metadata core and storage together; the decisions are theirs.
//!
//! - REAPI on one listener: the cache services and `Execution` from `kbf-front`, over
//!   a [`kbf_front::Cache`] (metadata in a single-voter Raft log on the local disk,
//!   [`raft_log`]; an S3 or local-directory object store, with an optional local hot
//!   store in front of it; the flags that choose them are `--cold-store`,
//!   `--hot-store` and `--meta-dir`, see [`stores`]).
//! - The worker listener on another: storage nodes register and heartbeat with
//!   `kbf.map.v1.Membership`, and a host's updater makes its node calls
//!   ([`node_self`]). Under mutual TLS a certificate must name its node, and a deny list
//!   refuses certificates and nodes ([`identity`]). No node opens a session to a server.
//! - The operator API ([`api`]): HTTP/JSON under `/v1` on a third listener, off unless
//!   `--api-listen` is given; the storage members and the frontend's drain, whose writes
//!   need the token of `--api-token-file` ([`token`]), and, behind the farm's proxy
//!   (`--proxy-auth-file`, [`proxy_auth`]), a call that came through it. On the same listener,
//!   `GET /healthz` and `GET /readyz` for a front's health check ([`health`]), and
//!   `GET /metrics`, the cache's upload-path metrics for Prometheus ([`metrics`]).
//!   The REAPI listener answers `/readyz`'s readiness over `grpc.health.v1`
//!   ([`grpc_health`]). And the build pages, `GET /v1/invocations/{id}` and below,
//!   over the invocation records kept in memory ([`http_invocations`]), off unless
//!   the REAPI policy names `invocations.getAuthorizer` (or `--invocations-open` is
//!   given); and `GET /v1/blobs/{digest}`, a CAS blob's bytes for a browser
//!   ([`http_blobs`]), off unless the policy also names `blobs.getAuthorizer`. Also on the
//!   same listener, on by default (`--ui=false` turns it off), the read-only, open web UI under
//!   `/ui` ([`ui_mount`]), whose reads are bounded per caller ([`read_limit`]).
//! - A drain of the frontend ([`drain`]) that a stop signal or `POST /v1/frontend/drain`
//!   begins, a second signal escalates, and `kbf cluster` calls through [`api_client`]. A
//!   frontend over a state store runs it as a sequence that works behind any load balancer
//!   ([`sequence`]): not ready first, then refusing and GOAWAY, then the open executions
//!   run out, then the process exits.
//! - Node rolling ([`update`]): the durable rows and facts of a split-mode update
//!   (`docs/design/split-mode-updates.md`, sections 2.6 to 2.11). Nothing picks hosts or
//!   steps them: updates are OS packages, driven by `kbf-updater`.
//! - Build history ([`history`]): off unless `--history-objects` is given, each attempt and
//!   cache hit of an invocation a client named is recorded in an object store, off the
//!   path of the call it belongs to, with a run id the build pages name the run by. It
//!   drops and counts when a store is slow; it never delays or fails a call.
//! - The durable log ([`log_file`]): a size-capped rotating file in `--log-dir` (by default
//!   `log/` in the hot store's directory) that gets every event stderr gets.
//! - The [`executor`]: each Execute that the action cache does not answer is one job of the
//!   job service, submitted at once and read back until it ends; the frontend's ingest of a
//!   job that ended well writes the action cache, before the callers are answered. Clients
//!   never write it. kbf schedules nothing: where and when a job runs is the job service's.
//!
//! - REAPI authentication and authorization (`kbf-auth`): the REAPI listener runs the
//!   policy of `--reapi-auth-policy` (`docs/reapi-auth.md`), an authentication layer
//!   on that listener alone and an authorizer per kind of call in the services;
//!   without the flag every call is accepted and allowed.
//!
//! - The storage node ([`storage_node`], `kbf storage`): the same cache and stores,
//!   served alone over gRPC by `kbf_storage::service` (blobs through the standard
//!   protocols, the action cache through `kbf.storage.v1`), each call admitted by the
//!   role in the caller's certificate. No executor, REAPI or state store.
//!
//! - The frontend ([`frontend`], `kbf frontend`): the REAPI listener, the executor and the
//!   operator API, with no cache of its own. CAS, ByteStream and the action cache are
//!   proxied to a storage node over mutual TLS, and `/readyz` follows it, the state store
//!   and the job service.
//!
//! Not yet: REAPI authentication other than the policies `kbf-auth` builds (`allow`, `deny`,
//! `any`, `all`), and operator roles (the API's writes need one token, [`token`]).

pub mod action_usage;
pub mod api;
pub mod api_client;
#[cfg(test)]
mod build_commit;
#[doc(hidden)]
pub mod build_seq;
pub mod cold_apply;
mod cold_counted;
pub mod config;
mod deny_rows;
pub mod drain;
pub mod enroll;
pub mod executor;
pub mod fill;
mod flag_set;
pub mod front_maps;
pub mod frontend;
pub mod grpc_health;
pub mod health;
mod health_summary;
pub mod history;
pub mod http_blobs;
mod http_history;
pub mod http_invocations;
pub mod identity;
pub mod job_tree;
pub mod join;
mod key_file;
mod listen;
pub mod log_file;
pub mod members;
pub mod members_rpc;
pub mod metrics;
pub mod node_map;
pub mod node_self;
mod notify;
pub mod object_store;
pub mod proxy_auth;
pub mod raft_log;
mod ram_drain;
pub mod read_limit;
mod ready;
mod rebuild;
pub mod release;
pub mod replication;
pub mod retire;
pub mod retired;
pub mod sequence;
pub mod serve;
#[cfg(test)]
mod serve_goaway_tests;
#[cfg(test)]
mod serve_remote_tests;
#[cfg(test)]
mod serve_start_tests;
mod stand;
mod start;
mod start_exit;
mod startup_pass;
pub mod state_store;
mod stop;
pub mod storage_gate;
pub mod storage_health;
pub mod storage_node;
pub mod store_config;
pub mod stores;
pub mod streams;
#[cfg(test)]
mod test_dir;
#[cfg(test)]
mod test_log;
pub mod token;
pub mod ui_mount;
pub mod verbs;
pub mod vitals;
pub mod worker;
pub mod worker_roles;
pub mod write_tier;

pub use config::{Args, ConfigError, Role, SERVER, s3_store};
pub use drain::{Drain, DrainStatus};
pub use executor::rows::{DONE_FLOOR, ID_BLOCK};
pub use health::Readiness;
pub use http_history::run_json;
pub use identity::{DenyList, DenyListError, Peers};
pub use kbf_tls::{TlsError, TlsFiles, TlsSource};
pub use listen::Held;
pub use node_map::NodeMap;
pub use raft_log::{RaftLogError, RaftMetaLog};
pub use serve::{
    Api, Behind, Bound, BoundStorage, Listeners, ServeError, StorageListener, StreamTiming,
    WorkerTls, bind_server, bind_server_with_api, bind_server_with_policy, bind_server_with_state,
    bind_server_with_storage, bind_service, bind_storage,
};
pub use start::{ServerHandle, interrupted, serve_with, sizes_on, start, start_interruptible};
pub use start_exit::{
    NOT_READY, REFUSED, STALE_WRITER_HINT, StartFailure, hint as start_hint, said as start_said,
};
pub use state_store::{
    OpenFailure, PackIndexFlagError, StateError, StateStore, StateStoreUri, StateStoreUriError,
};
pub use streams::{StreamArgs, StreamLimits};
pub use ui_mount::{bind_server_holding, bind_server_with_options, bind_server_with_ui};

/// The commit the server was built from: 12 hex digits, or `unknown` when the build
/// had no git checkout. `build.rs` embeds it (`src/build_commit.rs` has the rule,
/// including the CI-only `KBF_BUILD_COMMIT_OVERRIDE`).
pub const BUILD_COMMIT: &str = env!("KBF_BUILD_COMMIT");

/// The commit the server was built from in full: 40 lowercase hex digits, or the empty text
/// when the build did not know it (no git checkout, or a `KBF_BUILD_COMMIT_OVERRIDE` stamp).
/// [`BUILD_COMMIT`] is its first 12 digits when both are known.
pub const BUILD_COMMIT_FULL: &str = env!("KBF_BUILD_COMMIT_FULL");

/// The number of commits behind the commit the server was built from (`git rev-list --count
/// HEAD`), or 0 when the build could not count them (a shallow clone, no git): the rule is
/// `src/build_seq.rs`, and nothing is newer than 0. `kbf --version --json` prints it as `seq`.
pub const BUILD_SEQ: u64 = match u64::from_str_radix(env!("KBF_BUILD_SEQ"), 10) {
    Ok(seq) => seq,
    Err(_) => build_seq::UNKNOWN,
};

/// The server's version as it reports it in `--version`, the start line and the
/// `kbf --version --json`: the package version, `+`, and [`BUILD_COMMIT`].
pub const SERVER_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KBF_BUILD_COMMIT"));

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{BUILD_COMMIT, BUILD_COMMIT_FULL, SERVER_VERSION};

    /// `git rev-parse --short=12 HEAD` in this crate's checkout.
    fn git_head() -> String {
        let head = Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .expect("git runs");
        assert!(head.status.success(), "the tests run in a git checkout");
        String::from_utf8(head.stdout)
            .expect("UTF-8")
            .trim()
            .to_owned()
    }

    /// Catches: a version that names the package version alone, so two builds of it
    /// read the same in `--version` and the start line; and a commit that
    /// is not the one built (the build script fell back to `unknown`, embedded another,
    /// or ignored `KBF_BUILD_COMMIT_OVERRIDE` when the build set it).
    #[test]
    fn the_version_names_the_commit_built() {
        let (package, commit) = SERVER_VERSION.split_once('+').expect("a + in the version");
        assert_eq!(package, env!("CARGO_PKG_VERSION"));
        assert_eq!(commit, BUILD_COMMIT);
        let stamp = option_env!("KBF_BUILD_COMMIT_OVERRIDE");
        let expected = stamp.map_or_else(git_head, str::to_owned);
        assert_eq!(commit, expected);
        assert_eq!(commit.len(), 12, "{commit}");
        assert!(commit.bytes().all(|b| b.is_ascii_hexdigit()), "{commit}");
    }

    /// Catches: a full commit that is not the commit built, or one that disagrees with the
    /// 12 digits: it is `git rev-parse HEAD` when nothing was stamped, else unknown.
    #[test]
    fn the_full_commit_is_the_one_built_or_unknown() {
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git runs");
        let head = String::from_utf8(head.stdout)
            .expect("UTF-8")
            .trim()
            .to_owned();
        let stamped = option_env!("KBF_BUILD_COMMIT_OVERRIDE").is_some();
        let want = if stamped { "" } else { head.as_str() };
        assert_eq!(BUILD_COMMIT_FULL, want);
        assert!(BUILD_COMMIT_FULL.is_empty() || BUILD_COMMIT_FULL.starts_with(BUILD_COMMIT));
    }
}
