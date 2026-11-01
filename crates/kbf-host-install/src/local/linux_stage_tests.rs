//! Staging a release on the Linux backend: the fetch, the attestation per channel, the
//! channels file and token rules, and every refusal that leaves nothing on the host.

use super::*;

/// The release is fetched as the unprivileged user, checked by `gh attestation verify` offline
/// for the full commit and the release workflow on hosted runners and the channel's branch, run
/// once as that user and installed as `releases/<commit>`; no link moves, nothing restarts, and
/// the scratch is gone.
#[test]
fn a_release_is_fetched_verified_and_laid_out_beside_the_running_one() {
    let rig = Rig::new("stage");
    assert_eq!(rig.stage().unwrap(), COMMIT);

    let release = rig.releases().join(COMMIT);
    assert_eq!(fs::read(release.join("kbf")).unwrap(), binary("b"));
    assert_eq!(
        fs::metadata(release.join("kbf")).unwrap().mode() & 0o7777,
        0o755
    );
    assert_eq!(
        fs::read_to_string(release.join("VERIFIED")).unwrap(),
        format!("{COMMIT}\n")
    );
    let state = rig.state();
    assert_eq!(state.current.unwrap().to_string(), A);
    assert!(state.pending_confirm.is_none() && state.staged.is_none());
    assert_eq!(rig.link("current").as_deref(), Some(A));
    assert!(rig.systemctl().is_empty(), "nothing restarts");

    let verify: Vec<String> = rig.installer.borrow()[0]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let has = |flag: &str, value: &str| verify.windows(2).any(|w| w[0] == flag && w[1] == value);
    assert!(has("--source-digest", COMMIT));
    assert!(has(
        "--signer-workflow",
        "komira-ai/kbf/.github/workflows/artifacts.yml"
    ));
    assert!(has("--source-ref", "refs/heads/main"));
    assert!(has("--repo", "komira-ai/kbf"));
    assert!(verify.contains(&"--deny-self-hosted-runners".to_owned()));
    let probe = rig.installer.borrow()[1].clone();
    assert_eq!(probe.args, ["--version", "--json"]);
    assert_eq!(probe.run_as, Some(me()));

    let download = rig
        .calls
        .borrow()
        .iter()
        .find(|c| c.args.first().map(String::as_str) == Some("release"))
        .cloned()
        .unwrap();
    assert!(download.args.contains(&tarball_name(COMMIT)));
    assert!(download.args.contains(&bundle_name(COMMIT)));
    let who = me();
    assert_eq!(
        download.run_as.map(|i| (i.uid, i.gid)),
        Some((who.uid, who.gid)),
        "fetched as the unprivileged user"
    );
    assert!(!download.env.iter().any(|(k, _)| k == "GH_TOKEN"));
    let work = rig.root().join("var/lib/kbf-installer/work");
    assert_eq!(
        fs::read_dir(work).unwrap().count(),
        0,
        "the scratch is gone"
    );
}

const OTHER_REPO: &str = "owner/other";
const OTHER_WORKFLOW: &str = "owner/other/.github/workflows/release.yml";

/// The channels file of a `stable` channel for the repository `owner/other`, with
/// `trusted` as the file's `trusted_workflows` (none: the key is absent).
fn other_channels(trusted: Option<&[&str]>) -> String {
    let list = trusted.map_or_else(String::new, |names| {
        let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
        format!("trusted_workflows = [{}]\n", quoted.join(", "))
    });
    format!(
        "{list}[channels.stable]\nsource = \"github\"\nrepository = \"{OTHER_REPO}\"\nsigner_workflow = \"{OTHER_WORKFLOW}\"\nartifact_prefix = \"other\"\n"
    )
}

