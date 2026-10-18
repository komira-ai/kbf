//! The frontend's own update (`docs/design/pull-updates.md`): `POST /v1/update/frontend
//! {"commit": "<40 hex>"}` and `GET /v1/update/frontend`, over the `update/frontend` row of
//! [`crate::update::Rows`].
//!
//! The frontend writes a `frontend_update` order record into **its own host's** order spool,
//! with the same [`kbf_update::spool::Spool`] the nodes write theirs with. The root
//! `kbf-installer` on that host pulls the release, verifies it, starts the new instance behind
//! the installed Envoy, waits for `/readyz` and hands over. This route does none of that: it
//! orders it and shows what the installer says.
//!
//! - `POST` passes the gates of every `/v1` write ([`crate::api::operator_gate`]) and wants a
//!   JSON body with no other field. `503` without `--state-store`, `--releases-dir` or
//!   `--installer-orders-dir`, each naming its flag. `400` for a commit that is not 40
//!   lowercase hex digits. `409` unless the release store holds the commit complete (tarball
//!   and attestation) **for this host's own platform**: another platform's tarballs are no use
//!   to the installer here, unlike the desired release, which asks for any platform.
//! - Then, in this order: the row is opened (a compare-and-set; `409 frontend_busy` when an
//!   update is already in flight), **then** the record is written. A write that fails closes
//!   the row again and answers `500`. A crash between the two leaves an open row and no
//!   record, which the deadline below closes. `202 {order_id, epoch, generation, commit}`.
//! - One update is in flight at a time. The row is closed by the next call, read, when the
//!   installer's `status.json` has a `frontend` section naming this row's order (`order_id`
//!   and `epoch`) in a terminal state (`confirmed`, `reverted` or `refused`). An installer
//!   that predates the section never says so; after [`DEADLINE`] the row is closed whatever
//!   the installer says, so one that died, or never wrote the section, cannot hold the route
//!   for ever. A `GET` reads the row and closes nothing.
//! - The generation is the last row's plus one: a rollback is an older commit under a higher
//!   generation. The order id is drawn from the store's counter under the frontend's lease
//!   epoch, as the nodes' orders are.
//!
//! Serve the router with `into_make_service_with_connect_info`, as the API's.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use kbf_types::ReleasePlatform;
use kbf_update::helper::{self, Helper, HelperState};
use kbf_update::spool::Spool;
use kbf_update::{Locator, OrderRecord, Verb};
use serde::Deserialize;
use serde_json::{Value, json};

use super::desired::{blocking, row_failed, unavailable, write_gate};
use super::key::{Commit, Key};
use super::store::ReleaseStore;
use crate::api::{error, json};
use crate::farm::Farm;
use crate::token::ApiToken;
use crate::update::{Began, FrontendRow, FrontendState, Release, RowError, Rows};

/// How long an update may stay open before the route closes it whatever the installer says.
/// The installer's own limits (the fetch, the `/readyz` wait, the confirm window) are all
/// well inside it.
pub const DEADLINE: Duration = Duration::from_secs(3600);

/// The confirm window the record asks for, in seconds; the installer clamps it.
const WINDOW_S: u32 = 600;

