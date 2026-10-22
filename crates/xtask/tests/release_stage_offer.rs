//! `cargo xtask release stage-offer` as the offer job runs it, with `xtask-fake` as `gh`. The
//! fake is GitHub (`support/release_fake.rs`): the release of N (`SHA`, tag `TAG`) is what
//! `release publish` leaves, and the release before it (`PREV`, `PREV_TAG`) is N-1. The
//! `kbf` of a release is the fake itself, answering what the test put in its trailer, and the
//! version facts of each `kbf` are the bytes it prints (the release before N may predate them).

use std::path::{Path, PathBuf};

use kbf_gate::{Beta, OfferDoc, SetHash, Target};
use serde_json::{Value, json};
use xtask::package::tarball;
use xtask::release::stage_offer::{BUNDLE_FILE, OFFER_FILE, WAIVER_OPERATOR, waiver_reason};

#[path = "support/release_fake.rs"]
mod release_fake;
use release_fake::{
    Fx, REPO, Ran, annotated, bundle_flags, bundle_json, entries_of, entry, found, identity,
    kbf_bytes, names_in, s, sha_of, tag_answers, value,
};

/// N, the commit whose release is staged.
const SHA: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
/// The commit of the release before N.
const PREV: &str = "abcdef0123456789abcdef0123456789abcdef01";
/// The tag of N's release.
const TAG: &str = "v0.1.2";
/// The tag of the release before it.
const PREV_TAG: &str = "v0.1.1";
const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/",
    "stage-offer.txt"
);

/// A tarball of a release: the `stem` of its name (`<binary>-<commit>-<platform>`), the tarball,
/// and the version facts the build job kept of the `kbf` in it (none for another binary, and
/// for a release made before the facts).
struct Asset {
    stem: String,
    tgz: Vec<u8>,
    facts: Option<Vec<u8>>,
}

/// `binary` of `commit` for `platform`, with no facts: bytes that tell which one it is.
fn plain(binary: &str, platform: &str, commit: &str) -> Asset {
    let bytes = format!("{binary} of {commit} for {platform}\n");
    let tgz = tarball(binary, bytes.as_bytes()).expect("tarball");
    let stem = format!("{binary}-{commit}-{platform}");
    Asset {
        stem,
        tgz,
        facts: None,
    }
}

/// What the `kbf` of `commit` for `platform` prints for `--version --json`, as the facts of a
/// `kbf` that is never run: its own for each platform.
fn says(platform: &str, commit: &str) -> String {
    format!(
        "{{\"commit\":\"{}\",\"meta_log\":4,\"platform\":\"{platform}\",\"profile\":\"release\",\
         \"protocol\":1}}\n",
        &commit[..12]
    )
}

/// The `kbf` of `commit` for `platform`, which is never run, with its facts.
fn plain_kbf(platform: &str, commit: &str) -> Asset {
    let mut asset = plain("kbf", platform, commit);
    asset.facts = Some(says(platform, commit).into_bytes());
    asset
}

/// What a fake `kbf` told to answer `json` prints: the answer, without the `tamper <name>`
/// line that makes it change a file first.
fn printed(json: &str) -> String {
    let asked = json
        .strip_prefix("tamper ")
        .and_then(|t| t.split_once('\n'));
    asked.map_or(json, |(_, answer)| answer).to_owned()
}

/// The `kbf` of `commit` for `platform` that runs: the fake, which answers `json` to
/// `--version --json`, and whose facts are that answer.
fn runnable(platform: &str, commit: &str, json: &str) -> Asset {
    let tgz = tarball("kbf", &kbf_bytes(&commit[..12], json)).expect("tarball");
    Asset {
        stem: format!("kbf-{commit}-{platform}"),
        tgz,
        facts: Some(printed(json).into_bytes()),
    }
}

/// The assets of N as the artifacts workflow builds them: the `kbf` of linux-x86_64, which
/// answers `json`, and the others, which do not run (the `kbf` of linux-arm64 and of the Mac,
/// and a `kbf-installer`): a Mac runs its `kbf`, and the workflow builds no `kbf-daemon` for it.
fn standard(json: &str) -> Vec<Asset> {
    vec![
        runnable("linux-x86_64", SHA, json),
        plain_kbf("linux-arm64", SHA),
        plain_kbf("darwin-arm64", SHA),
        plain("kbf-installer", "linux-x86_64", SHA),
    ]
}

/// What GitHub holds of the release before N.
#[derive(Clone, Copy)]
enum Prev<'a> {
    /// None: N's tag is the only one.
    Nothing,
    /// `PREV_TAG` is a release of `PREV` whose `kbf` answers the first for linux-x86_64 and
    /// the second for linux-arm64.
    Released(&'a str, &'a str),
    /// [`Prev::Released`], made before the build jobs recorded version facts: it has none.
    Predating(&'a str, &'a str),
    /// `PREV_TAG` is a tag with no release.
    Unreleased,
}

fn fx(name: &str) -> Fx {
    release_fake::fx("xtask-release-stage-offer", name)
}

