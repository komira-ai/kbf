//! `kbf-installer update --commit <sha> --frontend`: swap this host's frontend through the
//! update API the proxy serves (`docs/design/frontend-proxy.md`, section 4), wait for the
//! update to end, and print the one summary line the `deploy` verb prints.
//!
//! It is the frontend step of a deploy and nothing else ([`super::update_frontend`]): it
//! writes no order itself. The API writes the `frontend_update` order to the one order
//! spool, so a second writer next to it (a root-run verb writing `orders/` on its own) would
//! take the same counter; and an order alone only stages the release, which is not a swap.
//! `nodes` is off unless `--nodes` is given, so the nodes are not touched by default.
//!
//! The connection is `deploy`'s: HTTP/1.1 over TLS to the address given, verified under the
//! name the certificate carries and against the CA file, with the bearer token sent only on
//! a connection that did that.
//!
//! Exit codes are `deploy`'s: 0 for `done` (and for a frontend that already runs the
//! commit, which the summary says), 1 for `failed`, `rolled_back`, a timeout or an API that
//! does not answer, 2 for a usage error, 3 when the release is incompatible or incomplete,
//! 4 when an update is open, an old child is draining or the installer is busy.

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::time::Duration;

use super::api::{Https, Trust};
use super::cli::{CALL_TIMEOUT, host_platform, read_token};
use super::smoke::random;
use super::summary::Summary;
use super::{Check, Env, NoCheck, NoSmoke, Options, SystemClock, update_frontend};
use crate::cli::UpdateArgs;
use crate::exit::Exit;

/// `update` for `args`: the summary goes to `out` (one line), what a person needs to fix a
/// usage error to `err`.
pub fn run(args: &UpdateArgs, out: &mut dyn Write, err: &mut dyn Write) -> Exit {
    let usage = |err: &mut dyn Write, why: &str| {
        let _ = writeln!(err, "kbf-installer: update: {why}");
        Exit::Usage
    };
    let addr: SocketAddr = match args.api.parse() {
        Ok(a) => a,
        Err(_) => return usage(err, &format!("--api {:?} is not host:port", args.api)),
    };
    let request_id = match &args.request_id {
        Some(id) => id.clone(),
        None => match random(16) {
            Ok(bytes) => format!("update-{}", hex::encode(bytes)),
            Err(e) => return usage(err, &format!("no request id: {e}")),
        },
    };
    let token = match read_token(&args.token_file) {
        Ok(t) => t,
        Err(why) => return usage(err, &why),
    };
    let ca_pem = match args.api_ca.as_ref().map(fs::read).transpose() {
        Ok(pem) => pem,
        Err(e) => return usage(err, &format!("--api-ca: {e}")),
    };
    let trust = Trust {
        server_name: args.api_server_name.clone(),
        ca_pem,
    };
    let api = match Https::new(addr, &trust, token, CALL_TIMEOUT) {
        Ok(api) => api,
        Err(e) => return usage(err, &format!("--api: {e}")),
    };
    let mut options = Options::new(args.commit.clone(), host_platform(), request_id);
    options.nodes = args.nodes;
    options.handover_timeout = Duration::from_secs(args.handover_timeout_s);
    let (clock, check) = (SystemClock, NoCheck);
    let env = Env {
        api: &api,
        clock: &clock,
        smoke: &NoSmoke,
        check: &check as &dyn Check,
    };
    let _ = writeln!(err, "kbf-installer: update: {}", options.commit);
    finish(&env, &options, out)
}

/// Run the update and print its summary; the exit code the summary names.
pub(super) fn finish(env: &Env, options: &Options, out: &mut dyn Write) -> Exit {
    // Placeholder: asks nothing of the API.
    let _ = (env, options, out, update_frontend, exit_of);
    Exit::Failed
}

/// The summary's exit as the installer's: the two enums share their codes.
fn exit_of(summary: &Summary) -> Exit {
    Exit::ALL
        .into_iter()
        .find(|e| i32::from(e.code()) == summary.exit)
        .unwrap_or(Exit::Failed)
}
