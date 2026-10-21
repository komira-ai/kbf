//! `setup --release-source bundled` on a host with no release installs the one the package
//! carries, through the same verify pipeline as a fetched one, between `installer.toml` and
//! `kbf.toml`; a host that has a release installs nothing. The first tests play the downloader
//! with a double (the order of the run); the last run the real one over the seed fetcher.

use std::fs;

use kbf_host::testkit::ScratchHost;
use kbf_host::{HostExec, Root};

use super::*;
use crate::fixtures::{FakeRunner, ran, trusted_host_on};
use crate::seed_fixture::{SEQ, Seeded};
use crate::setup::backend::Fetching;
use crate::state::{Locked, State};

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
    // `activate::first` applies a root plan, and the real `HostExec` refuses it to the user CI
    // runs as (`NotRoot`). `ScratchHost` is the suite's seam for that: it reports euid 0 and
    // trusts the running binary, and its file steps go to a `HostExec` on the seeded root.
    let mut exec = ScratchHost::new("setup-seed-activate");
    exec.host = HostExec::new(Root::new(&seeded.root).expect("absolute"));
    let releases = Fetching::new(&fetcher, seeded.layout.clone(), exec);
    let commit = releases.install_seed().expect("the package's release");
    assert_eq!(commit, seeded.commit);
    assert!(seeded.installed());
    let state = crate::state::read_unlocked(&seeded.layout)
        .expect("reads")
        .expect("there");
    assert_eq!(state.current, Some(commit));
    assert_eq!(state.staged, None);
    // The package's release is the release that runs, so the floor starts at its `seq`: nothing
    // at or below it, the package itself included, is a seed above the floor.
    assert_eq!(seeded.floor_text(), Some(format!("{SEQ}\n")));
}

/// A floor that cannot be written after the first release is installed is the operator's: the
/// release is current (it was made so first, and is not undone), the error says what is wrong,
/// and the exit is "needs an operator".
///
/// Catches the floor's failure swallowed (a host that thinks it has a floor), and the release
/// undone for it.
///
/// Mutants to plant: the floor's error ignored; the floor written before the release is made
/// current (red on the release being current).
#[test]
fn a_floor_that_cannot_be_written_after_the_first_release_needs_an_operator() {
    let seeded = Seeded::scratch("setup-seed-floor");
    let fetcher = seeded.fetcher();
    let mut exec = ScratchHost::new("setup-seed-floor-activate");
    exec.host = HostExec::new(Root::new(&seeded.root).expect("absolute"));
    let floor = seeded.layout.join(&seeded.layout.seq_floor());
    fs::create_dir(floor.with_file_name(".seq_floor.tmp")).expect("in the way");
    let releases = Fetching::new(&fetcher, seeded.layout.clone(), exec);
    let err = releases.install_seed().expect_err("no floor");
    assert!(matches!(err, SetupError::NeedsOperator(_)), "{err:?}");
    assert!(err.to_string().contains("seq_floor"), "{err}");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(seeded.installed(), "the release stands");
    assert_eq!(seeded.floor_text(), None);
}

/// What the double says of the release that runs.
struct Running(bool);

impl Releases for Running {
    fn install_first(&self, _: &CommitId) -> Result<(), SetupError> {
        Ok(())
    }

    fn install_seed(&self) -> Result<CommitId, SetupError> {
        Ok(COMMIT.parse().expect("a commit"))
    }

    fn running_relays(&self) -> Result<bool, SetupError> {
        Ok(self.0)
    }
}