/// A world where N is released with `assets`, and `prev` is the release before it.
fn world(name: &str, assets: Vec<Asset>, prev: Prev) -> Fx {
    let f = fx(name);
    let mut tags = vec![(TAG, "identical")];
    let mut more = tag_answers(TAG, SHA);
    match prev {
        Prev::Nothing => {}
        Prev::Released(..) | Prev::Predating(..) => {
            tags.push((PREV_TAG, "ahead"));
            more.extend(found(PREV_TAG, PREV, SHA));
        }
        Prev::Unreleased => {
            tags.push((PREV_TAG, "ahead"));
            more.extend(annotated(PREV_TAG, PREV, SHA));
        }
    }
    f.github_for(SHA, &tags, more);
    f.publish_assets(TAG, SHA, &assets);
    if let Prev::Released(x86_64, arm64) | Prev::Predating(x86_64, arm64) = prev {
        let mut before = [
            runnable("linux-x86_64", PREV, x86_64),
            runnable("linux-arm64", PREV, arm64),
            plain_kbf("darwin-arm64", PREV),
        ];
        if matches!(prev, Prev::Predating(..)) {
            for asset in &mut before {
                asset.facts = None;
            }
        }
        f.publish_assets(PREV_TAG, PREV, &before);
    }
    f
}

/// The release of N as the artifacts workflow builds it (`meta_log` 4), with none before it.
fn released(name: &str) -> Fx {
    world(name, standard(&identity(SHA, 4)), Prev::Nothing)
}

/// The arguments of `release stage-offer` for `sha` into `out`, on a runner of the platform
/// `runner` (none: the flag is not given).
fn args_for<'a>(sha: &'a str, out: &'a Path, runner: Option<&'a str>) -> Vec<&'a str> {
    let mut args = vec!["release", "stage-offer", "--repo", REPO, "--sha", sha];
    args.extend(["--out", s(out)]);
    if let Some(platform) = runner {
        args.extend(["--runner-platform", platform]);
    }
    args
}

impl Fx {
    /// The release `tag` of `commit` with `assets`: their tarballs, and the version facts of
    /// those that have them.
    fn publish_assets(&self, tag: &str, commit: &str, assets: &[Asset]) {
        let tarballs: Vec<(String, Vec<u8>)> = assets
            .iter()
            .map(|a| (a.stem.clone(), a.tgz.clone()))
            .collect();
        let facts: Vec<(String, Vec<u8>)> = assets
            .iter()
            .filter_map(|a| a.facts.clone().map(|f| (a.stem.clone(), f)))
            .collect();
        self.publish_with_facts(tag, commit, &tarballs, &facts);
    }

    /// Where the verb stages the offer.
    fn out(&self) -> PathBuf {
        self.root.join("out")
    }

    fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// A file of the release of N as the fake gh holds it.
    fn asset(&self, name: &str) -> PathBuf {
        self.root.join("gh/releases").join(TAG).join(name)
    }

    /// `release stage-offer` for N into `out/` on a runner of the platform `runner` (none:
    /// the flag is not given), with `extra` flags and `TMPDIR` at `tmp`.
    fn run_in(
        &self,
        tmp: &Path,
        runner: Option<&str>,
        extra: &[&str],
        defect: Option<&str>,
    ) -> Ran {
        let out = self.out();
        let mut args = args_for(SHA, &out, runner);
        args.extend(extra);
        self.xtask(tmp, &args, defect)
    }

    fn run_with(&self, runner: Option<&str>, extra: &[&str], defect: Option<&str>) -> Ran {
        self.run_in(&self.tmp(), runner, extra, defect)
    }

