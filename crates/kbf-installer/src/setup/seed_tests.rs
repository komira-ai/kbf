//! `setup --release-source bundled` on a host with no release installs the one the package
//! carries, through the same verify pipeline as a fetched one, between `installer.toml` and
//! `kbf.toml`; a host that has a release installs nothing. The first tests play the downloader
//! with a double (the order of the run); the last run the real one over the seed fetcher.

use kbf_host::{HostExec, Root};

use super::*;
use crate::fixtures::trusted_host_on;
use crate::seed_fixture::Seeded;
use crate::setup::backend::Fetching;

/// What the double records for the install of the package's release.
const SEED: &str = "the package's release";

/// The flags of a host that fetches nothing and names no release.
fn bundled(mode: Mode) -> Raw {
    let mut r = raw(mode);
    r.release_source = Some(ReleaseSource::Bundled);
    r.release_url = None;
    r
}

/// A bundled host with no release has the one its package carries installed between the
/// settings and the config, on a node, a frontend and an all-in-one host, and the mode's units
/// start because it is there. The downloader is asked once, for the package's release, and
/// never for a first release by commit (a bundled host names none).
///
/// Catches a bundled host set up with no release (its units never start, and nothing ever
/// installs one: it fetches nothing), the install made before `installer.toml` says where from
/// or after `kbf.toml`, and the package's release fetched by a commit nobody gave.
///
/// Mutants to plant: the arm for `None`, `false` and a bundled source removed; the install made
/// in a dry run; `have_release` left false after it (the units are not enabled).
#[test]
fn a_bundled_host_with_no_release_installs_the_packages_between_settings_and_config() {
    for mode in [Mode::Daemon, Mode::Frontend, Mode::AllInOne] {
        let installs = Installs(RefCell::default());
        let mut exec = host();
        let out = go_with(
            &FakeOs::linux(),
            &mut exec,
            &installs,
            &bundled(mode),
            APPLY,
        )
        .expect("setup");
        assert_eq!(*installs.0.borrow(), [SEED], "{mode:?}");
        assert!(!out.contains("no release is installed yet"), "{mode:?}");
        if mode == Mode::Daemon {
            // The order the output shows: the settings, the install, then the config; and the
            // unit starts because a release is there.
            let at = |needle: &str| {
                out.find(needle)
                    .unwrap_or_else(|| panic!("{needle}: {out}"))
            };
            let install = at("+ install and verify the release the package carries");
            assert!(
                at(INSTALLER_TOML) < install && install < at(KBF_TOML),
                "{out}"
            );
            let start = "run /usr/bin/systemctl enable --now -- kbf-daemon.service";
            assert!(cmds(&exec).contains(&start.to_owned()), "{out}");
        }
    }
}

/// A bundled host that has a release installs nothing (setup installs the first release only),
/// and a dry run names the install without doing it and plans the units as starting.
///
/// Catches the package's release installed over a running host's, and a dry run that installs.
///
/// Mutants to plant: the arm taken whatever `have_release` is; the install made when `apply` is
/// false.
#[test]
fn a_bundled_host_with_a_release_installs_nothing_and_a_dry_run_only_names_the_install() {
    let installs = Installs(RefCell::default());
    let mut exec = released(host(), Platform::Linux);
    let out = go_with(
        &FakeOs::linux(),
        &mut exec,
        &installs,
        &bundled(Mode::Daemon),
        APPLY,
    )
    .expect("setup");
    assert!(installs.0.borrow().is_empty());
    assert!(!out.contains("the release the package carries"), "{out}");

    let mut exec = host();
    let dry = Options {
        apply: false,
        no_start: false,
    };
    let out = go_with(
        &FakeOs::linux(),
        &mut exec,
        &installs,
        &bundled(Mode::Daemon),
        dry,
    )
    .expect("dry run");
    assert!(installs.0.borrow().is_empty());
    assert!(
        out.contains("DRY-RUN: install and verify the release the package carries"),
        "{out}"
    );
    assert!(
        out.contains("DRY-RUN: run /usr/bin/systemctl enable --now -- kbf-daemon.service"),
        "{out}"
    );
}

/// A downloader with no package behind it fails the bundled setup the way a build with no
/// fetcher fails `--commit`: exit 1, `not_implemented`, after `installer.toml` and before
/// `kbf.toml`, and no unit is enabled.
///
/// Catches a setup that says it is done when no release could be installed.
///
/// Mutant to plant: the install error ignored.
#[test]
fn a_bundled_setup_with_no_package_fails_before_the_config_and_enables_no_unit() {
    let mut exec = host();
    let err = go(&FakeOs::linux(), &mut exec, &bundled(Mode::Daemon), APPLY).expect_err("fails");
    assert!(
        matches!(err, SetupError::Fetch(FetchError::NotWired)),
        "{err:?}"
    );
    assert_eq!((err.exit(), err.code()), (Exit::Failed, "not_implemented"));
    assert!(exec.entries.contains_key(&hp(INSTALLER_TOML)));
    assert!(!exec.entries.contains_key(&hp(KBF_TOML)));
    assert!(cmds(&exec).iter().all(|c| !c.contains("systemctl")));
}

