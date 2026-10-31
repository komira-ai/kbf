//! `cargo xtask release fetch-previous` as the pipeline runs it, with `xtask-fake` as `gh`. The
//! fake is GitHub: its `gh-api.json` is the API as each test writes it (the release tags, the
//! compare of each against N, the annotated tag and the release of the one that wins), its
//! `releases/<tag>/` the files that release holds, and its `attested` the attestations the
//! artifacts workflow made of them.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use xtask::bundles::DEAD_PROXY;
use xtask::package::tarball;

#[path = "support/release_fake.rs"]
mod release_fake;
use release_fake::{
    Fx, REPO, Ran, SBOM, bundle_flags, bundle_json, mode_of, names_in, refs, refs_path,
    release_answer, s, sha_of, value,
};

/// N, the commit whose previous release is asked for.
const SHA: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
/// The commit the tag of the previous release names.
const PREV: &str = "abcdef0123456789abcdef0123456789abcdef01";
/// The tag of the previous release in most tests.
const TAG: &str = "v0.1.2";
/// The id of a tag object, for the tests that write its answer by hand.
const OBJ: &str = "1111111111111111111111111111111111111111";
const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/release-fetch-previous.txt"
);

/// The tarballs each release holds, `(binary, platform)`, each with its SBOM and bundle.
const ASSETS: [(&str, &str); 3] = [
    ("kbf", "linux-x86_64"),
    ("kbf-server", "linux-x86_64"),
    ("kbf", "darwin-arm64"),
];

/// What `binary` of `commit` for `platform` is: bytes that tell which one it is.
fn asset_bytes(binary: &str, platform: &str, commit: &str) -> Vec<u8> {
    format!("{binary} of {commit} for {platform}\n").into_bytes()
}

/// GitHub's compare of the tag `tag` against N.
fn compare(tag: &str, status: &str) -> (String, Value) {
    release_fake::compare(tag, SHA, status)
}

/// The ref of the annotated tag `tag`, and the tag object it names, which names `commit`;
/// and GitHub's compare of `commit` against N, which says `ahead`.
fn annotated(tag: &str, commit: &str) -> Vec<(String, Value)> {
    release_fake::annotated(tag, commit, SHA)
}

/// What GitHub holds of the tag that wins: its annotated tag, naming `commit`, and its
/// release.
fn found(tag: &str, commit: &str) -> Vec<(String, Value)> {
    release_fake::found(tag, commit, SHA)
}

/// The arguments of `release fetch-previous` for `sha` into `out`: platform linux-x86_64 and the
/// binaries `kbf` and `kbf-server`.
fn args_for<'a>(sha: &'a str, out: &'a str) -> Vec<&'a str> {
    let mut args = vec!["release", "fetch-previous", "--repo", REPO, "--sha", sha];
    args.extend(["--platform", "linux-x86_64", "--binary", "kbf"]);
    args.extend(["--binary", "kbf-server", "--out", out]);
    // A release that is not found is asked for again with no pause between the asks.
    args.extend(["--release-poll-secs", "0"]);
    args
}

/// One test's world: a directory holding the fake as `gh/gh` and an empty `tmp/` (the
/// verb's `TMPDIR`); the verb unpacks into `out/`.
fn fx(name: &str) -> Fx {
    release_fake::fx("xtask-release-fetch-previous", name)
}

/// A world where `TAG` is the previous release: `ahead` of N, annotated, and published.
fn world(name: &str) -> Fx {
    let f = fx(name);
    f.github(&[(TAG, "ahead")], found(TAG, PREV));
    f.release(TAG, PREV);
    f
}

impl Fx {
    /// Where the binaries are unpacked.
    fn out(&self) -> PathBuf {
        self.root.join("out")
    }

    /// A file of the release `TAG` as the fake gh holds it.
    fn asset(&self, name: &str) -> PathBuf {
        self.root.join("gh/releases").join(TAG).join(name)
    }

    /// GitHub with the release tags `tags`, `(tag, status of its compare against N)`, and
    /// the answers `more`.
    fn github(&self, tags: &[(&str, &str)], more: Vec<(String, Value)>) {
        self.github_for(SHA, tags, more);
    }