/// The arguments of the `i`th spawn of the installer's runner.
fn spawn_args(rig: &Rig, i: usize) -> Vec<String> {
    rig.installer.borrow()[i]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn flag_value(args: &[String], flag: &str) -> String {
    args[args.iter().position(|a| a == flag).unwrap() + 1].clone()
}

/// A channel for a repository other than kbf's is read, and refused when it is used, unless the
/// same file lists its workflow in `trusted_workflows`. The refusal is
/// `channel_not_allowed`, its words name the channel, the workflow and the key, and nothing is
/// fetched, run or left behind. A list that names other workflows (another repository's, or another file of this one) does not help.
#[test]
fn a_channel_of_another_repository_is_refused_unless_the_file_trusts_it() {
    for trusted in [
        None,
        Some(&[][..]),
        Some(
            &[
                "owner/unrelated/.github/workflows/release.yml",
                "owner/other/.github/workflows/other.yml",
            ][..],
        ),
    ] {
        let rig = Rig::new("untrusted-repo");
        put(&rig.dir.join("releases.toml"), other_channels(trusted));
        let fault = rig.stage().unwrap_err();
        assert!(refused(Code::ChannelNotAllowed)(&fault), "{fault:?}");
        for word in ["stable", OTHER_REPO, "trusted_workflows"] {
            assert!(fault.detail.contains(word), "{word}: {}", fault.detail);
        }
        assert!(rig.calls.borrow().is_empty(), "nothing was fetched");
        assert!(rig.installer.borrow().is_empty(), "nothing was attested");
        rig.assert_nothing_installed();
    }
}

/// A channel for a trusted repository lists that repository's releases, downloads from it,
/// takes the files under the channel's prefix and has the installer's `gh attestation verify`
/// name that repository and workflow, once: kbf's former name is not tried for it. Every other
/// rule is the channel's as before: the branch, the full commit, hosted runners.
#[test]
fn a_trusted_channel_is_listed_downloaded_and_attested_under_its_own_names() {
    let rig = Rig::new("trusted-repo");
    put(
        &rig.dir.join("releases.toml"),
        other_channels(Some(&[OTHER_WORKFLOW])),
    );
    rig.offer_as("other", COMMIT, &tar_gz(&[file("kbf", &binary("b"))]));
    assert_eq!(rig.stage().unwrap(), COMMIT);

    let calls = rig.calls.borrow();
    let listing = calls.iter().find(|c| c.program.ends_with("curl")).unwrap();
    assert_eq!(
        listing.args.last().unwrap(),
        "https://api.github.com/repos/owner/other/releases?per_page=100&page=1"
    );
    let download = calls
        .iter()
        .find(|c| c.args.first().map(String::as_str) == Some("release"))
        .unwrap();
    assert_eq!(flag_value(&download.args, "--repo"), OTHER_REPO);
    assert!(download.args.contains(&named_tarball("other", COMMIT)));
    assert!(
        download
            .args
            .contains(&format!("{}.sigstore.json", named_tarball("other", COMMIT)))
    );
    assert!(!download.args.contains(&tarball_name(COMMIT)));

    assert_eq!(
        rig.installer.borrow().len(),
        2,
        "one attestation, one probe"
    );
    let verify = spawn_args(&rig, 0);
    assert_eq!(flag_value(&verify, "--repo"), OTHER_REPO);
    assert_eq!(flag_value(&verify, "--signer-workflow"), OTHER_WORKFLOW);
    assert_eq!(flag_value(&verify, "--source-digest"), COMMIT);
    assert_eq!(flag_value(&verify, "--source-ref"), "refs/heads/main");
    assert!(verify.contains(&"--deny-self-hosted-runners".to_owned()));
}

/// A release that kbf's workflow attested is not one a channel of another repository accepts:
/// `gh` is asked once, for that repository, and its refusal is the channel's `verify_failed`.
/// For a channel without the new keys it is asked twice, kbf's name and its former one.
#[test]
fn kbfs_former_name_is_accepted_for_kbfs_channel_only() {
    let rig = Rig::new("former-name");
    *rig.gh_exit.borrow_mut() = 1;
    let fault = rig.stage().unwrap_err();
    assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");
    let repos: Vec<String> = (0..rig.installer.borrow().len())
        .map(|i| flag_value(&spawn_args(&rig, i), "--repo"))
        .collect();
    assert_eq!(repos, ["komira-ai/kbf", "komira-ai/komira-build-farm"]);

    let rig = Rig::new("former-name-other");
    put(
        &rig.dir.join("releases.toml"),
        other_channels(Some(&[OTHER_WORKFLOW])),
    );
    rig.offer_as("other", COMMIT, &tar_gz(&[file("kbf", &binary("b"))]));
    *rig.gh_exit.borrow_mut() = 1;
    let fault = rig.stage().unwrap_err();
    assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");
    assert_eq!(rig.installer.borrow().len(), 1);
    assert_eq!(flag_value(&spawn_args(&rig, 0), "--repo"), OTHER_REPO);
}

/// The `tierstore` asset of a channel for komira-ai/tierstore is attested against tierstore's
/// artifacts workflow, then kbf's two (a rollback to the last release kbf shipped), each named
/// whole in one `gh` call; a channel for another asset of the same repository is asked for its
/// own pair only.
#[test]
fn the_tierstore_asset_is_attested_by_tierstore_then_kbf_and_no_other_asset_is() {
    const TIERSTORE: &str = "komira-ai/tierstore";
    const WORKFLOW: &str = "komira-ai/tierstore/.github/workflows/artifacts.yml";
    for (prefix, expected) in [
        (
            "tierstore",
            vec![
                (TIERSTORE, WORKFLOW),
                (
                    "komira-ai/kbf",
                    "komira-ai/kbf/.github/workflows/artifacts.yml",
                ),
                (
                    "komira-ai/komira-build-farm",
                    "komira-ai/komira-build-farm/.github/workflows/artifacts.yml",
                ),
            ],
        ),
        ("kbf", vec![(TIERSTORE, WORKFLOW)]),
    ] {
        let rig = Rig::new("tierstore-signers");
        put(
            &rig.dir.join("releases.toml"),
            format!(
                "trusted_workflows = [\"{WORKFLOW}\"]\n[channels.stable]\nsource = \"github\"\nrepository = \"{TIERSTORE}\"\nsigner_workflow = \"{WORKFLOW}\"\nartifact_prefix = \"{prefix}\"\n"
            ),
        );
        rig.offer_as(prefix, COMMIT, &tar_gz(&[file("kbf", &binary("b"))]));
        *rig.gh_exit.borrow_mut() = 1;
        let fault = rig.stage().unwrap_err();
        assert!(refused(Code::VerifyFailed)(&fault), "{prefix}: {fault:?}");
        let asked: Vec<(String, String)> = (0..rig.installer.borrow().len())
            .map(|i| {
                let args = spawn_args(&rig, i);
                (
                    flag_value(&args, "--repo"),
                    flag_value(&args, "--signer-workflow"),
                )
            })
            .collect();
        let expected: Vec<(String, String)> = expected
            .into_iter()
            .map(|(r, w)| (r.to_owned(), w.to_owned()))
            .collect();
        assert_eq!(asked, expected, "{prefix}");
    }
}

/// A repository or workflow that is not safe is refused as `config_invalid` before anything
/// runs, and so is a prefix on a channel whose file names are fixed.
#[test]
fn a_bad_repository_or_workflow_in_the_channels_file_is_config_invalid() {
    let good = format!(
        "trusted_workflows = [\"{OTHER_WORKFLOW}\"]\n[channels.stable]\nsource = \"github\"\nrepository = \"{OTHER_REPO}\"\nsigner_workflow = \"{OTHER_WORKFLOW}\"\n"
    );
    for (why, text) in [
        (
            "a repository with a space",
            good.replace(
                &format!("repository = \"{OTHER_REPO}\""),
                "repository = \"owner/o ther\"",
            ),
        ),
        (
            "an option as repository",
            good.replace(
                &format!("repository = \"{OTHER_REPO}\""),
                "repository = \"--x/y\"",
            ),
        ),
        (
            "a workflow of another repository",
            good.replace(OTHER_WORKFLOW, "owner/else/.github/workflows/release.yml"),
        ),
        (
            "a workflow outside the workflows directory",
            good.replace(OTHER_WORKFLOW, "owner/other/.github/release.yml"),
        ),
        (
            "a trusted entry that is no repository",
            good.replace(&format!("[\"{OTHER_WORKFLOW}\"]"), "[\"other\"]"),
        ),
        (
            "an unknown key",
            good.replace("source = \"github\"", "source = \"github\"\nsigner = \"x\""),
        ),
    ] {
        let rig = Rig::new("bad-repo");
        put(&rig.dir.join("releases.toml"), text);
        let fault = rig.stage().unwrap_err();
        assert!(refused(Code::ConfigInvalid)(&fault), "{why}: {fault:?}");
        assert!(rig.calls.borrow().is_empty(), "{why}");
    }
}

/// A channel's token file is read as root's, and goes to the listing on its standard input and
/// to the download as `GH_TOKEN`, never in an argument.
#[test]
fn a_channels_token_reaches_the_listing_and_the_download_and_no_argument() {
    let rig = Rig::new("token");
    let token = rig.dir.join("token");
    put(&token, "ghp_secret-token\n");
    put(
        &rig.dir.join("releases.toml"),
        format!(
            "[channels.stable]\nsource = \"github\"\ntoken_file = \"{}\"\n",
            token.display()
        ),
    );
    rig.stage().unwrap();
    let calls = rig.calls.borrow();
    let listing = calls.iter().find(|c| c.program.ends_with("curl")).unwrap();
    assert!(String::from_utf8_lossy(&listing.stdin).contains("ghp_secret-token"));
    let download = calls
        .iter()
        .find(|c| c.args.first().map(String::as_str) == Some("release"))
        .unwrap();
    assert!(
        download
            .env
            .contains(&("GH_TOKEN".to_owned(), "ghp_secret-token".to_owned()))
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.args.iter().any(|a| a.contains("ghp_secret-token")))
    );
}

