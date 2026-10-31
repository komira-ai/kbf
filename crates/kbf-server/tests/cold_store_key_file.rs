//! A storage node with an `s3://` cold store signs with the key pair of
//! `--cold-store-credentials-file`, against an S3 service that refuses any other pair, and
//! refuses a key file it must not use before it opens anything.

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/cold_node.rs"]
mod node;
#[path = "support/store_config.rs"]
mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use kbf_server::StartFailure;
use node::{ID, SECRET, bucket, class, dir, flags, no_keys, rig, round_trip, started};

/// The key file of a node `name` holding `id` and `secret` at `mode`.
fn key_file(name: &str, id: &str, secret: &str, mode: u32) -> PathBuf {
    let path = dir(name).join("cold.json");
    let text = format!(r#"{{"access_key_id": "{id}", "secret_access_key": "{secret}"}}"#);
    std::fs::write(&path, text).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode set");
    path
}

fn wrong_keys(name: &str) -> Option<String> {
    match name {
        "AWS_ACCESS_KEY_ID" | "AWS_SECRET_ACCESS_KEY" => Some("not-the-key".to_owned()),
        _ => None,
    }
}

/// Catches a key file that is read and then ignored (the node would have no keys and refuse
/// to start, or sign with the environment's), and one whose pair is not the one signed with:
/// the service takes one pair only, so a write through the node's cache fails if it is wrong.
/// The environment gives a wrong pair in the second node, so the file must win.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_store_s3_signs_with_the_file_key() {
    let rig = rig(0, |s3| node::registered(s3, "bkt", ID, SECRET)).await;
    for (name, env) in [
        ("signs", no_keys as fn(&str) -> Option<String>),
        ("signs-over-env", wrong_keys),
    ] {
        let mut node = flags(&rig, name, &bucket(&rig), env);
        node.server.cold_store_credentials_file =
            Some(key_file(&format!("{name}-key"), ID, SECRET, 0o600));
        let half = started(&node).await.expect("starts on the key file");
        round_trip(&half, "written on the key file's pair").await;
    }
}

/// Catches a key file with a wrong mode, or a symbolic link, let through, and one refused
/// without the flag and the path in the message, or with the secret in it. A refusal is
/// not retried (exit 2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_file_the_node_must_not_use_refuses_its_start_naming_flag_and_path() {
    let rig = rig(0, |s3| node::registered(s3, "bkt", ID, SECRET)).await;
    let wide = key_file("wide-key", ID, SECRET, 0o644);
    let target = key_file("link-target-key", ID, SECRET, 0o600);
    let link = dir("link-key").join("cold.json");
    std::os::unix::fs::symlink(&target, &link).expect("link");
    for (name, path) in [("wide", wide), ("link", link)] {
        let mut node = flags(&rig, name, &bucket(&rig), no_keys);
        node.server.cold_store_credentials_file = Some(path.clone());
        let Err(error) = started(&node).await else {
            panic!("{name}: a node started on a key file it must refuse");
        };
        let said = error.to_string();
        assert!(
            said.contains("--cold-store-credentials-file"),
            "{name}: {said}"
        );
        assert!(said.contains(&path.display().to_string()), "{name}: {said}");
        assert!(!said.contains(SECRET), "{name}: {said}");
        assert_eq!(class(&*error), StartFailure::Refused, "{name}: {said}");
    }
}
