//! The host fetcher: that it reads the host when asked and not before, what it takes from
//! `installer.toml` and `installer.conf` and from where, and that each way it cannot be made
//! is a refusal with its reason. The whole path with a stand-in child is
//! `tests/fetcher_real.rs`; the file's own rules are in `installer_toml_tests.rs`.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use kbf_host::exec::Entry;
use kbf_host::step::Perms;
use kbf_host::testkit::FakeExec;
use kbf_update::Source as OrderSource;

use super::*;
use crate::fixtures::{FakeRunner, SETTINGS_PATH, me, ran, trusted_host};
use crate::helper::leases::fake::FakeLeases;
use crate::layout::path;
use crate::seed::SeedFacts;
use crate::seed_fixture::{EARLIER_SEQ, SEQ, Seeded};
use crate::testkit::{A, c, host_root};

const FILE: &str = "/etc/kbf/installer.toml";
const ROOT: Owner = Owner { uid: 0, gid: 0 };

const URL_FILE: &str = "release_source = \"url\"\nrelease_url = \"https://mirror.example\"\n";

/// The frontend snapshot of a LAN-pull host: the worker listener, the root-owned copy of the
/// CA that authenticates it, and that copy's digest.
const SNAPSHOT: &str = "frontend_url = \"https://frontend.lan:8443\"\nfrontend_ca = \"/var/lib/kbf-installer/frontend-ca.pem\"\nfrontend_ca_sha256 = \"d2a84f4b8b650937ec8f73cd8be2c74add5a911ba64df27458ed8229da804a26\"\n";

/// What is on the fake host, changed by a test between two fetches.
#[derive(Default)]
struct Disk {
    /// The text of `installer.toml`; `None` for no file.
    toml: Option<String>,
    /// Who owns it and its mode, when a test changes them (root's 0644 otherwise).
    toml_uid: u32,
    toml_mode: Option<u32>,
    /// The text of `installer.conf`; `None` for no file (the defaults).
    conf: Option<String>,
}

fn entry(text: &str, mode: u32, uid: u32) -> Entry {
    Entry::File {
        perms: Perms::new(mode),
        owner: Owner { uid, gid: 0 },
        bytes: text.as_bytes().to_vec(),
    }
}

/// The host as `disk` says, with the trust list and root's `/etc/kbf`.
fn exec(disk: &Disk) -> FakeExec {
    let mut exec = trusted_host();
    for at in ["/etc", "/etc/kbf"] {
        let dir = Entry::Dir {
            perms: Perms::new(0o755),
            owner: ROOT,
        };
        exec.entries.insert(path(at), dir);
    }
    if let Some(text) = &disk.toml {
        let mode = disk.toml_mode.unwrap_or(0o644);
        exec.entries
            .insert(path(FILE), entry(text, mode, disk.toml_uid));
    }
    if let Some(text) = &disk.conf {
        exec.entries
            .insert(path(SETTINGS_PATH), entry(text, 0o644, 0));
    }
    exec
}

/// A child that cannot start: a fetch that reaches it ends in its spawn failure.
fn nowhere() -> ChildProgram {
    ChildProgram {
        program: PathBuf::from("/nonexistent/kbf-installer"),
        run_as: None,
    }
}

fn owner() -> Owner {
    let me = me();
    Owner {
        uid: me.uid,
        gid: me.gid,
    }
}

fn parts(disk: &Disk, leases: &FakeLeases) -> Parts {
    Parts {
        trust: Box::new(exec(disk)),
        runner: Box::new(FakeRunner::new(|_| Ok(ran(0, "", "")))),
        leases: Box::new(leases.clone()),
        owner: owner(),
        child: nowhere(),
    }
}

