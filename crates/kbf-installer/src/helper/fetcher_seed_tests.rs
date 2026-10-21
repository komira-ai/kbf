//! The seed fetcher on a scratch host: the real verify pipeline over the files a package
//! leaves, with `gh` and the binary's `--version --json` played by a fake runner. What each
//! test proves is named on it, with the mutant that turns it red.

use std::fs;
use std::os::unix::fs::symlink;

use kbf_host::exec::Entry;
use kbf_host::testkit::FakeExec;

use super::*;
use crate::CommitId;
use crate::seed_fixture::{SEQ, Seeded};
use crate::testkit::{A, B, c};

/// What an honest seed installs, as the pipeline reports it.
fn installed(seeded: &Seeded) -> InstalledRelease {
    InstalledRelease {
        commit: seeded.commit.clone(),
        platform: "linux-x86_64".to_owned(),
        protocol: 4,
        min_protocol: 3,
        state_schema: 7,
        profile: "release".to_owned(),
    }
}

/// What `work/` holds: a refused seed leaves no scratch behind.
fn scratch_left(seeded: &Seeded) -> Vec<String> {
    let work = seeded.layout.join(&seeded.layout.work_dir());
    let names = |dir: fs::ReadDir| -> Vec<String> {
        dir.flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    };
    fs::read_dir(work).map(names).unwrap_or_default()
}

/// A fetch of the seed with `plant` done to the package and `tweak` to the trust view of the
/// host: the seed as planted, and what the fetch said.
fn tried(
    name: &str,
    plant: impl FnOnce(&mut Seeded),
    tweak: impl FnOnce(&Seeded, &mut FakeExec),
) -> (Seeded, Result<InstalledRelease, FetchError>) {
    let mut seeded = Seeded::scratch(name);
    plant(&mut seeded);
    seeded.write_tarball();
    let mut exec = seeded.exec();
    tweak(&seeded, &mut exec);
    let got = seeded.fetcher_on(exec).fetch(&seeded.locator());
    (seeded, got)
}

/// That the fetch of `seeded` said `got`, an error whose words start with `want`, and that
/// nothing was installed or left in `work/`.
fn refused(seeded: &Seeded, got: Result<InstalledRelease, FetchError>, want: &str) {
    let why = got.expect_err("refused").to_string();
    assert!(why.starts_with(want), "{why}");
    assert!(!seeded.installed(), "nothing is installed");
    assert_eq!(scratch_left(seeded), Vec::<String>::new());
}

/// A commit that shares the first 12 digits with `B` and differs after them.
fn same_build_other_commit() -> CommitId {
    c(&format!("{}{}", &B[..12], "f".repeat(28)))
}

/// An honest seed is installed by both forms and read by the hook: the seed's manifest is what
/// the hook reports, `fetch` (a frontend update) installs it and reports what the binary
/// declared, the release is verified and in its directory, no scratch is left, and the
/// activation form installs the same release again.
///
/// Catches a fetcher that reports a release it did not install, that leaves its scratch
/// behind, or whose second form is not the first with the order's facts.
///
/// Mutants planted: the pipeline's report replaced by the manifest's; `Partial` leaked (the
/// directory stays in `work/`); the activation form calling the update form.
#[test]
fn an_honest_seed_is_installed_by_both_forms_and_the_hook_reports_its_manifest() {
    let seeded = Seeded::scratch("seed-fetch-honest");
    let fetcher = seeded.fetcher();
    assert_eq!(
        fetcher.seed(),
        Ok(SeedFacts {
            commit: c(B),
            seq: SEQ,
            facts: seeded.facts(),
        })
    );
    assert!(!seeded.installed(), "reading the manifest installs nothing");

    assert_eq!(fetcher.fetch(&seeded.locator()), Ok(installed(&seeded)));
    assert!(seeded.installed());
    assert_eq!(scratch_left(&seeded), Vec::<String>::new());

    let again = fetcher.fetch_activation(&seeded.locator(), &seeded.facts());
    assert_eq!(again, Ok(installed(&seeded)));
}

