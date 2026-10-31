//! The `tierstore` binary started from a file with `[cold_store]` alone: a front that sends every
//! call to an S3 store, in a process.

use std::path::Path;

use bytes::Bytes;
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::{ByteRange, ObjectKey, ObjectStore, ObjectStoreError};
use rustix::process::Signal;

use crate::binary::{run, start};
use crate::files::Files;

/// The file of a pass-through front whose cold store is `endpoint`, read with the key file
/// `cold_key`, with `extra` in `[cold_store]`.
fn node_file(files: &Files, endpoint: &str, cold_key: &Path, extra: &str) -> String {
    let text = format!(
        r#"
[server]
listen          = "127.0.0.1:0"
proxy_id        = "bin-pass"
bucket          = "tier-bucket"
access_key_file = "{key}"

[cold_store]
kind             = "s3"
bucket           = "kbf-fake"
endpoint         = "{endpoint}"
credentials_file = "{cold_key}"
{extra}
"#,
        key = files.key.display(),
        cold_key = cold_key.display(),
    );
    files.secret("pass.toml", &text).display().to_string()
}

fn args(config: &str) -> Vec<String> {
    ["--config", config, "--drain-lb-wait", "50ms"]
        .map(str::to_owned)
        .to_vec()
}

/// Asserts that a pass-through front passes a put to the S3 store with the cold store's own key
/// (the store refuses any other), keeps create-only (a second put is refused and the store's
/// object is the first's), reads from the store, leaves in the store the object and nothing else
/// (no journal, no object of a proxy; the probe keys of the start-up test are removed), and exits
/// with status 0 on SIGTERM.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_store_alone_is_served_through_an_s3_store() {
    let fake = FakeS3::start().await;
    fake.require_credentials("COLDKEY", "cold-secret");
    let files = Files::new("bin-cold-node");
    let cold_key = files.secret("cold.key", "COLDKEY\ncold-secret\n");
    let config = node_file(&files, fake.endpoint(), &cold_key, "");
    let mut running = start(&args(&config));
    let store = running.store();

    let key = ObjectKey::new("cas/obj").unwrap();
    let body = Bytes::from_static(b"0123456789");
    store.put_new(&key, body.clone(), None).await.unwrap();
    assert_eq!(fake.object_len(&key), Some(10));
    let again = store
        .put_new(&key, Bytes::from_static(b"replacement"), None)
        .await;
    assert!(matches!(again, Err(ObjectStoreError::AlreadyExists(_))));
    assert_eq!(fake.object_len(&key), Some(10));
    let part = ByteRange::new(2, 3).unwrap();
    assert_eq!(store.get_range(&key, part).await.unwrap(), body.slice(2..5));
    assert_eq!(fake.keys(), ["cas/obj"]);

    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that the file of a cold store that does not keep create-only on a completion (the only
/// way a file can say so is `skip_checks`) is refused with status 2 and nothing on stdout, with
/// the key and the file named, and that the store is not asked anything.
#[test]
fn a_file_that_skips_a_check_of_the_cold_store_is_refused_with_status_2() {
    let files = Files::new("bin-cold-skip");
    let cold_key = files.secret("cold.key", "COLDKEY\ncold-secret\n");
    let skip =
        "skip_checks = [\"multipart-conditional=store-ignores-if-none-match-on-completion\"]";
    let config = node_file(&files, "http://127.0.0.1:1", &cold_key, skip);
    let (code, out, err) = run(&args(&config));
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("cold_store.skip_checks"), "{err}");
    assert!(err.contains("pass.toml"), "{err}");
    assert!(!err.contains("ignores-if-none-match"), "{err}");
}

fn checking(config: &str) -> Vec<String> {
    ["--config", config, "--check-config"]
        .map(str::to_owned)
        .to_vec()
}

/// Asserts that `--check-config` of a good pass-through file prints nothing and exits 0 without a
/// call to the network (the endpoint is an address nothing answers on), and that a request timeout
/// of zero is refused with status 2, nothing on stdout and the message of the start's refusal on stderr.
#[test]
fn check_config_of_a_pass_through_file_passes_silently_and_refuses_a_zero_timeout() {
    let files = Files::new("bin-cold-check");
    let cold_key = files.secret("cold.key", "COLDKEY\ncold-secret\n");
    let config = node_file(&files, "http://192.0.2.1:9300", &cold_key, "");
    assert_eq!(run(&checking(&config)), (0, String::new(), String::new()));

    let text = std::fs::read_to_string(&config).unwrap();
    let zero = text.replace(
        "proxy_id        = \"bin-pass\"",
        "proxy_id        = \"bin-pass\"\ns3_request_timeout = \"0s\"",
    );
    assert_ne!(zero, text);
    let zero = files.secret("zero.toml", &zero).display().to_string();
    let (code, out, said) = run(&checking(&zero));
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(said.contains("must be longer than zero"), "{said}");
}
