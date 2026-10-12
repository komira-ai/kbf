use std::collections::BTreeMap;
use std::fs;

use super::*;
use crate::testkit::{TlsServer, http, tls_server};
use kbf_host::testkit::scratch;

/// What the test server holds: path -> bytes.
fn files() -> BTreeMap<String, Vec<u8>> {
    Tool::ALL
        .into_iter()
        .map(|t| {
            (
                format!("/r/v1.0/{t}.zip"),
                format!("{t} archive").into_bytes(),
            )
        })
        .collect()
}

/// A server for `served` (path -> bytes; any other path is 404), and pins for every
/// tool at it whose hashes are those of `pinned`.
fn setup(
    served: BTreeMap<String, Vec<u8>>,
    pinned: &BTreeMap<String, Vec<u8>>,
) -> (TlsServer, Pins) {
    let server = tls_server(&["127.0.0.1"], move |path| match served.get(path) {
        Some(bytes) => http("200 OK", bytes),
        None => http("404 Not Found", b"no"),
    });
    let port = server.addr.port();
    let text: String = Tool::ALL
        .into_iter()
        .map(|t| {
            let path = format!("/r/v1.0/{t}.zip");
            let sum = hex::encode(Sha256::digest(&pinned[&path]));
            format!("{t} 1.0 https://127.0.0.1:{port}{path} {sum}\n")
        })
        .collect();
    (server, Pins::parse(&text).expect("valid pins"))
}