/// An activation holds the lease check and an update does not: with work running the
/// activation is refused with the word `lease` and installs nothing, and the update of the same
/// seed goes through (it moves no node link).
///
/// Catches the two kinds of order verified as one kind: an activation that ignores the leases,
/// or an update that waits for them.
///
/// Mutants planted: the activation form made an update; the update form made an activation.
#[test]
fn an_activation_waits_for_no_lease_and_an_update_does_not() {
    let seeded = Seeded::scratch("seed-fetch-leases");
    seeded.leases.set(2);
    let fetcher = seeded.fetcher();
    let got = fetcher.fetch_activation(&seeded.locator(), &seeded.facts());
    assert!(
        matches!(&got, Err(FetchError::Rejected(why)) if why.starts_with("lease: ")),
        "{got:?}"
    );
    assert!(!seeded.installed());
    assert_eq!(fetcher.fetch(&seeded.locator()), Ok(installed(&seeded)));
}

/// A flipped byte in the tarball is refused by the pipeline's digest check (the manifest still
/// names the digest of the honest bytes), and nothing is installed or left behind.
///
/// Catches a seed whose tarball is not the one the manifest names being installed, and a
/// fetcher that takes its digest from the file it copied.
///
/// Mutants planted: the locator's digest computed from the copied bytes; the pipeline's step
/// skipped.
#[test]
fn a_flipped_byte_in_the_tarball_is_refused_as_a_hash_mismatch() {
    let (seeded, got) = tried(
        "seed-flipped",
        |s| {
            let middle = s.tarball.len() / 2;
            s.tarball[middle] ^= 1;
        },
        |_, _| {},
    );
    refused(&seeded, got, "fetch: rejected: hash: ");
}

/// A bundle that is not the one the manifest names is refused before the pipeline runs, saying
/// so, and so is a manifest that is not this installer's build's.
///
/// Catches a swapped bundle (an attestation of another release, or none) handed to `gh`, and
/// a package assembled from two builds.
///
/// Mutants planted: the bundle's digest not compared; the manifest's commit not compared with
/// the seed directory's name.
#[test]
fn a_swapped_bundle_and_a_manifest_of_another_build_are_refused_as_a_mismatch() {
    let (seeded, got) = tried(
        "seed-swapped-bundle",
        |s| s.bundle = b"the bundle of another release\n".to_vec(),
        |_, _| {},
    );
    let want = "commit_or_ref: seed_mismatch: the bundle";
    refused(&seeded, got, want);

    let (seeded, got) = tried(
        "seed-other-build",
        |s| s.manifest["commit"] = A.into(),
        |_, _| {},
    );
    let want = "commit_or_ref: seed_mismatch: the manifest is for";
    refused(&seeded, got, want);

    let (seeded, got) = tried(
        "seed-garbled-manifest",
        |_| {},
        |s, exec| {
            let entry = Entry::File {
                perms: kbf_host::step::Perms::new(0o644),
                owner: kbf_host::step::Owner { uid: 0, gid: 0 },
                bytes: b"not a manifest".to_vec(),
            };
            exec.entries.insert(s.manifest_path(), entry);
        },
    );
    let want = "commit_or_ref: seed_mismatch: manifest.json: ";
    refused(&seeded, got, want);
}

