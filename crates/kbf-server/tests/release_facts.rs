//! The version facts of a release in the store: what the upload takes, refuses and answers
//! (`kbf_server::release`, `docs/design/pull-updates.md` section 6.2).
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Bytes;
use kbf_server::release::{self, Commit, Limits, PutError, ReleaseStore};
use kbf_server::token::ApiToken;
use kbf_types::ReleasePlatform;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const FILE: &str = "kbf-0123456789abcdef0123456789abcdef01234567-linux-x86_64.version.json";

struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "kbf-server-release-facts-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a test directory");
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Served {
    dir: Dir,
    store: Arc<ReleaseStore>,
    addr: SocketAddr,
}

async fn serve() -> Served {
    let dir = Dir::new();
    let path = dir.0.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let token = ApiToken::from_file(&path).expect("a usable token");
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("store"), Limits::default()).expect("open"));
    let app = release::router(Some(store.clone()), Some(token));
    let app = app.layer(axum::middleware::from_fn_with_state(
        None::<Arc<kbf_server::proxy_auth::ProxySecret>>,
        kbf_server::proxy_auth::gate_requests,
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, service).await.expect("serve");
    });
    Served { dir, store, addr }
}

async fn request(addr: SocketAddr, method: &str, path: &str, payload: &[u8]) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nAuthorization: Bearer \
         {TOKEN}\r\nContent-Length: {}\r\n\r\n",
        payload.len()
    );
    stream.write_all(head.as_bytes()).await.expect("write");
    stream.write_all(payload).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read the reply");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text[9..12].parse().expect("a status");
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// What `kbf --version --json` prints, with `edit` applied to the text.
fn doc(edit: impl Fn(String) -> String) -> Vec<u8> {
    edit(
        r#"{"commit":"0123456789ab","min_protocol":6,"profile":"release","protocol":7,"state_schema":3,"version":"0.1.0"}"#
            .to_owned(),
    )
    .into_bytes()
}

fn facts_path(platform: &str) -> String {
    format!("/v1/releases/{COMMIT}/{platform}/version.json")
}

fn body(bytes: &[u8]) -> impl futures::Stream<Item = io::Result<Bytes>> + Unpin + Send + use<> {
    futures::stream::iter(vec![Ok(Bytes::copy_from_slice(bytes))])
}

/// Every file under the store directory, relative and sorted (what an upload left).
fn files(served: &Served) -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read a directory") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .expect("inside")
                        .display()
                        .to_string(),
                );
            }
        }
    }
    let root = served.dir.0.join("store");
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out.sort();
    out
}

/// Catches a version facts document stored that the node must not trust, or a
/// refusal that does not say which file and why: a document that is not a report, names
/// another commit, names another platform than the path, is a debug build, or states numbers
/// a node refuses is `400`, the answer names `kbf-<commit>-<platform>.version.json` and the
/// reason, no path of the host, and nothing is stored (no file, no listing of facts).
///
/// Catches: the platform comparison removed (a document naming another platform
/// is stored); the check removed (every document is stored).
#[tokio::test]
async fn a_facts_document_that_does_not_parse_or_contradicts_the_upload_is_refused() {
    let served = serve().await;
    let before = files(&served);
    let refused: [(Vec<u8>, &str); 6] = [
        (b"kbf 0.1.0".to_vec(), "it printed no JSON object"),
        (
            doc(|d| d.replace("0123456789ab", "fedcba987654")),
            "it says commit fedcba987654, and the release is 0123456789ab",
        ),
        (
            doc(|d| d.replace('}', ",\"platform\":\"darwin-arm64\"}")),
            "it says platform darwin-arm64, and the upload is for linux-x86_64",
        ),
        (
            doc(|d| d.replace("\"release\"", "\"debug\"")),
            "it says profile \"debug\"",
        ),
        (
            doc(|d| d.replace("\"protocol\":7", "\"protocol\":0")),
            "it has no protocol or state schema (0)",
        ),
        (
            doc(|d| d.replace("\"min_protocol\":6", "\"min_protocol\":8")),
            "min_protocol 8 is above protocol 7",
        ),
    ];
    for (payload, why) in refused {
        let (code, answer) =
            request(served.addr, "PUT", &facts_path("linux-x86_64"), &payload).await;
        assert_eq!(code, 400, "{why}: {answer}");
        let said = answer["error"].as_str().expect("an error");
        assert!(said.starts_with(&format!("{FILE}: ")), "{said}");
        assert!(said.contains(why), "{said}");
        assert!(
            !said.contains(&served.dir.0.display().to_string()),
            "{said}"
        );
    }
    assert_eq!(files(&served), before, "nothing was stored");

    // The same is said of the path: a segment that is not a platform is the key error.
    let (code, _) = request(served.addr, "PUT", &facts_path("plan9-mips"), &doc(|d| d)).await;
    assert_eq!(code, 400);
    let (code, _) = request(
        served.addr,
        "PUT",
        "/v1/releases/short/linux-arm64/version.json",
        &doc(|d| d),
    )
    .await;
    assert_eq!(code, 400);
    assert_eq!(files(&served), before);
}