/// What cannot be staged is refused with a code, and nothing of it stays on the host.
#[test]
fn what_cannot_be_staged_is_refused_and_leaves_nothing() {
    type Break = Box<dyn Fn(&Rig)>;
    let cases: Vec<(&str, Break, Code)> = vec![
        (
            "not-in-the-allowlist",
            Box::new(|rig| {
                put(
                    &rig.dir.join("releases.toml"),
                    "[channels.other]\nsource = \"github\"\n",
                );
            }),
            Code::ChannelNotAllowed,
        ),
        (
            "no-allowlist",
            Box::new(|rig| fs::remove_file(rig.dir.join("releases.toml")).unwrap()),
            Code::ConfigInvalid,
        ),
        (
            "writable-allowlist",
            Box::new(|rig| {
                fs::set_permissions(
                    rig.dir.join("releases.toml"),
                    fs::Permissions::from_mode(0o666),
                )
                .unwrap();
            }),
            Code::ConfigUnsafe,
        ),
        (
            "a-directory-channel",
            Box::new(|rig| {
                put(
                    &rig.dir.join("releases.toml"),
                    "[channels.stable]\ndir = \"/srv/releases\"\n",
                );
            }),
            Code::ConfigInvalid,
        ),
        (
            "a-branch-the-trust-files-do-not-allow",
            Box::new(|rig| {
                put(
                    &rig.dir.join("releases.toml"),
                    "[channels.stable]\nsource = \"github\"\nsource_ref = \"refs/heads/other\"\n",
                );
            }),
            Code::ConfigInvalid,
        ),
        (
            "unsafe-token-file",
            Box::new(|rig| {
                let token = rig.dir.join("token");
                put(&token, "tok");
                fs::set_permissions(&token, fs::Permissions::from_mode(0o666)).unwrap();
                put(
                    &rig.dir.join("releases.toml"),
                    format!(
                        "[channels.stable]\nsource = \"github\"\ntoken_file = \"{}\"\n",
                        token.display()
                    ),
                );
            }),
            Code::ConfigUnsafe,
        ),
        (
            "token-with-a-space",
            Box::new(|rig| {
                let token = rig.dir.join("token");
                put(&token, "two words");
                put(
                    &rig.dir.join("releases.toml"),
                    format!(
                        "[channels.stable]\nsource = \"github\"\ntoken_file = \"{}\"\n",
                        token.display()
                    ),
                );
            }),
            Code::ConfigInvalid,
        ),
        (
            "no-such-commit",
            Box::new(|rig| rig.net.borrow_mut().pages = vec!["[]".into()]),
            Code::UnknownRelease,
        ),
        (
            "two-commits",
            Box::new(|rig| {
                let twin = format!("{}{}", &COMMIT[..12], "f".repeat(28));
                rig.offer(&twin, b"x");
                let first = rig.net.borrow().pages[0].clone();
                rig.offer(COMMIT, &tar_gz(&[file("kbf", &binary("b"))]));
                let both = format!(
                    "{},{}",
                    &first[1..first.len() - 1],
                    &rig.net.borrow().pages[0][1..rig.net.borrow().pages[0].len() - 1]
                );
                rig.net.borrow_mut().pages = vec![format!("[{both}]")];
            }),
            Code::AmbiguousRelease,
        ),
        (
            "listing-fails",
            Box::new(|rig| {
                rig.net.borrow_mut().broken.insert("list-fails");
            }),
            Code::FetchFailed,
        ),
        (
            "rate-limited",
            Box::new(|rig| {
                rig.net.borrow_mut().broken.insert("list-limited");
            }),
            Code::FetchFailed,
        ),
        (
            "download-fails",
            Box::new(|rig| {
                rig.net.borrow_mut().broken.insert("download-fails");
            }),
            Code::FetchFailed,
        ),
        (
            "the-listing-says-too-big",
            Box::new(|rig| rig.offer(COMMIT, &vec![0; 70_000])),
            Code::FetchFailed,
        ),
        (
            "the-file-is-bigger-than-listed",
            Box::new(|rig| {
                let tag = format!("v-{}", &COMMIT[..8]);
                rig.net
                    .borrow_mut()
                    .assets
                    .get_mut(&tag)
                    .unwrap()
                    .insert(tarball_name(COMMIT), vec![0; 70_000]);
            }),
            Code::FetchFailed,
        ),
        (
            "the-tarball-is-a-directory",
            Box::new(|rig| {
                rig.net.borrow_mut().broken.insert("tarball-is-a-directory");
            }),
            Code::FetchFailed,
        ),
        (
            "the-tarball-is-hard-linked",
            Box::new(|rig| {
                rig.net.borrow_mut().broken.insert("tarball-is-hard-linked");
            }),
            Code::FetchFailed,
        ),
        (
            "the-bundle-is-a-directory",
            Box::new(|rig| {
                rig.net.borrow_mut().broken.insert("bundle-is-a-directory");
            }),
            Code::FetchFailed,
        ),
        (
            "the-bundle-is-hard-linked",
            Box::new(|rig| {
                rig.net.borrow_mut().broken.insert("bundle-is-hard-linked");
            }),
            Code::FetchFailed,
        ),
        (
            "the-bundle-is-a-link",
            Box::new(|rig| {
                rig.net.borrow_mut().broken.insert("bundle-is-a-link");
            }),
            Code::FetchFailed,
        ),
        (
            "the-bundle-is-too-big",
            Box::new(|rig| {
                let tag = format!("v-{}", &COMMIT[..8]);
                rig.net
                    .borrow_mut()
                    .assets
                    .get_mut(&tag)
                    .unwrap()
                    .insert(bundle_name(COMMIT), vec![b'x'; 300 * 1024]);
            }),
            Code::FetchFailed,
        ),
        (
            "not-attested",
            Box::new(|rig| *rig.gh_exit.borrow_mut() = 1),
            Code::VerifyFailed,
        ),
        (
            "another-commit",
            Box::new(|rig| *rig.probe.borrow_mut() = what_kbf_says(OTHER)),
            Code::VerifyFailed,
        ),
        (
            "a-binary-of-another-platform",
            Box::new(|rig| {
                let tar = tar_gz(&[file("kbf", &macho(0x0100_000c, "b"))]);
                rig.offer(COMMIT, &tar);
            }),
            Code::VerifyFailed,
        ),
        (
            "a-tarball-with-the-wrong-member",
            Box::new(|rig| {
                let tar = tar_gz(&[file("other", &binary("b"))]);
                rig.offer(COMMIT, &tar);
            }),
            Code::VerifyFailed,
        ),
    ];
    for (name, ruin, code) in cases {
        let rig = Rig::new(&format!("refuse-{}", &name[..name.len().min(18)]));
        ruin(&rig);
        let fault = rig.stage().unwrap_err();
        assert!(refused(code)(&fault), "{name}: {fault:?}");
        rig.assert_nothing_installed();
    }
}

