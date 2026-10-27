use std::io;
use std::time::Duration;

use kbf_host::path::Root;

use super::*;
use crate::fixtures::{COMMIT, FakeRunner, me, ran, trusted_host};
use crate::layout::Platform;

fn verifier(refs: &[&str]) -> GhCheck {
    let host = Layout::new(Root::new("/").expect("absolute"), Platform::Linux);
    let mut settings = Settings::defaults(&host);
    settings.run_as = me();
    settings.source_refs = refs
        .iter()
        .map(|r| SourceRef::new(r).expect("ref"))
        .collect();
    settings.timeout = Duration::from_secs(9);
    GhCheck::pinned(&host, &settings)
}

fn commit() -> CommitId {
    CommitId::new(COMMIT).expect("commit")
}

fn verify(v: &GhCheck, runner: &FakeRunner) -> Result<Verified, Refusal> {
    let trust = trusted_host();
    let tools = Tools {
        trust: &trust,
        runner,
    };
    v.verify(
        &tools,
        Path::new("/work/gh"),
        Path::new("/work/in/kbf.tar.gz"),
        Path::new("/work/in/provenance.jsonl"),
        &commit(),
    )
}

fn strings(args: &[OsString]) -> Vec<&str> {
    args.iter().map(|a| a.to_str().expect("utf-8")).collect()
}

/// Catches an argument list that is not the pinned one: every flag the check depends on
/// must be present with its value, in the order the dev installer passed them, and the
/// file and bundle are the stage's copies. A missing `--source-digest` would accept the
/// release of any commit the workflow ever attested; a missing
/// `--deny-self-hosted-runners` would accept a build from a runner anyone can register; a
/// missing `--signer-workflow` or `--repo` would accept another workflow or repository.
///
/// Catches: `--source-digest` dropped; `--deny-self-hosted-runners` dropped;
/// the `--signer-workflow` value changed.
#[test]
fn gh_is_asked_for_exactly_the_pinned_attestation() {
    let runner = FakeRunner::new(|_| Ok(ran(0, "", "")));
    let v = verifier(&["refs/heads/main"]);
    let verified = verify(&v, &runner).expect("accepted");
    assert_eq!(verified.source_ref.as_str(), "refs/heads/main");

    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.program, PathBuf::from("/usr/share/kbf/trust/gh"));
    assert_eq!(
        strings(&call.args),
        [
            "attestation",
            "verify",
            "/work/in/kbf.tar.gz",
            "--bundle",
            "/work/in/provenance.jsonl",
            "--custom-trusted-root",
            "/usr/share/kbf/trust/trusted_root.jsonl",
            "--repo",
            "komira-ai/kbf",
            "--signer-workflow",
            "komira-ai/kbf/.github/workflows/artifacts.yml",
            "--cert-oidc-issuer",
            "https://token.actions.githubusercontent.com",
            "--source-ref",
            "refs/heads/main",
            "--source-digest",
            COMMIT,
            "--predicate-type",
            "https://slsa.dev/provenance/v1",
            "--deny-self-hosted-runners",
        ]
    );
    assert_eq!(call.run_as, Some(me()));
    assert_eq!(call.timeout, Duration::from_secs(9));
    assert_eq!(call.cwd, PathBuf::from("/work/gh"));
}

/// Catches an environment that leaks the host's: `gh` gets the nine variables listed and
/// no others, the `PATH` is the fixed one, home and config are the scratch directory, and
/// all three proxy variables name the dead proxy (the check must be offline whatever the
/// host's network does); no token is in it.
///
/// Catches: the proxy variables dropped; a `GH_TOKEN` from the process
/// environment added; `PATH` taken from the process.
#[test]
fn gh_gets_a_fixed_environment_and_a_dead_proxy() {
    let runner = FakeRunner::new(|_| Ok(ran(0, "", "")));
    verify(&verifier(&["refs/heads/main"]), &runner).expect("accepted");
    let env = &runner.calls()[0].env;
    let mut got: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    got.sort_unstable();
    assert_eq!(
        got,
        [
            ("ALL_PROXY", "http://127.0.0.1:9"),
            ("GH_CONFIG_DIR", "/work/gh/config"),
            ("GH_NO_UPDATE_NOTIFIER", "1"),
            ("GH_PROMPT_DISABLED", "1"),
            ("HOME", "/work/gh"),
            ("HTTPS_PROXY", "http://127.0.0.1:9"),
            ("HTTP_PROXY", "http://127.0.0.1:9"),
            ("NO_COLOR", "1"),
            ("PATH", "/usr/bin:/bin"),
        ]
    );
}

