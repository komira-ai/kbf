//! Two more operator verbs as routes (`docs/api.md`):
//!
//! - `GET /v1/denied`: the storage deny list with the reason and time of each entry, as the
//!   state store's rows hold it now ([`Members::denials`]).
//! - `POST /v1/backups/state`: a checked copy of the frontend's state file, made with
//!   SQLite's `VACUUM INTO` in the directory `--state-backup-dir` names, under a name the
//!   server chooses. The request carries no path, so a caller cannot place a file. A
//!   restore stays a local verb: it needs the frontend stopped.
//!
//! Both pass every gate of a write ([`crate::api::gate`]), the `GET` included: the reasons of
//! a deny list are for the operator. Their answers are `Cache-Control: no-store`.

use std::fs::{self, OpenOptions};
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use kbf_state::StoreError;
use kbf_types::operator_api::{DeniedEntry, DeniedList, StateBackupMade};
use time::OffsetDateTime;

use crate::api::{DENIED, NoBody, error, gate, json, no_members};
use crate::members::{Members, MembershipError};
use crate::token::ApiToken;

/// The path of `POST /v1/backups/state`.
pub const BACKUPS_STATE: &str = "/v1/backups/state";

/// Where the state file is and where its copies go (`--state-store`, `--state-backup-dir`).
#[derive(Clone, Debug)]
pub struct StateBackup {
    /// The state file this frontend holds.
    pub db: PathBuf,
    /// The directory the copies are made in.
    pub dir: PathBuf,
}

/// Makes `dir` (and its parents) with mode 0700 where it is missing.
///
/// # Errors
/// The directory cannot be made.
pub fn make_backup_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// What the handlers share: the token the gate checks (`None`: writes are off), the
/// storage members (`None`: no state store) and the backup place (`None`: no
/// `--state-backup-dir`).
struct Verbs {
    token: Option<ApiToken>,
    members: Option<Arc<Members>>,
    backup: Option<StateBackup>,
}

/// The two routes. `token` is the operator API token the gate checks. Serve them with
/// `into_make_service_with_connect_info`, as [`crate::api::router`] is; the router merges
/// with it, the `GET` joining the `POST` and `DELETE` of the same path.
pub fn routes(
    token: Option<ApiToken>,
    members: Option<Arc<Members>>,
    backup: Option<StateBackup>,
) -> Router {
    Router::new()
        .route(DENIED, get(list_denied))
        .route(BACKUPS_STATE, post(backup_state))
        .with_state(Arc::new(Verbs {
            token,
            members,
            backup,
        }))
}

/// `answer` that no cache keeps.
fn no_store(mut answer: Response) -> Response {
    answer
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    answer
}

/// A failed call as the status and fixed text the operator gets: never a path, an OS error
/// or a stored row.
type Failure = (StatusCode, &'static str);

fn refuse((code, why): Failure) -> Response {
    no_store(error(code, why))
}

/// Runs a call that blocks (every store call and the copy do) off the async threads.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(call)
        .await
        .unwrap_or(Err((StatusCode::INTERNAL_SERVER_ERROR, "the task failed")))
}

/// The status a failed store call answers with.
fn store_failure(e: &MembershipError) -> Failure {
    match e {
        MembershipError::Store(StoreError::Fenced) => (
            StatusCode::CONFLICT,
            "this frontend no longer holds the writer role of the state store: \
             call the current frontend",
        ),
        MembershipError::Store(_) | MembershipError::Contended => {
            (StatusCode::SERVICE_UNAVAILABLE, "the state store failed")
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "a stored row is unreadable",
        ),
    }
}

async fn list_denied(
    State(verbs): State<Arc<Verbs>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Err((code, why)) = gate(verbs.token.as_ref(), peer, &headers) {
        return no_store(error(code, &why));
    }
    let Some(members) = verbs.members.clone() else {
        return no_store(no_members());
    };
    let listed = blocking(move || members.denials().map_err(|e| store_failure(&e))).await;
    match listed {
        Ok(denials) => no_store(json(
            StatusCode::OK,
            &DeniedList {
                denied: denials
                    .into_iter()
                    .map(|d| DeniedEntry {
                        entry: d.entry.to_string(),
                        reason: d.reason,
                        at_ms: d.at_ms,
                    })
                    .collect(),
            },
        )),
        Err(failure) => refuse(failure),
    }
}