/// A fetcher whose host is `disk`, which the test changes.
struct Rig {
    fetcher: HostFetcher,
    disk: Rc<RefCell<Disk>>,
    leases: FakeLeases,
    /// How many times the host was asked for its parts.
    asked: Rc<Cell<usize>>,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let (layout, _) = host_root(name);
        let disk = Rc::new(RefCell::new(Disk::default()));
        let leases = FakeLeases::default();
        let asked = Rc::new(Cell::new(0));
        let (seen, counted, held) = (Rc::clone(&disk), Rc::clone(&asked), leases.clone());
        let fetcher = HostFetcher::new(layout, path(FILE), move || {
            counted.set(counted.get() + 1);
            Ok(parts(&seen.borrow(), &held))
        });
        Rig {
            fetcher,
            disk,
            leases,
            asked,
        }
    }

    fn write(&self, text: &str) {
        self.disk.borrow_mut().toml = Some(text.to_owned());
    }

    fn made(&self) -> RealFetcher {
        self.fetcher.assemble().expect("a fetcher is made")
    }

    fn refused(&self) -> String {
        self.fetcher.assemble().err().expect("refused").to_string()
    }
}

fn locator() -> ReleaseLocator {
    ReleaseLocator {
        commit: c(A),
        components: Vec::new(),
        source: OrderSource::Fetch,
    }
}

/// The locator of an order that asks for the release the package carries.
fn staged() -> ReleaseLocator {
    ReleaseLocator {
        source: OrderSource::Staged,
        ..locator()
    }
}

fn facts() -> Facts {
    Facts {
        platform: "linux-x86_64".to_owned(),
        protocol: 1,
        min_protocol: 1,
        state_schema: 1,
        profile: "release".to_owned(),
    }
}

fn names(real: &RealFetcher) -> Vec<&str> {
    real.settings
        .source_refs
        .iter()
        .map(crate::attest::SourceRef::as_str)
        .collect()
}

/// `setup` builds its fetcher before it has written `installer.toml` or made the fetch user,
/// so the host is read when a fetch asks and again at each fetch: the parts are asked for
/// once per fetch, none at construction. (The file written between two fetches is the next
/// test.)
///
/// Catches a fetcher that reads, or asks the host for its parts, when it is built: `setup`'s
/// would have no file to read and no user to find.
///
/// Mutants planted: the parts asked for in `HostFetcher::new`; the parts kept after the
/// first fetch.
#[test]
fn the_host_is_asked_at_each_fetch_and_not_when_the_fetcher_is_built() {
    let rig = Rig::new("host-fetcher-asks");
    assert_eq!(rig.asked.get(), 0, "nothing at construction");
    let _ = rig.fetcher.fetch(&locator());
    assert_eq!(rig.asked.get(), 1);
    let _ = rig.fetcher.fetch_activation(&locator(), &facts());
    assert_eq!(rig.asked.get(), 2);
}

/// A file written after the fetcher was built is the one used, and a file changed after a fetch
/// is read again: the source of a fetch is whatever `installer.toml` says at that moment, so a
/// second `setup` takes effect in a running `run` with no restart.
///
/// Catches a fetcher that reads the file once (at construction, or at the first fetch that got
/// that far) and keeps it.
///
/// Mutants planted: the parsed file cached in the fetcher after its first successful read.
#[test]
fn installer_toml_is_read_at_each_fetch_so_a_file_written_later_is_used() {
    let rig = Rig::new("host-fetcher-lazy");
    let before = rig.refused();
    assert!(
        before.ends_with("is not trusted: it is missing"),
        "no file yet: {before}"
    );
    rig.write(URL_FILE);
    assert_eq!(
        rig.made().source,
        Source::Url {
            base: "https://mirror.example".to_owned(),
            ca_file: None
        },
        "the file written after the fetcher was built"
    );
    rig.write("release_source = \"url\"\nrelease_url = \"https://other.example/kbf\"\n");
    assert_eq!(
        rig.made().source,
        Source::Url {
            base: "https://other.example/kbf".to_owned(),
            ca_file: None
        },
        "the file changed since the last fetch"
    );
}