/// An order for another commit than the seed's is refused by the fetcher itself (the
/// decisive check, whoever called it) whichever form asks, including a commit that shares the
/// 12 digits that name the seed directory; nothing is made on the host.
///
/// Catches the order's commit not compared with the manifest's, and the comparison made on the
/// short form.
///
/// Mutants planted: the comparison removed; the commits compared by their first 12 digits.
#[test]
fn an_order_for_another_commit_is_refused_by_both_forms_and_makes_nothing() {
    let seeded = Seeded::scratch("seed-wrong-commit");
    let fetcher = seeded.fetcher();
    for other in [c(A), same_build_other_commit()] {
        let locator = ReleaseLocator {
            commit: other.clone(),
            ..seeded.locator()
        };
        let want = FetchError::Order {
            word: "commit_or_ref",
            why: format!("seed_mismatch: this host carries {B}, not {other}"),
        };
        assert_eq!(fetcher.fetch(&locator).unwrap_err(), want);
        let activation = fetcher.fetch_activation(&locator, &seeded.facts());
        assert_eq!(activation.unwrap_err(), want);
    }
    assert!(!seeded.installed());
    assert_eq!(scratch_left(&seeded), Vec::<String>::new());
}

/// A file the package should have and does not is a missing seed (the word `fetch`, naming
/// `seed_missing`), whichever file it is: the tarball on the disk, the tarball, the bundle or
/// the manifest in the trust view, or the seed directory itself; a tarball that is a directory
/// or a link is no tarball (a link is not followed). Nothing is installed.
///
/// Catches a missing file reported as a bad release (it is the package that is wrong), a link
/// followed to bytes the package does not own, and a gap in the checks that lets a seed with a
/// file missing go on to the pipeline.
///
/// Mutants planted: `O_NOFOLLOW` dropped from the tarball's open (the link's bytes install);
/// the tarball's trust check removed (a tarball absent from the view is copied from the disk);
/// the manifest read made to treat a directory as an empty file.
#[test]
fn a_file_the_package_lacks_is_a_missing_seed_and_a_link_is_not_followed() {
    type Tweak = fn(&Seeded, &mut FakeExec);
    let cases: [(&str, Tweak); 7] = [
        ("the tarball is not on the disk", |s, _| {
            fs::remove_file(s.layout.join(&s.tarball_path())).expect("remove");
        }),
        ("the tarball is a directory", |s, _| {
            let path = s.layout.join(&s.tarball_path());
            fs::remove_file(&path).expect("remove");
            fs::create_dir(&path).expect("a directory");
        }),
        ("the tarball is a link", |s, _| {
            let path = s.layout.join(&s.tarball_path());
            let elsewhere = s.layout.join(&s.layout.seed_file(&s.build, "elsewhere"));
            fs::rename(&path, &elsewhere).expect("move");
            symlink(&elsewhere, &path).expect("a link");
        }),
        ("the tarball is not in the view", |s, exec| {
            exec.entries.remove(&s.tarball_path());
        }),
        ("the bundle is not in the view", |s, exec| {
            exec.entries.remove(&s.bundle_path());
        }),
        ("the manifest is not in the view", |s, exec| {
            exec.entries.remove(&s.manifest_path());
        }),
        ("the seed directory is not in the view", |s, exec| {
            exec.entries.remove(&s.layout.seed_dir(&s.build));
        }),
    ];
    for (index, (what, tweak)) in cases.into_iter().enumerate() {
        let (seeded, got) = tried(&format!("seed-missing-{index}"), |_| {}, tweak);
        let why = got.expect_err(what).to_string();
        assert!(why.starts_with("fetch: seed_missing: "), "{what}: {why}");
        assert!(!seeded.installed(), "{what}");
        assert_eq!(scratch_left(&seeded), Vec::<String>::new(), "{what}");
    }

    // The manifest in the view is a directory, not a file.
    let (seeded, got) = tried(
        "seed-manifest-dir",
        |_| {},
        |s, exec| {
            let entry = Entry::Dir {
                perms: kbf_host::step::Perms::new(0o755),
                owner: kbf_host::step::Owner { uid: 0, gid: 0 },
            };
            exec.entries.insert(s.manifest_path(), entry);
        },
    );
    refused(&seeded, got, "fetch: seed_missing: ");
}