    /// The release `tag` of `commit` as the pipeline publishes it: a tarball for each of
    /// [`ASSETS`] with its SBOM and bundle, the version facts of each `kbf`, and the `SHA256SUMS`; and the artifacts
    /// workflow's attestations of them. The tarball of `kbf-server` holds the entry
    /// `server_entry`.
    fn release_with(&self, tag: &str, commit: &str, server_entry: &str) {
        let mut assets = Vec::new();
        let mut facts = Vec::new();
        for (binary, platform) in ASSETS {
            let entry = if binary == "kbf-server" {
                server_entry
            } else {
                binary
            };
            let tgz = tarball(entry, &asset_bytes(binary, platform, commit)).expect("tarball");
            let stem = format!("{binary}-{commit}-{platform}");
            if binary == "kbf" {
                facts.push((
                    stem.clone(),
                    format!("{{\"stem\":\"{stem}\"}}\n").into_bytes(),
                ));
            }
            assets.push((stem, tgz));
        }
        self.publish_with_facts(tag, commit, &assets, &facts);
    }

    /// The release `tag` of `commit`, whole.
    fn release(&self, tag: &str, commit: &str) {
        self.release_with(tag, commit, "kbf-server");
    }

    /// `release fetch-previous` for N into `out/`, with `extra` flags and `TMPDIR` at `tmp`.
    fn run_in(&self, tmp: &Path, extra: &[&str], defect: Option<&str>) -> Ran {
        let out = self.out();
        let mut args = args_for(SHA, s(&out));
        args.extend(extra);
        self.xtask(tmp, &args, defect)
    }

    /// `release fetch-previous` for N into `out/`, with `extra` flags.
    fn run(&self, extra: &[&str], defect: Option<&str>) -> Ran {
        self.run_in(&self.root.join("tmp"), extra, defect)
    }

    /// A refusal: exit status 1 with `why` on stderr and nothing on stdout, no `out/` made
    /// and no scratch directory left.
    fn assert_refused(&self, ran: Ran, why: &str) {
        let (code, out, err) = ran;
        assert_eq!(code, Some(1), "{err}");
        assert!(err.contains(why), "wanted {why:?} in {err}");
        assert_eq!(out, "");
        assert!(!self.out().exists(), "out/ was made");
        assert!(self.tmp_is_empty(), "the scratch directory was left");
    }
}

#[test]
fn the_previous_release_is_verified_and_its_binaries_are_unpacked_into_out() {
    // Catches: the wrong platform's tarball taken (the darwin `kbf` says another thing), the
    // object id of the ref printed in place of the tag's commit, a file written without
    // the executable bit or writable by others, a file written that was not asked for, a
    // scratch left behind, a download without `--repo` (the checkout's repository would be
    // used).
    let f = world("accept");
    let (code, out, err) = f.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "previous-tag"), TAG);
    assert_eq!(value(&out, "previous-sha"), PREV);
    assert_eq!(value(&out, "previous-source"), "release");
    assert_eq!(names_in(&f.out()), ["kbf", "kbf-server"]);
    for binary in ["kbf", "kbf-server"] {
        let path = f.out().join(binary);
        let want = asset_bytes(binary, "linux-x86_64", PREV);
        let got = std::fs::read(&path).expect("binary");
        assert_eq!(got, want, "{binary}");
        // Owner-executable, and not writable by group or others whatever the umask.
        let mode = mode_of(&path);
        assert_ne!(mode & 0o100, 0, "{binary}: mode {mode:o}");
        assert_eq!(mode & 0o022, 0, "{binary}: mode {mode:o}");
    }
    assert!(f.tmp_is_empty(), "the scratch directory was left");
    // The release is asked of the repository named, not of the checkout's.
    let asked = f.gh_args();
    let download = format!("release download {TAG} --repo {REPO} --dir ");
    assert!(asked.contains(&download), "{asked}");
}