/// A version with no commit in it, and a trust list or settings the host does not believe, are
/// refused before anything is fetched.
#[test]
fn a_version_without_a_commit_and_untrusted_settings_are_refused_before_a_fetch() {
    let rig = Rig::new("nocommit");
    let fault = rig
        .backend
        .stage(&Version::new("1.2.3-1").unwrap(), &stable())
        .unwrap_err();
    assert!(refused(Code::UnknownRelease)(&fault), "{fault:?}");

    // The pinned `gh` is a file someone else owns: nothing is fetched to be checked by it.
    let mut broken = Rig::new("settings");
    let mut trust = trusted_host_on(Platform::Linux);
    let gh = HostPath::new(kbf_installer::fixtures::GH_PATH).unwrap();
    let Some(Entry::File { owner, .. }) = trust.entries.get_mut(&gh) else {
        panic!("the trust list has a gh")
    };
    *owner = Owner {
        uid: 1000,
        gid: 1000,
    };
    broken.backend.trust = Box::new(trust);
    let fault = broken.stage().unwrap_err();
    assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");
    assert!(
        !broken
            .calls
            .borrow()
            .iter()
            .any(|c| c.program.ends_with("curl")),
        "nothing was fetched"
    );

    // Settings that are not parsed: an unknown key.
    let mut odd = Rig::new("settings-odd");
    let mut trust = trusted_host_on(Platform::Linux);
    trust.entries.insert(
        HostPath::new(SETTINGS_PATH).unwrap(),
        Entry::File {
            perms: Perms::new(0o644),
            owner: Owner { uid: 0, gid: 0 },
            bytes: b"SURPRISE=1\n".to_vec(),
        },
    );
    odd.backend.trust = Box::new(trust);
    let fault = odd.stage().unwrap_err();
    assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");
}