    /// On a linux-x86_64 runner.
    fn run(&self, extra: &[&str], defect: Option<&str>) -> Ran {
        self.run_with(Some("linux-x86_64"), extra, defect)
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

    /// The offer the verb wrote, read as a deploy reads it.
    fn offer(&self) -> OfferDoc {
        let bytes = std::fs::read(self.out().join(OFFER_FILE)).expect("offer.json");
        OfferDoc::from_bytes(&bytes).expect("an offer a reader accepts")
    }

    /// The names of the entries of the bundle the verb wrote.
    fn bundle_names(&self) -> Vec<String> {
        let bytes = std::fs::read(self.out().join(BUNDLE_FILE)).expect("bundle.tar");
        let entries = entries_of(&bytes);
        entries.into_iter().map(|e| e.name).collect()
    }
}

/// The fields of the offer the golden pins.
fn offer_lines(doc: &OfferDoc) -> String {
    let target = match doc.target {
        Target::Gamma => "gamma",
        Target::Prod => "prod",
    };
    let beta = match &doc.beta {
        Beta::Waived { waiver } => format!("waived by {}: {}", waiver.by, waiver.reason),
        Beta::Attested { run_id } => format!("attested by run {run_id}"),
    };
    let previous = doc.previous.as_ref().map_or("none", |p| p.as_str());
    let meta_log = match doc.meta_log {
        Some(n) => n.to_string(),
        None => "unknown".to_owned(),
    };
    format!(
        "offer:\ntarget={target}\ncommit={}\nplatforms={}\nbeta={beta}\nmeta_log={meta_log}\n\
         previous={previous}\nreversible={}\n",
        doc.commit.as_str(),
        doc.platforms.join(","),
        doc.reversible
    )
}

#[test]
fn a_released_commit_is_staged_as_the_golden_has_it() {
    // Catches: a changed output (the workflow reads these keys), an offer for another stage
    // or another waiver than the fixed one (golden), a reason that does not name the tag,
    // a bundle whose entries are not those the installer reads (SHA256SUMS first, then the
    // offer, then each platform's version facts and `kbf` with its attestation, in the order
    // of the platform list, with no SBOM and no `kbf-installer`), version facts left out of
    // the bundle or changed (the installer uploads them for the store to make a node's offer
    // from), an attestation under the name of another tarball, a changed copy of SHA256SUMS,
    // a header that is not fixed, a Mac left out of the offer (its `kbf` is what it runs), an
    // offer that names a set other than the bundle's, an `offer.json` in the bundle that the
    // installer's `from_bundle` does not read as the offer written to `out/`, a `created`
    // that is not the clock's, and output that replaces the file's earlier lines.
    // Mutants it catches: the facts not appended to the bundle (or appended after the
    // tarballs); the Mac dropped from the platforms; `offer.json` appended under another name.
    let f = released("golden");
    let output = f.root.join("github-output");
    std::fs::write(&output, "earlier=line\n").expect("output");
    let before = kbf_gate::system_clock();
    let (code, out, err) = f.run(&["--output", s(&output)], None);
    let after = kbf_gate::system_clock();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(names_in(&f.out()), [BUNDLE_FILE, OFFER_FILE]);
    assert!(!err.contains("left out of the offer"), "{err}");

    let doc = f.offer();
    let made = doc.created;
    assert!((before..=after).contains(&made), "{made}");
    match &doc.beta {
        Beta::Waived { waiver } => {
            assert_eq!(waiver.what, "beta");
            assert_eq!(waiver.by, WAIVER_OPERATOR);
            assert_eq!(waiver.reason, waiver_reason(TAG));
        }
        Beta::Attested { .. } => panic!("beta was not waived"),
    }

    let tar_bytes = std::fs::read(f.out().join(BUNDLE_FILE)).expect("bundle.tar");
    let entries = entries_of(&tar_bytes);
    for e in &entries {
        let header = (e.mode, e.uid, e.gid, e.mtime);
        assert_eq!(header, (0o644, 0, 0, 0), "{}", e.name);
    }
    let release = |name: &str| std::fs::read(f.asset(name)).expect("a file of the release");
    let sums = entry(&entries, "SHA256SUMS");
    assert_eq!(sums.bytes, release("SHA256SUMS"));
    assert_eq!(doc.set_hash, SetHash::of_sums(&sums.bytes));
    let offer_bytes = std::fs::read(f.out().join(OFFER_FILE)).expect("offer.json");
    assert_eq!(entry(&entries, OFFER_FILE).bytes, offer_bytes);
    // The offer in the bundle is the offer on disk, as a document.
    let in_bundle = OfferDoc::from_bytes(&offer_bytes).expect("the bundle's offer is an offer");
    assert_eq!(in_bundle, doc);
    for platform in ["linux-x86_64", "linux-arm64", "darwin-arm64"] {
        let facts = format!("kbf-{SHA}-{platform}.version.json");
        assert_eq!(entry(&entries, &facts).bytes, release(&facts), "{facts}");
        let name = format!("kbf-{SHA}-{platform}.tar.gz");
        let packed = entry(&entries, &name);
        assert_eq!(packed.bytes, release(&name), "{name}");
        let att = format!("{name}.attestation");
        let bundle = release(&format!("{name}.sigstore.json"));
        assert_eq!(entry(&entries, &att).bytes, bundle, "{att}");
    }

    let mut actual = out.clone();
    actual.push_str("entries:\n");
    for e in &entries {
        actual.push_str(&format!("{}\n", e.name));
    }
    actual.push_str(&offer_lines(&doc));
    let golden = std::fs::read_to_string(GOLDEN).expect("golden file");
    assert_eq!(actual, golden);
    let appended = std::fs::read_to_string(&output).expect("output");
    assert_eq!(appended, format!("earlier=line\n{out}"));
    assert!(f.tmp_is_empty(), "the scratch directory was left");
    // The release is asked of the repository named, not of the checkout's.
    let download = format!("release download {TAG} --repo {REPO} --dir ");
    assert!(f.gh_args().contains(&download), "{}", f.gh_args());
}

/// With `tags`, `(tag, status of its compare against N)`, the verb succeeds with
/// `staged=false`, appends only that to the output file, makes no `out/` and goes no further
/// than the tags.
fn says_no_release(name: &str, tags: &[(&str, &str)]) {
    let f = fx(name);
    f.github_for(SHA, tags, Vec::new());
    let output = f.root.join("github-output");
    let (code, out, err) = f.run(&["--output", s(&output)], None);
    assert_eq!(code, Some(0), "{name}: {err}");
    let detail = format!("no release tag names {SHA}: nothing to stage");
    let want = format!("staged=false\nreason=no-release\ndetail={detail}\n");
    assert_eq!(out, want, "{name}");
    assert_eq!(std::fs::read_to_string(&output).expect("output"), want);
    assert!(!f.out().exists(), "{name}: out/ was made");
    assert!(f.tmp_is_empty(), "{name}: a scratch directory was made");
    let asked = f.gh_args();
    for later in ["git/ref", "release download", "attestation"] {
        assert!(!asked.contains(later), "{name}: {later} in {asked}");
    }
}

#[test]
fn a_commit_that_released_nothing_is_not_staged_and_that_is_not_an_error() {
    // Catches: a commit with no release tag, or only an older one (a push that changed the
    // docs), staged for the older tag or failed: the workflow would go red on every such
    // push, or deploy a release the commit is not.
    says_no_release("untagged", &[]);
    says_no_release("older-tag", &[(PREV_TAG, "ahead")]);
}

#[test]
fn a_release_newer_than_the_commit_is_an_error_and_nothing_is_downloaded() {
    // Catches: Rule S skipped (a re-run of an old run would deploy over a newer release), and
    // `behind` or `diverged` read as "the commit is at or after the tag".
    for status in ["behind", "diverged"] {
        let f = fx(&format!("superseded-{status}"));
        let tags = [(TAG, "identical"), ("v0.1.3", status)];
        f.github_for(SHA, &tags, tag_answers(TAG, SHA));
        let ran = f.run(&[], None);
        f.assert_refused(ran, &format!("compare says {status}"));
        assert!(!f.gh_args().contains("release download"), "{status}");
    }
}

/// With the answers `more` for the tag (the highest, at N), the verb is refused with `why`,
/// and nothing is downloaded.
fn refuses_tag(name: &str, more: Vec<(String, Value)>, why: &str) {
    let f = fx(name);
    f.github_for(SHA, &[(TAG, "identical")], more);
    let ran = f.run(&[], None);
    f.assert_refused(ran, why);
    assert!(!f.gh_args().contains("release download"), "{name}");
}

#[test]
fn a_tag_that_is_not_annotated_or_names_another_commit_or_is_not_there_is_an_error() {
    // Catches: the tag taken on the word of the compare alone (it says `identical` in every
    // case here): a lightweight tag, which carries no audit record; an annotated tag whose object
    // names another commit; a tag GitHub does not list a ref for.
    let ref_path = format!("repos/{REPO}/git/ref/tags/{TAG}");
    let to_commit = json!({ "object": { "type": "commit", "sha": SHA } });
    let lightweight = vec![(ref_path, to_commit)];
    refuses_tag("lightweight", lightweight, "is not an annotated tag");
    let why = format!("names {PREV}, not {SHA}");
    refuses_tag("elsewhere", tag_answers(TAG, PREV), &why);
    refuses_tag("no-ref", Vec::new(), "git/ref/tags/v0.1.2: not found");
}

/// The release of N, published with a `kbf` that fails when it runs, is damaged by `damage`:
/// the verb refuses with `why`, and what it refuses with is not the failure of that `kbf`
/// (`--version --json`): it was never run.
fn refuses_damaged(name: &str, damage: fn(&Fx), why: &str) {
    let f = world(name, standard("fail\n"), Prev::Nothing);
    damage(&f);
    let output = f.root.join("github-output");
    let ran = f.run(&["--output", s(&output)], None);
    let ran_kbf = ran.2.contains("--version --json");
    assert!(!ran_kbf, "the kbf ran: {}", ran.2);
    f.assert_refused(ran, why);
    assert!(!output.exists(), "{name}: wrote an output");
}

fn damage_sum(f: &Fx) {
    let name = format!("kbf-{SHA}-linux-arm64.tar.gz");
    std::fs::write(f.asset(&name), b"other bytes").expect("write");
}

fn damage_no_bundle(f: &Fx) {
    let name = format!("kbf-{SHA}-linux-arm64.tar.gz.sigstore.json");
    std::fs::remove_file(f.asset(&name)).expect("remove");
}

fn damage_unattested(f: &Fx) {
    std::fs::write(f.root.join("gh/attested"), "").expect("write");
}

/// The bundle of the linux-arm64 tarball attests PREV, not N.
fn damage_bundle_commit(f: &Fx) {
    let name = format!("kbf-{SHA}-linux-arm64.tar.gz");
    let sha = sha_of(&std::fs::read(f.asset(&name)).expect("tarball"));
    let bundle = f.asset(&format!("{name}.sigstore.json"));
    std::fs::write(bundle, bundle_json(&sha, &bundle_flags(PREV))).expect("write");
}

/// The version facts of the linux-arm64 `kbf` are other bytes than the sums list.
fn damage_facts_sum(f: &Fx) {
    let name = format!("kbf-{SHA}-linux-arm64.version.json");
    std::fs::write(f.asset(&name), b"{\"meta_log\":9}\n").expect("write");
}

/// No attestation of the version facts of the linux-arm64 `kbf` is stored (the other
/// attestations are).
fn damage_facts_unattested(f: &Fx) {
    let name = format!("kbf-{SHA}-linux-arm64.version.json");
    let sha = sha_of(&std::fs::read(f.asset(&name)).expect("facts"));
    let store = f.root.join("gh/attested");
    let text = std::fs::read_to_string(&store).expect("attested");
    let kept: String = text
        .lines()
        .filter(|l| !l.starts_with(&sha))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&store, kept).expect("write");
}