/// Where the frontend's host keeps what the installer reads and writes.
pub struct Installer {
    /// The installer's order spool on this host (`--installer-orders-dir`).
    pub spool: Spool,
    /// The installer's `status.json`, world-readable.
    pub status_file: PathBuf,
    /// This host's platform: the one the release must be complete for. `None` on a host no
    /// release is built for.
    pub platform: Option<ReleasePlatform>,
    /// Wall-clock milliseconds since the Unix epoch. A value so a test can move it.
    pub now_ms: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl Installer {
    /// The installer's spool at `orders_dir` (made, mode 0700, if missing) and its status file
    /// `status_file`, or `status.json` beside the spool's directory when none is named, this
    /// host's own platform and the wall clock.
    ///
    /// # Errors
    /// The spool cannot be opened.
    pub fn open(orders_dir: &Path, status_file: Option<&Path>) -> io::Result<Self> {
        let status_file = status_file.map_or_else(
            || {
                orders_dir
                    .parent()
                    .unwrap_or(Path::new(""))
                    .join("status.json")
            },
            Path::to_owned,
        );
        Ok(Self {
            spool: Spool::open(orders_dir)?,
            status_file,
            platform: own_platform(),
            now_ms: Box::new(unix_ms),
        })
    }
}

impl std::fmt::Debug for Installer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Installer")
            .field("spool", &self.spool)
            .field("status_file", &self.status_file)
            .field("platform", &self.platform)
            .finish_non_exhaustive()
    }
}

/// The platform this process runs on, as releases name it.
fn own_platform() -> Option<ReleasePlatform> {
    ReleasePlatform::from_report(std::env::consts::OS, release_arch(std::env::consts::ARCH))
}

/// The word releases use for a CPU: Rust's `aarch64` is `arm64`.
fn release_arch(arch: &str) -> &str {
    if arch == "aarch64" { "arm64" } else { arch }
}

#[cfg(test)]
mod tests {
    use super::release_arch;

    /// Catches the arm word going wrong on the host that is not arm (the CI that measures
    /// this runs on x86_64): `aarch64` is `arm64`, anything else is itself.
    #[test]
    fn rusts_aarch64_is_the_releases_arm64() {
        assert_eq!(release_arch("aarch64"), "arm64");
        assert_eq!(release_arch("x86_64"), "x86_64");
    }
}

fn unix_ms() -> u64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

struct Route {
    store: Option<Arc<ReleaseStore>>,
    rows: Option<Rows>,
    token: Option<ApiToken>,
    farm: Arc<Farm>,
    installer: Option<Arc<Installer>>,
}

/// The routes. Each of `store`, `rows` and `installer` is `None` when the server has none:
/// the routes then answer `503` naming the flag.
pub fn router(
    store: Option<Arc<ReleaseStore>>,
    rows: Option<Rows>,
    token: Option<ApiToken>,
    farm: Arc<Farm>,
    installer: Option<Arc<Installer>>,
) -> Router {
    Router::new()
        .route("/v1/update/frontend", get(view).post(begin))
        .with_state(Arc::new(Route {
            store,
            rows,
            token,
            farm,
            installer,
        }))
}

fn state_word(state: HelperState) -> &'static str {
    match state {
        HelperState::Idle => "idle",
        HelperState::Fetching => "fetching",
        HelperState::Staged => "staged",
        HelperState::ActivatedUnconfirmed => "activated_unconfirmed",
        HelperState::Confirmed => "confirmed",
        HelperState::Reverted => "reverted",
        HelperState::Refused => "refused",
    }
}

/// The installer's status as the operator sees it.
fn installer_view(installer: &Installer) -> Value {
    match helper::read(&installer.status_file) {
        Helper::Missing => json!({ "present": false }),
        Helper::Unreadable(why) => json!({ "present": true, "error": why }),
        Helper::Reported(r) => json!({
            "present": true,
            "state": state_word(r.state),
            "installed_commit": r.installed,
            "previous_commit": r.previous,
            "staged_commit": r.staged,
            "refusal": r.refusal,
        }),
    }
}

fn row_view(row: &FrontendRow, installer: &Installer) -> Value {
    json!({
        "order_id": row.order.seq,
        "epoch": row.order.epoch,
        "generation": row.generation,
        "commit": row.commit.as_str(),
        "state": row.state.name(),
        "written_at_ms": row.written_at_ms,
        "record_in_spool": installer.spool.has(row.order.epoch, row.order.seq),
    })
}