/// A seed directory, manifest, tarball or bundle that root does not alone control is refused
/// by the trust rule, with its word, and nothing is installed.
///
/// Catches a package file that a service user could have replaced being believed, and a check
/// made on the files but not on the directories above them.
///
/// Mutants planted: `trusted_chain` skipped for the tarball; skipped for the seed directory.
#[test]
fn a_package_file_that_root_does_not_alone_control_is_refused_as_untrusted() {
    type Tweak = fn(&Seeded, &mut FakeExec);
    let cases: [(&str, Tweak); 4] = [
        ("the manifest is another user's", |s, exec| {
            if let Some(Entry::File { owner, .. }) = exec.entries.get_mut(&s.manifest_path()) {
                owner.uid = 1000;
            }
        }),
        ("the tarball is group-writable", |s, exec| {
            if let Some(Entry::File { perms, .. }) = exec.entries.get_mut(&s.tarball_path()) {
                *perms = kbf_host::step::Perms::new(0o664);
            }
        }),
        ("the bundle is another user's", |s, exec| {
            if let Some(Entry::File { owner, .. }) = exec.entries.get_mut(&s.bundle_path()) {
                owner.uid = 1000;
            }
        }),
        ("the seed directory is writable", |s, exec| {
            let dir = s.layout.seed_dir(&s.build);
            if let Some(Entry::Dir { perms, .. }) = exec.entries.get_mut(&dir) {
                *perms = kbf_host::step::Perms::new(0o777);
            }
        }),
    ];
    for (index, (what, tweak)) in cases.into_iter().enumerate() {
        let (seeded, got) = tried(&format!("seed-untrusted-{index}"), |_| {}, tweak);
        let why = got.expect_err(what).to_string();
        assert!(
            why.starts_with("fetch: rejected: trust_chain: "),
            "{what}: {why}"
        );
        assert!(!seeded.installed(), "{what}");
    }
}

/// An installer that does not know the commit it was built from (a build with no checkout)
/// or names it in a form that is no directory name has no seed directory to look in: both forms
/// and the hook say so as a missing seed.
///
/// Catches a name made from `unknown` or from anything but 12 lower-case hex digits joined into
/// a path.
///
/// Mutants planted: the check removed (the path is built from the text).
#[test]
fn an_installer_with_no_build_commit_knows_no_seed_directory() {
    let seeded = Seeded::scratch("seed-no-build");
    for build in ["unknown", "0123456789AB", "0123456789a", "../../etc/x", ""] {
        let mut fetcher = seeded.fetcher();
        fetcher.build = build.to_owned();
        for got in [
            fetcher.fetch(&seeded.locator()).map(drop),
            fetcher.seed().map(drop),
        ] {
            let why = got.expect_err(build).to_string();
            assert!(
                why.starts_with("fetch: seed_missing: this installer does not know its build"),
                "{build}: {why}"
            );
        }
    }
}