/// A scratch the host cannot clear fails the stage before anything is fetched.
#[test]
fn a_scratch_that_cannot_be_cleared_is_a_failure() {
    let rig = Rig::new("stuck");
    let work = rig.root().join("var/lib/kbf-installer/work");
    fs::create_dir_all(&work).unwrap();
    fs::write(
        work.join(format!("{}.fetch", &COMMIT[..12])),
        "a file where a directory goes",
    )
    .unwrap();
    let fault = rig.stage().unwrap_err();
    assert!(
        !fault.refused && fault.code == Code::StageFailed,
        "{fault:?}"
    );
    assert!(
        !rig.calls
            .borrow()
            .iter()
            .any(|c| c.program.ends_with("curl")),
        "nothing was fetched"
    );
}

/// A scratch the host cannot make fails the stage and does not touch the release.
#[test]
fn a_scratch_that_cannot_be_made_is_a_failure() {
    let rig = Rig::new("scratch");
    // The installer's scratch directory cannot be a file, and a stale partial is not reused.
    let work = rig.root().join("var/lib/kbf-installer/work");
    fs::write(&work, "in the way").unwrap();
    let fault = rig.stage().unwrap_err();
    assert!(
        !fault.refused && fault.code == Code::StageFailed,
        "{fault:?}"
    );
    fs::remove_file(&work).unwrap();

    fs::create_dir_all(work.join(format!("{COMMIT}.partial"))).unwrap();
    let fault = rig.stage().unwrap_err();
    assert!(
        !fault.refused && fault.code == Code::StageFailed,
        "{fault:?}"
    );
    assert!(
        work.join(format!("{COMMIT}.partial")).exists(),
        "not ours to remove"
    );
}
