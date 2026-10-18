//! `GET /v1/denied` and `POST /v1/backups/state` over real listeners: the deny list with its
//! reasons, and a checked copy of the state file the server names itself, behind the gates
//! every operator write passes (`docs/api.md`).

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::DenyEntry;
use kbf_objstore::MemoryStore;
use kbf_server::members::{Members, MembershipConfig};
use kbf_server::token::ApiToken;
use kbf_server::verbs::{StateBackup, routes};
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, StateStoreUri, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Fenced, ManualClock, Store};
use kbf_types::operator_api::{DeniedList, StateBackupMade};
use support::{HELLO_WAIT, INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const OPERATOR: &str = "Authorization: Bearer kbf-test-token-0123456789abcdef0123456789\r\nContent-Type: application/json\r\n";

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 1,
    }
}

/// A new empty directory of this test binary's, under the target directory.
fn scratch(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "kbf-server-operator-verbs-{}-{n}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn token() -> ApiToken {
    let dir = scratch("token");
    let path = dir.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
}

fn listeners() -> Listeners {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

/// A whole server over a real SQLite state file, with backups into `dir`; its stop switch
/// is dropped with it.
struct Server {
    api: SocketAddr,
    db: PathBuf,
    dir: PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn start() -> Server {
    let base = scratch("server");
    let (db, dir) = (base.join("state").join("state.db"), base.join("backups"));
    std::fs::create_dir(&dir).expect("the backup directory");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    start_over(db, dir)
}

fn start_over(db: PathBuf, dir: PathBuf) -> Server {
    let state = StateStoreUri::Sqlite(db.clone())
        .open()
        .expect("the state file");
    let state = StateStore {
        members: config(),
        epoch: EpochPolicy::Keep,
        ..state
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: Some(token()),
        local_cache: None,
        releases: None,
        state_backup: Some(StateBackup {
            db: db.clone(),
            dir: dir.clone(),
        }),
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        Some(api),
        kbf_auth::Policy::allow_all(),
        Some(state),
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let api = bound.api.expect("an API address");
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        api,
        db,
        dir,
        stop: Some(stop),
    }
}

/// Just the two routes over `members` and `backup`, on a listener of their own.
async fn serve_routes(
    token: Option<ApiToken>,
    members: Option<Arc<Members>>,
    backup: Option<StateBackup>,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = routes(token, members, backup);
    tokio::spawn(async move {
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, service).await.expect("serve");
    });
    addr
}

/// Members over a fake store that `Fenced` hands to the first of two writers, which is then
/// the deposed one.
fn deposed_members() -> Arc<Members> {
    let clock = Arc::new(ManualClock::default());
    let fake: Arc<dyn Store> = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let deposed: Arc<dyn Store> = Arc::new(Fenced::take(Arc::clone(&fake), None).expect("first"));
    let _newer = Fenced::take(fake, None).expect("second");
    Arc::new(Members::new(deposed, config()))
}

/// Members over a fake store this handle still writes.
fn live_members() -> Arc<Members> {
    let clock = Arc::new(ManualClock::default());
    let fake: Arc<dyn Store> = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let writer: Arc<dyn Store> = Arc::new(Fenced::take(fake, None).expect("writer"));
    Arc::new(Members::new(writer, config()))
}

struct Answer {
    code: u16,
    head: String,
    body: String,
}

async fn request(api: SocketAddr, method: &str, path: &str, headers: &str, body: &str) -> Answer {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let length = body.len();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: {length}\r\n\
         {headers}Connection: close\r\n\r\n{body}"
    );
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    Answer {
        code,
        head: head.to_ascii_lowercase(),
        body: body.to_owned(),
    }
}

async fn deny(api: SocketAddr, entry: &str, reason: &str) {
    let body = serde_json::json!({ "entry": entry, "reason": reason }).to_string();
    let answer = request(api, "POST", "/v1/denied", OPERATOR, &body).await;
    assert_eq!(answer.code, 200, "{}", answer.body);
}

async fn listed(api: SocketAddr) -> DeniedList {
    let answer = request(api, "GET", "/v1/denied", OPERATOR, "").await;
    assert_eq!(answer.code, 200, "{}", answer.body);
    serde_json::from_str(&answer.body).expect("a deny list")
}