/// The pipeline's verdicts on a seed that is consistent with itself stand, whichever form
/// asks, with the pipeline's own words: a bundle `gh` does not accept (`attestation`), a
/// binary that declares other facts (`facts`), another commit (`commit_or_ref`), another CPU's
/// binary (`platform`) and a size that is not the tarball's (`size`). Nothing is installed:
/// this is what a path that skipped the pipeline would install.
///
/// Catches the seed installed without the attestation, the facts, the shape or the size being
/// checked: each is a defect the manifest cannot see.
///
/// Mutants planted: `Pipeline::verify` replaced by a copy of the tarball into `releases/` (all
/// cases red).
#[test]
fn a_seed_the_pipeline_refuses_is_not_installed_by_either_form() {
    type Plant = fn(&mut Seeded);
    let cases: [(&str, Plant, &str); 5] = [
        (
            "gh does not accept the bundle",
            |s| s.attested = b"another bundle\n".to_vec(),
            "fetch: rejected: attestation: ",
        ),
        (
            "the binary declares another protocol",
            |s| s.probe = s.probe.replace("\"protocol\":4", "\"protocol\":5"),
            "fetch: rejected: facts: ",
        ),
        (
            "the binary is another commit's",
            |s| s.probe = s.probe.replace(&s.build, "ffffffffffff"),
            "fetch: rejected: commit_or_ref: ",
        ),
        (
            "the binary is another CPU's",
            |s| {
                s.tarball = crate::fixtures::tar_gz(&[crate::fixtures::file(
                    "kbf",
                    &crate::fixtures::elf(0xb7, "payload"),
                )]);
                s.reseal();
            },
            "fetch: rejected: platform: ",
        ),
        (
            "the manifest names another size",
            |s| s.manifest["tarball_size"] = (s.tarball.len() + 1).into(),
            "fetch: rejected: size: ",
        ),
    ];
    for (index, (what, plant, want)) in cases.into_iter().enumerate() {
        let (seeded, got) = tried(&format!("seed-pipeline-{index}"), plant, |_, _| {});
        refused(&seeded, got, want);

        let mut seeded = Seeded::scratch(&format!("seed-pipeline-act-{index}"));
        plant(&mut seeded);
        seeded.write_tarball();
        let got = seeded
            .fetcher()
            .fetch_activation(&seeded.locator(), &seeded.facts());
        let why = got.expect_err(what).to_string();
        assert!(why.starts_with(want), "{what}: {why}");
        assert!(!seeded.installed(), "{what}");
    }
}

/// The facts an order expects must be the manifest's, or the order is refused under the word
/// `facts` before anything is made; and the `seq` the binary declares must be the manifest's
/// (a binary that declares none, a build that predates the field, is not): the release is
/// installed by then, and the order is refused.
///
/// Catches an activation checked against facts of its own while the manifest says others, and
/// a manifest whose `seq` the floor is judged by while the binary says another.
///
/// Mutants planted: the equality of the order's facts and the manifest's removed; the `seq`
/// comparison removed; a missing `seq` taken for the manifest's.
#[test]
fn the_facts_and_the_seq_of_the_manifest_are_the_binarys_and_the_orders() {
    let seeded = Seeded::scratch("seed-facts");
    let mut other = seeded.facts();
    other.state_schema = 8;
    let fetcher = seeded.fetcher();
    let got = fetcher.fetch_activation(&seeded.locator(), &other);
    assert!(
        matches!(&got, Err(FetchError::Order { word: "facts", .. })),
        "{got:?}"
    );
    assert!(!seeded.installed());
    assert_eq!(scratch_left(&seeded), Vec::<String>::new());

    let declared = format!("\"seq\":{SEQ},");
    let others = [
        ("another seq", format!("\"seq\":{},", SEQ + 1)),
        ("no seq", String::new()),
    ];
    for (index, (what, instead)) in others.into_iter().enumerate() {
        let mut seeded = Seeded::scratch(&format!("seed-seq-{index}"));
        seeded.probe = seeded.probe.replace(&declared, &instead);
        let got = seeded.fetcher().fetch(&seeded.locator());
        let why = got.expect_err(what).to_string();
        assert!(
            why.starts_with("facts: seq: the binary declares"),
            "{what}: {why}"
        );
    }
}

/// A scratch directory of a stage that was killed, left where this stage would make its own,
/// is refused as the host's trouble (no verdict on the seed): the sweep at `run`'s start is
/// what removes it.
///
/// Catches the stale directory reused or removed by the seed path.
///
/// Mutant planted: the stale directory removed before the stage.
#[test]
fn a_stale_scratch_directory_of_the_commit_is_refused_and_left() {
    let seeded = Seeded::scratch("seed-stale");
    let stale = seeded
        .layout
        .join(&seeded.layout.work_dir())
        .join(format!("{}.partial", seeded.commit));
    fs::create_dir_all(&stale).expect("a stale stage");
    let got = seeded.fetcher().fetch(&seeded.locator());
    assert!(
        matches!(&got, Err(FetchError::Unavailable(why)) if why.contains("cannot use")),
        "{got:?}"
    );
    assert!(stale.is_dir(), "not this call's to remove");
}