#[test]
fn a_release_that_does_not_verify_is_refused_before_any_of_it_runs() {
    // Catches: the `kbf` taken out and run before the release is verified, or from the files
    // as they were downloaded, or never verified: the runner's `kbf` fails when it runs, so
    // each refusal below would be that failure instead of its own. Each damage is to a file of
    // linux-arm64, which is not the tarball run. The version facts are bundled for the store
    // to make a node's offer from, so they are held to their line of SHA256SUMS and to the
    // artifacts workflow's attestation as a tarball is.
    // Mutants it catches (each red at its own line): the `kbf` run before `check`; `check`
    // skipped; `artifacts` skipped; the facts left out of the digest comparison of the sums
    // (facts-sum); the facts left out of the attestation check (facts-unattested).
    refuses_damaged("sum", damage_sum, "does not match its line");
    refuses_damaged("no-bundle", damage_no_bundle, "no provenance bundle");
    let unattested = "no build provenance attestation signed by";
    refuses_damaged("unattested", damage_unattested, unattested);
    let wrong_commit = "tar.gz.sigstore.json does not verify";
    refuses_damaged("bundle-commit", damage_bundle_commit, wrong_commit);
    refuses_damaged("facts-sum", damage_facts_sum, "does not match its line");
    refuses_damaged("facts-unattested", damage_facts_unattested, unattested);
}