/// What a fetch takes from `installer.toml` and where it puts it: the `url` source with the
/// system's roots, the component cap and timeout as limits, the cap and the `min_protocol`
/// floor in the policy, and from the host its owner, lease source and child.
///
/// Catches a value taken from the wrong key or left at its default, a source that is not the
/// file's, and a host part dropped or replaced on the way into the real fetcher.
///
/// Mutants planted: `Policy::of` replaced by a policy of defaults; `Limits::of` by the
/// defaults; the source's base changed; the lease source replaced by `NotWired`.
#[test]
fn a_fetch_is_made_from_the_file_and_the_hosts_parts() {
    let rig = Rig::new("host-fetcher-parts");
    let text = "release_source = \"url\"\nrelease_url = \"https://Mirror.example:8443/kbf/\"\nmin_protocol = 4\nfetch_timeout_s = 1234\nmax_component_bytes = 7000001\n";
    rig.write(text);
    rig.leases.set(3);
    let real = rig.made();
    let toml = InstallerToml::parse(text).expect("parses");
    assert_eq!(
        real.source,
        Source::Url {
            base: "https://mirror.example:8443/kbf".to_owned(),
            ca_file: None
        }
    );
    assert_eq!(
        real.limits,
        Limits {
            max_component_bytes: 7_000_001,
            fetch_timeout: Duration::from_secs(1234)
        }
    );
    assert_eq!(real.policy, Policy::of(&toml));
    assert_eq!(real.policy.min_protocol, 4);
    assert_eq!(real.policy.max_component_bytes, 7_000_001);
    assert_eq!(real.owner, owner());
    assert_eq!(real.child.program, nowhere().program);
    assert_eq!(real.leases.live(), Ok(3), "the host's lease source");
    assert_eq!(real.host.platform(), Platform::Linux);
}

/// The frontend source is not buildable yet: `installer.toml` can now hold the frontend's
/// address and its pinned CA (`frontend_url`, `frontend_ca`, `frontend_ca_sha256`), and this
/// build reads them without using them, so the fetch is refused with that said. The default
/// file (which is a frontend source), the file that names the source, and the file with the
/// whole snapshot are the cases.
///
/// Catches a frontend source built from nothing (a fetch to no address, or with the system's
/// roots where the farm's CA is required), a message that still says the file has no key for
/// them, and the snapshot read as a licence to fetch.
///
/// Mutants planted: the frontend arm returning a `url` source.
#[test]
fn the_frontend_source_is_refused_until_the_fetcher_uses_the_snapshot() {
    let rig = Rig::new("host-fetcher-frontend");
    let snapshot = format!("release_source = \"frontend\"\n{SNAPSHOT}");
    for text in ["", "release_source = \"frontend\"\n", snapshot.as_str()] {
        rig.write(text);
        let why = rig.refused();
        assert!(
            why.contains("release_source = \"frontend\"")
                && why.contains("cannot fetch from the frontend yet"),
            "{text:?}: {why}"
        );
    }
}

/// A bundled host fetches nothing: an order that fetches is denied by the fetcher itself,
/// whoever calls it and whichever form (so a caller that did not look at the configuration, or
/// looked at one the file no longer says, cannot make it fetch), with the word `denied` and
/// the reason, and not read as a `url` or a `frontend` source.
///
/// Catches `bundled` falling through to a source that fetches (the network where the host is
/// air-gapped), the denial left to the callers that ask, and a refusal that does not say why.
///
/// Mutants to plant: the gate only in the callers (the direct calls here fetch); the `bundled`
/// arm returning the `frontend` refusal (red: the message); returning a `url` source built from
/// nothing (red: no refusal).
#[test]
fn a_bundled_host_denies_every_fetch_by_the_fetcher_itself() {
    let rig = Rig::new("host-fetcher-bundled");
    rig.write("release_source = \"bundled\"\n");
    let why = "release_source is bundled; source=fetch is refused (use source=staged)";
    assert_eq!(rig.refused(), why);
    let denied = || FetchError::Order {
        word: "denied",
        why: why.to_owned(),
    };
    assert_eq!(rig.fetcher.fetch(&locator()), Err(denied()));
    assert_eq!(
        rig.fetcher.fetch_activation(&locator(), &facts()),
        Err(denied())
    );
}