/// The frontend section of `GET /v1/update` and the body of `GET /v1/update/frontend`: the row
/// (`null` before any update), and the installer's status. Without an installer the section
/// says it is off.
///
/// # Errors
/// [`RowError`]: the store failed or the row cannot be read.
pub async fn section(rows: Rows, installer: Option<Arc<Installer>>) -> Result<Value, RowError> {
    let Some(installer) = installer else {
        return Ok(json!({ "enabled": false, "row": null, "installer": null }));
    };
    let row = blocking(move || rows.frontend()).await?.value;
    let installer = blocking(move || {
        Ok((
            row.as_ref().map(|r| row_view(r, &installer)),
            installer_view(&installer),
        ))
    })
    .await?;
    Ok(json!({ "enabled": true, "row": installer.0, "installer": installer.1 }))
}

async fn view(State(route): State<Arc<Route>>) -> Response {
    let Some(rows) = route.rows.clone() else {
        return unavailable("the update state", "--state-store");
    };
    if route.installer.is_none() {
        return unavailable("the frontend update", "--installer-orders-dir");
    }
    match section(rows, route.installer.clone()).await {
        Ok(section) => json(StatusCode::OK, &section),
        Err(e) => row_failed(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Order {
    commit: String,
}

/// Whether the open `row` is over: past [`DEADLINE`], or the installer's `status.json` carries
/// a `frontend` section for exactly this order (`order_id` and `epoch`) in a terminal state
/// (confirmed, reverted or refused). A section for another order, a state still moving
/// (`pending`, `needs_operator`), no section (an installer that predates it) and a status file
/// that cannot be read are not an end: only the deadline closes the row then. Reads the status
/// file.
fn settled(row: &FrontendRow, installer: &Installer, now_ms: u64) -> bool {
    let waited = now_ms.saturating_sub(row.written_at_ms);
    if waited >= u64::try_from(DEADLINE.as_millis()).unwrap_or(u64::MAX) {
        return true;
    }
    let Helper::Reported(report) = helper::read(&installer.status_file) else {
        return false;
    };
    report.frontend.is_some_and(|f| {
        f.order_id == row.order.seq && f.epoch == row.order.epoch && f.state.is_terminal()
    })
}

/// The components of `commit` for `platform` as the record carries them, if the store holds
/// the platform complete.
async fn locators(
    store: &ReleaseStore,
    commit: &Commit,
    platform: ReleasePlatform,
) -> Result<Option<Vec<Locator>>, String> {
    let listing = store.list().await.map_err(|e| format!("list: {e}"))?;
    let Some(held) = listing
        .iter()
        .filter(|c| c.commit == *commit)
        .flat_map(|c| &c.platforms)
        .find(|p| p.platform == platform && p.complete)
    else {
        return Ok(None);
    };
    let mut found = Vec::new();
    for info in held
        .components
        .iter()
        .filter(|c| platform.components().contains(&c.component.as_str()))
    {
        let key = Key {
            commit: commit.clone(),
            platform,
            component: info.component,
        };
        let bundle = store
            .attestation(&key)
            .await
            .map_err(|e| format!("attestation: {e}"))?
            .ok_or("a component lost its attestation")?;
        found.push(Locator {
            name: info.component.as_str().to_owned(),
            sha256: info.sha256.as_str().to_owned(),
            size: info.size,
            attestation: String::from_utf8(bundle).map_err(|_| {
                format!("the attestation of {} is not text", info.component.as_str())
            })?,
        });
    }
    Ok(Some(found))
}

/// The record for the order the row opened.
fn record_of(row: &FrontendRow, components: Vec<Locator>) -> OrderRecord {
    OrderRecord {
        order_id: row.order.seq,
        epoch: row.order.epoch,
        generation: row.generation,
        verb: Verb::FrontendUpdate,
        commit: row.commit.as_str().to_owned(),
        window_s: WINDOW_S,
        components,
        expected: None,
    }
}

async fn begin(
    State(route): State<Arc<Route>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((code, why)) = write_gate(route.token.as_ref(), peer, &headers, true) {
        return error(code, &why);
    }
    let Some(rows) = route.rows.clone() else {
        return unavailable("the update state", "--state-store");
    };
    let Some(store) = route.store.as_deref() else {
        return unavailable("the release store", "--releases-dir");
    };
    let Some(installer) = route.installer.clone() else {
        return unavailable("the frontend update", "--installer-orders-dir");
    };
    let Ok(Order { commit }) = serde_json::from_slice::<Order>(&body) else {
        return error(
            StatusCode::BAD_REQUEST,
            "the body must be {\"commit\": \"<40 hex>\"}",
        );
    };
    let Some(parsed) = Commit::parse(&commit) else {
        return error(
            StatusCode::BAD_REQUEST,
            "commit must be 40 lowercase hex digits",
        );
    };
    let Some(platform) = installer.platform else {
        return unavailable(
            "the frontend update",
            "a platform this host's releases are built for",
        );
    };
    let components = match locators(store, &parsed, platform).await {
        Ok(Some(components)) => components,
        Ok(None) => {
            return error(
                StatusCode::CONFLICT,
                &format!(
                    "the release store holds no {platform} release of this commit complete \
                     (tarball and attestation): this frontend runs on {platform}"
                ),
            );
        }
        Err(e) => {
            tracing::error!("release store: {e}");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the release store failed",
            );
        }
    };

    // An open row whose update is over is closed first; one that is not is busy.
    let now = (installer.now_ms)();
    let stale = {
        let (rows, installer) = (rows.clone(), Arc::clone(&installer));
        blocking(move || {
            let Some(open) = rows
                .frontend()?
                .value
                .filter(|r| r.state == FrontendState::Open)
            else {
                return Ok(None);
            };
            let over = settled(&open, &installer, now);
            if over {
                rows.end_frontend(open.order)?;
            }
            Ok(over.then_some(open.order.seq))
        })
        .await
    };
    match stale {
        Ok(Some(closed)) => tracing::info!(order_id = closed, "the frontend update row closed"),
        Ok(None) => {}
        Err(e) => return row_failed(&e),
    }

    // The row first, then the record: a crash between the two leaves an open row and no
    // record, which the deadline closes. A record with no row would be an update nothing
    // guards against a second.
    let epoch = route.farm.lease_epoch();
    let release = Release::new(commit);
    let opened = {
        let rows = rows.clone();
        blocking(move || rows.begin_frontend(&release, epoch, now)).await
    };
    let row = match opened {
        Ok(Began::Opened(row)) => row,
        Ok(Began::Busy(open)) => {
            return json(
                StatusCode::CONFLICT,
                &json!({
                    "error": "frontend_busy",
                    "detail": format!(
                        "the frontend is already being updated to {} (order {})",
                        open.commit.as_str(),
                        open.order.seq
                    ),
                    "order_id": open.order.seq,
                    "commit": open.commit.as_str(),
                }),
            );
        }
        Err(e) => return row_failed(&e),
    };
    let record = record_of(&row, components);
    // One small file in the host's own spool, written on the request's thread (there is no
    // task to lose, so no join error to invent an answer for), after the record is checked.
    let written = record
        .check()
        .map_err(io::Error::other)
        .and_then(|()| installer.spool.write(&record));
    if let Err(e) = written {
        tracing::error!(order_id = row.order.seq, "the frontend's order record: {e}");
        let order = row.order;
        if let Err(closing) = blocking(move || rows.end_frontend(order)).await {
            tracing::error!("the frontend update row stays open until its deadline: {closing}");
        }
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the order record could not be written: nothing was ordered",
        );
    }
    json(
        StatusCode::ACCEPTED,
        &json!({
            "order_id": row.order.seq,
            "epoch": row.order.epoch,
            "generation": row.generation,
            "commit": row.commit.as_str(),
        }),
    )
}
