//! The base tree cache: what it fetches, what it refuses to keep and what it keeps. A fake of the
//! release's page runs in this process on an address the system picks; nothing here leaves the
//! machine.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use kbf_front::base_publish::PublishError;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::{Cache, FetchError, Origin, STAGING, prune};
use crate::frontend::base_tree::tests::{bundle, sha};
use crate::test_dir::scratch_dir;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const PLATFORM: &str = "linux-x86_64";

/// What the fake answers a path with.
#[derive(Clone)]
pub(in crate::frontend) enum Reply {
    /// 200 and this body.
    Body(Vec<u8>),
    /// This status and no body.
    Status(u16),
    /// 200 that says it has `declared` bytes and sends `sent` of them, then closes.
    Cut { declared: usize, sent: Vec<u8> },
    /// 302 to this path of the same server.
    Redirect(String),
}

/// A release page in this process: the paths it answers and the paths it was asked for.
pub(in crate::frontend) struct Fake {
    pub(in crate::frontend) addr: SocketAddr,
    routes: Arc<Mutex<HashMap<String, Reply>>>,
    asked: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fake {
    pub(in crate::frontend) async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a listener");
        let addr = listener.local_addr().expect("an address");
        let routes: Arc<Mutex<HashMap<String, Reply>>> = Arc::default();
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let (seen, told) = (Arc::clone(&routes), Arc::clone(&asked));
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (routes, asked) = (Arc::clone(&seen), Arc::clone(&told));
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&head).into_owned();
                    let target = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    asked.lock().expect("asked").push(target.clone());
                    let reply = routes.lock().expect("routes").get(&target).cloned();
                    let _ = stream.write_all(&answer(reply)).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            addr,
            routes,
            asked,
            task,
        }
    }

    /// The URL of `target` (a path and query) on this server.
    pub(in crate::frontend) fn url(&self, target: &str) -> String {
        format!("http://{}{target}", self.addr)
    }

    pub(in crate::frontend) fn serve(&self, target: &str, reply: Reply) {
        self.routes
            .lock()
            .expect("routes")
            .insert(target.to_owned(), reply);
    }

    /// The paths asked for so far, in order.
    pub(in crate::frontend) fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("asked").clone()
    }

    /// Serves a list of releases, as GitHub does: `releases` is each release's assets as
    /// `(asset name, path on this server)`, page 1 holds them all and page 2 is empty.
    pub(in crate::frontend) fn list(&self, releases: &[&[(&str, &str)]]) {
        self.pages(&[releases]);
    }

    /// [`Fake::list`] over several pages (a page after the last is empty).
    pub(in crate::frontend) fn pages(&self, pages: &[&[&[(&str, &str)]]]) {
        for (n, releases) in pages.iter().enumerate() {
            let json: Vec<String> = releases
                .iter()
                .map(|assets| {
                    let assets: Vec<String> = assets
                        .iter()
                        .map(|(name, path)| {
                            format!(
                                r#"{{"name":"{name}","browser_download_url":"{}"}}"#,
                                self.url(path)
                            )
                        })
                        .collect();
                    format!(
                        r#"{{"tag_name":"v0.0.{n}","assets":[{}]}}"#,
                        assets.join(",")
                    )
                })
                .collect();
            let body = format!("[{}]", json.join(","));
            let target = format!("/releases?per_page=100&page={}", n + 1);
            self.serve(&target, Reply::Body(body.into_bytes()));
        }
        let after = format!("/releases?per_page=100&page={}", pages.len() + 1);
        self.serve(&after, Reply::Body(b"[]".to_vec()));
    }

    /// Serves the release of `commit` as the release page does: its list entry, its sums file
    /// and its bundle for `PLATFORM`.
    pub(in crate::frontend) fn release(&self, commit: &str) {
        let (bytes, _) = bundle(commit, PLATFORM);
        let name = kbf_front::base_publish::bundle_name(commit, PLATFORM);
        let sums = format!("{}  {name}\n", sha(&bytes));
        self.serve("/sums", Reply::Body(sums.into_bytes()));
        self.serve("/bundle", Reply::Body(bytes));
        self.list(&[&[("SHA256SUMS", "/sums"), (&name, "/bundle")]]);
    }
}

