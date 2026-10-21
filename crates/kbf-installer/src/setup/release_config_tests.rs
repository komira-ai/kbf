//! Tests of the release keys `setup` writes into `installer.toml` (docs/design/kbf-installer.md
//! section 3): `--release-source bundled`, `--release-ca` and the frontend snapshot flags
//! (`--frontend-worker-url`, `--frontend-ca`, `--frontend-ca-sha256`). `setup` checks and
//! writes the values; installing the release a bundled host carries is `seed_tests.rs`'s,
//! and it does not read the CA files. The helpers are `tests.rs`'s. Each test says the defect it
//! catches and the mutants to plant to see it go red.

use std::path::PathBuf;

use super::*;
use crate::installer_toml::{FrontendPull, InstallerToml};
use crate::record::Sha256Hex;
use crate::setup::release_keys::ReleaseFlags;

/// A digest: 64 lowercase hex characters.
const SHA: &str = "d2a84f4b8b650937ec8f73cd8be2c74add5a911ba64df27458ed8229da804a26";
const CA: &str = "/var/lib/kbf-installer/frontend-ca.pem";
const WORKER: &str = "https://frontend.lan:8981";
const RELEASE_CA: &str = "/var/lib/kbf-installer/release-ca.pem";
/// The file server `raw` gives a frontend or all-in-one host.
const RELEASE_URL: &str = "https://releases.example/kbf";
/// What a bundled node's `installer.toml` is, byte for byte.
const BUNDLED_TOML: &str = include_str!("../../testdata/golden/installer_bundled.toml");

fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

/// The flags of a host that fetches nothing: no URL, no first release to name.
fn bundled(mode: Mode) -> Raw {
    let mut r = raw(mode);
    r.release_source = Some(ReleaseSource::Bundled);
    r.release_url = None;
    r
}

/// The flags of a node whose LAN-pull snapshot is given whole.
fn pulled() -> Raw {
    let mut r = raw(Mode::Daemon);
    r.release = ReleaseFlags {
        frontend_worker_url: some(WORKER),
        frontend_ca: some(CA),
        frontend_ca_sha256: some(SHA),
        ..ReleaseFlags::default()
    };
    r
}

fn refused(r: &Raw) -> String {
    r.validate(Platform::Linux).expect_err("refused").join("; ")
}

/// The `installer.toml` the host holds, parsed by the installer's own closed reader: a file
/// `setup` wrote that this cannot read is a failure of the test that wrote it.
fn written(exec: &FakeExec) -> InstallerToml {
    let file = text(exec, INSTALLER_TOML);
    InstallerToml::parse(&file).unwrap_or_else(|e| panic!("{file}: {e}"))
}

/// `bundled` is a release source on every mode, node or not (a frontend and an all-in-one host
/// take it, where `frontend` is refused to them), and it takes neither `--release-url` (nothing
/// is fetched) nor `--commit` (the release is the one the package carries).
///
/// Catches `bundled` accepted with a place to fetch from, refused on a mode that does not enroll,
/// or taking a commit it will not install.
///
/// Mutants to plant: the `(Frontend | Bundled, Some(_))` arm without `Bundled` (a URL accepted
/// beside it); the commit check without its `Bundled` arm; `Bundled` mapped to `Source::Url`.
#[test]
fn bundled_is_valid_on_every_mode_and_takes_no_url_and_no_commit() {
    for mode in [
        Mode::Storage,
        Mode::Daemon,
        Mode::StorageAndDaemon,
        Mode::Frontend,
        Mode::AllInOne,
    ] {
        let valid = bundled(mode).validate(Platform::Linux).expect("valid");
        assert_eq!(valid.source, Source::Bundled, "{mode:?}");
        let mut with_url = bundled(mode);
        with_url.release_url = some(RELEASE_URL);
        let found = refused(&with_url);
        assert!(
            found.contains("--release-url is only for --release-source url"),
            "{mode:?}: {found}"
        );
        let mut with_commit = bundled(mode);
        with_commit.commit = Some(COMMIT.parse().expect("a commit"));
        let found = refused(&with_commit);
        assert!(
            found.contains("bundled takes no --commit"),
            "{mode:?}: {found}"
        );
    }
}