/// An order for the release the package carries is carried out on every kind of host, a
/// `bundled` one, a `url` one with or without a CA of its own, the default (a frontend) and a
/// frontend with its snapshot, by the seed fetcher and not by the fetch child: the seed's
/// bundle is one `gh` does not accept, so what answers is the pipeline (`attestation`), not a
/// refusal of the source, not a denial and not the child that cannot start.
///
/// Catches a staged order read as a fetch (the frontend source is not buildable, and a `url`
/// host would start the child), and `staged` allowed on a `bundled` host only.
///
/// Mutants to plant: the staged form building the real fetcher (every case but `url`, which
/// starts the child, is red); the staged form refused unless the file says `bundled`.
#[test]
fn a_staged_order_takes_the_package_on_every_kind_of_host_and_never_starts_the_child() {
    let mut seeded = Seeded::scratch("host-fetcher-staged");
    seeded.attested = b"the bundle of another release\n".to_vec();
    let ca = format!("{URL_FILE}release_ca = \"/var/lib/kbf-installer/release-ca.pem\"\n");
    let frontend = format!("release_source = \"frontend\"\n{SNAPSHOT}");
    let files = [
        "release_source = \"bundled\"\n",
        URL_FILE,
        ca.as_str(),
        "",
        frontend.as_str(),
    ];
    for file in files {
        let fetcher = seeded.host_fetcher(file);
        let locator = ReleaseLocator {
            commit: seeded.commit.clone(),
            ..staged()
        };
        for got in [
            fetcher.fetch(&locator),
            fetcher.fetch_activation(&locator, &seeded.facts()),
        ] {
            let why = got.expect_err(file).to_string();
            assert!(
                why.starts_with("fetch: rejected: attestation: "),
                "{file:?}: {why}"
            );
        }
    }
    assert!(!seeded.installed());
}

/// The hook reports the package's manifest through the host fetcher, whatever the source
/// file says, and a host whose installer was built from another commit than the package's, or
/// that cannot supply its parts, reports why it has no seed.
///
/// Catches the hook answered from the configuration, and a missing seed taken for a seed.
///
/// Mutant to plant: the hook delegating to the fetch-source fetcher (a `url` host without a
/// child refuses).
#[test]
fn the_host_fetcher_reports_the_seed_of_its_package_or_why_it_has_none() {
    let seeded = Seeded::scratch("host-fetcher-seed");
    let want = SeedFacts {
        commit: seeded.commit.clone(),
        seq: SEQ,
        facts: seeded.facts(),
    };
    for file in ["release_source = \"bundled\"\n", URL_FILE] {
        let got = seeded.host_fetcher(file).seed();
        assert_eq!(got, Ok(want.clone()), "{file:?}");
    }
    let other = seeded.host_fetcher(URL_FILE).with_build("ffffffffffff");
    let got = other.seed();
    assert!(
        matches!(&got, Err(FetchError::Unavailable(why)) if why.starts_with("seed_missing: ")),
        "{got:?}"
    );

    let (layout, _) = host_root("host-fetcher-seed-no-parts");
    let reason = "the fetch user: there is no user \"kbf-fetch\"";
    let fetcher = HostFetcher::new(layout, path(FILE), || Err(reason.to_owned()));
    let unavailable = || FetchError::Unavailable(reason.to_owned());
    assert_eq!(fetcher.seed(), Err(unavailable()));
    assert_eq!(fetcher.fetch(&staged()), Err(unavailable()));
    assert_eq!(
        fetcher.fetch_activation(&staged(), &facts()),
        Err(unavailable())
    );
}

