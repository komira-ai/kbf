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
//! It is refused through `sudo` (`app::update_verb`): it takes the API address, its CA and the
//! token file from its caller, which `deploy` refuses of a sudo caller for the same reason.
//!
//! Until the proxy has a source for the schema the host runs, the API refuses every `POST`
//! before anything is fetched (`start_blocked`, exit 1 or 3 here), so no frontend can be
//! deployed through this verb yet.
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
use super::observe::{Observe, Observed, Slot};
use super::smoke::random;
use super::summary;
use super::{Check, Env, NoCheck, NoProbe, NoSmoke, Options, SystemClock, update_frontend};
use crate::cli::UpdateArgs;
use crate::exit::Exit;

/// How often the status is read while the update goes on.
const POLL: Duration = Duration::from_secs(1);

/// `update` for `args`: the summary goes to `out` (one line), what a person needs to fix a
/// usage error to `err`.
pub fn run(args: &UpdateArgs, out: &mut dyn Write, err: &mut dyn Write) -> Exit {
    let (api, options) = match plan(args, random) {
        Ok(plan) => plan,
        Err(why) => {
            let _ = writeln!(err, "kbf-installer: update: {why}");
            return Exit::Usage;
        }
    };
    let _ = writeln!(err, "kbf-installer: update: {}", options.commit);
    let (clock, check) = (SystemClock, NoCheck);
    let env = Env {
        api: &api,
        observe: &NoObserve,
        clock: &clock,
        smoke: &NoSmoke,
        check: &check as &dyn Check,
        probe: &NoProbe,
    };
    finish(&env, &options, out)
}

/// `update --frontend` runs no gate, so there is no frontend to observe.
pub(super) struct NoObserve;

impl Observe for NoObserve {
    fn resolve(&self, _: Option<Slot>) -> Result<Observed<'_>, String> {
        Err("update --frontend observes no frontend".into())
    }
}

/// The client and the options for `args`, or what is wrong with them. `rng` is the random
/// source of a request id the caller did not give.
fn plan(
    args: &UpdateArgs,
    rng: fn(usize) -> Result<Vec<u8>, String>,
) -> Result<(Https, Options), String> {
    let addr: SocketAddr = args
        .api
        .parse()
        .map_err(|_| format!("--api {:?} is not host:port", args.api))?;
    let request_id = request_id(args.request_id.as_deref(), rng)?;
    let token = read_token(&args.token_file)?;
    let ca_pem = args
        .api_ca
        .as_ref()
        .map(|path| fs::read(path).map_err(|e| format!("--api-ca {}: {e}", path.display())))
        .transpose()?;
    let trust = Trust {
        server_name: args.api_server_name.clone(),
        ca_pem,
    };
    let api = Https::new(addr, &trust, token, CALL_TIMEOUT).map_err(|e| format!("--api: {e}"))?;
    let mut options = Options::new(args.commit.clone(), host_platform(), request_id);
    options.nodes = args.nodes;
    options.poll = POLL;
    options.handover_timeout = Duration::from_secs(args.handover_timeout_s);
    Ok((api, options))
}

/// The request id the caller gave, else a random one.
pub(super) fn request_id(
    given: Option<&str>,
    rng: fn(usize) -> Result<Vec<u8>, String>,
) -> Result<String, String> {
    match given {
        Some(id) => Ok(id.to_owned()),
        None => rng(16)
            .map(|bytes| format!("update-{}", hex::encode(bytes)))
            .map_err(|e| format!("no request id: {e}")),
    }
}

/// Run the update and print its summary; the exit code it names.
pub(super) fn finish(env: &Env, options: &Options, out: &mut dyn Write) -> Exit {
    let (summary, exit) = update_frontend(env, options);
    let _ = writeln!(out, "{}", summary.line());
    installer_exit(exit)
}

/// The summary's exit as the installer's. The two enums share their codes; an update ends
/// in `Ok`, `Failed`, `Refused` or `Busy`, and the rest are here so that the match is whole.
pub(super) fn installer_exit(exit: summary::Exit) -> Exit {
    match exit {
        summary::Exit::Ok | summary::Exit::Plan => Exit::Ok,
        summary::Exit::Failed => Exit::Failed,
        summary::Exit::Usage => Exit::Usage,
        summary::Exit::Refused => Exit::Refused,
        summary::Exit::Busy => Exit::Busy,
        summary::Exit::NeedsOperator => Exit::NeedsOperator,
    }
}
