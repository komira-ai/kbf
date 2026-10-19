//! Failure paths of the deploy that the scripted farm's good answers do not reach: a call
//! that cannot be made, an answer that is not what was asked for, and the readers of the
//! API's JSON given what a half-working API sends.

use std::fs::File;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::deploy::api::parse_reply;

fn broken(call: &'static str, how: Breaks, after_desired: bool) -> Broken {
    let b = Broken::new(call, how);
    if after_desired { b.after_desired() } else { b }
}

/// Proves each call the deploy makes before it touches the frontend, when it fails, ends
/// the deploy as an API error (exit 1) naming the step and applying nothing further: the
/// reads of the baseline and of the update status. Catches a deploy that goes on after an
/// answer it did not get, or ends with another reason.
///
/// Mutants planted: the baseline `members` error ignored; the status error ignored.
#[test]
fn a_call_that_fails_before_the_update_ends_the_deploy_as_an_api_error() {
    for (call, how) in [
        ("GET /v1/update", Breaks::Io),
        ("GET /v1/update", Breaks::Body("nope")),
        ("GET /v1/members", Breaks::Io),
        ("GET /v1/update/frontend", Breaks::Io),
        ("GET /v1/update/frontend", Breaks::Status(500)),
        ("GET /v1/update/frontend", Breaks::Body("nope")),
    ] {
        let mut f = farm(two_nodes());
        f.broken.push(Broken::new(call, how));
        let s = go(&f);
        assert_eq!(
            (s.exit, s.reason.as_str()),
            (1, "api_error"),
            "{call}: {}",
            s.line()
        );
        assert_eq!(f.requests("POST"), 0, "{call}");
    }
}

/// Proves an upload that cannot be sent at all (not one the store refuses) fails the deploy
/// as an upload failure. Catches an upload error that is read as "uploaded".
///
/// Mutants planted: the send error of an upload ignored.
#[test]
fn an_upload_that_cannot_be_sent_is_an_upload_failure() {
    let mut f = farm(two_nodes());
    f.broken
        .push(broken("PUT /v1/releases/*", Breaks::Io, false));
    let bundle = tarball(&[(&format!("kbf-{TARGET}-linux-x86_64.tar.gz"), b"t")]);
    let mut input: &[u8] = &bundle;
    let s = run_with(&f, &opts(true), SmokeResult::Pass, Some(&mut input));
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "upload_failed"),
        "{}",
        s.line()
    );
}

/// Proves a nodes list that cannot be read while waiting is not a confirmation: the deploy
/// times out. Catches an unreadable list read as every node confirmed.
///
/// Mutants planted: an unreadable nodes list read as an empty list that confirms everyone.
#[test]
fn an_unreadable_nodes_list_times_out() {
    let mut f = farm(two_nodes());
    f.broken.push(broken("GET /v1/nodes", Breaks::Io, true));
    let mut o = opts(true);
    o.node_timeout = Duration::from_secs(600);
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "node_not_confirmed"),
        "{}",
        s.line()
    );
}

/// Proves a farm whose every node may be offline and is not listed is no roll that has
/// finished: an empty list confirms nothing, and the deploy times out with no node to name.
/// Catches an `all()` over nodes that are all offline passing on an empty list.
///
/// Mutants planted: the `!list.is_empty()` check dropped.
#[test]
fn nodes_that_may_all_be_offline_and_are_not_listed_confirm_nothing() {
    let unlisted = |id: &'static str| {
        let mut n = node(id, "linux-x86_64", Fate::Confirm(100));
        n.relisted_after = Some(1_000_000);
        n
    };
    let f = farm(vec![unlisted("a-store"), unlisted("b-store")]);
    let mut o = opts(true);
    o.allow_offline = vec!["a-store".to_owned(), "b-store".to_owned()];
    o.node_timeout = Duration::from_secs(60);
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "node_not_confirmed"),
        "{}",
        s.line()
    );
    assert!(
        s.detail.starts_with("no node listed: timed_out"),
        "{}",
        s.detail
    );
    let by: Vec<_> = s.nodes.iter().map(|n| n.state.as_str()).collect();
    assert_eq!(by, ["offline", "offline"]);
}