/// The look the run loop takes reports the word `installer.toml` holds for the source (whatever
/// that source can or cannot fetch, and whether the package carries a seed) beside what it
/// finds of the package: the seed found whole on every kind of host, nothing when the package
/// has none, and why not when the host cannot say (a file the closed reader refuses, a host
/// without its parts).
///
/// Catches the source reported from the fetch (a `bundled` host unable to say so), a package
/// looked at only on a host that fetches nothing, and a file that cannot be read taken for a
/// default source.
///
/// Mutants to plant: the word taken from the order's source; the look delegating to the
/// fetch-source fetcher (a `url` host without a child cannot look); the source reported as
/// `None` when no seed is found.
#[test]
fn the_host_fetcher_looks_at_the_package_and_reports_the_source_the_file_names() {
    let seeded = Seeded::scratch("host-fetcher-look");
    let want = SeedFacts {
        commit: seeded.commit.clone(),
        seq: SEQ,
        facts: seeded.facts(),
    };
    let frontend = format!("release_source = \"frontend\"\n{SNAPSHOT}");
    let files = [
        ("release_source = \"bundled\"\n", "bundled"),
        (URL_FILE, "url"),
        ("", "frontend"),
        (frontend.as_str(), "frontend"),
    ];
    for (file, word) in files {
        let noticed = seeded.host_fetcher(file).look(None);
        assert_eq!(noticed.source, Some(word), "{file:?}");
        let found = noticed.seed.expect("looks").expect("a seed");
        assert_eq!(found.seed, want, "{file:?}");
    }

    // A package without a seed for this installer's build: nothing found, the source still told.
    let other = seeded.host_fetcher(URL_FILE).with_build("ffffffffffff");
    let noticed = other.look(None);
    assert_eq!((noticed.source, noticed.seed), (Some("url"), Ok(None)));

    // A file the closed reader refuses: no source, and the reason.
    let noticed = seeded.host_fetcher("surprise = 1\n").look(None);
    assert_eq!(noticed.source, None);
    let why = noticed.seed.expect_err("refused").to_string();
    assert!(why.contains("surprise"), "{why}");

    // A host that cannot supply its parts.
    let (layout, _) = host_root("host-fetcher-look-no-parts");
    let reason = "the fetch user: there is no user \"kbf-fetch\"";
    let fetcher = HostFetcher::new(layout, path(FILE), || Err(reason.to_owned()));
    let noticed = fetcher.look(None);
    assert_eq!(noticed.source, None);
    let unavailable = FetchError::Unavailable(reason.to_owned());
    assert_eq!(noticed.seed, Err(unavailable));
}

/// An installed release is asked its version through the host fetcher on every kind of host,
/// and a host that cannot supply its parts says why it cannot ask.
///
/// Catches the question answered only where a source can fetch.
///
/// Mutant to plant: the probe delegating to the fetch-source fetcher.
#[test]
fn the_host_fetcher_asks_an_installed_release_for_its_version() {
    let seeded = Seeded::scratch("host-fetcher-probe");
    for file in ["release_source = \"bundled\"\n", URL_FILE] {
        let said = seeded.host_fetcher(file).probe(&c(A)).expect("asked");
        assert_eq!(said.seq, Some(EARLIER_SEQ), "{file:?}");
    }
    let (layout, _) = host_root("host-fetcher-probe-no-parts");
    let fetcher = HostFetcher::new(layout, path(FILE), || Err("no parts".to_owned()));
    let got = fetcher.probe(&c(A));
    assert_eq!(got, Err(FetchError::Unavailable("no parts".to_owned())));
}

/// `release_ca` names the roots a `url` source is checked against instead of the system's, and
/// this build does not use the file yet: a fetch with the key set is refused, not made with
/// the system's roots (which would widen what the operator narrowed). The same file without the
/// key is a `url` source as before.
///
/// Catches a key that is parsed and silently dropped on the way to the fetch child.
///
/// Mutants to plant: the `release_ca` arm removed (the key ignored: red, no refusal).
#[test]
fn a_url_host_with_a_release_ca_is_refused_not_fetched_with_the_system_roots() {
    let rig = Rig::new("host-fetcher-release-ca");
    rig.write(&format!(
        "{URL_FILE}release_ca = \"/var/lib/kbf-installer/release-ca.pem\"\n"
    ));
    let why = rig.refused();
    assert!(
        why.contains("release_ca") && why.contains("does not use it yet"),
        "{why}"
    );
    rig.write(URL_FILE);
    assert_eq!(
        rig.made().source,
        Source::Url {
            base: "https://mirror.example".to_owned(),
            ca_file: None
        }
    );
}