#[test]
fn the_highest_tag_is_found_by_number_not_by_text() {
    // Catches: tags ordered as text, which puts v0.0.9 above v0.0.10 and unpacks an older
    // build than the previous release.
    let f = fx("numeric");
    let tags = [
        ("v0.0.2", "ahead"),
        ("v0.0.9", "ahead"),
        ("v0.0.10", "ahead"),
    ];
    f.github(&tags, found("v0.0.10", PREV));
    f.release("v0.0.10", PREV);
    let (code, out, err) = f.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "previous-tag"), "v0.0.10");
    let asked = f.gh_args();
    assert!(!asked.contains("compare/v0.0.9"), "{asked}");
}

#[test]
fn suffixed_and_odd_tags_are_never_the_previous_release() {
    // Catches: a `-rc1` or `-probe` tag, or one that is not three plain numbers, read as a
    // release: all of them are `ahead` here and above v0.0.3, so any would be taken.
    let f = fx("suffix");
    let tags = [
        ("v0.0.3", "ahead"),
        ("v0.0.5-rc1", "ahead"),
        ("v0.0.7-probe", "ahead"),
        ("v0.0.08", "ahead"),
        ("v0.0", "ahead"),
    ];
    f.github(&tags, found("v0.0.3", PREV));
    f.release("v0.0.3", PREV);
    let (code, out, err) = f.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "previous-tag"), "v0.0.3");
    let asked = f.gh_args();
    assert!(!asked.contains("rc1"), "{asked}");
    assert!(!asked.contains("probe"), "{asked}");
}

#[test]
fn a_tag_that_is_not_an_ancestor_is_passed_over() {
    // Catches: the ancestor check dropped or widened: a tag the commit is behind (a newer
    // release) or has diverged from, taken because it is the highest.
    for status in ["behind", "diverged"] {
        let f = fx(&format!("passed-over-{status}"));
        let tags = [("v0.0.1", "ahead"), ("v0.0.2", status)];
        f.github(&tags, found("v0.0.1", PREV));
        f.release("v0.0.1", PREV);
        let (code, out, err) = f.run(&[], None);
        assert_eq!(code, Some(0), "{status}: {err}");
        assert_eq!(value(&out, "previous-tag"), "v0.0.1", "{status}");
        let asked = f.gh_args();
        assert!(asked.contains("compare/v0.0.2"), "{status}: {asked}");
    }
}

#[test]
fn the_tag_of_the_commit_itself_is_not_its_previous_release() {
    // Catches: `identical` taken for an ancestor, which makes N its own N-1 when N is
    // already tagged (a re-run of the release pipeline).
    let f = fx("own-tag");
    let tags = [("v0.0.1", "ahead"), ("v0.0.2", "identical")];
    f.github(&tags, found("v0.0.1", PREV));
    f.release("v0.0.1", PREV);
    let (code, out, err) = f.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "previous-tag"), "v0.0.1");
}

/// With `tags`, the verb succeeds with `previous-source=none`, appends only that to the
/// output file, makes no `out/` and goes no further than the compares.
fn says_none(name: &str, tags: &[(&str, &str)]) {
    let f = fx(name);
    f.github(tags, Vec::new());
    let output = f.root.join("github-output");
    std::fs::write(&output, "earlier=line\n").expect("output");
    let (code, out, err) = f.run(&["--output", s(&output)], None);
    assert_eq!(code, Some(0), "{name}: {err}");
    assert_eq!(out, "previous-source=none\n", "{name}");
    let appended = std::fs::read_to_string(&output).expect("output");
    let want = "earlier=line\nprevious-source=none\n";
    assert_eq!(appended, want, "{name}");
    assert!(!f.out().exists(), "{name}: out/ was made");
    let asked = f.gh_args();
    for later in ["git/ref", "releases/tags", "release download"] {
        assert!(!asked.contains(later), "{later} in {asked}");
    }
}

#[test]
fn a_repository_with_no_release_tag_has_no_previous_release() {
    // Catches: no tag at all failing the run (the first release has no N-1) or unpacking
    // something.
    says_none("no-tags", &[]);
}

#[test]
fn a_commit_whose_only_tag_is_its_own_has_no_previous_release() {
    // Catches: N's own tag read as its N-1.
    says_none("only-own-tag", &[("v0.0.1", "identical")]);
}