#[test]
fn a_kbf_tarball_without_its_version_facts_is_refused_before_any_of_it_runs() {
    // Catches: `release check` made with the facts waived for the release being offered (the
    // waiver is for the release before): a platform whose facts the build job did not keep
    // would be bundled without them, and the store would keep it and never offer it to a node.
    // The runner's `kbf` fails when it runs, so the refusal is not that failure.
    // Mutant it catches: `Pairing::Waived` in place of `Pairing::Required` in `stage_release`.
    let mut assets = standard("fail\n");
    assets[1].facts = None;
    let f = world("no-facts", assets, Prev::Nothing);
    let ran = f.run(&[], None);
    let ran_kbf = ran.2.contains("--version --json");
    assert!(!ran_kbf, "the kbf ran: {}", ran.2);
    let facts = format!("kbf-{SHA}-linux-arm64.version.json");
    f.assert_refused(ran, &format!("its version facts {facts} are not listed"));
}

#[test]
fn version_facts_that_are_not_what_the_runners_kbf_prints_are_refused() {
    // Catches: facts in the bundle of another build than the one whose `meta_log` the offer
    // states (the facts are what a node's offer is made from, the offer's number is read from
    // the run): the facts of linux-x86_64 say a `meta_log` of 5 where its `kbf` prints 4, and
    // are listed, hashed and attested as they should be, so only the comparison with the run
    // can refuse them. The same release with the facts it prints is staged by the other tests.
    // Mutant it catches: the comparison of the facts with the run removed.
    let mut assets = standard(&identity(SHA, 4));
    assets[0].facts = Some(identity(SHA, 5).into_bytes());
    let f = world("facts-differ", assets, Prev::Nothing);
    let ran = f.run(&[], None);
    let what = format!("kbf-{SHA}-linux-x86_64.version.json is not what the kbf in ");
    f.assert_refused(ran, &what);
}

#[test]
fn a_download_that_fails_is_an_error() {
    // Catches: a failed `gh release download` ignored, so that whatever is in the scratch
    // directory is verified.
    let f = released("download-fails");
    let ran = f.run(&[], Some("release-fails"));
    f.assert_refused(ran, "gh release download: xtask-fake: release failed");
}

#[test]
fn a_kbf_that_fails_or_names_another_commit_is_an_error() {
    // Catches: a failed `kbf --version --json` read as an unknown `meta_log` (the offer
    // would be made, not reversible, from a binary that does not run), and an identity of
    // another build accepted.
    let f = world("kbf-fails", standard("fail\n"), Prev::Nothing);
    let ran = f.run(&[], None);
    f.assert_refused(ran, "--version --json: exit status");
    let other = standard(&identity(PREV, 4));
    let f = world("kbf-other", other, Prev::Nothing);
    let ran = f.run(&[], None);
    f.assert_refused(ran, "names commit");
}

#[test]
fn the_offer_names_the_installer_level_of_the_tree_that_made_it() {
    // Catches: an offer that does not say which installer may deploy it (a host's older
    // installer would then fail half way at the version probe instead of refusing first), and
    // one that names a level other than this tree's installer's.
    // Mutants: `min_installer` left `None`; written as a constant.
    let f = released("min-installer");
    let (code, _, err) = f.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(f.offer().min_installer, Some(kbf_installer::version::LEVEL));
}