/// `installer.toml`'s `source_refs` narrows `installer.conf`'s list through the fetcher: the
/// settings a fetch verifies with name only refs both allow, a file that names none of the
/// package's refs is refused with that said, and a file with no `source_refs` leaves the
/// list whole. A host with no `installer.conf` has the default `refs/heads/main` to narrow.
///
/// Catches the narrowing never applied (the file's refs only parsed and rendered, as they were
/// before), applied to the wrong settings, or widening.
///
/// Mutants planted: the `narrowed_to` call removed from `assemble`.
#[test]
fn installer_tomls_source_refs_narrow_installer_confs_in_a_fetch() {
    let rig = Rig::new("host-fetcher-refs");
    rig.disk.borrow_mut().conf =
        Some("SOURCE_REFS=refs/heads/main refs/heads/feature/arch-v2\n".to_owned());
    rig.write(URL_FILE);
    assert_eq!(
        names(&rig.made()),
        ["refs/heads/main", "refs/heads/feature/arch-v2"],
        "no source_refs: no narrowing"
    );
    rig.write(&format!(
        "{URL_FILE}source_refs = [\"refs/heads/feature/arch-v2\", \"refs/heads/not-allowed\"]\n"
    ));
    assert_eq!(
        names(&rig.made()),
        ["refs/heads/feature/arch-v2"],
        "narrowed, and the ref the package does not allow is not added"
    );
    rig.write(&format!(
        "{URL_FILE}source_refs = [\"refs/heads/not-allowed\"]\n"
    ));
    let why = rig.refused();
    assert!(
        why.contains("source_refs") && why.contains("never widen"),
        "{why}"
    );

    // No installer.conf: the default ref is what a file narrows.
    rig.disk.borrow_mut().conf = None;
    rig.write(&format!("{URL_FILE}source_refs = [\"refs/heads/main\"]\n"));
    assert_eq!(names(&rig.made()), ["refs/heads/main"]);
    rig.write(&format!(
        "{URL_FILE}source_refs = [\"refs/heads/feature/arch-v2\"]\n"
    ));
    assert!(rig.refused().contains("source_refs"));
}

/// The file is the host's authority on where releases come from, so a fetch refuses one that
/// root does not alone control, whatever it says, and an `installer.conf` that is wrong is
/// refused too, each with its reason in the refusal.
///
/// Catches the ownership check not made on the fetch path (the reader is tested on its own;
/// this is that it is the one used).
///
/// Mutants planted: `InstallerToml::read` replaced by a plain parse of the file's bytes.
#[test]
fn a_fetch_refuses_a_file_that_is_not_roots_and_a_bad_settings_file() {
    let rig = Rig::new("host-fetcher-untrusted");
    rig.write(URL_FILE);
    rig.disk.borrow_mut().toml_uid = 1000;
    let why = rig.refused();
    assert!(why.contains("owned by uid 1000, not root"), "{why}");
    rig.disk.borrow_mut().toml_uid = 0;
    rig.disk.borrow_mut().toml_mode = Some(0o666);
    let why = rig.refused();
    assert!(why.contains("writable by group or others"), "{why}");
    rig.disk.borrow_mut().toml_mode = None;
    assert!(rig.fetcher.assemble().is_ok());

    rig.disk.borrow_mut().conf = Some("NOT_A_KEY=1\n".to_owned());
    let why = rig.refused();
    assert!(
        why.contains("installer.conf") && why.contains("NOT_A_KEY"),
        "{why}"
    );
}

/// A host that cannot supply its parts (no fetch user) is refused first, with that reason and
/// nothing about the file; and the trait's two forms each refuse as `Unavailable` (the
/// cycle's word `fetch`), not as a verdict on the release.
///
/// Catches the host's reason lost or worded as the file's, and a refusal that would be taken
/// for a bad release.
#[test]
fn a_host_without_its_parts_is_unavailable_with_the_reason() {
    let (layout, _) = host_root("host-fetcher-no-parts");
    let fetcher = HostFetcher::new(layout, path(FILE), || {
        Err("the fetch user: there is no user \"kbf-fetch\"".to_owned())
    });
    let want = FetchError::Unavailable("the fetch user: there is no user \"kbf-fetch\"".to_owned());
    assert_eq!(fetcher.fetch(&locator()), Err(want));
    let want = FetchError::Unavailable("the fetch user: there is no user \"kbf-fetch\"".to_owned());
    assert_eq!(fetcher.fetch_activation(&locator(), &facts()), Err(want));
}