#[test]
fn tags_that_are_not_ancestors_leave_no_previous_release() {
    // Catches: a tag that is newer than the commit, or on another line of history, taken
    // for the previous release when nothing else qualifies.
    let tags = [("v0.0.1", "behind"), ("v0.0.2", "diverged")];
    says_none("no-ancestors", &tags);
}

#[test]
fn tags_are_read_page_after_page() {
    // Catches: only the first 100 tags read, which hides the highest on page 2 (the highest
    // of page 1 is an ancestor too, but it has no release: it would fail, not pass).
    let f = fx("pages");
    let page1: Vec<Value> = (1..=100)
        .map(|n| json!({ "ref": format!("refs/tags/v0.0.{n}") }))
        .collect();
    let mut all = vec![
        (refs_path(1), Value::Array(page1)),
        (refs_path(2), json!([{ "ref": "refs/tags/v0.0.101" }])),
        compare("v0.0.101", "ahead"),
        compare("v0.0.100", "ahead"),
    ];
    all.extend(found("v0.0.101", PREV));
    f.answers(&all);
    f.release("v0.0.101", PREV);
    let (code, out, err) = f.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "previous-tag"), "v0.0.101");
    let asked = f.gh_args();
    assert_eq!(asked.matches("matching-refs").count(), 2, "{asked}");
    assert!(!asked.contains("page=3"), "{asked}");
}

#[test]
fn a_tag_with_no_release_is_an_error_not_a_step_back_to_a_lower_tag() {
    // Catches: GitHub's 404 for the release read as "no previous release" (exit 0, `none`),
    // or skipped for the next lower tag (exit 0, a wrong N-1), and a download tried.
    let f = fx("no-release");
    let tags = [("v0.0.1", "ahead"), ("v0.0.2", "ahead")];
    let mut more = found("v0.0.1", PREV);
    more.extend(annotated("v0.0.2", PREV));
    f.github(&tags, more);
    f.release("v0.0.1", PREV);
    let ran = f.run(&[], None);
    f.assert_refused(ran, "releases/tags/v0.0.2: not found (HTTP 404)");
    let asked = f.gh_args();
    assert!(!asked.contains("release download"), "{asked}");
}

#[test]
fn a_release_that_is_still_a_draft_is_waited_for_and_then_fetched() {
    // Catches: GitHub's 404 for the previous tag's release while it is still a draft (its tag
    // is pushed minutes before the release is published) taken as an error at the first ask,
    // which turned every push to main in that window red; and a step back to a lower tag
    // instead of waiting for the release before N.
    let f = fx("draft");
    let tags = [("v0.0.1", "ahead"), (TAG, "ahead")];
    let mut more = found(TAG, PREV);
    more.extend(annotated("v0.0.1", PREV));
    f.github(&tags, more);
    f.release(TAG, PREV);
    std::fs::write(f.root.join("gh/gh-fault"), format!("draft-release:{TAG}:2")).expect("fault");
    let (code, out, err) = f.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "previous-tag"), TAG);
    assert_eq!(names_in(&f.out()), ["kbf", "kbf-server"]);
    assert!(err.contains("not found yet"), "{err}");
    let asked = std::fs::read_to_string(f.root.join("gh/gh-draft-asked")).expect("asked");
    assert_eq!(asked.trim(), "3", "two drafts, then the published release");
}

#[test]
fn a_release_never_published_is_still_an_error_after_the_asks() {
    // Catches: the wait made endless, or its end taken as "no previous release" (exit 0,
    // `none`) instead of the error a missing release is.
    let f = world("never-published");
    std::fs::write(
        f.root.join("gh/gh-fault"),
        format!("draft-release:{TAG}:99"),
    )
    .expect("fault");
    let ran = f.run(&["--release-attempts", "3"], None);
    f.assert_refused(
        ran,
        &format!("releases/tags/{TAG}: not found (HTTP 404), asked 3 times"),
    );
    let asked = std::fs::read_to_string(f.root.join("gh/gh-draft-asked")).expect("asked");
    assert_eq!(asked.trim(), "3");
}