#[test]
fn the_release_before_decides_whether_going_back_is_reversible() {
    // Catches: reversibility from anything but the two builds' `meta_log` (N-1's, run, is
    // 5, 4 or 3 against N's 4), `previous` not the commit of the release before, and
    // `previous-source` not saying where it came from.
    for (name, before, want) in [
        ("prev-higher", 5, true),
        ("prev-equal", 4, true),
        ("prev-lower", 3, false),
    ] {
        let answer = identity(PREV, before);
        let assets = standard(&identity(SHA, 4));
        let f = world(name, assets, Prev::Released(&answer, &answer));
        let (code, out, err) = f.run(&[], None);
        assert_eq!(code, Some(0), "{name}: {err}");
        assert_eq!(value(&out, "previous-source"), "release", "{name}");
        assert_eq!(value(&out, "reversible"), want.to_string(), "{name}");
        let doc = f.offer();
        assert_eq!(doc.previous.as_ref().map(|p| p.as_str()), Some(PREV));
        assert_eq!(doc.previous_meta_log, Some(before), "{name}");
        assert_eq!(doc.reversible, want, "{name}");
    }
}

#[test]
fn a_release_before_that_cannot_be_fetched_or_run_is_an_error() {
    // Catches: an N-1 that fails read as "no previous release": the offer would be made
    // not reversible and the deploy would go on over a build that was never looked at.
    let assets = standard(&identity(SHA, 4));
    let f = world("prev-unpublished", assets, Prev::Unreleased);
    let ran = f.run(&[], None);
    f.assert_refused(ran, &format!("releases/tags/{PREV_TAG}: not found"));
    let assets = standard(&identity(SHA, 4));
    let f = world("prev-fails", assets, Prev::Released("fail\n", "fail\n"));
    let ran = f.run(&[], None);
    f.assert_refused(ran, "--version --json: exit status");
}

#[test]
fn the_release_before_is_run_as_the_runner_platform_and_verified_with_the_trusted_root() {
    // Catches: the release before run for linux-x86_64 whatever the runner (its linux-x86_64
    // `kbf` fails here, and its linux-arm64 one says a `meta_log` of 5), and its bundles
    // verified without the trusted root, over the network: every verification of a bundle,
    // of either release, names the root.
    let mut assets = standard(&identity(SHA, 4));
    assets[1] = runnable("linux-arm64", SHA, &identity(SHA, 4));
    let before = identity(PREV, 5);
    let f = world("prev-arm", assets, Prev::Released("fail\n", &before));
    let (code, _, err) = f.run_with(Some("linux-arm64"), &[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(f.offer().previous_meta_log, Some(5));
    let asked = f.gh_args();
    let bundled: Vec<&str> = asked.lines().filter(|l| l.contains("--bundle")).collect();
    assert!(!bundled.is_empty(), "{asked}");
    for line in bundled {
        assert!(line.contains("--custom-trusted-root"), "{line}");
    }
}

#[test]
fn a_release_file_a_kbf_changed_while_it_ran_is_refused_not_bundled() {
    // Catches: the release verified once, before the runs, and bundled as the runs left it
    // (the offer's own check of the files left to a stand-in): the runner's `kbf` appends a
    // byte to the linux-arm64 tarball and makes its line of SHA256SUMS say so, which the
    // hashes agree with and only the attestation of the file does not: the bundle's subject
    // digest is the old file's, so the refusal names the file's bundle and the digest
    // mismatch, and is not the SHA256SUMS line check.
    let answer = identity(SHA, 4);
    let name = format!("kbf-{SHA}-linux-arm64.tar.gz");
    let tamper = format!("tamper {name}\n{answer}");
    let f = world("changed", standard(&tamper), Prev::Nothing);
    let (code, out, err) = f.run(&[], None);
    assert_eq!(code, Some(1), "{err}");
    let named = format!("{name}.sigstore.json: has the subject digest sha256:");
    assert!(err.contains(&named), "wanted {named:?} in {err}");
    assert!(err.contains(", not the tarball's sha256:"), "{err}");
    assert!(!err.contains("does not match its line"), "{err}");
    assert_eq!(out, "");
    assert!(!f.out().exists(), "out/ was made");
    assert!(f.tmp_is_empty(), "the scratch directory was left");
}

/// The names of the entries of the bundle that are the files of `platform`.
fn of_platform(names: &[String], platform: &str) -> Vec<String> {
    let suffix = format!("-{platform}");
    names
        .iter()
        .filter(|n| n.contains(&suffix))
        .cloned()
        .collect()
}

#[test]
fn a_platform_the_release_does_not_complete_is_left_out_and_printed_not_an_error() {
    // Catches: a platform the release has no tarball of staged all the same (its files missing
    // from the bundle, which would fail the run, or named in the offer), and a platform left
    // out without a word: here the release has no `kbf` for the Mac, which a Mac runs, though
    // it holds the `kbf-installer` of linux-x86_64. The Mac is the last of the platform list.
    // Mutants it catches: all platforms handed to the bundle and the offer; the `skipped` list
    // not printed.
    let assets = vec![
        runnable("linux-x86_64", SHA, &identity(SHA, 4)),
        plain_kbf("linux-arm64", SHA),
        plain("kbf-installer", "linux-x86_64", SHA),
    ];
    let f = world("no-mac", assets, Prev::Nothing);
    let (code, out, err) = f.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "platform-skipped"), "darwin-arm64");
    let left_out = "left out of the offer: platform darwin-arm64: no tarball of";
    assert!(err.contains(left_out), "{err}");
    assert_eq!(f.offer().platforms, ["linux-arm64", "linux-x86_64"]);
    let names = f.bundle_names();
    assert!(of_platform(&names, "darwin-arm64").is_empty(), "{names:?}");
    assert_eq!(of_platform(&names, "linux-arm64").len(), 3, "{names:?}");
}