/// Catches the document stored wrongly once it is good: `201` with its digest and size, the
/// same bytes again `200`, other bytes `409` naming the file (a release is immutable), the
/// store then reports the facts, a platform listed with `version_facts` and no note, and a
/// document of the Mac's own accepted beside the Linux one.
///
#[tokio::test]
async fn a_good_facts_document_is_stored_once_and_immutably() {
    let served = serve().await;
    let good = doc(|d| d);
    let (code, first) = request(served.addr, "PUT", &facts_path("linux-x86_64"), &good).await;
    assert_eq!(
        (code, first["size"].as_u64()),
        (201, Some(good.len() as u64)),
        "{first}"
    );
    assert_eq!(first.get("note"), None);
    let (code, again) = request(served.addr, "PUT", &facts_path("linux-x86_64"), &good).await;
    assert_eq!((code, &again), (200, &first));
    let other = doc(|d| d.replace("\"state_schema\":3", "\"state_schema\":4"));
    let (code, answer) = request(served.addr, "PUT", &facts_path("linux-x86_64"), &other).await;
    assert_eq!(code, 409, "{answer}");
    assert!(
        answer["error"].as_str().expect("error").starts_with(FILE),
        "{answer}"
    );
    let mac = doc(|d| d.replace('}', ",\"platform\":\"darwin-arm64\"}"));
    let (code, _) = request(served.addr, "PUT", &facts_path("darwin-arm64"), &mac).await;
    assert_eq!(code, 201);

    let commit = Commit::parse(COMMIT).expect("a commit");
    let facts = served
        .store
        .facts(&commit, ReleasePlatform::LinuxX86_64)
        .await
        .expect("read")
        .expect("stored");
    assert_eq!(
        (
            facts.protocol,
            facts.min_protocol,
            facts.state_schema,
            facts.profile.as_str()
        ),
        (7, 6, 3, "release"),
        "the first document, not the refused one"
    );
    let none = served
        .store
        .facts(&commit, ReleasePlatform::LinuxArm64)
        .await
        .expect("read");
    assert_eq!(none, None);
}

/// Catches the limits of the facts body: empty is `400`, one byte past 4 KiB `413`, a body
/// cut short `400`; and a failing directory a `500` that names no path.
#[tokio::test]
async fn the_facts_body_is_bounded_and_a_failing_directory_is_an_error() {
    let served = serve().await;
    let (code, _) = request(served.addr, "PUT", &facts_path("linux-x86_64"), b"").await;
    assert_eq!(code, 400);
    let big = vec![b' '; 4097];
    let (code, _) = request(served.addr, "PUT", &facts_path("linux-x86_64"), &big).await;
    assert_eq!(code, 413);

    let commit = Commit::parse(COMMIT).expect("a commit");
    let cut = futures::stream::iter(vec![Err::<Bytes, _>(io::Error::other("cut"))]);
    let mut cut = Box::pin(cut);
    let err = served
        .store
        .put_facts(&commit, ReleasePlatform::LinuxX86_64, &mut cut)
        .await
        .expect_err("a cut body");
    assert!(matches!(err, PutError::Body(_)), "{err}");

    // A file where the platform's directory belongs: the put fails on the disk.
    let index = served.dir.0.join("store").join("index").join(COMMIT);
    std::fs::create_dir_all(&index).expect("dir");
    std::fs::write(index.join("linux-arm64"), b"in the way").expect("file");
    let (code, answer) = request(served.addr, "PUT", &facts_path("linux-arm64"), &doc(|d| d)).await;
    assert_eq!(code, 500, "{answer}");
    let text = answer.to_string();
    assert!(
        !text.contains(&served.dir.0.display().to_string()),
        "{text}"
    );
    let err = served
        .store
        .put_facts(&commit, ReleasePlatform::LinuxArm64, &mut body(&doc(|d| d)))
        .await
        .expect_err("the disk");
    assert!(matches!(err, PutError::Disk(_)), "{err}");
}