/// The bytes of the answer `reply` says.
fn answer(reply: Option<Reply>) -> Vec<u8> {
    let head = |status: &str, length: usize, extra: &str| {
        format!("HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n{extra}\r\n")
            .into_bytes()
    };
    match reply {
        Some(Reply::Body(body)) => [head("200 OK", body.len(), ""), body].concat(),
        Some(Reply::Cut { declared, sent }) => [head("200 OK", declared, ""), sent].concat(),
        Some(Reply::Redirect(to)) => head("302 Found", 0, &format!("Location: {to}\r\n")),
        Some(Reply::Status(code)) => head(&format!("{code} Answer"), 0, ""),
        None => head("404 Not Found", 0, ""),
    }
}

fn cache(dir: &Path, fake: &Fake) -> Cache {
    Cache {
        dir: dir.to_owned(),
        url: fake.url("/releases"),
        keep: 1,
        timeout: Duration::from_secs(5),
    }
}

fn bundle_file(commit: &str) -> String {
    kbf_front::base_publish::bundle_name(commit, PLATFORM)
}

/// Asserts that a cache with nothing in it fetches the release's sums and bundle by the bundle's
/// name from the release's list, follows the redirect to the file, checks the bundle against
/// the listed digest and keeps both under the commit; and that the next start finds them and
/// asks the page for nothing. Catches: a start that downloads on every start, and a cache that
/// is not under the commit.
#[tokio::test]
async fn a_missing_release_is_fetched_once_and_then_held() {
    let dir = scratch_dir("base-fetch-once");
    let fake = Fake::start().await;
    fake.release(COMMIT);
    fake.serve("/bundle", Reply::Redirect("/cdn/bundle".to_owned()));
    fake.serve("/cdn/bundle", Reply::Body(bundle(COMMIT, PLATFORM).0));
    let cache = cache(dir.path(), &fake);

    let (held, origin) = cache.ensure(COMMIT, PLATFORM).await.expect("fetched");

    assert_eq!(origin, Origin::Fetched);
    assert_eq!(held, dir.path().join(COMMIT));
    assert_eq!(
        std::fs::read(held.join(bundle_file(COMMIT))).expect("the bundle"),
        bundle(COMMIT, PLATFORM).0
    );
    assert!(held.join("SHA256SUMS").is_file());
    assert!(!dir.path().join(format!("{STAGING}{COMMIT}")).exists());
    let asked = fake.asked();
    assert_eq!(
        asked,
        [
            "/releases?per_page=100&page=1",
            "/sums",
            "/bundle",
            "/cdn/bundle"
        ]
    );

    let (again, origin) = cache.ensure(COMMIT, PLATFORM).await.expect("held");

    assert_eq!((again, origin), (held, Origin::Cached));
    assert_eq!(fake.asked(), asked, "a hit asks the page for nothing");
}