/// The SBOM of the linux-x86_64 `kbf` is gone from the release and from its `SHA256SUMS`.
fn damage_no_sbom(f: &Fx) {
    let sbom = format!("kbf-{SHA}-linux-x86_64.cdx.json");
    std::fs::remove_file(f.asset(&sbom)).expect("remove");
    let sums = f.asset("SHA256SUMS");
    let text = std::fs::read_to_string(&sums).expect("sums");
    let kept: String = text
        .lines()
        .filter(|l| !l.ends_with(&sbom))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&sums, kept).expect("write");
}

#[test]
fn a_release_that_completes_no_platform_is_refused() {
    // Catches: a release that completes no platform offered all the same. The only `kbf` is
    // the runner's, and its SBOM is gone from the release (and from its sums, so the release
    // still checks), so no platform has a `kbf`, its SBOM, its bundle and its facts; the
    // refusal lists what each platform lacks. Nothing is written.
    // Mutant it catches: the `complete.is_empty()` refusal removed (an offer for no platform).
    let assets = vec![runnable("linux-x86_64", SHA, &identity(SHA, 4))];
    let f = world("none", assets, Prev::Nothing);
    damage_no_sbom(&f);
    let ran = f.run(&[], None);
    let no_tarball = "platform darwin-arm64: no tarball of";
    assert!(ran.2.contains(no_tarball), "{}", ran.2);
    f.assert_refused(ran, "platform linux-x86_64: kbf-");
}

#[test]
fn a_release_before_that_predates_the_version_facts_is_still_the_previous_release() {
    // Catches: the facts required of the release before (it is held to what the build jobs of
    // its day did: none recorded them), which would refuse every offer made while the release
    // before is one of those; and a previous release whose `kbf` is not run when it has no
    // facts. N-1 here has none, and its `meta_log` is read from its run: 5 against N's 4.
    // Mutant it catches: `Pairing::Required` in place of `Pairing::Waived` in `fetch_previous`.
    let answer = identity(PREV, 5);
    let assets = standard(&identity(SHA, 4));
    let f = world("prev-predates", assets, Prev::Predating(&answer, &answer));
    let (code, out, err) = f.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "previous-source"), "release");
    assert_eq!(value(&out, "reversible"), "true");
    assert_eq!(f.offer().previous_meta_log, Some(5));
}

#[test]
fn the_kbf_that_runs_is_the_runner_platforms_own() {
    // Catches: the same platform's `kbf` run on every runner: linux-x86_64's says meta_log 4
    // and linux-arm64's says 7. Also the same platform's facts compared with the run: each
    // platform's facts are what its own `kbf` prints, so the other's would be refused.
    // Mutants it catches: the runner's `kbf` taken from the first platform's tarball; the
    // facts compared with the first platform's whatever the runner's.
    for (platform, want) in [("linux-x86_64", 4), ("linux-arm64", 7)] {
        let mut assets = standard(&identity(SHA, 4));
        assets[1] = runnable("linux-arm64", SHA, &identity(SHA, 7));
        let f = world(&format!("runner-{platform}"), assets, Prev::Nothing);
        let (code, _, err) = f.run_with(Some(platform), &[], None);
        assert_eq!(code, Some(0), "{platform}: {err}");
        assert_eq!(f.offer().meta_log, Some(want), "{platform}");
    }
}

#[test]
fn a_runner_platform_the_release_has_no_kbf_for_is_an_error() {
    // Catches: a tarball that is not there, or one with no `kbf` in it, read as a binary to
    // run (or skipped, and the offer made from a build that never ran).
    // Mutant it catches: a runner platform with no tarball, or none with a `kbf`, skipped.
    let assets = vec![runnable("linux-x86_64", SHA, &identity(SHA, 4))];
    let f = world("no-tarball", assets, Prev::Nothing);
    let ran = f.run_with(Some("linux-arm64"), &[], None);
    f.assert_refused(ran, &format!("kbf-{SHA}-linux-arm64.tar.gz"));

    let other = tarball("other", b"not a kbf").expect("tarball");
    let assets = vec![Asset {
        stem: format!("kbf-{SHA}-linux-x86_64"),
        tgz: other,
        facts: Some(says("linux-x86_64", SHA).into_bytes()),
    }];
    let f = world("no-entry", assets, Prev::Nothing);
    let ran = f.run(&[], None);
    f.assert_refused(ran, "holds no kbf");
}