/// Catches a platform stored without facts refused, or left unexplained: the tarball and the
/// attestation of `linux-x86_64` are accepted (`201`), each answer names the missing file
/// with the note, the facts arriving silences it for that platform, and another platform
/// without any is still noted. The listing says so for the same platforms.
///
/// Catches: the upload of a platform without facts refused (`409`).
#[tokio::test]
async fn a_platform_without_facts_is_accepted_and_the_answers_say_so() {
    let served = serve().await;
    let note = format!(
        "no version facts ({FILE}): nodes will not be offered this platform; rebuild the \
         release to include it"
    );
    let tarball = format!("/v1/releases/{COMMIT}/linux-x86_64/kbf");
    let (code, answer) = request(served.addr, "PUT", &tarball, b"tarball").await;
    assert_eq!(code, 201, "{answer}");
    assert_eq!(answer["note"], note.as_str());
    let (code, answer) = request(
        served.addr,
        "PUT",
        &format!("{tarball}.attestation"),
        b"bundle",
    )
    .await;
    assert_eq!(code, 201, "{answer}");
    assert_eq!(answer["note"], note.as_str());

    let (_, listing) = request(served.addr, "GET", "/v1/releases", b"").await;
    let platform = &listing["releases"][0]["platforms"][0];
    assert_eq!(platform["complete"], true, "held as complete");
    assert_eq!(platform["version_facts"], false);
    assert_eq!(platform["note"], note.as_str());

    let (code, _) = request(served.addr, "PUT", &facts_path("linux-x86_64"), &doc(|d| d)).await;
    assert_eq!(code, 201);
    let (code, answer) = request(served.addr, "PUT", &tarball, b"tarball").await;
    assert_eq!(code, 200, "{answer}");
    assert_eq!(answer.get("note"), None, "the facts are there now");
    let (_, listing) = request(served.addr, "GET", "/v1/releases", b"").await;
    let platform = &listing["releases"][0]["platforms"][0];
    assert_eq!(
        (&platform["version_facts"], platform.get("note")),
        (&Value::Bool(true), None)
    );
}

/// Catches a stored facts document that no longer reads (edited on the disk) being passed
/// over as "no facts" in an answer, or its path leaking: the put of a tarball beside it is a
/// `500` that names no path, and the listing, which only looks for the file, still lists the
/// platform.
///
/// Catches: the read error taken as no document (the put answers `201` with a note).
#[tokio::test]
async fn a_stored_facts_document_that_cannot_be_read_fails_the_put_without_a_path() {
    let served = serve().await;
    let tarball = format!("/v1/releases/{COMMIT}/linux-x86_64/kbf");
    let (code, _) = request(served.addr, "PUT", &facts_path("linux-x86_64"), &doc(|d| d)).await;
    assert_eq!(code, 201);
    let (code, _) = request(served.addr, "PUT", &tarball, b"tarball").await;
    assert_eq!(code, 201);
    let file = served
        .dir
        .0
        .join("store")
        .join("index")
        .join(COMMIT)
        .join("linux-x86_64")
        .join("version.json");
    std::fs::write(&file, b"edited by hand").expect("corrupt it");
    let (code, answer) = request(served.addr, "PUT", &format!("{tarball}-daemon"), b"daemon").await;
    assert_eq!(code, 500, "{answer}");
    let text = answer.to_string();
    assert!(
        !text.contains(&served.dir.0.display().to_string()),
        "{text}"
    );
}
