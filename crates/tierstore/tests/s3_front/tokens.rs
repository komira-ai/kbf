//! Tokens against the S3 front over loopback, with the client of kbf: the longest id a token has
//! goes through the credential scope of its signing, and the client's calls work with a token as
//! with any key.

use kbf_objstore::{KeyPrefix, ObjectStore, PageSize};
use tierstore::server::token::{
    Ceiling, Claims, MAX_KEY_ID, MAX_PREFIX, MintingKey, Ops, TokenRole, mint,
};
use tierstore::{Clock, SystemClock};

use crate::client::{CONDITIONAL, assert_refused, key, put, read};
use crate::support::Harness;

/// Asserts that the client of kbf puts, reads, heads and lists with a token whose id is the
/// longest there is (a minting key id of 32 bytes and a prefix of 256), and that the same client
/// is refused a key outside the prefix with the S3 error document. It catches a client or a
/// server that cannot take a long access key id in the credential scope.
#[tokio::test]
async fn the_client_of_kbf_works_with_a_token_of_the_longest_id() {
    let ceiling = Ceiling {
        prefix: "jobs/".to_owned(),
        ops: Ops {
            create: true,
            read: true,
            delete: false,
        },
        max_ttl_secs: 3600,
        max_object_bytes: None,
    };
    let minting = MintingKey::new(&"k".repeat(MAX_KEY_ID), &[1_u8; 32], ceiling).unwrap();
    let held = minting.clone();
    let h = Harness::start(move |config| config.token_keys = vec![held]).await;
    let now = SystemClock.now();
    let prefix = format!("jobs/{}/", "p".repeat(MAX_PREFIX - 6));
    assert_eq!(prefix.len(), MAX_PREFIX);
    let claims = Claims {
        role: TokenRole::Client,
        ops: Ops {
            create: true,
            read: true,
            delete: false,
        },
        prefix: prefix.clone(),
        not_after: now / 1000 + 600,
    };
    let credential = mint(&minting, &claims, now).unwrap();
    assert_eq!(credential.access_key_id.len(), 426);
    let store = h.store_with(
        &credential.access_key_id,
        &credential.secret_access_key,
        CONDITIONAL,
    );
    let name = format!("{prefix}object");
    put(&store, &name, b"hello").await;
    assert_eq!(read(&store, &name, 0, 5).await, "hello");
    assert_eq!(store.head(&key(&name)).await.unwrap().size, 5);
    let listed = KeyPrefix::new(&prefix).unwrap();
    let page = store.list(&listed, None, PageSize::MAX).await.unwrap();
    assert_eq!(page.objects.len(), 1);
    let again = store
        .put_new(&key(&name), bytes::Bytes::from_static(b"other"), None)
        .await;
    assert!(again.is_err());
    assert_eq!(read(&store, &name, 0, 5).await, "hello");
    let outside = store
        .put_new(
            &key("jobs/elsewhere"),
            bytes::Bytes::from_static(b"x"),
            None,
        )
        .await;
    assert_refused(outside, 403, "AccessDenied");
}