#[cfg(target_os = "linux")]
#[test]
fn without_a_runner_platform_the_hosts_own_is_used() {
    // Catches: a default that is not this host's platform: each platform's `kbf` runs here
    // and says a `meta_log` of its own (4 for linux-x86_64, 7 for linux-arm64, 9 for the
    // Mac), so a host mapped to another platform is found by the offer's.
    // Mutant it catches: the host's architecture mapped to the other Linux platform.
    let assets = vec![
        runnable("linux-x86_64", SHA, &identity(SHA, 4)),
        runnable("linux-arm64", SHA, &identity(SHA, 7)),
        runnable("darwin-arm64", SHA, &identity(SHA, 9)),
    ];
    let f = world("host-platform", assets, Prev::Nothing);
    let (code, out, err) = f.run_with(None, &[], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "staged"), "true");
    let want = if cfg!(target_arch = "aarch64") { 7 } else { 4 };
    assert_eq!(f.offer().meta_log, Some(want));
}

#[test]
fn the_trusted_root_is_asked_of_github_unless_given_and_the_bundles_are_checked_with_it() {
    // Catches: a root fetched when one is given, or not fetched and not used (the bundles
    // would be checked over the network), a root that cannot be fetched ignored, and the
    // fetch made after the download.
    let f = released("root-default");
    let (code, _, err) = f.run(&[], None);
    assert_eq!(code, Some(0), "{err}");
    let asked = f.gh_args();
    assert!(asked.contains("attestation trusted-root"), "{asked}");
    assert!(asked.contains("--custom-trusted-root"), "{asked}");

    let g = released("root-given");
    let root = g.root.join("my-root.jsonl");
    std::fs::write(&root, "{}\n").expect("root");
    let (code, _, err) = g.run(&["--trusted-root", s(&root)], None);
    assert_eq!(code, Some(0), "{err}");
    let asked = g.gh_args();
    assert!(!asked.contains("attestation trusted-root"), "{asked}");
    let used = format!("--custom-trusted-root {}", root.display());
    assert!(asked.contains(&used), "{asked}");

    let h = released("root-fails");
    let ran = h.run(&[], Some("trusted-root-fails"));
    h.assert_refused(ran, "gh attestation trusted-root");
    assert!(!h.gh_args().contains("release download"));
}

#[test]
fn a_flag_that_would_choose_the_stage_or_the_waiver_is_a_usage_error_before_anything_is_asked() {
    // Catches: a `--target`, `--waive-beta` or `--operator` flag (the verb offers to gamma
    // only, with one waiver), a short `--sha` sent to GitHub, a runner platform that is not
    // one, an `--out` that exists (a stale offer would look current), and a help text that
    // does not say the offer is for gamma.
    let f = released("usage");
    for extra in [
        ["--target", "prod"],
        ["--target", "gamma"],
        ["--waive-beta", "why"],
        ["--operator", "me"],
    ] {
        let (code, _, err) = f.run(&extra, None);
        assert_eq!(code, Some(2), "{extra:?}: {err}");
        assert!(err.contains(extra[0]), "{err}");
        assert_eq!(f.gh_args(), "", "{extra:?}");
    }
    let out = f.out();
    let short = args_for("abc", &out, None);
    let (code, _, err) = f.xtask(&f.tmp(), &short, None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--sha"), "{err}");
    let (code, _, err) = f.run_with(Some("linux-riscv"), &[], None);
    assert_eq!(code, Some(2), "{err}");
    assert_eq!(f.gh_args(), "");
    let help_args = ["release", "stage-offer", "--help"];
    let (code, help, _) = f.xtask(&f.tmp(), &help_args, None);
    assert_eq!(code, Some(0));
    assert!(help.contains("gamma"), "{help}");
    std::fs::create_dir_all(&out).expect("out");
    let (code, _, err) = f.run(&[], None);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("exists"), "{err}");
    assert_eq!(f.gh_args(), "");
}

#[test]
fn a_scratch_directory_or_an_out_that_cannot_be_made_is_an_error() {
    // Catches: a failure to make the download directory ignored, so the download meets a
    // directory that is not there, and an `--out` that cannot be made reported as staged.
    let f = released("no-scratch");
    let tmp = f.root.join("tmp-is-a-file");
    std::fs::write(&tmp, "x").expect("tmp");
    let (code, _, err) = f.run_in(&tmp, Some("linux-x86_64"), &[], None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("create "), "{err}");
    assert!(!f.gh_args().contains("release download"));
    assert!(!f.out().exists());

    let g = released("no-out");
    let file = g.root.join("a-file");
    std::fs::write(&file, "x").expect("file");
    let nested = file.join("out");
    let args = args_for(SHA, &nested, Some("linux-x86_64"));
    let (code, _, err) = g.xtask(&g.tmp(), &args, None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("create "), "{err}");
    assert!(g.tmp_is_empty(), "the scratch directory was left");
}

#[test]
fn an_output_file_that_cannot_be_written_fails_the_run() {
    // Catches: the lines for the workflow lost without a failure. The output file is written
    // last (documented): the offer and the bundle are already in out/.
    let f = released("bad-output");
    let missing = f.root.join("no-such-dir/out");
    let (code, _, err) = f.run(&["--output", s(&missing)], None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("append to"), "{err}");
    assert_eq!(names_in(&f.out()), [BUNDLE_FILE, OFFER_FILE]);
}