/// Asserts that a release found on the second page of the list is fetched, and that a list that
/// never names the bundle, or names it in a release with no sums file, is refused naming the
/// bundle. Catches: a lookup that reads only the first page, and one that takes a bundle
/// without the sums that vouch for it.
#[tokio::test]
async fn the_release_is_looked_up_by_its_bundle_name_page_by_page() {
    let dir = scratch_dir("base-fetch-pages");
    let fake = Fake::start().await;
    let (bytes, _) = bundle(COMMIT, PLATFORM);
    let name = bundle_file(COMMIT);
    fake.serve(
        "/sums",
        Reply::Body(format!("{}  {name}\n", sha(&bytes)).into_bytes()),
    );
    fake.serve("/bundle", Reply::Body(bytes));
    let other = [("kbf-base-tree-other.tree.tar", "/x")];
    let ours = [("SHA256SUMS", "/sums"), (name.as_str(), "/bundle")];
    fake.pages(&[&[&other], &[&ours]]);

    let (_, origin) = cache(dir.path(), &fake)
        .ensure(COMMIT, PLATFORM)
        .await
        .expect("found on page 2");
    assert_eq!(origin, Origin::Fetched);

    let nowhere = scratch_dir("base-fetch-nowhere");
    fake.list(&[&other]);
    let refused = cache(nowhere.path(), &fake)
        .ensure(COMMIT, PLATFORM)
        .await
        .expect_err("not released");
    assert!(
        matches!(refused, FetchError::NotReleased { .. }),
        "{refused}"
    );
    assert!(refused.to_string().contains(&name), "{refused}");

    fake.list(&[&[(name.as_str(), "/bundle")]]);
    let unsummed = cache(nowhere.path(), &fake)
        .ensure(COMMIT, PLATFORM)
        .await
        .expect_err("no sums");
    assert!(
        matches!(unsummed, FetchError::NotReleased { .. }),
        "{unsummed}"
    );
    assert!(!nowhere.path().join(COMMIT).exists());
}

/// Asserts that a bundle whose bytes are not the digest the release's sums file lists is
/// refused, naming both digests, and that nothing of it stays: no cache entry and no download
/// directory. Catches: a cache that keeps what it could not verify.
#[tokio::test]
async fn a_bundle_that_is_not_the_listed_digest_is_refused_and_not_kept() {
    let dir = scratch_dir("base-fetch-digest");
    let fake = Fake::start().await;
    fake.release(COMMIT);
    let mut wrong = bundle(COMMIT, PLATFORM).0;
    wrong.push(0);
    fake.serve("/bundle", Reply::Body(wrong.clone()));

    let refused = cache(dir.path(), &fake)
        .ensure(COMMIT, PLATFORM)
        .await
        .expect_err("a wrong digest");

    let FetchError::Refused(PublishError::Digest { actual, listed, .. }) = &refused else {
        panic!("not a digest refusal: {refused}");
    };
    assert_eq!(*actual, sha(&wrong));
    assert_eq!(*listed, sha(&bundle(COMMIT, PLATFORM).0));
    assert!(refused.to_string().contains("refused"), "{refused}");
    assert_eq!(std::fs::read_dir(dir.path()).expect("a dir").count(), 0);
}

/// Asserts that a download that ends before the length it announced fails, leaves nothing in
/// the cache, and that the next start, with the page now whole, fetches the release again over
/// the leftover of the first. Catches: a partial bundle taken as the release's.
#[tokio::test]
async fn a_partial_download_is_never_used() {
    let dir = scratch_dir("base-fetch-partial");
    let fake = Fake::start().await;
    fake.release(COMMIT);
    let whole = bundle(COMMIT, PLATFORM).0;
    let half = whole[..whole.len() / 2].to_vec();
    fake.serve(
        "/bundle",
        Reply::Cut {
            declared: whole.len(),
            sent: half,
        },
    );
    let cache = cache(dir.path(), &fake);

    let cut = cache.ensure(COMMIT, PLATFORM).await.expect_err("cut short");

    assert!(matches!(cut, FetchError::Download { .. }), "{cut}");
    assert!(!dir.path().join(COMMIT).exists());
    assert!(!dir.path().join(format!("{STAGING}{COMMIT}")).exists());

    // A download a stopped start left behind is replaced, not read.
    let leftover = dir.path().join(format!("{STAGING}{COMMIT}"));
    std::fs::create_dir(&leftover).expect("a leftover");
    std::fs::write(leftover.join("SHA256SUMS"), "junk").expect("junk");
    fake.serve("/bundle", Reply::Body(whole));

    let (held, origin) = cache.ensure(COMMIT, PLATFORM).await.expect("whole");

    assert_eq!(origin, Origin::Fetched);
    assert!(kbf_front::base_publish::is_staged(&held, COMMIT, PLATFORM));
    assert!(!leftover.exists());
}