fn names(list: &DeniedList) -> Vec<(&str, &str)> {
    list.denied
        .iter()
        .map(|d| (d.entry.as_str(), d.reason.as_str()))
        .collect()
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read the directory")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Catches a deny list that cannot be read back: `GET /v1/denied` must list every entry kind
/// (serial, key hash, node) with the reason it was added with and a time, in the order the
/// list sorts, as `POST` stored it (canonical spelling), and drop an entry that `DELETE`
/// took off. It is served by the real server, so it also pins that the new `GET` shares the
/// path with `POST` and `DELETE` (a merge that lost one method would answer 405). The empty
/// list is `{"denied":[]}`, never 404, and every answer is `no-store`.
///
/// Mutants seen red: the listing reading only `node` rows; the reason left out (empty); the
/// route merged so that `GET` was never reached (405).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deny_list_is_read_back_with_every_kind_and_reason() {
    let server = start();
    let api = server.api;
    let answer = request(api, "GET", "/v1/denied", OPERATOR, "").await;
    assert_eq!(answer.code, 200, "{}", answer.body);
    assert_eq!(answer.body, r#"{"denied":[]}"#);
    assert!(
        answer.head.contains("cache-control: no-store"),
        "{}",
        answer.head
    );
    assert!(answer.head.contains("application/json"), "{}", answer.head);

    deny(api, "node Mac-07", "retired").await;
    deny(api, "serial 0A:1B", "a lost laptop").await;
    let hash = "ab".repeat(32);
    deny(api, &format!("spki-sha256 {hash}"), "leaked key").await;

    let list = listed(api).await;
    assert_eq!(
        names(&list),
        [
            ("serial a1b", "a lost laptop"),
            (&format!("spki-sha256 {hash}")[..], "leaked key"),
            ("node mac-07", "retired"),
        ]
    );
    assert!(list.denied.iter().all(|d| d.at_ms > 0), "{list:?}");

    let gone = request(api, "DELETE", "/v1/denied?entry=serial+a1b", OPERATOR, "").await;
    assert_eq!(gone.code, 200, "{}", gone.body);
    let list = listed(api).await;
    assert_eq!(
        names(&list),
        [
            (&format!("spki-sha256 {hash}")[..], "leaked key"),
            ("node mac-07", "retired"),
        ]
    );
}

/// Catches both routes answering a caller that fails a gate, or answering a gate failure
/// with something other than the gate's own status: no token and a wrong token (401), a
/// browser page's `Origin` (403), a request without `Content-Type: application/json` (415,
/// on the `GET` too, as for `/v1/enroll`), and a server started without `--api-token-file`
/// (403). A refused backup writes nothing, and a refused listing shows no entry.
///
/// Mutants seen red: the `gate` call removed from the listing; from the backup; the
/// content-type check skipped on the `GET`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_routes_pass_the_write_gate() {
    let dir = scratch("gates");
    let members = live_members();
    let backup = StateBackup {
        db: dir.join("state.db"),
        dir: dir.clone(),
    };
    let api = serve_routes(Some(token()), Some(members), Some(backup.clone())).await;
    let wrong = "Authorization: Bearer nope-nope-nope-nope-nope-nope-nope-nope\r\nContent-Type: application/json\r\n";
    let no_type = "Authorization: Bearer kbf-test-token-0123456789abcdef0123456789\r\n";
    let browser = format!("{OPERATOR}Origin: https://example.com\r\n");
    for (method, path) in [("GET", "/v1/denied"), ("POST", "/v1/backups/state")] {
        for (what, headers, want) in [
            ("no token", "Content-Type: application/json\r\n", 401),
            ("a wrong token", wrong, 401),
            ("an Origin", browser.as_str(), 403),
            ("no content type", no_type, 415),
        ] {
            let answer = request(api, method, path, headers, "").await;
            assert_eq!(
                answer.code, want,
                "{method} {path} with {what}: {}",
                answer.body
            );
            assert!(
                !answer.body.contains("denied\":["),
                "{method} {path} with {what} listed: {}",
                answer.body
            );
        }
    }
    assert_eq!(
        files_in(&dir),
        Vec::<String>::new(),
        "a refused backup wrote"
    );

    let off = serve_routes(None, Some(live_members()), Some(backup)).await;
    for (method, path) in [("GET", "/v1/denied"), ("POST", "/v1/backups/state")] {
        let answer = request(off, method, path, OPERATOR, "").await;
        assert_eq!(
            answer.code, 403,
            "{method} {path} with writes off: {}",
            answer.body
        );
    }
}