#[test]
fn no_asks_at_all_is_refused_as_a_usage_error() {
    // Catches: `--release-attempts 0` taken as one ask, or as no ask (no previous release).
    let f = world("zero-asks");
    let (code, out, err) = f.run(&["--release-attempts", "0"], None);
    assert_eq!(code, Some(2), "{err}");
    assert_eq!(out, "");
    assert!(err.contains("--release-attempts"), "{err}");
}

#[test]
fn a_failed_call_is_an_error_and_never_no_tags() {
    // Catches: an API failure (HTTP 502) or an answer that is not JSON read as "no release
    // tags": exit 0 and `previous-source=none`, a green job on an N-1 test that never ran.
    for (defect, why) in [("api-fails", "HTTP 502"), ("api-garbage", "not JSON")] {
        let f = world(&format!("failed-{defect}"));
        let output = f.root.join("github-output");
        let ran = f.run(&["--output", s(&output)], Some(defect));
        f.assert_refused(ran, why);
        assert!(!output.exists(), "{defect}: wrote an output");
    }
}

#[test]
fn a_compare_github_cannot_answer_is_an_error_not_a_skip() {
    // Catches: a 404 or an unknown status for the highest tag read as "not an ancestor",
    // which steps down to v0.0.1 (exit 0, a wrong N-1) instead of failing.
    let f = fx("compare-404");
    let mut all = vec![refs(&["v0.0.1", "v0.0.2"]), compare("v0.0.1", "ahead")];
    all.extend(found("v0.0.1", PREV));
    f.answers(&all);
    f.release("v0.0.1", PREV);
    let ran = f.run(&[], None);
    let why = format!("compare/v0.0.2...{SHA}: not found (HTTP 404)");
    f.assert_refused(ran, &why);

    let f = fx("compare-weird");
    let tags = [("v0.0.1", "ahead"), ("v0.0.2", "weird")];
    f.github(&tags, found("v0.0.1", PREV));
    f.release("v0.0.1", PREV);
    let ran = f.run(&[], None);
    f.assert_refused(ran, "says \"weird\"");
}

/// With the answers `more` for the winning tag (and its release published), the verb
/// refuses with `why` and downloads nothing.
fn refuses_tag(name: &str, more: Vec<(String, Value)>, why: &str) {
    let f = fx(name);
    f.github(&[(TAG, "ahead")], more);
    f.release(TAG, PREV);
    let ran = f.run(&[], None);
    f.assert_refused(ran, why);
    let asked = f.gh_args();
    assert!(!asked.contains("release download"), "{name}: {asked}");
}

#[test]
fn a_tag_that_is_not_annotated_or_not_a_commit_or_not_there_is_refused() {
    // Catches: a lightweight tag taken as a release (its ref names the commit, so the tag
    // has no audit record to read), a tag object that names a tree taken for a commit, and a ref
    // or a tag object GitHub does not have taken for "no previous release".
    let ref_path = format!("repos/{REPO}/git/ref/tags/{TAG}");
    let tag_path = format!("repos/{REPO}/git/tags/{OBJ}");
    let to_tag = json!({ "object": { "type": "tag", "sha": OBJ } });
    let to_commit = json!({ "object": { "type": "commit", "sha": PREV } });
    let to_tree = json!({ "object": { "type": "tree", "sha": PREV } });
    let lightweight = vec![(ref_path.clone(), to_commit), release_answer(TAG)];
    refuses_tag("lightweight", lightweight, "is not an annotated tag");
    let tree = vec![
        (ref_path.clone(), to_tag.clone()),
        (tag_path, to_tree),
        release_answer(TAG),
    ];
    refuses_tag("tree", tree, "does not name a commit");
    let no_ref = vec![release_answer(TAG)];
    refuses_tag("no-ref", no_ref, "git/ref/tags/v0.1.2: not found");
    let no_object = vec![(ref_path, to_tag), release_answer(TAG)];
    refuses_tag("no-object", no_object, "git/tags/11111111");
}