/// Asserts that an answer that is not success (a 404 for the bundle, a 500 for the list), a list
/// that is not JSON and one that is not an array are each refused naming the URL, with nothing
/// kept. Catches: a failed answer taken for a file.
#[tokio::test]
async fn answers_that_are_not_a_release_are_refused() {
    let dir = scratch_dir("base-fetch-answers");
    let fake = Fake::start().await;
    fake.release(COMMIT);
    fake.serve("/bundle", Reply::Status(404));
    let cache = cache(dir.path(), &fake);

    let gone = cache.ensure(COMMIT, PLATFORM).await.expect_err("a 404");
    assert!(matches!(gone, FetchError::Download { .. }), "{gone}");
    assert!(gone.to_string().contains("/bundle"), "{gone}");
    assert!(gone.to_string().contains("404"), "{gone}");

    fake.serve("/releases?per_page=100&page=1", Reply::Status(500));
    let broken = cache.ensure(COMMIT, PLATFORM).await.expect_err("a 500");
    assert!(matches!(broken, FetchError::Download { .. }), "{broken}");

    fake.serve(
        "/releases?per_page=100&page=1",
        Reply::Body(b"nope".to_vec()),
    );
    let text = cache.ensure(COMMIT, PLATFORM).await.expect_err("not JSON");
    assert!(matches!(text, FetchError::Index { .. }), "{text}");

    fake.serve("/releases?per_page=100&page=1", Reply::Body(b"{}".to_vec()));
    let object = cache
        .ensure(COMMIT, PLATFORM)
        .await
        .expect_err("not an array");
    assert!(matches!(object, FetchError::Index { .. }), "{object}");
    assert_eq!(std::fs::read_dir(dir.path()).expect("a dir").count(), 0);
}

/// Asserts that the files an operator staged loose in the directory, as every earlier release
/// asked, are used as they are and nothing is fetched, and that an empty URL, or a commit that
/// cannot name a directory, fetches nothing either and leaves the publish to name what is
/// missing. Catches: a cache that breaks a staged directory, and one that dials out with the
/// fetch off.
#[tokio::test]
async fn staged_files_and_a_fetch_that_is_off_ask_the_page_for_nothing() {
    let dir = scratch_dir("base-fetch-staged");
    let fake = Fake::start().await;
    fake.release(COMMIT);
    let cache = cache(dir.path(), &fake);
    crate::frontend::base_tree::tests::bundle_dir(dir.path(), COMMIT, &[PLATFORM]);

    let (used, origin) = cache.ensure(COMMIT, PLATFORM).await.expect("staged");
    assert_eq!((used.as_path(), origin), (dir.path(), Origin::Staged));

    let empty = scratch_dir("base-fetch-off");
    let off = Cache {
        url: String::new(),
        ..self::cache(empty.path(), &fake)
    };
    let (used, origin) = off.ensure(COMMIT, PLATFORM).await.expect("off");
    assert_eq!((used.as_path(), origin), (empty.path(), Origin::Staged));
    let (used, origin) = cache
        .ensure("../not-a-commit", PLATFORM)
        .await
        .expect("a name that is no commit");
    assert_eq!((used.as_path(), origin), (dir.path(), Origin::Staged));
    assert!(fake.asked().is_empty(), "{:?}", fake.asked());
}

