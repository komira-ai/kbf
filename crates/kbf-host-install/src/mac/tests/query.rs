//! `query` as the root process answers it.

use std::fs;

use super::fx::{Fx, NEW, NEW_VERSION, OLD, OLD_VERSION, mkdir, write};
use crate::mac::layout::APPLY_LABEL;
use crate::mac::status::{self, ApplyState};

fn query(fx: &Fx, sim: &super::fx::Sim) -> serde_json::Value {
    let answer = fx.handle(sim, "query");
    assert_eq!(answer.code, 0, "{}", answer.err);
    answer.json()
}

/// The version `current` was installed as, the commit the RUNNING daemon executes, the
/// node's id and whether an install is in progress.
#[test]
fn a_query_says_what_is_installed_and_what_runs() {
    let fx = Fx::new("mac-query");
    let sim = fx.sim();
    assert_eq!(
        query(&fx, &sim),
        serde_json::json!({
            "result": "query",
            "installed_version": OLD_VERSION,
            "running_commit": OLD,
            "node_id": "mac-a",
            "in_progress": false,
            "apply": null
        })
    );
    assert_eq!(fx.audit_lines()[0]["outcome"], "query");
    assert_eq!(
        sim.count("/bin/launchctl bootstrap"),
        0,
        "a query changes nothing"
    );
}

/// The running commit is read from the process, not from the disk: with `current` flipped to
/// a new release and the daemon not yet restarted, the answer is still the old commit.
#[test]
fn the_running_commit_is_not_the_installed_one() {
    let fx = Fx::new("mac-query-running");
    fx.install_release(NEW, NEW_VERSION, &fx.new_binary);
    let sim = fx.sim();
    let json = query(&fx, &sim);
    assert_eq!(json["installed_version"], NEW_VERSION);
    assert_eq!(json["running_commit"], OLD);
    // Not running at all: nothing is claimed.
    sim.loaded.borrow_mut().clear();
    assert_eq!(query(&fx, &sim)["running_commit"], serde_json::Value::Null);
}

/// What cannot be read is null, not a failure: no VERSION, an unusable `current`, no
/// settings file.
#[test]
fn what_is_unknown_is_null() {
    let fx = Fx::new("mac-query-null");
    let sim = fx.sim();
    fs::remove_file(fx.host.release(OLD).join("VERSION")).unwrap();
    fs::remove_file(&fx.host.node_conf).unwrap();
    let json = query(&fx, &sim);
    assert_eq!(json["installed_version"], serde_json::Value::Null);
    assert_eq!(json["node_id"], serde_json::Value::Null);
    fs::remove_file(fx.host.current()).unwrap();
    std::os::unix::fs::symlink("nowhere", fx.host.current()).unwrap();
    assert_eq!(
        query(&fx, &sim)["installed_version"],
        serde_json::Value::Null
    );
}

/// An install is in progress while its apply job runs or while an install holds the lock;
/// the apply's standing is reported as it is, and a lost job as failed.
#[test]
fn a_query_says_how_the_apply_stands() {
    let fx = Fx::new("mac-query-apply");
    let sim = fx.sim();
    mkdir(&fx.host.work, 0o711);
    status::write(&fx.host, &ApplyState::Running).unwrap();
    sim.loaded.borrow_mut().insert(APPLY_LABEL.to_owned());
    sim.apply_running.set(true);
    let json = query(&fx, &sim);
    assert_eq!(
        (json["in_progress"].as_bool(), json["apply"].as_str()),
        (Some(true), Some("running"))
    );
    sim.apply_running.set(false);
    let json = query(&fx, &sim);
    assert_eq!(
        (json["in_progress"].as_bool(), json["apply"].as_str()),
        (Some(false), Some("failed:apply_lost"))
    );
    status::write(
        &fx.host,
        &ApplyState::Failed("not_welcomed:rolled_back".into()),
    )
    .unwrap();
    assert_eq!(query(&fx, &sim)["apply"], "failed:not_welcomed:rolled_back");
    status::write(&fx.host, &ApplyState::Done).unwrap();
    assert_eq!(query(&fx, &sim)["apply"], "done");
    let _held = crate::host::take_lock(&fx.host.lock_file()).unwrap();
    assert_eq!(query(&fx, &sim)["in_progress"], true);
    // Between `running` and the job's load, an install holds the lock: not a lost apply.
    status::write(&fx.host, &ApplyState::Running).unwrap();
    sim.loaded.borrow_mut().clear();
    let json = query(&fx, &sim);
    assert_eq!(
        (json["in_progress"].as_bool(), json["apply"].as_str()),
        (Some(true), Some("running"))
    );
    // A failure that is not a loss is reported as it is, lock or not.
    status::write(
        &fx.host,
        &ApplyState::Failed("bootstrap_failed:rolled_back".into()),
    )
    .unwrap();
    assert_eq!(
        query(&fx, &sim)["apply"],
        "failed:bootstrap_failed:rolled_back"
    );
}

/// The node's id is `NODE_ID` of the settings; a file that is not root's gives none.
#[test]
fn the_node_id_is_the_settings_node_id() {
    let fx = Fx::new("mac-query-node");
    let sim = fx.sim();
    write(&fx.host.node_conf, 0o644, b"NODE_ID=mac-z\n");
    assert_eq!(query(&fx, &sim)["node_id"], "mac-z");
    write(&fx.host.node_conf, 0o666, b"NODE_ID=mac-z\n");
    assert_eq!(query(&fx, &sim)["node_id"], serde_json::Value::Null);
}