/// Catches the first ref that works not being the one reported, and a ref being skipped:
/// with two allowed refs `gh` is run for each in order until one accepts, and the one
/// that accepted is reported.
#[test]
fn each_allowed_ref_is_tried_in_order_until_one_is_accepted() {
    let runner = FakeRunner::new(|s| {
        let accepted = s.args.iter().any(|a| a == "refs/heads/release/2");
        Ok(ran(i32::from(!accepted), "", "no matching attestation"))
    });
    let v = verifier(&[
        "refs/heads/main",
        "refs/heads/release/1",
        "refs/heads/release/2",
    ]);
    let verified = verify(&v, &runner).expect("the third ref");
    assert_eq!(verified.source_ref.as_str(), "refs/heads/release/2");
    let tried: Vec<String> = runner
        .calls()
        .iter()
        .map(|c| c.args[14].to_str().expect("utf-8").to_owned())
        .collect();
    assert_eq!(
        tried,
        [
            "refs/heads/main",
            "refs/heads/release/1",
            "refs/heads/release/2"
        ]
    );
}

/// `gh` run for the signer pair named in `args` (`--repo`, `--signer-workflow`).
fn pair(args: &[OsString]) -> (String, String) {
    let at = |flag: &str| {
        let i = args.iter().position(|a| a == flag).expect(flag) + 1;
        args[i].to_str().expect("utf-8").to_owned()
    };
    (at("--repo"), at("--signer-workflow"))
}

/// Catches the rename locking out releases attested before it (and the new name being
/// refused): the verifier accepts the current repository first, then the former one,
/// each as a pair (never the repository of one with the workflow of the other); a
/// release only the former name vouches for is accepted on the second call.
///
/// Catches: the former signer left out of `pinned` (a release attested under
/// the old name is `not-attested`); the pair's repository taken from one signer and its
/// workflow from the other.
#[test]
fn a_release_attested_under_the_former_name_is_accepted_in_the_transition() {
    let former = FakeRunner::new(|s| {
        let accepted = pair(&s.args).0 == FORMER_REPO;
        Ok(ran(i32::from(!accepted), "", "no matching attestation"))
    });
    verify(&verifier(&["refs/heads/main"]), &former).expect("former name accepted");
    let tried: Vec<(String, String)> = former.calls().iter().map(|c| pair(&c.args)).collect();
    assert_eq!(
        tried,
        [
            (REPO.to_owned(), SIGNER_WORKFLOW.to_owned()),
            (FORMER_REPO.to_owned(), FORMER_SIGNER_WORKFLOW.to_owned()),
        ]
    );

    let current = FakeRunner::new(|s| Ok(ran(i32::from(pair(&s.args).0 != REPO), "", "")));
    verify(&verifier(&["refs/heads/main"]), &current).expect("current name accepted");
    assert_eq!(current.calls().len(), 1);
}

/// Catches a third name being accepted: a `gh` that vouches only for a repository that is
/// neither the current nor the former name is never asked for it, so the release is
/// `not-attested` after exactly the two pinned signers.
///
/// Catches: `signers` taken from the environment or from a file of the host's settings
/// (the default settings name kbf's two).
#[test]
fn a_third_name_is_refused() {
    let other = FakeRunner::new(|s| {
        let (repo, workflow) = pair(&s.args);
        let accepted = repo == "komira-ai/komira" || workflow.starts_with("komira-ai/komira/");
        Ok(ran(i32::from(!accepted), "", "no matching attestation"))
    });
    let err = verify(&verifier(&["refs/heads/main"]), &other).expect_err("refused");
    assert_eq!(err.code(), "not-attested");
    assert_eq!(other.calls().len(), 2);
}

