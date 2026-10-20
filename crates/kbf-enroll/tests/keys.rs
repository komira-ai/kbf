//! The registered cold store of a storage node: fetched over its mutual-TLS session and held
//! in memory only.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use kbf_enroll::client::{CallError, Enrollment, connect, enroll};
use kbf_enroll::keys::{KeysError, fetch};
use kbf_enroll::pin::Anchor;
use kbf_enroll::store::Store;
use kbf_testkit::dir::TestDir;
use kbf_types::{Role, RoleSet};
use support::fake::{self, Running};
use support::{config_for, facts, scratch};

/// The scratch directory is first so that, bound by a pattern, it drops after the rest.
async fn enrolled(name: &str, roles: &[Role]) -> (TestDir, Running, Store) {
    let running = fake::start();
    let root = scratch(name);
    let config = config_for(&root, running.enroll_addr.port(), "");
    let token = running.fake.mint(0x3333, 1);
    std::fs::write(&config.token_file, token.to_line()).expect("token file");
    std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600))
        .expect("mode");
    let store = Store::open(&config.state_dir).expect("store");
    let frontend = connect(&config.frontend, Anchor::Pin(*token.pin()))
        .await
        .expect("connect");
    enroll(
        &frontend,
        &store,
        &Enrollment {
            config: &config,
            token: &token,
            roles: RoleSet::new(roles.iter().copied()).expect("roles"),
            facts: facts(),
            now: running.fake.now(),
        },
    )
    .await
    .expect("enrolled");
    (root, running, store)
}

fn worker(running: &Running) -> String {
    format!("https://localhost:{}", running.worker_addr.port())
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[tokio::test]
async fn a_storage_node_gets_the_store_over_its_certificate_and_it_reaches_no_file() {
    let (root, running, store) = enrolled("keys", &[Role::Storage, Role::Daemon]).await;
    let got = fetch(&store, &worker(&running), 0)
        .await
        .expect("the store");
    assert_eq!(got.generation, 3);
    let config = got.config.as_ref().expect("a store");
    assert_eq!(config.access_key_id, "AKIATESTKEY0000");
    assert_eq!(config.secret_access_key, "secret-that-must-stay-in-memory");
    assert_eq!((config.bucket.as_str(), config.secret_version), ("bkt", 3));
    assert_eq!(running.fake.keys_calls(), 1);

    // Catches: a Debug or log line that prints the secret, of the store or of the answer.
    assert!(!format!("{config:?}").contains("secret-that-must-stay"));
    assert!(!format!("{got:?}").contains("secret-that-must-stay"));
    assert!(format!("{got:?}").contains("Secret(..)"));

    // Catches: the keys landing in a file (a cache, a state file, a temp file).
    let mut all = Vec::new();
    files(&root, &mut all);
    assert!(all.len() > 4, "the node's files are there to be searched");
    for path in all {
        let text = String::from_utf8_lossy(&std::fs::read(&path).expect("read")).into_owned();
        assert!(
            !text.contains("secret-that-must-stay-in-memory"),
            "{}",
            path.display()
        );
        assert!(!text.contains("AKIATESTKEY0000"), "{}", path.display());
    }
}

/// Catches: the node's generation not sent (the frontend would always answer whole), an
/// unchanged answer that carries the store, and a different generation, lower ones included,
/// that does not: the node holds 3 and the frontend is at 3, then at 2 (a restored state).
#[tokio::test]
async fn the_node_sends_the_generation_it_holds_and_is_told_when_it_is_current() {
    let (_root, running, store) = enrolled("keys-have", &[Role::Storage]).await;
    let same = fetch(&store, &worker(&running), 3).await.expect("same");
    assert_eq!(running.fake.last_have(), 3);
    assert_eq!((same.generation, same.config.is_none()), (3, true));

    running.fake.set_store_generation(2);
    let back = fetch(&store, &worker(&running), 3).await.expect("back");
    assert_eq!(back.generation, 2);
    assert!(back.config.is_some(), "a lower generation is fetched whole");

    running.fake.set_store_generation(0);
    let none = fetch(&store, &worker(&running), 0).await.expect("none");
    assert_eq!((none.generation, none.config.is_none()), (0, true));
    assert_eq!(running.fake.keys_calls(), 3);
}

#[tokio::test]
async fn a_revoked_node_is_refused_the_keys() {
    let (_root, running, store) = enrolled("keys-revoked", &[Role::Storage]).await;
    running.fake.revoke_keys();
    let err = fetch(&store, &worker(&running), 0).await.unwrap_err();
    assert!(
        matches!(&err, KeysError::Call(CallError::Status(s)) if s.code() == tonic::Code::PermissionDenied),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_node_without_a_storage_identity_has_nothing_to_ask_with() {
    let (_root, running, store) = enrolled("keys-daemon-only", &[Role::Daemon]).await;
    let err = fetch(&store, &worker(&running), 0).await.unwrap_err();
    assert!(
        matches!(&err, KeysError::Identity(m) if m.contains("storage.pem")),
        "{err}"
    );
    assert_eq!(running.fake.keys_calls(), 0);
}

#[tokio::test]
async fn an_endpoint_that_is_not_a_url_or_not_listening_is_a_transport_error() {
    let (_root, running, store) = enrolled("keys-endpoint", &[Role::Storage]).await;
    let err = fetch(&store, "not a url", 0).await.unwrap_err();
    assert!(matches!(err, KeysError::Transport(_)), "{err}");
    let closed = kbf_testkit::port::dead_addr().port();
    let err = fetch(&store, &format!("https://localhost:{closed}"), 0)
        .await
        .unwrap_err();
    assert!(matches!(err, KeysError::Transport(_)), "{err}");
    drop(running);
}