/// Through the real downloader, the package's release is verified, installed as
/// `releases/<commit>` and made the release that runs, with nothing staged, and the commit it
/// reports is the manifest's. No floor is asked of it: nothing runs yet, and the host has none.
///
/// Catches a first release that is installed and not made current, one that is left staged (the
/// frontend's first update would then be busy for ever), and a first release held to a floor a
/// host that has never run cannot have.
///
/// Mutants to plant: `activate::first` not called; the seed's `seq` compared with a floor.
#[test]
fn the_packages_release_is_verified_installed_and_made_current_by_the_downloader() {
    let seeded = Seeded::scratch("setup-seed");
    let fetcher = seeded.fetcher();
    let exec = HostExec::new(Root::new(&seeded.root).expect("absolute"));
    let releases = Fetching::new(&fetcher, seeded.layout.clone(), exec);
    let commit = releases.install_seed().expect("the package's release");
    assert_eq!(commit, seeded.commit);
    assert!(seeded.installed());
    let state = crate::state::read_unlocked(&seeded.layout)
        .expect("reads")
        .expect("there");
    assert_eq!(state.current, Some(commit));
    assert_eq!(state.staged, None);
}

/// The package's release goes through the verify pipeline on this path as on the others: a
/// bundle `gh` does not accept ends `setup` with the pipeline's word, exit 3, installing
/// nothing and making nothing current; a host with no package ends it with a missing seed.
///
/// Catches a first release installed without being verified because the package is root's.
///
/// Mutants to plant: `Pipeline::verify` replaced by the install of the copied tarball.
#[test]
fn a_package_release_the_pipeline_refuses_is_not_installed_by_setup() {
    let mut seeded = Seeded::scratch("setup-seed-unattested");
    seeded.attested = b"the bundle of another release\n".to_vec();
    let exec = HostExec::new(Root::new(&seeded.root).expect("absolute"));
    let fetcher = seeded.fetcher();
    let releases = Fetching::new(&fetcher, seeded.layout.clone(), exec);
    let err = releases.install_seed().expect_err("refused");
    assert!(
        matches!(&err, SetupError::Fetch(FetchError::Rejected(why)) if why.starts_with("attestation: ")),
        "{err:?}"
    );
    assert_eq!((err.exit(), err.code()), (Exit::Refused, "fetch"));
    assert!(!seeded.installed());
    let state = crate::state::read_unlocked(&seeded.layout).expect("reads");
    assert!(state.is_none_or(|s| s.current.is_none()));

    let seeded = Seeded::scratch("setup-seed-absent");
    let bare = trusted_host_on(seeded.layout.platform());
    let fetcher = seeded.fetcher_on(bare);
    let exec = HostExec::new(Root::new(&seeded.root).expect("absolute"));
    let releases = Fetching::new(&fetcher, seeded.layout.clone(), exec);
    let err = releases.install_seed().expect_err("no package");
    assert!(
        matches!(&err, SetupError::Fetch(FetchError::Unavailable(why)) if why.starts_with("seed_missing: ")),
        "{err:?}"
    );
    assert_eq!((err.exit(), err.code()), (Exit::Failed, "fetch"));
}

/// The downloader is a caller of the fetcher like the cycle, and the denial is the fetcher's:
/// asked for a first release by commit on a host whose `installer.toml` says `bundled` (a way
/// in the flags do not allow, so only the fetcher stands in it), it is denied, with the word,
/// and `setup` exits 3 with nothing installed.
///
/// Catches a gate kept in the callers: this is the third one.
///
/// Mutant to plant: the denial made by the cycle's callers only.
#[test]
fn the_downloader_cannot_fetch_on_a_bundled_host_either() {
    let seeded = Seeded::scratch("setup-bundled-fetch");
    let fetcher = seeded.host_fetcher("release_source = \"bundled\"\n");
    let exec = HostExec::new(Root::new(&seeded.root).expect("absolute"));
    let releases = Fetching::new(&fetcher, seeded.layout.clone(), exec);
    let err = releases.install_first(&seeded.commit).expect_err("denied");
    assert!(
        matches!(&err, SetupError::Fetch(FetchError::Order { word: "denied", .. })),
        "{err:?}"
    );
    assert_eq!(err.exit(), Exit::Refused);
    assert!(!seeded.installed());
}