/// A host that has a release takes `bundled` only if the release that runs declares the relay:
/// otherwise the frontend would never be told the host fetches nothing, and would order a fetch
/// the installer can only refuse. The refusal is a missing prerequisite (exit 2, `unsupported`),
/// names the relay, comes before anything is written (a dry run is refused too), and does not
/// reach the other sources, nor a host with no release (the one it installs is the package's).
///
/// Catches a bundled host set up beside a `kbf` that cannot tell the frontend, a refusal made
/// after `installer.toml` is written, and the check made for every source or for a fresh host.
///
/// Mutants to plant: the check removed; the check made after the plan is emitted; the check made
/// whatever the source; the check made on a host with no release.
#[test]
fn bundled_is_refused_beside_a_kbf_that_does_not_relay_before_anything_is_written() {
    let dry = Options {
        apply: false,
        no_start: false,
    };
    for mode in [Mode::Daemon, Mode::Frontend, Mode::AllInOne] {
        for options in [APPLY, dry] {
            let mut exec = released(host(), Platform::Linux);
            let got = beside(&mut exec, false, &bundled(mode), options);
            let err = got.expect_err("refused");
            assert!(matches!(err, SetupError::Unsupported(_)), "{mode:?}: {err:?}");
            assert!(err.to_string().contains("relay"), "{mode:?}: {err}");
            assert_eq!((err.exit(), err.code()), (Exit::Usage, "unsupported"));
            assert!(!exec.entries.contains_key(&hp(INSTALLER_TOML)), "{mode:?}");
            assert!(!exec.entries.contains_key(&hp(KBF_TOML)), "{mode:?}");
            assert_eq!(cmds(&exec), Vec::<String>::new(), "{mode:?}");
        }
    }

    // A running kbf that relays: set up as before.
    let mut exec = released(host(), Platform::Linux);
    beside(&mut exec, true, &bundled(Mode::Daemon), APPLY).expect("setup");
    assert!(exec.entries.contains_key(&hp(INSTALLER_TOML)));

    // Another source fetches, and asks nothing of the relay.
    for mode in [Mode::Daemon, Mode::Frontend] {
        let mut exec = released(host(), Platform::Linux);
        beside(&mut exec, false, &raw(mode), APPLY).expect("setup");
    }

    // A host with no release has none to ask: the one it installs is the package's.
    let mut exec = host();
    beside(&mut exec, false, &bundled(Mode::Daemon), APPLY).expect("setup");
}

/// `setup` on `exec` beside a downloader whose running release relays, or not, as `relays` says.
fn beside(
    exec: &mut FakeExec,
    relays: bool,
    raw: &Raw,
    options: Options,
) -> Result<String, SetupError> {
    go_with(&FakeOs::linux(), exec, &Running(relays), raw, options)
}

/// The real downloader asks the release that runs, by the state's `current`: no release is no
/// relay, a release that declares the level `bundled` needs does, one that declares none (a
/// build before the field) does not, and a release that cannot be asked is an error and not a
/// "no".
///
/// Catches the question put to the wrong release, a missing release taken for one that relays,
/// and a failure to ask taken for an answer.
///
/// Mutants to plant: the probe made of the seed's commit instead of `current`; a missing state
/// answered `true`; the probe's error mapped to `false`.
#[test]
fn the_downloader_asks_the_release_that_runs_whether_it_relays() {
    let mut seeded = Seeded::scratch("setup-relay");
    let ask = |seeded: &Seeded| {
        let fetcher = seeded.fetcher();
        let exec = HostExec::new(Root::new(&seeded.root).expect("absolute"));
        Fetching::new(&fetcher, seeded.layout.clone(), exec).running_relays()
    };
    assert!(!ask(&seeded).expect("asks"), "no release runs");

    let state = State {
        current: Some(crate::testkit::c(crate::testkit::A)),
        ..State::default()
    };
    Locked::acquire(&seeded.layout)
        .expect("lock")
        .write(&state)
        .expect("state");
    assert!(ask(&seeded).expect("asks"), "A declares the relay");

    seeded.earlier = seeded.earlier.replace("\"relay\":1,", "");
    assert!(!ask(&seeded).expect("asks"), "A declares none");

    let mut fetcher = seeded.fetcher();
    fetcher.runner = Box::new(FakeRunner::new(|_| Ok(ran(1, "", "it failed"))));
    let exec = HostExec::new(Root::new(&seeded.root).expect("absolute"));
    let got = Fetching::new(&fetcher, seeded.layout.clone(), exec).running_relays();
    assert!(matches!(got, Err(SetupError::Fetch(_))), "{got:?}");
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
        matches!(
            &err,
            SetupError::Fetch(FetchError::Order { word: "denied", .. })
        ),
        "{err:?}"
    );
    assert_eq!(err.exit(), Exit::Refused);
    assert!(!seeded.installed());
}