#[test]
fn a_tag_whose_commit_is_not_an_ancestor_of_the_commit_is_refused() {
    // Catches: the commit a tag names taken on the word of the tag's name alone: the scan
    // compared `v0.1.2...N` and said `ahead`, but the commit the tag object names is N
    // itself, a newer one, one on another line of history, or one GitHub cannot compare.
    let mut gone = found(TAG, PREV);
    let compared = compare(PREV, "ahead").0;
    gone.retain(|(path, _)| *path != compared);
    let why = format!("compare/{PREV}...{SHA}: not found");
    refuses_tag("commit-compare-404", gone, &why);
    for status in ["identical", "behind", "diverged", "weird"] {
        let mut more = found(TAG, PREV);
        more.push(compare(PREV, status));
        let why = format!("no ancestor of {SHA}: compare says \"{status}\"");
        refuses_tag(&format!("commit-{status}"), more, &why);
    }
}

#[test]
fn a_download_that_fails_is_an_error() {
    // Catches: a failed `gh release download` ignored, so that whatever is in the scratch
    // directory is verified or unpacked.
    let f = world("download-fails");
    let ran = f.run(&[], Some("release-fails"));
    f.assert_refused(ran, "gh release download: xtask-fake: release failed");
}

/// The release `TAG` is damaged by `damage` after it is published: the verb refuses with
/// `why`, unpacks nothing, appends nothing to `--output` and leaves no scratch.
fn refuses_damaged(name: &str, damage: fn(&Fx), why: &str) {
    let f = world(name);
    damage(&f);
    let output = f.root.join("github-output");
    let ran = f.run(&["--output", s(&output)], None);
    f.assert_refused(ran, why);
    assert!(!output.exists(), "{name}: wrote an output");
}

fn damage_sum(f: &Fx) {
    let name = format!("kbf-server-{PREV}-linux-x86_64.tar.gz");
    std::fs::write(f.asset(&name), b"other bytes").expect("write");
}

fn damage_unlisted(f: &Fx) {
    std::fs::write(f.asset("extra.txt"), b"stray\n").expect("write");
}

fn damage_no_bundle(f: &Fx) {
    let name = format!("kbf-{PREV}-linux-x86_64.tar.gz.sigstore.json");
    std::fs::remove_file(f.asset(&name)).expect("remove");
}

/// The bundle of `kbf` attests N, not the commit of the release.
fn damage_bundle_commit(f: &Fx) {
    let name = format!("kbf-{PREV}-linux-x86_64.tar.gz");
    let sha = sha_of(&std::fs::read(f.asset(&name)).expect("tarball"));
    let bundle = f.asset(&format!("{name}.sigstore.json"));
    std::fs::write(bundle, bundle_json(&sha, &bundle_flags(SHA))).expect("write");
}

fn damage_unattested(f: &Fx) {
    std::fs::write(f.root.join("gh/attested"), "").expect("write");
}

/// Every attestation stays but the SBOM attestations of the tarballs.
fn damage_no_sbom(f: &Fx) {
    let store = f.root.join("gh/attested");
    let text = std::fs::read_to_string(&store).expect("attested");
    let kept: Vec<&str> = text.lines().filter(|l| !l.ends_with(SBOM)).collect();
    std::fs::write(&store, format!("{}\n", kept.join("\n"))).expect("write");
}

/// The release lists the `kbf` tarball and no longer its version facts: a release made before
/// the build jobs recorded them.
fn damage_no_version_facts(f: &Fx) {
    let facts = format!("kbf-{PREV}-linux-x86_64.version.json");
    std::fs::remove_file(f.asset(&facts)).expect("remove");
    let sums = f.asset("SHA256SUMS");
    let text = std::fs::read_to_string(&sums).expect("sums");
    let kept: String = text
        .lines()
        .filter(|l| !l.ends_with(&facts))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_ne!(kept, text, "the facts were listed");
    std::fs::write(&sums, kept).expect("write");
}

#[test]
fn a_previous_release_without_the_version_facts_of_its_kbf_is_refused() {
    // Catches: the previous release read with the facts waived (as it was for the release made
    // before the build jobs recorded them), so a release without them is unpacked and run.
    let why = format!(
        "kbf-{PREV}-linux-x86_64.tar.gz: its version facts kbf-{PREV}-linux-x86_64.version.json \
         are not listed in SHA256SUMS"
    );
    refuses_damaged("no-version-facts", damage_no_version_facts, &why);
}