/// Catches a server without the thing a route needs answering anything but 404 (after the
/// gate): the listing with no state store, the backup with no state store or with no
/// `--state-backup-dir`. A server with members and no backup directory still lists.
///
/// Mutants seen red: the backup route configured by default (a path of its own choosing);
/// the missing-members answer a 500.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_route_without_what_it_needs_answers_404() {
    let none = serve_routes(Some(token()), None, None).await;
    for (method, path) in [("GET", "/v1/denied"), ("POST", "/v1/backups/state")] {
        let answer = request(none, method, path, OPERATOR, "").await;
        assert_eq!(answer.code, 404, "{method} {path}: {}", answer.body);
    }
    let dir = scratch("no-backup");
    let backup = StateBackup {
        db: dir.join("state.db"),
        dir: dir.clone(),
    };
    let no_dir = serve_routes(Some(token()), Some(live_members()), None).await;
    let answer = request(no_dir, "POST", "/v1/backups/state", OPERATOR, "").await;
    assert_eq!(answer.code, 404, "{}", answer.body);
    assert_eq!(listed(no_dir).await, DeniedList::default());
    // No members, a backup: the backup needs the writer check, so it needs them too.
    let no_members = serve_routes(Some(token()), None, Some(backup)).await;
    let answer = request(no_members, "POST", "/v1/backups/state", OPERATOR, "").await;
    assert_eq!(answer.code, 404, "{}", answer.body);
    assert_eq!(files_in(&dir), Vec::<String>::new());
}

/// Catches a backup that is not a copy of the state a restore would need: a row written
/// before the call must be in the copy (so an empty or stale file fails), the copy is a
/// state file a store opens, its summary matches what the answer says, the file is mode 0600
/// whatever the umask, it lies in the configured directory under a name the server chose,
/// nothing else is left there (no temporary or journal file), and a second call makes a
/// second file.
///
/// Mutants seen red: the copy replaced by an empty file; the file made with the process's
/// umask (mode 0644 here); the temporary file left behind; both calls choosing one name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backup_is_a_readable_state_file_with_the_rows_before_it() {
    let server = start();
    let api = server.api;
    deny(api, "node mac-07", "retired").await;

    let answer = request(api, "POST", "/v1/backups/state", OPERATOR, "").await;
    assert_eq!(answer.code, 200, "{}", answer.body);
    assert!(
        answer.head.contains("cache-control: no-store"),
        "{}",
        answer.head
    );
    let made: StateBackupMade = serde_json::from_str(&answer.body).expect("a backup answer");
    let path = PathBuf::from(&made.path);
    assert_eq!(path.parent(), Some(server.dir.as_path()), "{made:?}");
    let name = path.file_name().and_then(|n| n.to_str()).expect("a name");
    assert!(
        name.starts_with("state-") && name.ends_with(".db"),
        "{name}"
    );
    assert_eq!(files_in(&server.dir), [name], "only the backup is left");
    let mode = std::fs::metadata(&path)
        .expect("the backup")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "the backup's mode");

    let summary = kbf_state::sqlite::check_backup(&path).expect("a state file");
    assert_eq!(
        (summary.epoch, summary.head, summary.keys),
        (made.epoch, made.head, made.keys)
    );
    assert!(made.keys > 0 && made.head > 0, "{made:?}");

    // Open a copy of the copy, as a restore would take it: the row is there.
    let kept = scratch("opened").join("state.db");
    std::fs::copy(&path, &kept).expect("copy the backup");
    let store = kbf_state::sqlite::SqliteStore::open(
        &kept,
        Arc::new(kbf_state::SystemClock::default()),
        kbf_state::sqlite::Options::default(),
    )
    .expect("the backup opens as a state store");
    let members = Members::new(Arc::new(store), config());
    assert_eq!(
        members.denied().expect("the list"),
        [DenyEntry::Node("mac-07".to_owned())]
    );
    assert!(server.db.exists());

    let again = request(api, "POST", "/v1/backups/state", OPERATOR, "{}").await;
    assert_eq!(again.code, 200, "{}", again.body);
    let second: StateBackupMade = serde_json::from_str(&again.body).expect("an answer");
    assert_ne!(second.path, made.path);
    assert_eq!(files_in(&server.dir).len(), 2);
}