async fn backup_state(
    State(verbs): State<Arc<Verbs>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((code, why)) = gate(verbs.token.as_ref(), peer, &headers) {
        return no_store(error(code, &why));
    }
    let (Some(members), Some(backup)) = (verbs.members.clone(), verbs.backup.clone()) else {
        return refuse((
            StatusCode::NOT_FOUND,
            "this server takes no state backups: it has no --state-store or no --state-backup-dir",
        ));
    };
    if !body.is_empty() && serde_json::from_slice::<NoBody>(&body).is_err() {
        return refuse((
            StatusCode::BAD_REQUEST,
            "a state backup takes no body, or {}: the server chooses the file's name and place",
        ));
    }
    match blocking(move || take(&members, &backup)).await {
        Ok(made) => no_store(json(StatusCode::OK, &made)),
        Err(failure) => refuse(failure),
    }
}

/// Copies the state file into the backup directory, if this frontend still holds the
/// writer role and the directory is private to this user.
fn take(members: &Members, backup: &StateBackup) -> Result<StateBackupMade, Failure> {
    members.check_writer().map_err(|e| store_failure(&e))?;
    private_dir(&backup.dir)?;
    let name = next_name();
    let (tmp, dest) = (
        backup.dir.join(format!(".{name}.partial")),
        backup.dir.join(&name),
    );
    // The file exists, empty and mode 0600, before SQLite writes it: the copy is never
    // readable by anyone else, whatever the umask. SQLite fills an empty file.
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| failed("create the partial backup", &e))?;
    let made = kbf_state::sqlite::backup(&backup.db, &tmp)
        .map_err(|e| {
            tracing::warn!(error = %e, "state backup failed");
            (StatusCode::SERVICE_UNAVAILABLE, "the state backup failed")
        })
        .and_then(|summary| {
            // The link at `dest` is the backup, and never replaces a file.
            fs::hard_link(&tmp, &dest).map_err(|e| failed("name the backup", &e))?;
            sync_dir(&backup.dir).map_err(|e| failed("sync the backup directory", &e))?;
            Ok(StateBackupMade {
                path: dest.display().to_string(),
                epoch: summary.epoch,
                head: summary.head,
                keys: summary.keys,
            })
        });
    // The partial name goes, copied or not (and any journal SQLite left beside it).
    let _ = fs::remove_file(&tmp);
    if made.is_ok() {
        tracing::info!(path = %dest.display(), "state backup made");
    }
    made
}

fn failed(what: &str, e: &io::Error) -> Failure {
    tracing::warn!(error = %e, "state backup: could not {what}");
    (StatusCode::SERVICE_UNAVAILABLE, "the state backup failed")
}

/// Whether `dir` is a directory (not a link to one) that this user owns and no one else can
/// enter or list.
fn private_dir(dir: &Path) -> Result<(), Failure> {
    let unsafe_dir = (
        StatusCode::INTERNAL_SERVER_ERROR,
        "the backup directory must be a real directory, owned by the server's user, with no \
         group or other access (mode 0700)",
    );
    let meta = fs::symlink_metadata(dir).map_err(|_| unsafe_dir)?;
    let mine = meta.uid() == rustix::process::geteuid().as_raw();
    if meta.is_dir() && mine && meta.mode() & 0o077 == 0 {
        Ok(())
    } else {
        Err(unsafe_dir)
    }
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// A name for the next copy: the UTC second it is made in and a count of this process's
/// copies, so two calls in one second differ.
fn next_name() -> String {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let t = OffsetDateTime::from(SystemTime::now());
    format!(
        "state-{:04}{:02}{:02}T{:02}{:02}{:02}Z-{}.db",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        COUNT.fetch_add(1, Ordering::Relaxed) + 1,
    )
}