#[test]
fn a_tarball_that_does_not_match_its_line_is_refused() {
    // Catches: `release check` skipped, or the sums not compared with the bytes: the
    // attestations alone would fail with another message, and the sums never be read.
    refuses_damaged("sum", damage_sum, "does not match its line");
}

#[test]
fn a_file_the_sums_do_not_list_is_refused() {
    // Catches: everything downloaded trusted, not only what SHA256SUMS lists: nothing else
    // checks that a stray file is absent.
    let why = "not listed in SHA256SUMS: extra.txt";
    refuses_damaged("unlisted", damage_unlisted, why);
}

#[test]
fn a_tarball_without_its_bundle_is_refused() {
    // Catches: a release a host could not verify offline accepted.
    refuses_damaged("no-bundle", damage_no_bundle, "no provenance bundle");
}

#[test]
fn a_bundle_that_attests_another_commit_is_refused() {
    // Catches: the attestation check skipped, or made without the commit: the bundle is a
    // well-formed bundle of the very tarball, for N instead of the release's commit.
    let why = "tar.gz.sigstore.json does not verify";
    refuses_damaged("bundle-commit", damage_bundle_commit, why);
}

#[test]
fn files_nobody_attested_are_refused() {
    // Catches: the attestation check skipped: every other check passes on this release.
    let why = "no build provenance attestation signed by";
    refuses_damaged("unattested", damage_unattested, why);
}

#[test]
fn a_tarball_without_its_sbom_attestation_is_refused() {
    // Catches: only provenance asked for, as a deployment would not.
    let why = "no https://cyclonedx.org/bom attestation signed by";
    refuses_damaged("no-sbom", damage_no_sbom, why);
}

#[test]
fn a_binary_the_release_lacks_writes_nothing() {
    // Catches: files written one by one as they are read (kbf would be in out/ when
    // kbf-nothere fails), and a missing tarball that the error does not name.
    let f = world("lacks");
    let ran = f.run(&["--binary", "kbf-nothere"], None);
    let why = format!("kbf-nothere-{PREV}-linux-x86_64.tar.gz");
    f.assert_refused(ran, &why);
}

#[test]
fn a_tarball_that_is_not_the_binary_is_not_unpacked() {
    // Catches: whatever a verified tarball holds written out as the binary: here the sums,
    // bundles and attestations all hold, and the kbf-server tarball's one entry is named
    // otherwise, which `package` never writes.
    let f = fx("not-the-binary");
    f.github(&[(TAG, "ahead")], found(TAG, PREV));
    f.release_with(TAG, PREV, "other");
    let ran = f.run(&[], None);
    f.assert_refused(ran, "holds no kbf-server");
}

#[test]
fn an_out_that_cannot_be_made_or_written_is_an_error_and_nothing_is_replaced() {
    // Catches: --out being a file ignored, and a binary already in out/ replaced.
    let f = world("out-is-a-file");
    std::fs::write(f.out(), "not a directory").expect("out");
    let (code, _, err) = f.run(&[], None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("create "), "{err}");
    let kept = std::fs::read_to_string(f.out()).expect("out");
    assert_eq!(kept, "not a directory");

    let f = world("out-has-kbf");
    std::fs::create_dir_all(f.out()).expect("out");
    std::fs::write(f.out().join("kbf"), "mine").expect("kbf");
    let (code, _, err) = f.run(&[], None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("write "), "{err}");
    let kept = std::fs::read_to_string(f.out().join("kbf")).expect("kbf");
    assert_eq!(kept, "mine");
    assert!(!f.out().join("kbf-server").exists());
}

#[test]
fn a_later_refusal_removes_the_files_this_run_wrote_and_keeps_the_one_that_was_there() {
    // Catches: a half-populated out/: `kbf` is written (it sorts first) and then the
    // `kbf-server` already there is refused, which would leave a `kbf` of this run beside
    // a `kbf-server` of another.
    let f = world("out-has-server");
    std::fs::create_dir_all(f.out()).expect("out");
    std::fs::write(f.out().join("kbf-server"), "mine").expect("kbf-server");
    let (code, _, err) = f.run(&[], None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("write "), "{err}");
    assert_eq!(names_in(&f.out()), ["kbf-server"]);
    let kept = std::fs::read_to_string(f.out().join("kbf-server")).expect("kbf-server");
    assert_eq!(kept, "mine");
}

