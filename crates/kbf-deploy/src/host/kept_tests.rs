//! `host status` and the releases a host keeps: each one with its commit, its version and why
//! it stays. Children of the status tests, which run the command line on a scratch root.

use std::time::{Duration, UNIX_EPOCH};

use kbf_installer::retain::{self, Retained};

use super::*;

const D: &str = "dddddddddddddddddddddddddddddddddddddddd";

/// A host with four releases laid out `secs` after the epoch, newest first: A (current, and on
/// trial), B (previous), C (last good, no version recorded) and D (pinned); and the link.
fn host_with_four_releases(name: &str) -> (Layout, std::path::PathBuf) {
    let (root, dir) = host_root(name);
    Locked::acquire(&root)
        .expect("lock")
        .write(&full_state())
        .expect("write");
    let releases = dir.join("var/lib/kbf-installer/releases");
    for (commit, secs) in [(A, 4_000), (B, 3_000), (C, 2_000), (D, 1_000)] {
        let release = releases.join(commit);
        fs::create_dir(&release).expect("release");
        fs::File::open(&release)
            .expect("open")
            .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .expect("mtime");
    }
    std::os::unix::fs::symlink(A, releases.join("current")).expect("link");
    Retained::update(&retain::path(&root), |r| {
        r.pinned.insert(kbf_installer::testkit::c(D));
        r.last_good = Some(kbf_installer::testkit::c(C));
        r.versions
            .insert(kbf_installer::testkit::c(A), "v-a".to_owned());
        r.versions
            .insert(kbf_installer::testkit::c(B), "v-b".to_owned());
        r.versions
            .insert(kbf_installer::testkit::c(D), "v-d".to_owned());
    })
    .expect("retain.json");
    (root, dir)
}

/// Catches a status that does not say what the host keeps: after the link verdict a count and
/// a line for each release, newest first, with its commit, its version (`unknown` when none
/// was recorded) and each reason it stays; and the same in `--json` as a list of objects
/// with `commit`, `version` and `why`.
///
/// Catches: the lines left out of the text or the list out of the JSON; the versions of other
/// releases; the reasons misattributed (previous for current, last good for pinned); the
/// order by name rather than by age.
#[test]
fn status_lists_the_kept_releases_with_commit_version_and_why() {
    let (_root, dir) = host_with_four_releases("deploy-host-kept");
    let (code, out, err) = status_cli(&dir, &[]);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    assert!(
        out.ends_with(&format!(
            "current link: {A} (matches the state)\n\
             releases kept: 4\n  \
             {A} v-a (current, on_trial)\n  \
             {B} v-b (previous)\n  \
             {C} unknown (last_good)\n  \
             {D} v-d (pinned)\n"
        )),
        "{out}"
    );

    let (code, out, _) = status_cli(&dir, &["--json"]);
    assert_eq!(code, ExitCode::SUCCESS);
    let json: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(
        json["releases"],
        serde_json::json!([
            {"commit": A, "version": "v-a", "why": ["current", "on_trial"]},
            {"commit": B, "version": "v-b", "why": ["previous"]},
            {"commit": C, "version": null, "why": ["last_good"]},
            {"commit": D, "version": "v-d", "why": ["pinned"]},
        ])
    );
}

/// Catches a status that hides a `retain.json` it cannot read behind an empty list: the
/// text says it cannot tell and why, the JSON carries the error in place of the list, the
/// rest of the status is as before, and the exit is the link verdict's (not a failure).
///
/// Catches: the error mapped to an empty list; the error failing the whole status.
#[test]
fn status_says_when_it_cannot_tell_what_is_kept() {
    let (root, dir) = host_with_four_releases("deploy-host-kept-garbled");
    fs::write(retain::path(&root), "{").expect("garble");
    let (code, out, err) = status_cli(&dir, &[]);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    assert!(
        out.contains(&format!("current link: {A} (matches the state)\n")),
        "{out}"
    );
    assert!(
        out.contains("releases kept: cannot tell: ")
            && out.contains("retain.json cannot be parsed"),
        "{out}"
    );
    assert!(!out.contains("releases kept: 0"), "{out}");

    let (code, out, _) = status_cli(&dir, &["--json"]);
    assert_eq!(code, ExitCode::SUCCESS);
    let json: serde_json::Value = serde_json::from_str(&out).expect("json");
    let error = json["releases"]["error"].as_str().expect("an error text");
    assert!(error.contains("retain.json cannot be parsed"), "{error}");
}

/// Catches `host gc` keeping fewer than the floor, or another default than the installer's:
/// with no `--keep` it keeps the four newest others, and `--keep 1` is refused before anything
/// is read.
#[test]
fn gc_keeps_four_by_default_and_refuses_a_count_below_two() {
    let (_root, dir) = host_root("deploy-host-gc-floor");
    let root = [OsStr::new("--root"), dir.as_os_str()];
    let (code, out, _) = host_cli(&[&[OsStr::new("gc")], &root[..]].concat());
    assert_eq!(
        (code, out.as_str()),
        (
            ExitCode::SUCCESS,
            "keeping: none and the 4 newest others; removing: none\nnothing to do\n"
        )
    );
    let low = ["gc", "--keep", "1"].map(OsStr::new);
    let (code, out, err) = host_cli(&[&low[..], &root[..]].concat());
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""));
    assert!(err.contains("at least 2"), "{err}");
}