/// `--release-ca` is the roots a file server's certificate is checked against, an absolute path
/// of plain characters, and it belongs to the `url` source: with the frontend (named or the
/// default of a node) or bundled it is refused, since those never read it.
///
/// Catches a flag accepted under the wrong source and a path rule loosened for it.
///
/// Mutants to plant: the exclusion removed; `absolute_path` skipped; the flag ignored (the
/// valid case then has no `release_ca`).
#[test]
fn the_release_ca_flag_is_an_absolute_path_for_the_url_source_only() {
    let with = |mut r: Raw, ca: &str| {
        r.release.release_ca = some(ca);
        r
    };
    let valid = with(raw(Mode::Frontend), RELEASE_CA)
        .validate(Platform::Linux)
        .expect("valid");
    assert_eq!(valid.release_ca, Some(PathBuf::from(RELEASE_CA)));
    let plain = raw(Mode::Frontend)
        .validate(Platform::Linux)
        .expect("valid");
    assert_eq!(plain.release_ca, None);
    for bad in ["release-ca.pem", "/a/../b", "/a b", "/"] {
        let found = refused(&with(raw(Mode::Frontend), bad));
        assert!(found.contains("--release-ca:"), "{bad}: {found}");
    }
    for r in [
        raw(Mode::Daemon),
        bundled(Mode::Frontend),
        bundled(Mode::Daemon),
    ] {
        let found = refused(&with(r, RELEASE_CA));
        assert!(
            found.contains("--release-ca is for --release-source url"),
            "{found}"
        );
    }
}

/// The snapshot flags give the worker listener, the root-owned copy of its CA and that copy's
/// digest, and come together; they belong to the `frontend` source (a node's default), and each
/// is held to the rule the file's reader applies, in words that name the flag and never repeat
/// the value.
///
/// Catches half a snapshot accepted (a CA with no digest to check it against), a snapshot beside
/// a source that never reads it, a rule dropped from one flag, and a value quoted back.
///
/// Mutants to plant: the `(Some, Some, Some)` arm taking two flags; the source check removed;
/// `FrontendPull::checked` skipping the digest; the address's own message passed through.
#[test]
fn the_snapshot_flags_come_together_and_are_for_the_frontend_source() {
    let want = FrontendPull {
        url: WORKER.to_owned(),
        ca: PathBuf::from(CA),
        ca_sha256: Sha256Hex::try_from(SHA.to_owned()).expect("a digest"),
    };
    let valid = pulled().validate(Platform::Linux).expect("valid");
    assert_eq!(valid.frontend, Some(want));
    let plain = raw(Mode::Daemon).validate(Platform::Linux).expect("valid");
    assert_eq!(plain.frontend, None);

    let one_of_three = [
        ReleaseFlags {
            frontend_worker_url: some(WORKER),
            ..ReleaseFlags::default()
        },
        ReleaseFlags {
            frontend_ca: some(CA),
            ..ReleaseFlags::default()
        },
        ReleaseFlags {
            frontend_ca_sha256: some(SHA),
            ..ReleaseFlags::default()
        },
    ];
    for flags in one_of_three {
        let mut r = raw(Mode::Daemon);
        r.release = flags;
        assert!(refused(&r).contains("come together"));
    }
    let mut no_digest = pulled();
    no_digest.release.frontend_ca_sha256 = None;
    assert!(refused(&no_digest).contains("come together"));

    let mut url = raw(Mode::Daemon);
    url.release_source = Some(ReleaseSource::Url);
    url.release_url = some(RELEASE_URL);
    for mut r in [url, bundled(Mode::Daemon), raw(Mode::Frontend)] {
        r.release = pulled().release;
        let found = refused(&r);
        assert!(
            found.contains("are for --release-source frontend"),
            "{found}"
        );
    }

    let flags = [
        "--frontend-worker-url:",
        "--frontend-ca:",
        "--frontend-ca-sha256:",
    ];
    for (which, bad) in [
        (0, "http://frontend.lan"),
        (0, "https://frontend.lan/x"),
        (0, "https://user:PW@frontend.lan"),
        (1, "frontend-ca.pem"),
        (1, "/a/../b"),
        (2, "ABC"),
        (2, &SHA[..63]),
    ] {
        let mut r = pulled();
        let slot = match which {
            0 => &mut r.release.frontend_worker_url,
            1 => &mut r.release.frontend_ca,
            _ => &mut r.release.frontend_ca_sha256,
        };
        *slot = some(bad);
        let found = refused(&r);
        assert!(found.contains(flags[which]), "{bad}: {found}");
        assert!(!found.contains("PW"), "{found}");
    }
}