/// Asserts that after a fetch the cache keeps the release and the `keep` newest others, removes
/// the older ones and a download a stopped start left, and touches nothing that is not a
/// commit's directory (loose staged files, other names). Catches: a cache that grows by a
/// release at every upgrade, and a prune that takes the operator's files.
#[tokio::test]
async fn a_fetch_keeps_the_previous_release_and_removes_the_older_ones() {
    let dir = scratch_dir("base-fetch-prune");
    let fake = Fake::start().await;
    fake.release(COMMIT);
    let (oldest, older, newest) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    for (index, commit) in [&oldest, &older, &newest].into_iter().enumerate() {
        let path = dir.path().join(commit);
        std::fs::create_dir(&path).expect("a release");
        std::fs::write(path.join("SHA256SUMS"), commit).expect("a file");
        let at = base + Duration::from_secs(index as u64 * 10);
        std::fs::File::open(&path)
            .and_then(|dir| dir.set_modified(at))
            .expect("a time");
    }
    std::fs::create_dir(dir.path().join(format!("{STAGING}{}", "4".repeat(40)))).expect("stale");
    std::fs::create_dir(dir.path().join("notes")).expect("an unrelated directory");
    std::fs::write(dir.path().join("SHA256SUMS"), "loose").expect("a loose file");

    cache(dir.path(), &fake)
        .ensure(COMMIT, PLATFORM)
        .await
        .expect("fetched");

    let mut left: Vec<String> = std::fs::read_dir(dir.path())
        .expect("a dir")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    left.sort();
    let mut want = vec![
        COMMIT.to_owned(),
        newest,
        "SHA256SUMS".to_owned(),
        "notes".to_owned(),
    ];
    want.sort();
    assert_eq!(left, want);
}

/// Asserts that a pruning that cannot read its directory says which, and that a keep of 0 keeps
/// no other release. Catches: a prune error that names nothing.
#[test]
fn a_prune_names_what_it_cannot_read_and_keep_zero_keeps_no_other() {
    let dir = scratch_dir("base-fetch-prune-direct");
    let failed = prune(&dir.path().join("absent"), COMMIT, 1).expect_err("no directory");
    assert!(failed.contains("absent"), "{failed}");

    let other = "5".repeat(40);
    std::fs::create_dir(dir.path().join(&other)).expect("a release");
    std::fs::create_dir(dir.path().join(COMMIT)).expect("this release");
    let removed = prune(dir.path(), COMMIT, 0).expect("pruned");
    assert_eq!(removed, [dir.path().join(&other)]);
    assert!(dir.path().join(COMMIT).is_dir());
}

/// Asserts that a cache directory that cannot hold the download fails the start naming the path
/// (here the directory is a file, and a leftover download is a file too, which cannot be
/// replaced), and that a release directory that is not one is replaced by the fetched one.
/// Catches: a file error that says nothing of where.
#[tokio::test]
async fn files_that_cannot_be_made_are_named_and_a_broken_release_is_replaced() {
    let fake = Fake::start().await;
    fake.release(COMMIT);

    let scratch = scratch_dir("base-fetch-io");
    let not_a_dir = scratch.path().join("file");
    std::fs::write(&not_a_dir, "x").expect("a file");
    let refused = cache(&not_a_dir, &fake)
        .ensure(COMMIT, PLATFORM)
        .await
        .expect_err("not a directory");
    assert!(matches!(refused, FetchError::Io { .. }), "{refused}");
    assert!(refused.to_string().contains("file"), "{refused}");

    let stuck = scratch.path().join("stuck");
    std::fs::create_dir(&stuck).expect("a directory");
    std::fs::write(stuck.join(format!("{STAGING}{COMMIT}")), "x").expect("a leftover file");
    let blocked = cache(&stuck, &fake)
        .ensure(COMMIT, PLATFORM)
        .await
        .expect_err("a leftover that is a file");
    assert!(matches!(blocked, FetchError::Io { .. }), "{blocked}");

    let broken = scratch.path().join("broken");
    std::fs::create_dir_all(broken.join(COMMIT)).expect("a release");
    std::fs::write(broken.join(COMMIT).join("SHA256SUMS"), "not sums").expect("junk");
    let (held, origin) = cache(&broken, &fake)
        .ensure(COMMIT, PLATFORM)
        .await
        .expect("replaced");
    assert_eq!(origin, Origin::Fetched);
    assert!(kbf_front::base_publish::is_staged(&held, COMMIT, PLATFORM));
}