#[test]
fn a_binary_named_twice_is_unpacked_once_and_out_may_be_nested() {
    // Catches: a repeated --binary written twice, the second time refused as a file
    // already there; and --out made with `create_dir`, which needs its parent.
    let f = world("twice-nested");
    let nested = f.out().join("a/b");
    let mut args = args_for(SHA, s(&nested));
    args.extend(["--binary", "kbf"]);
    let (code, _, err) = f.xtask(&f.root.join("tmp"), &args, None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(names_in(&nested), ["kbf", "kbf-server"]);
}

#[test]
fn a_scratch_directory_that_cannot_be_made_is_an_error() {
    // Catches: a failure to make the download directory ignored, so the download or the
    // verification meets a directory that is not there.
    let f = world("no-scratch");
    let tmp = f.root.join("tmp-is-a-file");
    std::fs::write(&tmp, "x").expect("tmp");
    let (code, _, err) = f.run_in(&tmp, &[], None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("create "), "{err}");
    let asked = f.gh_args();
    assert!(!asked.contains("release download"), "{asked}");
    assert!(!f.out().exists());
}

#[test]
fn output_is_appended_to_the_file_and_printed_as_the_golden_has_it() {
    // Catches: a changed output (the workflow reads these keys), an output that replaces
    // the file's earlier lines, and one that differs from what is printed.
    let f = world("golden");
    let file = f.root.join("github-output");
    std::fs::write(&file, "earlier=line\n").expect("output");
    let (code, out, err) = f.run(&["--output", s(&file)], None);
    assert_eq!(code, Some(0), "{err}");
    let golden = std::fs::read_to_string(GOLDEN).expect("golden file");
    assert_eq!(out, golden);
    assert_eq!(
        std::fs::read_to_string(&file).expect("output"),
        format!("earlier=line\n{golden}")
    );
}

#[test]
fn an_output_file_that_cannot_be_written_fails_the_run() {
    let f = world("bad-output");
    let missing = f.root.join("no-such-dir/out");
    let (code, _, err) = f.run(&["--output", s(&missing)], None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("append to"), "{err}");
    // The output file is written last: the binaries are already in out/ (documented).
    assert_eq!(names_in(&f.out()), ["kbf", "kbf-server"]);
}

#[test]
fn a_trusted_root_makes_the_bundle_checks_offline() {
    // Catches: --trusted-root accepted and not passed on, so the bundles are verified over
    // the network; and a root passed when none was given.
    let f = world("offline");
    let root = f.root.join("trusted_root.jsonl");
    std::fs::write(&root, "{}\n").expect("root");
    let (code, _, err) = f.run(&["--trusted-root", s(&root)], None);
    assert_eq!(code, Some(0), "{err}");
    let asked = f.gh_args();
    assert!(asked.contains("--custom-trusted-root"), "{asked}");
    let env = std::fs::read_to_string(f.root.join("gh/gh-env")).expect("gh-env");
    let proxy = format!("HTTPS_PROXY={DEAD_PROXY}");
    assert!(env.contains(&proxy), "{env}");

    let g = world("online");
    let (code, _, err) = g.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    let asked = g.gh_args();
    assert!(!asked.contains("--custom-trusted-root"), "{asked}");
}

#[test]
fn a_wrong_call_is_a_usage_error_before_anything_is_asked_of_github() {
    // Catches: a short --sha sent to GitHub (a compare would resolve an abbreviation), and
    // a call without a binary to take out accepted.
    let f = world("usage");
    let tmp = f.root.join("tmp");
    let out = f.out();
    let (code, _, err) = f.xtask(&tmp, &args_for("abc", s(&out)), None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--sha"), "{err}");
    assert_eq!(f.gh_args(), "");

    let mut args = vec!["release", "fetch-previous", "--repo", REPO, "--sha", SHA];
    args.extend(["--platform", "linux-x86_64", "--out", s(&out)]);
    let (code, _, err) = f.xtask(&tmp, &args, None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--binary"), "{err}");
    assert_eq!(f.gh_args(), "");
}
