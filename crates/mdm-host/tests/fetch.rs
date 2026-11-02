//! `fetch` on the fake host and a fake network.

mod common;

use common::{FakeNet, kit, with_env};
use host_step::Mode;
use host_step::fake::FakeHost;
use mdm_core::pins::Tool;
use mdm_host::MdmError;
use mdm_host::fetch::{Fetch, run};

const DIR: &str = "/srv/op/archives";

fn go(host: &FakeHost, net: &FakeNet, only: &[Tool], mode: Mode) -> (Result<(), MdmError>, String) {
    let kit = kit();
    let mut out = Vec::new();
    let f = Fetch {
        dir: DIR.to_owned(),
        only: only.to_vec(),
    };
    let result = with_env(host, net, |env| run(env, &f, &kit.pins, mode, &mut out));
    (result, String::from_utf8(out).unwrap())
}

/// Catches a dry run that downloads or writes: it must print each download as a `DRY-RUN:`
/// line and touch neither the network nor the disk.
#[test]
fn a_dry_run_prints_the_downloads_and_changes_nothing() {
    let (host, net) = (FakeHost::new(1000), FakeNet::new(kit().urls));
    let (result, shown) = go(&host, &net, &[], Mode::DryRun);
    result.unwrap();
    assert_eq!(
        shown
            .matches("DRY-RUN: download https://downloads.example.org/")
            .count(),
        6
    );
    assert!(shown.ends_with("dry run: nothing changed; re-run with --apply to carry it out\n"));
    assert!(net.asked.borrow().is_empty());
    assert!(host.paths().is_empty());
}

/// Catches an apply that writes an archive with the wrong mode or owner, and a rerun that
/// downloads again: the second run keeps what is there and asks the network for nothing.
#[test]
fn apply_downloads_checks_and_writes_then_a_rerun_keeps_them() {
    let kit = kit();
    let (host, net) = (FakeHost::new(1000), FakeNet::new(kit.urls.clone()));
    let (result, shown) = go(&host, &net, &[], Mode::Apply);
    result.unwrap();
    assert_eq!(net.asked.borrow().len(), 6);
    for (file, bytes) in &kit.archives {
        let node = host.node(&format!("{DIR}/{file}")).unwrap();
        assert_eq!(
            (node.mode, node.uid, node.gid, &node.bytes),
            (0o644, 1000, 1000, bytes)
        );
        assert!(shown.contains(&format!("ok: {DIR}/{file}\n")));
    }
    let (result, shown) = go(&host, &net, &[], Mode::Apply);
    result.unwrap();
    assert_eq!(net.asked.borrow().len(), 6);
    assert_eq!(shown.matches("kept: ").count(), 6);
    assert!(shown.ends_with("nothing to do\n"), "{shown}");
    // A dry run over what is kept has nothing to do either, and no dry-run note.
    let (result, shown) = go(&host, &net, &[], Mode::DryRun);
    result.unwrap();
    assert!(
        shown.ends_with("nothing to do\n") && !shown.contains("dry run:"),
        "{shown}"
    );
}

/// Catches `--only` that fetches every tool, or none: only the named tool is planned.
#[test]
fn only_limits_the_tools() {
    let (host, net) = (FakeHost::new(1000), FakeNet::new(kit().urls));
    let (result, shown) = go(&host, &net, &[Tool::Caddy, Tool::Lego], Mode::DryRun);
    result.unwrap();
    assert_eq!(shown.matches("fetch: ").count(), 2);
    assert!(shown.contains("fetch: caddy 1.0.0 from"));
    assert!(!shown.contains("fetch: nanohub"));
}

/// Catches the refusals: root (downloads are never root's), a relative directory, and a
/// directory in a git work tree. Each exits 2 and changes nothing.
#[test]
fn root_a_relative_dir_and_a_git_tree_are_refused() {
    let net = FakeNet::new(kit().urls);
    let (result, _) = go(&FakeHost::new(0), &net, &[], Mode::Apply);
    let err = result.unwrap_err();
    assert!(matches!(err, MdmError::FetchAsRoot));
    assert_eq!(err.exit_code(), 2);

    let host = FakeHost::new(1000);
    let kit = kit();
    let f = Fetch {
        dir: "archives".to_owned(),
        only: vec![],
    };
    let err = with_env(&host, &net, |env| {
        run(env, &f, &kit.pins, Mode::Apply, &mut Vec::new())
    })
    .unwrap_err();
    assert!(matches!(err, MdmError::Path(_)), "{err}");
    assert_eq!(err.exit_code(), 2);

    let host = FakeHost::new(1000);
    host.put_dir("/srv/.git", 0o755, (1000, 1000));
    let (result, _) = go(&host, &net, &[], Mode::Apply);
    let err = result.unwrap_err();
    assert_eq!(
        err.to_string(),
        "--dir /srv/op/archives is inside a git work tree (/srv/.git exists): keep archives and keys outside every repository"
    );
    assert!(net.asked.borrow().is_empty());
}