/// Proves the post-check's node reader wants every one of: a server on the commit, a
/// node, connected, serving, up to date, and on the commit. Catches a reader that passes a
/// node missing any one of them.
///
/// Mutants planted: each of the five conjuncts dropped in turn.
#[test]
fn the_post_check_wants_every_node_connected_serving_up_to_date_and_on_the_commit() {
    let f = farm(vec![node("n", "linux-x86_64", Fate::Confirm(0))]);
    let at = |mutate: &dyn Fn(&mut Value)| {
        let mut n = f.node_json(&f.nodes[0]);
        n["update"]["running_commit"] = json!(TARGET);
        n["update"]["up_to_date"] = json!(true);
        mutate(&mut n);
        json!({"server": {"commit": TARGET}, "nodes": [n]})
    };
    assert!(post::nodes_on_commit(&at(&|_| {}), TARGET));
    assert!(!post::nodes_on_commit(
        &at(&|n| n["connected"] = json!(false)),
        TARGET
    ));
    assert!(!post::nodes_on_commit(
        &at(&|n| n["placement"]["state"] = json!("cordoned")),
        TARGET
    ));
    assert!(!post::nodes_on_commit(
        &at(&|n| n["update"]["up_to_date"] = json!(false)),
        TARGET
    ));
    assert!(!post::nodes_on_commit(
        &at(&|n| n["update"]["up_to_date"] = Value::Null),
        TARGET
    ));
    assert!(!post::nodes_on_commit(
        &at(&|n| n["update"]["running_commit"] = json!(OLD)),
        TARGET
    ));
    let mut other_server = at(&|_| {});
    other_server["server"]["commit"] = json!(OLD);
    assert!(!post::nodes_on_commit(&other_server, TARGET));
    assert!(!post::nodes_on_commit(
        &json!({"server": {"commit": TARGET}, "nodes": []}),
        TARGET
    ));
}

/// Proves a platform needs components, every one attested, to be complete, and that a
/// commit shorter than twelve digits is no running commit. Catches a release with no
/// components called complete and a short prefix taken for a commit.
///
/// Mutants planted: the non-empty check dropped; the length floor dropped.
#[test]
fn a_platform_without_attested_components_is_not_complete() {
    let releases = |components: Value| {
        json!({"releases": [{"commit": TARGET, "platforms": [
            {"platform": "linux-x86_64", "complete": true, "components": components}]}]})
    };
    assert_eq!(
        nodes::platform_state(
            &releases(json!([{"attested": true}])),
            TARGET,
            "linux-x86_64"
        ),
        "complete"
    );
    for components in [
        json!([]),
        json!([{"attested": false}]),
        json!([{}]),
        Value::Null,
    ] {
        assert_eq!(
            nodes::platform_state(&releases(components), TARGET, "linux-x86_64"),
            "incomplete"
        );
    }
    assert_eq!(
        nodes::platform_state(&releases(json!([])), TARGET, "darwin-arm64"),
        "missing"
    );
    let unfinished = json!({"releases": [{"commit": TARGET, "platforms": [
        {"platform": "linux-x86_64", "complete": false, "components": [{"attested": true}]}]}]});
    assert_eq!(
        nodes::platform_state(&unfinished, TARGET, "linux-x86_64"),
        "incomplete"
    );
    assert!(nodes::same_commit(&TARGET[..12], TARGET));
    assert!(!nodes::same_commit(&TARGET[..11], TARGET));
}

fn entry_tar(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    build(&mut b);
    b.into_inner().unwrap()
}

/// Proves the bundle reader steps over what is not a file (a directory entry) and reports a
/// tar cut short in the middle of a file instead of uploading half of it. Catches a bundle
/// that refuses a tar for its directory entries, and one that takes a truncated read for a
/// whole file.
///
/// Mutants planted: the not-a-file skip turned into a refusal; the read error ignored.
#[test]
fn the_bundle_skips_directories_and_refuses_a_truncated_file() {
    let commit = c(TARGET);
    let name = format!("kbf-{TARGET}-linux-arm64.tar.gz");
    let with_dir = entry_tar(|b| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_size(0);
        h.set_mode(0o755);
        h.set_cksum();
        b.append_data(&mut h, "bundle/", io::empty()).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, format!("bundle/{name}"), &b"x"[..])
            .unwrap();
    });
    let got = bundle::read(&with_dir[..], &commit).expect("the directory is skipped");
    assert_eq!(got.len(), 1);

    let whole = entry_tar(|b| {
        let mut h = tar::Header::new_gnu();
        h.set_size(4096);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, &name, &[7u8; 4096][..]).unwrap();
    });
    /// A source that fails where the tar was cut, as a pipe does that is closed on the writer.
    struct Cut<'a>(&'a [u8]);
    impl io::Read for Cut<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Err(io::Error::other("the pipe broke"));
            }
            let n = self.0.len().min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }
    let err = bundle::read(Cut(&whole[..600]), &commit).expect_err("cut short");
    assert!(err.contains(&name), "{err}");
}

