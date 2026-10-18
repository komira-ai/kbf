//! Two more operator verbs as routes (`docs/api.md`): `GET /v1/denied`, the storage deny list
//! with its reasons, and `POST /v1/backups/state`, a checked copy of the frontend's state
//! file. STUB: both answer 501 until the next commit.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};

use crate::api::{DENIED, error};
use crate::members::Members;
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
pub fn make_backup_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

struct Verbs {
    _token: Option<ApiToken>,
    _members: Option<Arc<Members>>,
    _backup: Option<StateBackup>,
}

/// The two routes. `token` is the operator API token the gate checks.
pub fn routes(
    token: Option<ApiToken>,
    members: Option<Arc<Members>>,
    backup: Option<StateBackup>,
) -> Router {
    Router::new()
        .route(DENIED, get(unbuilt))
        .route(BACKUPS_STATE, post(unbuilt))
        .with_state(Arc::new(Verbs {
            _token: token,
            _members: members,
            _backup: backup,
        }))
}

async fn unbuilt(
    State(_): State<Arc<Verbs>>,
    ConnectInfo(_): ConnectInfo<SocketAddr>,
    _headers: HeaderMap,
) -> Response {
    error(StatusCode::NOT_IMPLEMENTED, "not built")
}
