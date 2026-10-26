//! `kbf-updater enroll`: a new host gets its updater certificate from the farm CA with a
//! one-time enrollment token (`docs/design/updates.md`, section 5).
//!
//! ```text
//! kbf-updater enroll --frontend <dns name>:<port> --node-id ID --token-file FILE --state-dir DIR
//! ```
//!
//! - The token is the farm's (`kbf-enroll`, the same `Enroll` call a node makes): the operator
//!   made it for this node id and this host role, for one use. It fixes both, so neither is in
//!   the certificate because of a flag here, and the CA signs only what the token allows.
//! - The key is made here and never leaves: only a request signed by it is sent. The CA's key
//!   is the frontend's, and the coordinator holds none.
//! - `--state-dir` is where `kbf_enroll::store::Store` puts the key (0600), the certificate and
//!   the CA, in a directory (0700) that must belong to the user running this command, the
//!   agent's own, so the files belong to the account that reads them. The token file must too
//!   (0600 or 0400); it is removed once the host is enrolled.
//! - The first contact is checked against the CA pin in the token, not against the system's
//!   roots, and the answer is checked (role, node id, key, chain to the pin) before any file is
//!   written. A command that is run again after a success finds the host enrolled and changes
//!   nothing.
//!
//! The agent renews the certificate by itself from then on ([`super::renew`]).

use std::path::PathBuf;

use kbf_enroll::client::{Enrollment, connect, enroll};
use kbf_enroll::config::{Config, FrontendUrl, HostName};
use kbf_enroll::pin::Anchor;
use kbf_enroll::store::Store;
use kbf_proto::kbf::enroll::v1::NodeFacts;
use kbf_types::{NodeId, Role, RoleSet};

use super::coordinator::Endpoint;
use super::flags::collect;

pub const USAGE: &str = "usage: kbf-updater enroll --frontend DNSNAME:PORT --node-id ID \
--token-file FILE --state-dir DIR\n";

const FLAGS: [&str; 4] = ["--frontend", "--node-id", "--token-file", "--state-dir"];

/// What an enrollment is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollFlags {
    /// The frontend's public address.
    pub frontend: Endpoint,
    /// The node id the token was made for.
    pub node_id: NodeId,
    /// The file the token is in.
    pub token_file: PathBuf,
    /// Where the key, certificate and CA are written.
    pub state_dir: PathBuf,
}

/// Read the flags (after `enroll`).
///
/// # Errors
/// What is wrong with them, in a line.
pub fn parse_flags(args: &[String]) -> Result<EnrollFlags, String> {
    let mut found = collect(args, &FLAGS)?;
    let mut take = |flag: &str| {
        found
            .remove(flag)
            .ok_or_else(|| format!("{flag} is required"))
    };
    let frontend =
        Endpoint::parse(&take("--frontend")?).map_err(|why| format!("--frontend: {why}"))?;
    let node_id = take("--node-id")?
        .parse()
        .map_err(|why| format!("--node-id: {why}"))?;
    Ok(EnrollFlags {
        frontend,
        node_id,
        token_file: PathBuf::from(take("--token-file")?),
        state_dir: PathBuf::from(take("--state-dir")?),
    })
}

/// An error as the line the operator reads.
pub(super) fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// The frontend as `kbf-enroll` dials it: an `https` address of a DNS name.
pub fn frontend_url(endpoint: &Endpoint) -> FrontendUrl {
    FrontendUrl {
        host: HostName::Dns(endpoint.host.clone()),
        port: endpoint.port,
    }
}

/// What the frontend is told about this host.
fn facts() -> NodeFacts {
    NodeFacts {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        commit: String::new(),
        protocol: 0,
        package_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// Enroll as `flags` say, as the user `euid`, at the unix time `now`. The text of what was
/// done, for the operator.
///
/// # Errors
/// Why not, in a line: the directory or the token file is not the user's or not private, the
/// frontend is not the one the token pins, it refuses the token, or the answer is wrong.
pub fn enroll_host(flags: &EnrollFlags, euid: u32, now: i64) -> Result<String, String> {
    let store = Store::open(&flags.state_dir).map_err(text)?;
    let token = kbf_enroll::token::read(&flags.token_file, euid).map_err(text)?;
    let config = Config::updater(
        frontend_url(&flags.frontend),
        flags.node_id.clone(),
        flags.token_file.clone(),
        flags.state_dir.clone(),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(text)?;
    let node = runtime.block_on(async {
        let frontend = connect(&config.frontend, Anchor::Pin(*token.pin()))
            .await
            .map_err(text)?;
        let attempt = Enrollment {
            config: &config,
            token: &token,
            roles: RoleSet::new([Role::Updater]).map_err(text)?,
            facts: facts(),
            now,
        };
        enroll(&frontend, &store, &attempt).await.map_err(text)
    })?;
    Ok(format!(
        "enrolled the updater of node {}; its certificate is valid until unix time {}\n",
        node.node_id,
        node.earliest_not_after()
    ))
}

/// The command, as a function: the arguments after `enroll` in, the exit code and the text out
/// (a usage error is 2, a failed enrollment 1).
pub fn run_enroll(args: &[String], euid: u32, now: i64) -> (u8, String) {
    match parse_flags(args) {
        Err(why) => (2, format!("kbf-updater: {why}\n{USAGE}")),
        Ok(flags) => match enroll_host(&flags, euid, now) {
            Ok(text) => (0, text),
            Err(why) => (1, format!("kbf-updater: {why}\n")),
        },
    }
}