/// Proves the lock is refused by a path that has no directory to open, is taken once, and
/// that an error from the lock call itself is passed on while "another holds it" is `None`.
/// Catches a lock that reports held for any failure, or success for a failed call.
///
/// Mutants planted: the lock error mapped to `None`; the open of a path with no parent
/// skipped.
#[test]
fn the_lock_reports_a_failed_call_apart_from_a_held_lock() {
    assert!(lock(Path::new("/")).is_err());
    let (_, dir) = crate::testkit::host_root("deploy-lock-take");
    let file = || File::create(dir.join("f")).unwrap();
    assert!(take(file(), |_| Ok(())).unwrap().is_some());
    assert!(
        take(file(), |_| Err(std::fs::TryLockError::WouldBlock))
            .unwrap()
            .is_none()
    );
    let err = take(file(), |_| {
        Err(std::fs::TryLockError::Error(io::Error::other("no locks")))
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "no locks");
}

/// Proves the system clock tells a plausible time (after 2026) and sleeps. Catches a clock
/// stuck at zero, which would date every summary 1970.
///
/// Mutants planted: `now` returning 0.
#[test]
fn the_system_clock_tells_the_time_and_sleeps() {
    let clock = SystemClock;
    assert!(clock.now() > 1_790_000_000);
    let before = std::time::Instant::now();
    clock.sleep(Duration::from_millis(5));
    assert!(before.elapsed() >= Duration::from_millis(5));
}

/// Proves a node that drops off while the deploy waits for it, and was not allowed to be
/// offline, is waited for like any unconfirmed node (and the deploy times out), and a farm
/// that is not ready after everything else is fine fails the post-check on that alone.
/// Catches an offline node taken for a confirmed one, and a readiness failure lost among
/// other checks.
///
/// Mutants planted: the allow-list test dropped (any offline node is skipped); `ready`
/// left out of the verdict.
#[test]
fn an_unallowed_offline_node_is_waited_for_and_readiness_alone_fails_the_post_check() {
    let mut f = farm(two_nodes());
    f.broken.push(broken(
        "GET /v1/nodes",
        Breaks::Body(
            r#"{"nodes":[{"node_id":"a-store","connected":false,"update":{"state":"staged"}}]}"#,
        ),
        true,
    ));
    let mut o = opts(true);
    o.node_timeout = Duration::from_secs(600);
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "node_not_confirmed"),
        "{}",
        s.line()
    );

    let mut f = farm(two_nodes());
    f.ready = false;
    let s = go(&f);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "postcheck_failed"),
        "{}",
        s.line()
    );
    assert_eq!(s.post.readyz, Some(false));
    assert_eq!(s.post.members_active, Some(true));
    assert_eq!(s.post.nodes_on_commit, Some(true));
}

/// Proves an answer whose head is not text is an error, not a status read from garbage.
/// Catches a reply parser that takes an answer from a server that is not speaking HTTP.
///
/// Mutants planted: the head read lossily.
#[test]
fn a_reply_whose_head_is_not_text_is_refused() {
    let err = parse_reply(b"HTTP/1.1 200 OK\r\nX: \xff\xfe\r\n\r\nbody").unwrap_err();
    assert!(err.to_string().contains("headers are not text"), "{err}");
}

/// Proves the deploy lock is free again as soon as it is dropped, even when a child
/// forked while it was held still has a copy of the descriptor: an `flock` belongs to
/// the open file description, which that copy keeps alive, so a lock released by the
/// close alone stays held until the child execs.
/// Catches a `Lock` that relies on the close to release the `flock`.
///
/// Mutant seen red: no `Drop` for `Lock` (the second take finds it held).
#[test]
fn the_lock_is_free_at_once_when_a_forked_child_has_a_copy() {
    let (_, dir) = crate::testkit::host_root("deploy-lock-forked");
    let path = dir.join("run/lock");
    let held = lock(&path).unwrap().expect("taken");
    let child = kbf_testkit::forked::ForkedChild::before_exec();
    drop(held);
    let again = lock(&path).unwrap();
    child.finish();
    assert!(again.is_some(), "the lock outlived its drop");
}