/// Catches a kept archive that is trusted without its hash: one that is not the pin is
/// refused before anything is downloaded.
#[test]
fn a_kept_archive_that_is_not_the_pin_is_refused() {
    let kit = kit();
    let (host, net) = (FakeHost::new(1000), FakeNet::new(kit.urls.clone()));
    host.put_file(
        &format!("{DIR}/caddy-v1.0.0.tar.gz"),
        0o644,
        (1000, 1000),
        b"tampered",
    );
    let (result, _) = go(&host, &net, &[], Mode::Apply);
    let err = result.unwrap_err();
    assert!(matches!(err, MdmError::KeptDiffers { .. }), "{err}");
    assert!(err.to_string().contains("delete it and re-run"));
    assert_eq!(err.exit_code(), 2);
    assert!(net.asked.borrow().is_empty());
}

/// Catches a download that is installed although it is not the pin: it is kept beside the
/// name for inspection, the archive is not written, and the run fails (exit 1).
#[test]
fn a_download_that_is_not_the_pin_is_kept_as_rejected() {
    let kit = kit();
    let mut urls = kit.urls.clone();
    let url = "https://downloads.example.org/caddy/v1.0.0/caddy-v1.0.0.tar.gz";
    urls.insert(url.to_owned(), b"evil".to_vec());
    let (host, net) = (FakeHost::new(1000), FakeNet::new(urls));
    host.put_dir(DIR, 0o700, (1000, 1000));
    let (result, _) = go(&host, &net, &[Tool::Caddy], Mode::Apply);
    let err = result.unwrap_err();
    assert!(matches!(err, MdmError::Rejected { .. }), "{err}");
    assert_eq!(err.exit_code(), 1);
    assert!(err.to_string().contains(&format!(
        "kept as {DIR}/caddy-v1.0.0.tar.gz.rejected for inspection"
    )));
    assert!(host.node(&format!("{DIR}/caddy-v1.0.0.tar.gz")).is_none());
    assert_eq!(
        host.node(&format!("{DIR}/caddy-v1.0.0.tar.gz.rejected"))
            .unwrap()
            .bytes,
        b"evil"
    );
    // The directory that was there keeps its mode.
    assert_eq!(host.node(DIR).unwrap().mode, 0o700);
}

/// Catches a failed download reported as success, or one that leaves a directory behind.
#[test]
fn a_failed_download_fails_the_run_and_writes_nothing() {
    let (host, net) = (FakeHost::new(1000), FakeNet::new(Default::default()));
    let (result, _) = go(&host, &net, &[Tool::Lego], Mode::Apply);
    let err = result.unwrap_err();
    assert!(matches!(err, MdmError::Download { .. }), "{err}");
    assert_eq!(err.exit_code(), 1);
    assert!(err.to_string().ends_with("failed: answered 404"));
    assert!(host.paths().is_empty());
}

/// Catches a git work tree hidden behind a symbolic link: `--dir` is resolved first, so a link
/// into a repository is refused (and names the directory given), while a link elsewhere is
/// followed and the archives land at its target.
#[test]
fn a_dir_that_links_into_a_git_tree_is_refused_and_a_link_elsewhere_is_followed() {
    let kit = kit();
    let net = FakeNet::new(kit.urls.clone());
    let host = FakeHost::new(1000);
    host.put_dir("/repo/.git", 0o755, (1000, 1000));
    host.put_symlink(DIR, "/repo/sub/archives");
    let (result, _) = go(&host, &net, &[], Mode::Apply);
    let err = result.unwrap_err();
    assert!(matches!(err, MdmError::InGitTree { .. }), "{err}");
    assert!(
        err.to_string()
            .starts_with("--dir /srv/op/archives is inside a git work tree (/repo/.git exists)"),
        "{err}"
    );
    assert!(net.asked.borrow().is_empty());

    let host = FakeHost::new(1000);
    host.put_symlink(DIR, "/data/archives");
    let (result, _) = go(&host, &net, &[Tool::Caddy], Mode::Apply);
    result.unwrap();
    assert!(host.node("/data/archives/caddy-v1.0.0.tar.gz").is_some());
}