/// Catches the settings' signer being ignored: a host whose settings name one signer (a release
/// channel of another repository) has `gh` asked for that repository and workflow, once per
/// ref, and never for kbf's names; a `gh` that vouches for kbf only does not accept it.
///
/// Catches: `pinned` building `signers` from the constants instead of the settings; the former
/// name added to a signer that is not kbf's.
#[test]
fn the_settings_signer_alone_is_asked_for() {
    let host = Layout::new(Root::new("/").expect("absolute"), Platform::Linux);
    let mut settings = Settings::defaults(&host);
    settings.run_as = me();
    assert_eq!(
        settings.signers,
        [Signer::current(), Signer::former()],
        "the default is kbf's two"
    );
    settings.signers = vec![Signer {
        repo: "owner/other".to_owned(),
        workflow: "owner/other/.github/workflows/release.yml".to_owned(),
    }];
    let v = GhCheck::pinned(&host, &settings);
    let accepting =
        FakeRunner::new(|s| Ok(ran(i32::from(pair(&s.args).0 != "owner/other"), "", "")));
    verify(&v, &accepting).expect("accepted");
    let asked: Vec<(String, String)> = accepting.calls().iter().map(|c| pair(&c.args)).collect();
    assert_eq!(
        asked,
        [(
            "owner/other".to_owned(),
            "owner/other/.github/workflows/release.yml".to_owned()
        )]
    );

    let kbf_only = FakeRunner::new(|s| Ok(ran(i32::from(pair(&s.args).0 != REPO), "", "")));
    let err = verify(&v, &kbf_only).expect_err("kbf's attestation is not this channel's");
    assert_eq!(err.code(), "not-attested");
    assert_eq!(kbf_only.calls().len(), 1);
}

/// Catches a `gh` verdict other than success being accepted: an exit other than 0 for
/// every ref is `NotAttested`, naming the file and `gh`'s first line of complaint with
/// control characters replaced; a `gh` killed by a signal (no exit code) is refused too.
///
/// Catches: the exit status not read (any run accepted).
#[test]
fn a_gh_verdict_other_than_success_is_not_attested() {
    let runner = FakeRunner::new(|_| Ok(ran(1, "", "verification failed\u{1b}[0m\nsecond line")));
    let err = verify(&verifier(&["refs/heads/main"]), &runner).expect_err("refused");
    assert_eq!(err.code(), "not-attested");
    assert_eq!(
        err.to_string(),
        "\"kbf.tar.gz\" is not attested by the pinned workflow for this commit: \
         verification failed?[0m"
    );

    let signalled = FakeRunner::new(|_| {
        Ok(Ran {
            code: None,
            ..ran(0, "", "")
        })
    });
    let err = verify(&verifier(&["refs/heads/main"]), &signalled).expect_err("refused");
    assert_eq!(err.code(), "not-attested");

    // No ref to try is no attestation either.
    let none = FakeRunner::new(|_| Ok(ran(0, "", "")));
    let mut v = verifier(&["refs/heads/main"]);
    v.source_refs.clear();
    let err = verify(&v, &none).expect_err("refused");
    assert_eq!((err.code(), none.calls().len()), ("not-attested", 0));
}

/// Catches a `gh` that could not be started, or ran out of time, being read as a verdict:
/// each is its own refusal, and neither is acceptance.
#[test]
fn a_gh_that_cannot_run_or_runs_out_of_time_is_its_own_refusal() {
    let missing = FakeRunner::new(|_| Err(io::Error::from(io::ErrorKind::NotFound)));
    let err = verify(&verifier(&["refs/heads/main"]), &missing).expect_err("refused");
    assert_eq!(err.code(), "tool-failed");

    let slow = FakeRunner::new(|_| {
        Ok(Ran {
            code: None,
            timed_out: true,
            ..ran(0, "", "")
        })
    });
    let err = verify(&verifier(&["refs/heads/main"]), &slow).expect_err("refused");
    assert_eq!(err.code(), "timed-out");
}