/// A fresh directory outside every git work tree (the system's temporary directory).
fn outside(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-deploy-fetch.{}.{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn fetch(
    dir: &Path,
    only: &[Tool],
    server: &TlsServer,
    pins: &Pins,
    mode: Mode,
) -> (Result<(), MdmError>, String) {
    let f = Fetch {
        dir: dir.to_owned(),
        only: only.to_vec(),
    };
    let mut out = Vec::new();
    let got = run(&f, pins, 501, &server.client(), mode, &mut out);
    (got, String::from_utf8_lossy(&out).into_owned())
}

/// Catches a fetch that downloads in a dry run, leaves a `.part` at the final path,
/// stores bytes other than the served ones, or downloads a kept archive again.
#[test]
fn a_dry_run_downloads_nothing_and_an_apply_fetches_each_once() {
    let (server, pins) = setup(files(), &files());
    let dir = outside("apply");
    let (got, text) = fetch(&dir, &[], &server, &pins, Mode::DryRun);
    assert!(got.is_ok(), "{got:?}");
    assert_eq!(
        text.matches("DRY-RUN: download https://127.0.0.1:").count(),
        6,
        "{text}"
    );
    assert!(
        text.contains("fetch: caddy 1.0 from https://127.0.0.1:"),
        "{text}"
    );
    assert!(text.ends_with(&format!("{DRY_RUN_NOTE}\n")), "{text}");
    assert!(server.paths.lock().expect("lock").is_empty());
    assert!(!dir.exists());

    let (got, text) = fetch(&dir, &[], &server, &pins, Mode::Apply);
    assert!(got.is_ok(), "{got:?}: {text}");
    assert_eq!(text.matches("ok: ").count(), 6, "{text}");
    for (path, bytes) in files() {
        let name = path.rsplit('/').next().unwrap_or_default();
        assert_eq!(fs::read(dir.join(name)).expect("fetched"), bytes, "{name}");
        assert!(!dir.join(format!("{name}.part")).exists());
    }
    let (got, text) = fetch(&dir, &[], &server, &pins, Mode::Apply);
    assert!(got.is_ok(), "{got:?}");
    assert_eq!(text.matches("kept: ").count(), 6, "{text}");
    assert!(text.ends_with(&format!("{NOTHING_TO_DO}\n")), "{text}");
    assert_eq!(server.paths.lock().expect("lock").len(), 6);
    let _ = fs::remove_dir_all(&dir);
}

/// Catches `--only` ignored (every archive fetched) or applied to the wrong tool.
#[test]
fn only_fetches_the_named_tools() {
    let (server, pins) = setup(files(), &files());
    let dir = outside("only");
    let (got, _) = fetch(&dir, &[Tool::Lego], &server, &pins, Mode::Apply);
    assert!(got.is_ok(), "{got:?}");
    let names: Vec<String> = fs::read_dir(&dir)
        .expect("dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["lego.zip"]);
    let _ = fs::remove_dir_all(&dir);
}

/// Catches a download whose sha256 is not the pin left at its final path (install
/// would refuse it later, but `fetch` said `ok`), or deleted (nothing to inspect).
#[test]
fn a_download_that_is_not_the_pin_is_kept_aside_and_fails() {
    let mut served = files();
    served.insert("/r/v1.0/caddy.zip".to_owned(), b"tampered".to_vec());
    let (server, pins) = setup(served, &files());
    let dir = outside("rejected");
    let (got, _) = fetch(&dir, &[Tool::Caddy], &server, &pins, Mode::Apply);
    let err = got.err();
    assert!(
        matches!(
            err,
            Some(MdmError::Rejected {
                tool: Tool::Caddy,
                ..
            })
        ),
        "{err:?}"
    );
    assert_eq!(err.map(|e| e.exit_code()), Some(1));
    assert_eq!(
        fs::read(dir.join("caddy.zip.rejected")).expect("kept"),
        b"tampered"
    );
    assert!(!dir.join("caddy.zip").exists());
    assert!(!dir.join("caddy.zip.part").exists());
    let _ = fs::remove_dir_all(&dir);
}

/// Catches an error page saved as the archive, or a failed download that leaves its
/// part behind.
#[test]
fn a_failed_download_leaves_nothing() {
    let mut served = files();
    served.remove("/r/v1.0/nanohub.zip");
    let (server, pins) = setup(served, &files());
    let dir = outside("failed");
    let (got, _) = fetch(&dir, &[Tool::Nanohub], &server, &pins, Mode::Apply);
    assert!(
        matches!(got, Err(MdmError::Status { status: 404, .. })),
        "{got:?}"
    );
    assert!(!dir.join("nanohub.zip.part").exists());
    assert!(!dir.join("nanohub.zip").exists());
    // A server that closes without an answer.
    let gone = tls_server(&["127.0.0.1"], |_| Vec::new());
    let (got, _) = fetch(&dir, &[Tool::Nanohub], &gone, &pins, Mode::Apply);
    assert!(matches!(got, Err(MdmError::Https(_))), "{got:?}");
    assert!(!dir.join("nanohub.zip.part").exists());
    let _ = fs::remove_dir_all(&dir);
}

/// Catches a part left by an interrupted run appended to (the archive would be
/// corrupt) or blocking the next run, and a part path that cannot be cleared reported
/// as anything but an error.
#[test]
fn a_leftover_part_is_replaced_and_one_that_cannot_be_is_an_error() {
    let (server, pins) = setup(files(), &files());
    let dir = outside("leftover");
    fs::create_dir_all(&dir).expect("dir");
    fs::write(dir.join("mdmctl.zip.part"), b"half").expect("part");
    let (got, _) = fetch(&dir, &[Tool::Mdmctl], &server, &pins, Mode::Apply);
    assert!(got.is_ok(), "{got:?}");
    assert_eq!(
        fs::read(dir.join("mdmctl.zip")).expect("fetched"),
        b"mdmctl archive"
    );
    fs::create_dir(dir.join("lego.zip.part")).expect("a directory in the way");
    let (got, _) = fetch(&dir, &[Tool::Lego], &server, &pins, Mode::Apply);
    assert!(matches!(got, Err(MdmError::Io { .. })), "{got:?}");
    let _ = fs::remove_dir_all(&dir);
}

/// Catches a kept archive that differs from its pin taken as fetched (exit 2: the
/// operator must look at it), and one that cannot be read reported as missing.
#[test]
fn a_kept_archive_must_be_the_pin() {
    let (server, pins) = setup(files(), &files());
    let dir = outside("kept");
    fs::create_dir_all(&dir).expect("dir");
    fs::write(dir.join("caddy.zip"), b"old").expect("old");
    let (got, _) = fetch(&dir, &[Tool::Caddy], &server, &pins, Mode::DryRun);
    let err = got.err();
    assert!(matches!(err, Some(MdmError::KeptDiffers { .. })), "{err:?}");
    let text = err.map(|e| (e.exit_code(), e.to_string()));
    assert!(text.is_some_and(|(code, t)| code == 2 && t.contains("delete it and re-run")));
    fs::create_dir(dir.join("lego.zip")).expect("a directory in the way");
    let (got, _) = fetch(&dir, &[Tool::Lego], &server, &pins, Mode::DryRun);
    assert!(matches!(got, Err(MdmError::Io { .. })), "{got:?}");
    let _ = fs::remove_dir_all(&dir);
}

/// Catches a fetch that runs as root, takes a relative directory, or writes into a git
/// work tree (where an archive or key could be committed).
#[test]
fn root_a_relative_directory_and_a_git_tree_are_refused() {
    let (server, pins) = setup(files(), &files());
    let f = Fetch {
        dir: outside("root"),
        only: Vec::new(),
    };
    let as_root = run(
        &f,
        &pins,
        0,
        &server.client(),
        Mode::DryRun,
        &mut Vec::new(),
    );
    assert!(matches!(as_root, Err(MdmError::FetchAsRoot)), "{as_root:?}");
    let (got, _) = fetch(Path::new("archives"), &[], &server, &pins, Mode::DryRun);
    assert!(matches!(got, Err(MdmError::Relative(_))), "{got:?}");
    let tree = scratch("fetch_git");
    fs::create_dir(tree.join(".git")).expect("a work tree");
    let (got, _) = fetch(&tree.join("archives"), &[], &server, &pins, Mode::DryRun);
    let err = got.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(err.contains("inside a git work tree"), "{err}");
}