/// Catches a backup whose destination the caller can influence: a body naming a path, a
/// directory or a name (400, `deny_unknown_fields`), a body that is not an object, bad JSON,
/// and a query string naming a path (ignored: the file lands in the configured directory).
/// Nothing is written at any path a request named, and nothing is made in the directory by
/// a refused request.
///
/// Mutants seen red: the body taking a `path` that the copy then honours (the file appears
/// at the named place and the answer is 200).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backup_takes_no_path_from_the_caller() {
    let server = start();
    let elsewhere = scratch("elsewhere");
    let target = elsewhere.join("evil.db");
    for body in [
        format!(r#"{{"path":"{}"}}"#, target.display()),
        r#"{"path":"../evil.db"}"#.to_owned(),
        r#"{"dir":"/tmp"}"#.to_owned(),
        r#"{"name":"../../x"}"#.to_owned(),
        r#"["x"]"#.to_owned(),
        r#""/etc/passwd""#.to_owned(),
        "{not json".to_owned(),
    ] {
        let answer = request(server.api, "POST", "/v1/backups/state", OPERATOR, &body).await;
        assert_eq!(answer.code, 400, "{body}: {}", answer.body);
    }
    assert_eq!(files_in(&elsewhere), Vec::<String>::new());
    assert_eq!(
        files_in(&server.dir),
        Vec::<String>::new(),
        "a refused call wrote"
    );

    let path = format!("/v1/backups/state?path={}&dir=..", target.display());
    let answer = request(server.api, "POST", &path, OPERATOR, "").await;
    assert_eq!(answer.code, 200, "{}", answer.body);
    let made: StateBackupMade = serde_json::from_str(&answer.body).expect("an answer");
    assert!(
        Path::new(&made.path).starts_with(&server.dir),
        "{} is not under {}",
        made.path,
        server.dir.display()
    );
    assert_eq!(files_in(&elsewhere), Vec::<String>::new());
}

/// Catches a frontend that another writer has fenced out still copying its state: the
/// backup answers 409 and names the cause, and writes nothing. (The fake store stands for
/// the state; the state file path is a file that is not there, so a call that did not stop
/// at the check would fail differently, with a 500.)
///
/// Mutants seen red: the writer check removed (the copy is attempted and answers 500).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fenced_frontend_refuses_a_backup() {
    let dir = scratch("fenced");
    let backup = StateBackup {
        db: dir.join("missing.db"),
        dir: dir.clone(),
    };
    let api = serve_routes(Some(token()), Some(deposed_members()), Some(backup)).await;
    let answer = request(api, "POST", "/v1/backups/state", OPERATOR, "").await;
    assert_eq!(answer.code, 409, "{}", answer.body);
    assert!(answer.body.contains("writer"), "{}", answer.body);
    assert_eq!(files_in(&dir), Vec::<String>::new());
}

/// Catches a backup directory that others can enter being written into: a directory with
/// group or world access, one that is a symbolic link, is refused with 500 and nothing is
/// written there; the same directory at mode 0700 works. The copy is a state file, so the
/// directory is its only protection besides its own mode.
///
/// Mutants seen red: the mode check removed (the copy lands in the open directory); the
/// link check removed (the copy lands behind the link).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backup_directory_others_can_enter_is_refused() {
    let base = scratch("unsafe");
    let (db, open) = (base.join("state").join("state.db"), base.join("open"));
    std::fs::create_dir(&open).expect("dir");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let server = start_over(db, open.clone());
    let answer = request(server.api, "POST", "/v1/backups/state", OPERATOR, "").await;
    assert_eq!(answer.code, 500, "{}", answer.body);
    assert_eq!(
        files_in(&open),
        Vec::<String>::new(),
        "written to an open directory"
    );
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let answer = request(server.api, "POST", "/v1/backups/state", OPERATOR, "").await;
    assert_eq!(answer.code, 200, "{}", answer.body);

    let real = base.join("real");
    std::fs::create_dir(&real).expect("dir");
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let link = base.join("link");
    symlink(&real, &link).expect("a link");
    let state = base.join("linked").join("state.db");
    let linked = start_over(state, link);
    let answer = request(linked.api, "POST", "/v1/backups/state", OPERATOR, "").await;
    assert_eq!(answer.code, 500, "{}", answer.body);
    assert_eq!(
        files_in(&real),
        Vec::<String>::new(),
        "written behind a link"
    );
}