/// Each form of the trait reaches the matching form of the real fetcher: an activation that
/// names no component is a verdict (`Rejected`, the order is malformed) and a `frontend_update`
/// that names none asks the source to describe the release, which here cannot start.
///
/// Catches the two forms swapped, either delegating to the other.
///
/// Mutants planted: `fetch` calling `fetch_activation`; `fetch_activation` calling `fetch`.
#[test]
fn the_two_forms_reach_the_real_fetchers_matching_forms() {
    let rig = Rig::new("host-fetcher-forms");
    rig.write(URL_FILE);
    let activation = rig.fetcher.fetch_activation(&locator(), &facts());
    assert!(
        matches!(&activation, Err(FetchError::Rejected(why)) if why.contains("names no component")),
        "{activation:?}"
    );
    let update = rig.fetcher.fetch(&locator());
    assert!(
        matches!(&update, Err(FetchError::Unavailable(why)) if why.contains("cannot start the fetch child")),
        "{update:?}"
    );
}

/// The parts of the host itself: the executor reads under the host's root, the owner is the
/// ids this process runs with, and the lease source and child are the ones given.
///
/// Catches an executor rooted at `/` (it would read the real host's files in a test and the
/// wrong ones under a root flag), a fixed owner (root's, where a scratch test is not root),
/// and a lease source or child dropped.
///
/// Mutants planted: `Root::new("/")` in place of the host's root; the owner fixed at 0:0
/// (equivalent when the tests run as root).
#[test]
fn the_hosts_own_parts_are_rooted_at_the_host_and_owned_by_this_process() {
    let (layout, root) = host_root("host-fetcher-host-parts");
    std::fs::write(root.join("probe"), b"here").expect("a file under the root");
    let leases = FakeLeases::default();
    leases.set(2);
    let parts = host_parts(&layout, Box::new(leases), nowhere());
    let seen = parts.trust.inspect(&path("/probe")).expect("read");
    assert!(
        matches!(seen, Some(Entry::File { bytes, .. }) if bytes == b"here"),
        "the executor reads under the host's root"
    );
    let (uid, gid) = (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    );
    assert_eq!(parts.owner, Owner { uid, gid });
    assert_eq!(parts.leases.live(), Ok(2));
    assert_eq!(parts.child.program, nowhere().program);
}

/// The host's own fetcher finds its child through a function, asked at each fetch: a host
/// without the fetch user is refused with the user named, and with a child the same fetcher
/// reads the host through the real executor (here a scratch root with no `installer.toml`).
///
/// Catches the fetch user's absence lost on the way (a fetch that would run the child as
/// whoever the caller is), and the child looked up when the fetcher is built, where `setup`
/// has not made the user yet.
///
/// Mutants planted: the child's error dropped for a default child.
#[test]
fn on_a_host_the_child_is_looked_up_per_fetch_and_the_real_executor_answers() {
    let (layout, _) = host_root("host-fetcher-on-host");
    let none = || Box::new(FakeLeases::default()) as Box<dyn LeaseSource>;
    let without = HostFetcher::on_host_with(layout.clone(), path(FILE), none, |_| {
        Err(crate::fetch::ProgramFault::User(
            crate::fetch::user::UserFault::Missing("kbf-fetch".to_owned()),
        ))
    });
    let got = without.fetch(&locator());
    assert!(
        matches!(&got, Err(FetchError::Unavailable(why)) if why.contains("there is no user \"kbf-fetch\"")),
        "{got:?}"
    );

    let with = HostFetcher::on_host_with(layout, path(FILE), none, |_| Ok(nowhere()));
    let got = with.fetch(&locator());
    assert!(
        matches!(&got, Err(FetchError::Unavailable(why)) if why.ends_with("is not trusted: it is missing")),
        "no file under the scratch root: {got:?}"
    );
}

/// `HostFetcher::on_host` is the production constructor: on a platform whose fetch user is
/// not on this box it refuses, naming the user.
#[cfg(target_os = "linux")]
#[test]
fn the_host_fetcher_of_a_host_without_the_fetch_user_names_it() {
    let (layout, _) = host_root("host-fetcher-no-user");
    let layout = Layout::new(layout.root().clone(), Platform::MacOs);
    let fetcher = HostFetcher::on_host(layout, path(FILE), || Box::new(FakeLeases::default()));
    let got = fetcher.fetch(&locator());
    assert!(
        matches!(&got, Err(FetchError::Unavailable(why)) if why.contains("_kbf-fetch")),
        "{got:?}"
    );
}