/// Catches a trust list that is checked after `gh` ran, or not at all: with a `gh` whose
/// directory others can write the run never happens, and the refusal names the path.
///
/// Catches: the trust check removed from `verify`.
#[test]
fn an_untrusted_gh_is_refused_before_it_runs() {
    let runner = FakeRunner::new(|_| Ok(ran(0, "", "")));
    let mut trust = trusted_host();
    trust.entries.insert(
        HostPath::new(crate::fixtures::TRUST_DIR).expect("path"),
        kbf_host::exec::Entry::Dir {
            perms: kbf_host::step::Perms::new(0o777),
            owner: kbf_host::step::Owner { uid: 0, gid: 0 },
        },
    );
    let tools = Tools {
        trust: &trust,
        runner: &runner,
    };
    let err = verifier(&["refs/heads/main"])
        .verify(
            &tools,
            Path::new("/w"),
            Path::new("/w/t.tar.gz"),
            Path::new("/w/b"),
            &commit(),
        )
        .expect_err("refused");
    assert_eq!(err.code(), "untrusted-writable");
    assert!(
        err.to_string().starts_with(crate::fixtures::TRUST_DIR),
        "{err}"
    );
    assert!(runner.calls().is_empty());

    // The trusted root alone being wrong is refused too.
    let mut trust = trusted_host();
    trust.entries.insert(
        HostPath::new(crate::fixtures::TRUSTED_ROOT_PATH).expect("path"),
        kbf_host::exec::Entry::File {
            perms: kbf_host::step::Perms::new(0o644),
            owner: kbf_host::step::Owner { uid: 501, gid: 0 },
            bytes: Vec::new(),
        },
    );
    let err = verifier(&["refs/heads/main"])
        .check_trust(&trust)
        .expect_err("refused");
    assert_eq!(err.code(), "untrusted-owner");
    assert!(
        err.to_string()
            .starts_with(crate::fixtures::TRUSTED_ROOT_PATH),
        "{err}"
    );
}

/// Catches a ref that is not a branch ref reaching `gh` as an argument: an option, a
/// path out of `refs/heads`, a space, an empty name, a doubled or trailing `/` and an
/// empty component are refused; branch names with `/`, `.`, `_` and `-` are not.
///
/// Catches: the character check dropped (a ref with a space passes).
#[test]
fn only_a_plain_branch_ref_is_a_source_ref() {
    for ok in [
        "refs/heads/main",
        "refs/heads/team/topic-2",
        "refs/heads/release_1.2",
    ] {
        assert_eq!(SourceRef::new(ok).expect(ok).as_str(), ok);
    }
    for bad in [
        "",
        "main",
        "refs/heads/",
        "refs/tags/v1",
        "refs/pull/1/merge",
        "--source-ref",
        "refs/heads/a b",
        "refs/heads/a\nb",
        "refs/heads//a",
        "refs/heads/a//b",
        "refs/heads/a/",
        "refs/heads/../x",
        "refs/heads/a;b",
        "refs/heads/é",
    ] {
        assert_eq!(SourceRef::new(bad), Err(NotARef(bad.to_owned())), "{bad:?}");
    }
    assert!(
        NotARef("x".into())
            .to_string()
            .starts_with("\"x\" is not a branch ref")
    );
}

/// Catches the pinned values drifting from the workflow they must name.
#[test]
fn the_pinned_values_are_the_artifacts_workflows() {
    assert_eq!(REPO, "komira-ai/kbf");
    assert!(SIGNER_WORKFLOW.starts_with(REPO));
    assert!(SIGNER_WORKFLOW.ends_with("/.github/workflows/artifacts.yml"));
    assert_eq!(FORMER_REPO, "komira-ai/komira-build-farm");
    assert!(FORMER_SIGNER_WORKFLOW.starts_with(FORMER_REPO));
    assert!(FORMER_SIGNER_WORKFLOW.ends_with("/.github/workflows/artifacts.yml"));
    assert_eq!(ISSUER, "https://token.actions.githubusercontent.com");
    assert_eq!(PREDICATE, "https://slsa.dev/provenance/v1");
}