/// `setup --release-source bundled` writes `release_source = "bundled"` and nothing that names a
/// place to fetch from (no node identity, which a `frontend` node gets), on a node, a frontend
/// and an all-in-one host, and the file is root's 0644 and reads back through the installer's
/// closed reader. A node's whole file is the golden, byte for byte.
///
/// Catches a bundled file that does not read back, one that still carries the node identity of
/// the frontend source, and a source written as another.
///
/// Mutants to plant: the node identity written whatever the source; `Source::Bundled` rendered
/// as `"url"`.
#[test]
fn setup_writes_bundled_and_it_reads_back_on_every_kind_of_host() {
    for mode in [Mode::Daemon, Mode::Frontend, Mode::AllInOne] {
        let mut exec = released(host(), Platform::Linux);
        go_ok(&FakeOs::linux(), &mut exec, &bundled(mode));
        let file = written(&exec);
        assert_eq!(file.source, Source::Bundled, "{mode:?}");
        assert!(
            file.node_cert.is_none() && file.node_key.is_none(),
            "{mode:?}"
        );
        assert_eq!(file.database.is_some(), mode.keeps_state_in_database());
        assert_eq!(file_meta(&exec, INSTALLER_TOML), (0o644, ROOT), "{mode:?}");
    }
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &bundled(Mode::Daemon));
    assert_eq!(text(&exec, INSTALLER_TOML), BUNDLED_TOML);
}

/// The snapshot flags are written as `frontend_url`, `frontend_ca` and `frontend_ca_sha256`
/// after the node identity, and read back whole.
///
/// Catches a flag that is checked and not written, a key written under another name, and a
/// snapshot dropped by the identity logic of the frontend source.
///
/// Mutants to plant: `settings.frontend` not assigned in `installer_settings`; the url and the
/// CA swapped.
#[test]
fn setup_writes_the_frontend_snapshot_and_it_reads_back() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &pulled());
    let want = format!(
        "{DAEMON_INSTALLER_TOML}frontend_url = \"{WORKER}\"\nfrontend_ca = \"{CA}\"\nfrontend_ca_sha256 = \"{SHA}\"\n"
    );
    assert_eq!(text(&exec, INSTALLER_TOML), want);
    let file = written(&exec);
    assert_eq!(file.source, Source::Frontend);
    assert!(file.frontend.is_some() && file.node_cert.is_some());
}

/// `--release-ca` is written as `release_ca` right after the `release_url` it goes with, and
/// read back.
///
/// Catches the flag checked and not written, and the key written under a source that refuses it.
///
/// Mutants to plant: `settings.release_ca` not assigned in `installer_settings`.
#[test]
fn setup_writes_release_ca_beside_the_url_it_belongs_to() {
    let mut exec = released(host(), Platform::Linux);
    let mut r = raw(Mode::Frontend);
    r.release.release_ca = some(RELEASE_CA);
    go_ok(&FakeOs::linux(), &mut exec, &r);
    assert_eq!(written(&exec).release_ca, Some(PathBuf::from(RELEASE_CA)));
    let file = text(&exec, INSTALLER_TOML);
    let adjacent = format!("release_url = \"{RELEASE_URL}\"\nrelease_ca = \"{RELEASE_CA}\"\n");
    assert!(file.contains(&adjacent), "{file}");
}

/// The flags decide the source's keys, so a rerun with another source leaves none of the old
/// source's keys behind (a file that would not read back, and that the next `setup` would then
/// refuse to rewrite), and the same flags again write nothing.
///
/// Catches a stale `frontend_*` or `release_ca` kept when the source changes (the closed reader
/// refuses the file), and a rewrite on every run.
///
/// Mutants to plant: `settings.frontend` or `settings.release_ca` taken from the file instead of
/// the flags.
#[test]
fn a_changed_source_drops_the_old_sources_keys_and_the_same_flags_write_nothing() {
    let os = FakeOs::linux();
    let mut exec = released(host(), Platform::Linux);
    go_ok(&os, &mut exec, &pulled());
    assert!(written(&exec).frontend.is_some());
    go_ok(&os, &mut exec, &bundled(Mode::Daemon));
    let file = written(&exec);
    assert!(file.frontend.is_none() && file.node_cert.is_none());
    go_ok(&os, &mut exec, &pulled());
    let file = written(&exec);
    assert!(file.frontend.is_some() && file.node_cert.is_some());
    exec.log.clear();
    go_ok(&os, &mut exec, &pulled());
    assert_eq!(write_index(&exec, INSTALLER_TOML), None);

    let mut exec = released(host(), Platform::Linux);
    let mut with_ca = raw(Mode::Frontend);
    with_ca.release.release_ca = some(RELEASE_CA);
    go_ok(&os, &mut exec, &with_ca);
    assert!(written(&exec).release_ca.is_some());
    go_ok(&os, &mut exec, &bundled(Mode::Frontend));
    assert!(written(&exec).release_ca.is_none());
    go_ok(&os, &mut exec, &with_ca);
    go_ok(&os, &mut exec, &raw(Mode::Frontend));
    assert!(written(&exec).release_ca.is_none());
}
