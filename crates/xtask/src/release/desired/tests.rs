//! `release desired`: the choice of the release, what it refuses and the targets it writes,
//! run against a fake [`Source`] (GitHub's answers and the CDN's files as the test gives them,
//! every call logged). No network is used and no program is started.

use std::cell::RefCell;
use std::collections::BTreeMap;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

use super::manifest::PARAMS;
use super::repo::{Listed, listed, sums_digest};
use super::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const AMD64: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const ARM64: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const OTHER: &str = "3333333333333333333333333333333333333333333333333333333333333333";

/// A scratch directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-desired-{name}"))
}

/// GitHub and the CDN as a test wrote them.
#[derive(Default)]
struct Fake {
    /// The answer of `gh api <path>`; a path not here is a 404.
    api: BTreeMap<String, Value>,
    /// The text `release download` writes as `SHA256SUMS`; `None` writes nothing.
    sums: Option<String>,
    /// The `Packages` file of an architecture; `None` is a failed read with this reason.
    packages: BTreeMap<String, Result<String, String>>,
    /// Every call, in order.
    log: RefCell<Vec<String>>,
}

impl Source for Fake {
    fn api(&self, path: &str) -> Result<Option<Value>, Error> {
        self.log.borrow_mut().push(format!("api {path}"));
        Ok(self.api.get(path).cloned())
    }

    fn download_sums(&self, repo: &str, tag: &str, dir: &Path) -> Result<(), Error> {
        self.log
            .borrow_mut()
            .push(format!("download {repo} {tag} {}", dir.display()));
        if let Some(sums) = &self.sums {
            std::fs::write(dir.join(SUMS), sums).expect("sums");
        }
        Ok(())
    }

    fn fetch(&self, url: &str, file: &Path) -> Result<Option<String>, Error> {
        self.log.borrow_mut().push(format!("fetch {url}"));
        let arch = url
            .trim_end_matches("/Packages")
            .rsplit("binary-")
            .next()
            .expect("arch");
        match self.packages.get(arch) {
            Some(Ok(text)) => {
                std::fs::write(file, text).expect("packages");
                Ok(None)
            }
            Some(Err(why)) => Ok(Some(why.clone())),
            None => Ok(Some("404".to_owned())),
        }
    }
}

impl Fake {
    fn calls(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

/// A `Packages` file with a stanza of `kbf` at `version` for `arch` with `sha256`, after
/// another package's.
fn packages(arch: &str, version: &str, sha256: &str) -> String {
    format!(
        "Package: other\nVersion: 1.0.0\nFilename: pool/other/other_1.0.0_{arch}.deb\nSHA256: {OTHER}\n\n\
         Package: kbf\nVersion: {version}\nArchitecture: {arch}\nFilename: pool/kbf/kbf_{version}_{arch}.deb\nSize: 10\nSHA256: {sha256}\n"
    )
}

/// The release JSON of a published release holding `assets`.
fn release(assets: &[&str]) -> Value {
    let assets: Vec<Value> = assets.iter().map(|a| json!({ "name": a })).collect();
    json!({ "draft": false, "prerelease": false, "assets": assets })
}

fn amd64_deb() -> String {
    format!("kbf-native-{COMMIT}-linux-x86_64.deb")
}

fn arm64_deb() -> String {
    format!("kbf-native-{COMMIT}-linux-arm64.deb")
}

/// The `SHA256SUMS` of a release that lists both `.deb`s at `AMD64` and `ARM64`.
fn sums() -> String {
    format!(
        "{AMD64}  {}\n{ARM64} *{}\n{OTHER}  kbf-{COMMIT}-linux-x86_64.tar.gz\n",
        amd64_deb(),
        arm64_deb()
    )
}

/// A world with the published release v0.0.66 (its tag names `COMMIT`), `SHA256SUMS` listing
/// both `.deb`s, and a stable suite that lists version 0.1.7472 for both.
fn consistent() -> Fake {
    let mut fake = Fake::default();
    fake.api.insert(
        "repos/o/r/releases/tags/v0.0.66".to_owned(),
        release(&["SHA256SUMS", &amd64_deb()]),
    );
    fake.api.insert(
        "repos/o/r/commits/refs/tags/v0.0.66".to_owned(),
        json!({ "sha": COMMIT }),
    );
    fake.sums = Some(sums());
    fake.packages
        .insert("amd64".to_owned(), Ok(packages("amd64", "0.1.7472", AMD64)));
    fake.packages
        .insert("arm64".to_owned(), Ok(packages("arm64", "0.1.7472", ARM64)));
    fake
}

/// The arguments of a run by version in `dir`.
fn args(dir: &TestDir) -> DesiredArgs {
    DesiredArgs {
        repo: "o/r".to_owned(),
        version: "0.0.66".to_owned(),
        after_run: String::new(),
        dir: dir.join("d"),
        output: Some(dir.join("out")),
        apt_url: "https://apt.example/apt".to_owned(),
        gh: PathBuf::from("gh"),
        curl: PathBuf::from("curl"),
    }
}

/// The refusal's lines.
fn refused(result: Result<Vec<String>, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused(lines)) => lines,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The `targets` value the verb writes for 0.1.7472 with the digests of the two packages: the
/// whole JSON, written out. Hostkeeper's `Target` in proto3 canonical JSON (lowerCamel names,
/// 64-bit integers as strings), one `apt` entry of the stable repository per platform.
fn golden() -> String {
    let entry = |digest: &str| {
        format!(
            "{{\"entries\":[{{\"digest\":\"{digest}\",\"installer\":\"apt\",\"manifest\":{{\"hook\":{{\"cleanupRetries\":3,\"command\":[\"/usr/bin/kbf\",\"package-hook\"],\"timeoutSecs\":{{\"cleanup\":\"120\",\"cutover\":\"600\",\"prepare\":\"0\",\"rollback\":\"900\",\"verify\":\"360\"}},\"verifyDeadlineSecs\":\"1500\"}},\"installerBounds\":{{\"activateSecs\":\"300\",\"removeSecs\":\"300\",\"stageSecs\":\"600\"}},\"minHostkeeper\":\"0.1.225\",\"package\":\"kbf\",\"params\":{{\"mode\":{{\"text\":\"storage-and-daemon\"}},\"state_dir\":{{\"text\":\"/var/lib/kbf-package-hook\"}},\"unit\":{{\"text\":\"kbf-storage-and-daemon.service\"}}}},\"run\":[\"kbf-storage-and-daemon.service\"]}},\"packageVersion\":\"0.1.7472\",\"repository\":\"komira-stable\"}}]}}"
        )
    };
    format!(
        "{{\"linux-arm64\":{},\"linux-x86_64\":{}}}",
        entry(ARM64),
        entry(AMD64)
    )
}

/// Asserts a run by version: the outputs are `set=true`, the version of the stable stanza and
/// the targets, exactly the golden JSON; the printed lines are the summary and the same lines;
/// and the calls are the ones the design says: the release, the tag's commit, the sums, and
/// each architecture's `Packages` over HTTPS.
#[test]
fn a_release_the_stable_suite_lists_is_set_with_its_targets() {
    let dir = scratch("version");
    let fake = consistent();
    let printed = run_with(&fake, &args(&dir)).expect("the run passes");
    let lines = format!("set=true\npackage-version=0.1.7472\ntargets={}\n", golden());
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).expect("output"),
        lines
    );
    assert_eq!(
        printed.join("\n"),
        format!(
            "release v0.0.66: the stable suite lists kbf 0.1.7472 for each of 2 architectures, each with the digest SHA256SUMS lists\n{}",
            lines.trim_end()
        )
    );
    assert_eq!(
        fake.calls(),
        [
            "api repos/o/r/releases/tags/v0.0.66".to_owned(),
            "api repos/o/r/commits/refs/tags/v0.0.66".to_owned(),
            format!("download o/r v0.0.66 {}", dir.join("d").display()),
            "fetch https://apt.example/apt/dists/stable/main/binary-amd64/Packages".to_owned(),
            "fetch https://apt.example/apt/dists/stable/main/binary-arm64/Packages".to_owned(),
        ]
    );
}

/// Asserts that the output is appended to what the file holds (Actions' `GITHUB_OUTPUT` has
/// other steps' lines) and that no `--output` still prints the lines.
#[test]
fn the_outputs_are_appended_and_optional() {
    let dir = scratch("append");
    let fake = consistent();
    let mut args = args(&dir);
    std::fs::write(dir.join("out"), "earlier=1\n").expect("file");
    run_with(&fake, &args).expect("passes");
    assert!(
        std::fs::read_to_string(dir.join("out"))
            .expect("output")
            .starts_with("earlier=1\nset=true\n")
    );
    args.output = None;
    let printed = run_with(&fake, &args).expect("passes").join("\n");
    assert!(
        printed.contains("\nset=true\npackage-version=0.1.7472\ntargets={"),
        "{printed}"
    );
}

/// Asserts the choice after a run: the job `publish` succeeded, so the newest published release
/// is set: the highest version by number (0.0.100 over 0.0.99), skipping a draft, a pre-release
/// and a tag that is not a version, then checked like any release.
#[test]
fn after_a_run_that_published_the_newest_release_is_set() {
    let dir = scratch("after-run");
    let mut fake = consistent();
    fake.api.insert(
        "repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest".to_owned(),
        json!({"total_count": 3, "jobs": [
            {"name": "plan", "conclusion": "success"},
            {"name": "publish", "conclusion": "failure"},
            {"name": "publish", "conclusion": "success"}]}),
    );
    let listing = |top: &str| {
        json!([
            {"tag_name": top, "draft": false, "prerelease": false},
            {"tag_name": "v0.0.66", "draft": false, "prerelease": false},
            {"tag_name": "v9.0.0", "draft": true, "prerelease": false},
            {"tag_name": "v8.0.0", "draft": false, "prerelease": true},
            {"tag_name": "nightly", "draft": false, "prerelease": false},
            {}
        ])
    };
    let mut args = args(&dir);
    args.version = String::new();
    args.after_run = "77".to_owned();
    // 0.0.100 is the newest (not 0.0.99) and has no release here.
    fake.api.insert(
        "repos/o/r/releases?per_page=100".to_owned(),
        listing("v0.0.100"),
    );
    let error = run_with(&fake, &args).expect_err("0.0.100 has no release in the fake");
    assert_eq!(
        error,
        Error::Failed("gh api repos/o/r/releases/tags/v0.0.100: not found".to_owned())
    );
    // With 0.0.66 the newest the whole run passes.
    fake.api.insert(
        "repos/o/r/releases?per_page=100".to_owned(),
        listing("v0.0.65"),
    );
    let printed = run_with(&fake, &args).expect("passes").join("\n");
    assert!(
        printed.contains("set=true\npackage-version=0.1.7472\n"),
        "{printed}"
    );
}

/// Asserts that when the job `publish` did not succeed (skipped, failed or absent) nothing is
/// set: `set=false` is appended and printed, the verb succeeds, and neither the releases nor
/// the CDN are asked.
#[test]
fn after_a_run_that_published_nothing_nothing_is_set() {
    for jobs in [
        json!({"total_count": 2, "jobs": [
            {"name": "plan", "conclusion": "success"},
            {"name": "publish", "conclusion": "skipped"}]}),
        json!({"total_count": 1, "jobs": [{"name": "publish", "conclusion": "failure"}]}),
        json!({"total_count": 0, "jobs": []}),
    ] {
        let dir = scratch("nothing");
        let mut fake = consistent();
        fake.api.insert(
            "repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest".to_owned(),
            jobs,
        );
        let mut args = args(&dir);
        args.version = String::new();
        args.after_run = "77".to_owned();
        let printed =
            run_with(&fake, &args).expect("a run that published nothing is not a failure");
        assert_eq!(
            printed,
            [
                "run 77 of o/r did not publish a release (publish did not succeed): nothing is set",
                "set=false"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("out")).expect("output"),
            "set=false\n"
        );
        assert_eq!(fake.calls().len(), 1);
        // Without `--output` the same is printed and no file is made.
        args.output = None;
        assert_eq!(run_with(&fake, &args).expect("passes"), printed);
    }
}

/// Asserts that a run listing more jobs than one page (or fewer than it says) is refused, and
/// that a run GitHub does not know is a failure, not "nothing to set".
#[test]
fn a_job_listing_of_more_than_one_page_is_refused() {
    let dir = scratch("pages");
    let mut fake = consistent();
    let mut args = args(&dir);
    args.version = String::new();
    args.after_run = "77".to_owned();
    assert_eq!(
        run_with(&fake, &args),
        Err(Error::Failed(
            "gh api repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest: not found"
                .to_owned()
        ))
    );
    fake.api.insert(
        "repos/o/r/actions/runs/77/jobs?per_page=100&filter=latest".to_owned(),
        json!({"total_count": 101, "jobs": [{"name": "plan"}]}),
    );
    assert_eq!(
        run_with(&fake, &args),
        Err(Error::Failed(
            "run 77 of o/r has 101 jobs; one page of 1 was read".to_owned()
        ))
    );
}

/// Asserts that a repository with no published release version has nothing to set.
#[test]
fn a_repository_with_no_published_release_is_refused() {
    let dir = scratch("none");
    let mut fake = consistent();
    fake.api.insert(
        "repos/o/r/actions/runs/5/jobs?per_page=100&filter=latest".to_owned(),
        json!({"total_count": 1, "jobs": [{"name": "publish", "conclusion": "success"}]}),
    );
    fake.api.insert(
        "repos/o/r/releases?per_page=100".to_owned(),
        json!([
            {"tag_name": "v1.0.0", "draft": true, "prerelease": false},
            {"tag_name": "v1.0.1", "draft": false, "prerelease": true},
            {}
        ]),
    );
    let mut args = args(&dir);
    args.version = String::new();
    args.after_run = "5".to_owned();
    assert_eq!(
        run_with(&fake, &args),
        Err(Error::Failed(
            "o/r has no published release vX.Y.Z".to_owned()
        ))
    );
}

/// Asserts the usage errors: both or neither of `--version` and `--after-run`, a version that
/// is not three numbers, a run id that is not digits and an apt URL that is not https or ends
/// in a slash; none asks GitHub.
#[test]
fn flags_that_do_not_fit_together_are_usage_errors() {
    let dir = scratch("usage");
    let fake = consistent();
    let with = |version: &str, after_run: &str, apt_url: &str| DesiredArgs {
        version: version.to_owned(),
        after_run: after_run.to_owned(),
        apt_url: apt_url.to_owned(),
        output: None,
        ..args(&dir)
    };
    let url = "https://apt.example/apt";
    let both = "give one of --version and --after-run, not both and not neither";
    assert_eq!(
        run_with(&fake, &with("0.0.66", "5", url)),
        Err(Error::Usage(both.to_owned()))
    );
    assert_eq!(
        run_with(&fake, &with("", "", url)),
        Err(Error::Usage(both.to_owned()))
    );
    assert_eq!(
        run_with(&fake, &with("0.1", "", url)),
        Err(Error::Usage(
            "--version \"0.1\" is not <major>.<minor>.<patch>, a release tag without its v"
                .to_owned()
        ))
    );
    assert_eq!(
        run_with(&fake, &with("", "12a", url)),
        Err(Error::Usage(
            "--after-run \"12a\" is not the id of a workflow run".to_owned()
        ))
    );
    for bad in ["http://apt.example/apt", "https://apt.example/apt/"] {
        assert_eq!(
            run_with(&fake, &with("0.0.66", "", bad)),
            Err(Error::Usage(format!(
                "--apt-url {bad:?} is not an https URL without a trailing /"
            )))
        );
    }
    assert_eq!(fake.calls(), Vec::<String>::new());
}

/// Asserts that a release GitHub has no record of, a draft and a pre-release are not a released
/// version, even when named by `--version`, and that a release without `SHA256SUMS` is refused
/// naming it before anything is downloaded.
#[test]
fn a_missing_release_a_draft_a_prerelease_and_a_release_without_sums_are_refused() {
    let dir = scratch("draft");
    let mut fake = consistent();
    let tag = "repos/o/r/releases/tags/v0.0.66".to_owned();
    let gone = fake.api.remove(&tag).expect("release");
    assert_eq!(
        run_with(&fake, &args(&dir)),
        Err(Error::Failed(format!("gh api {tag}: not found")))
    );
    for flag in ["draft", "prerelease"] {
        fake.api
            .insert(tag.clone(), json!({ flag: true, "assets": [] }));
        assert_eq!(
            run_with(&fake, &args(&dir)),
            Err(Error::Failed(
                "release v0.0.66 of o/r is a draft or a pre-release, not a released version"
                    .to_owned()
            ))
        );
    }
    drop(gone);
    fake.api.insert(tag, release(&[&amd64_deb()]));
    let before = fake.calls().len();
    assert_eq!(
        refused(run_with(&fake, &args(&dir))),
        ["release v0.0.66 has no SHA256SUMS: there are no digests to hold the stable suite to"]
    );
    assert!(
        fake.calls()[before..]
            .iter()
            .all(|c| !c.starts_with("fetch") && !c.starts_with("download"))
    );
}

/// Asserts that a tag GitHub does not know and a tag that names no commit are failures.
#[test]
fn a_tag_that_names_no_commit_is_a_failure() {
    let dir = scratch("tag");
    let mut fake = consistent();
    let path = "repos/o/r/commits/refs/tags/v0.0.66".to_owned();
    fake.api.remove(&path);
    assert_eq!(
        run_with(&fake, &args(&dir)),
        Err(Error::Failed(
            "the tag v0.0.66 does not exist in o/r".to_owned()
        ))
    );
    fake.api.insert(path, json!({ "sha": "nope" }));
    assert_eq!(
        run_with(&fake, &args(&dir)),
        Err(Error::Failed(
            "the commit of the tag \"nope\" is not a full commit SHA (40 lowercase hex digits)"
                .to_owned()
        ))
    );
}

/// Asserts that `SHA256SUMS` that does not list a `.deb` is refused naming it, and that the
/// other architecture is still held to the suite (every problem is reported at once).
#[test]
fn sums_that_miss_a_deb_are_refused_by_name() {
    let dir = scratch("missing-deb");
    let mut fake = consistent();
    fake.sums = Some(format!("{AMD64}  {}\n", amd64_deb()));
    assert_eq!(
        refused(run_with(&fake, &args(&dir))),
        [format!(
            "SHA256SUMS of release v0.0.66 lists no {}",
            arm64_deb()
        )]
    );
    assert_eq!(
        fake.calls()
            .iter()
            .filter(|c| c.starts_with("fetch"))
            .count(),
        1
    );
    // Neither listed; the old jobscheduler-style name does not count.
    fake.sums = Some(format!("{AMD64}  kbf_0.1.7472_amd64.deb\n"));
    assert_eq!(refused(run_with(&fake, &args(&dir))).len(), 2);
}

/// Asserts the refusals about the stable index, all in one run: an index that cannot be read, a
/// stanza with another SHA-256 (the versions it does list are named), no stanza for the
/// architecture (the release was not promoted), a stanza of another package or directory, and a
/// stanza with no SHA256 field.
#[test]
fn a_stable_index_that_does_not_list_the_deb_at_the_sums_digest_is_refused() {
    let dir = scratch("index");
    let mut fake = consistent();
    fake.packages
        .insert("amd64".to_owned(), Ok(packages("amd64", "0.1.7471", OTHER)));
    fake.packages
        .insert("arm64".to_owned(), Err("curl: (22) 404".to_owned()));
    assert_eq!(
        refused(run_with(&fake, &args(&dir))),
        [
            format!("the stable suite lists no kbf amd64 package with sha256 {AMD64} (the digest SHA256SUMS lists for {}); it lists versions 0.1.7471", amd64_deb()),
            "https://apt.example/apt/dists/stable/main/binary-arm64/Packages could not be read: curl: (22) 404".to_owned(),
        ]
    );
    let elsewhere = packages("amd64", "0.1.7472", AMD64).replace("pool/kbf/", "pool/other/");
    let no_field = "Package: kbf\nVersion: 0.1.7472\nFilename: pool/kbf/kbf_0.1.7472_arm64.deb\n";
    fake.packages.insert("amd64".to_owned(), Ok(elsewhere));
    fake.packages
        .insert("arm64".to_owned(), Ok(no_field.to_owned()));
    assert_eq!(
        refused(run_with(&fake, &args(&dir))),
        [
            "the stable suite lists no kbf package for amd64: the release is not promoted there"
                .to_owned(),
            format!(
                "the stable suite lists no kbf arm64 package with sha256 {ARM64} (the digest SHA256SUMS lists for {}); it lists versions 0.1.7472",
                arm64_deb()
            ),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).unwrap_or_default(),
        ""
    );
}

/// Asserts that two architectures whose stanzas state different versions are refused, naming
/// both, and that a version that is not three decimal numbers is refused.
#[test]
fn versions_that_differ_between_architectures_or_are_not_numbers_are_refused() {
    let dir = scratch("versions");
    let mut fake = consistent();
    fake.packages
        .insert("arm64".to_owned(), Ok(packages("arm64", "0.1.7473", ARM64)));
    assert_eq!(
        refused(run_with(&fake, &args(&dir))),
        [
            "the stable suite lists the two architectures with different versions: 0.1.7472, 0.1.7473"
        ]
    );
    for bad in ["0.1", "0.1.7472-1", "0.1.x", "0.1.7472.1", "0..1"] {
        fake.packages
            .insert("amd64".to_owned(), Ok(packages("amd64", bad, AMD64)));
        fake.packages
            .insert("arm64".to_owned(), Ok(packages("arm64", bad, ARM64)));
        assert_eq!(
            refused(run_with(&fake, &args(&dir))),
            [format!(
                "the stable suite lists version {bad:?}, which is not <major>.<minor>.<patch>"
            )]
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).unwrap_or_default(),
        ""
    );
}

/// Asserts the file failures: a `SHA256SUMS` line that is not a digest and a name is a failure
/// naming its number; an output that cannot be appended to, a download directory that cannot be
/// made, a missing `SHA256SUMS` after a download that wrote nothing and a `Packages` the fetch
/// said it wrote and did not, are errors naming the path.
#[test]
fn files_that_cannot_be_read_or_written_are_errors_naming_them() {
    let dir = scratch("io");
    let mut fake = consistent();
    fake.sums = Some("not a sums file\n".to_owned());
    assert_eq!(
        run_with(&fake, &args(&dir)),
        Err(Error::Failed(
            "SHA256SUMS line 1 is not `<sha256>  <name>`: \"not a sums file\"".to_owned()
        ))
    );
    let fake = consistent();
    let mut bad = args(&dir);
    bad.output = Some(dir.join("no-such-directory/out"));
    let Err(Error::Spawn { what, .. }) = run_with(&fake, &bad) else {
        panic!("an output that cannot be opened must fail");
    };
    assert!(what.ends_with("no-such-directory/out"), "{what}");
    let _ = std::fs::remove_dir_all(dir.join("d"));
    std::fs::write(dir.join("d"), "a file").expect("file");
    let Err(Error::Spawn { what, .. }) = run_with(&fake, &args(&dir)) else {
        panic!("a directory that cannot be made must fail");
    };
    assert_eq!(what, format!("create {}", dir.join("d").display()));
    std::fs::remove_file(dir.join("d")).expect("remove");
    let mut nothing = consistent();
    nothing.sums = None;
    let Err(Error::Spawn { what, .. }) = run_with(&nothing, &args(&dir)) else {
        panic!("a missing SHA256SUMS must fail");
    };
    assert_eq!(what, format!("read {}", dir.join("d/SHA256SUMS").display()));
}

/// Asserts the reader of `SHA256SUMS`: text and binary lines, the last line for a name that is
/// listed twice, nothing for a name not listed, and a failure naming the number of the first
/// line that is not 64 lowercase hex digits, a space, a space or `*` and a name.
#[test]
fn the_sums_reader_takes_what_sha256sum_writes() {
    let text = format!("{AMD64}  a\n{ARM64} *b\n{OTHER}  a\n");
    assert_eq!(sums_digest(&text, "a"), Ok(Some(OTHER.to_owned())));
    assert_eq!(sums_digest(&text, "b"), Ok(Some(ARM64.to_owned())));
    assert_eq!(sums_digest(&text, "c"), Ok(None));
    assert_eq!(sums_digest("", "c"), Ok(None));
    let upper = AMD64.replace('1', "A");
    for bad in [
        format!("{upper}  a"),
        format!("{}  a", &AMD64[1..]),
        format!("{AMD64} a"),
        AMD64.to_owned(),
        String::new(),
    ] {
        assert_eq!(
            sums_digest(&format!("{AMD64}  ok\n{bad}\n"), "ok"),
            Err(Error::Failed(format!(
                "SHA256SUMS line 2 is not `<sha256>  <name>`: {bad:?}"
            ))),
            "{bad:?}"
        );
    }
}

/// Asserts the reader of a `Packages` index: a stanza counts only for the package `kbf`, the
/// architecture asked and a `Filename` that is exactly `pool/kbf/kbf_<Version>_<arch>.deb` with
/// its own `Version`; a field of another name that starts alike does not count; the versions of
/// stanzas at other digests are returned, and a stanza without the field is no match.
#[test]
fn the_packages_reader_finds_a_stanza_by_package_architecture_and_filename() {
    let text = format!(
        "Package: kbf\nVersion: 0.1.1\nFilename: pool/kbf/kbf_0.1.1_amd64.deb\nSHA256: {OTHER}\n\n\
         Package: kbf\nVersion: 0.1.2\nFilename: pool/kbf/kbf_0.1.3_amd64.deb\nSHA256: {AMD64}\n\n\
         Package: kbf\nVersion: 0.1.2\nFilename-Extra: pool/kbf/kbf_0.1.2_amd64.deb\nSHA256: {AMD64}\n\n\
         Package: kbf-extra\nVersion: 0.1.2\nFilename: pool/kbf/kbf_0.1.2_amd64.deb\nSHA256: {AMD64}\n\n\
         Package: kbf\nVersion: 0.1.4\nFilename: pool/kbf/kbf_0.1.4_amd64.deb\n\n\
         Package: kbf\nVersion: 0.1.5\nFilename: pool/kbf/kbf_0.1.5_arm64.deb\nSHA256: {AMD64}\n\n\
         Package: kbf\nVersion: 0.1.6\nFilename: pool/kbf/kbf_0.1.6_amd64.deb\nSHA256-Extra: {AMD64}\nSHA256: {AMD64}\n"
    );
    assert_eq!(
        listed(&text, "amd64", AMD64),
        Listed::Yes("0.1.6".to_owned())
    );
    assert_eq!(
        listed(&text, "arm64", AMD64),
        Listed::Yes("0.1.5".to_owned())
    );
    assert_eq!(
        listed(&text, "amd64", ARM64),
        Listed::No(
            ["0.1.1", "0.1.4", "0.1.6"]
                .iter()
                .map(|v| (*v).to_owned())
                .collect()
        )
    );
    assert_eq!(listed("", "amd64", AMD64), Listed::No(Vec::new()));
}

/// Asserts that the params the manifest carries are exactly the keys `mode`, `state_dir` and
/// `unit`, all text, and that the unit is the one the manifest runs.
#[test]
fn the_params_are_the_three_text_keys_the_hook_reads() {
    let keys: Vec<&str> = PARAMS.iter().map(|(key, _)| *key).collect();
    assert_eq!(keys, ["mode", "state_dir", "unit"]);
    let params = manifest::params();
    assert_eq!(params["mode"], json!({"text": "storage-and-daemon"}));
    assert_eq!(
        params["state_dir"],
        json!({"text": "/var/lib/kbf-package-hook"})
    );
    assert_eq!(params["unit"], json!({"text": manifest::RUN_UNIT}));
    assert_eq!(manifest::manifest()["run"], json!([manifest::RUN_UNIT]));
}

/// Asserts the manifest's shape against the contract of hostkeeper's native entries: 64-bit
/// integers are strings, there is no `uninstall` bound and no health URL, the hook command is
/// the program the package installs, the drain is unbounded (`prepare` 0), and the verify
/// deadline covers `verify` plus `rollback`.
#[test]
fn the_manifest_follows_the_native_entry_contract() {
    let manifest = manifest::manifest();
    assert_eq!(
        manifest["hook"]["command"],
        json!(["/usr/bin/kbf", "package-hook"])
    );
    assert!(manifest["hook"]["timeoutSecs"].get("uninstall").is_none());
    assert!(manifest.get("healthUrl").is_none());
    assert_eq!(manifest["hook"]["timeoutSecs"]["prepare"], "0");
    const {
        assert!(manifest::VERIFY_DEADLINE_SECS >= manifest::VERIFY_SECS + manifest::ROLLBACK_SECS);
    }
    for secs in manifest["hook"]["timeoutSecs"]
        .as_object()
        .expect("bounds")
        .values()
    {
        assert!(secs.is_string(), "{secs}");
    }
    assert_eq!(manifest["hook"]["cleanupRetries"], 3);
    let value: Value = serde_json::from_str(&golden()).expect("golden");
    assert_eq!(value["linux-x86_64"]["entries"][0]["manifest"], manifest);
}

/// Asserts the targets of one platform: only the platforms given are keyed (no `darwin-arm64`).
#[test]
fn the_targets_hold_only_the_platforms_given() {
    let one = manifest::targets("0.1.7472", &[(Platform::LinuxArm64, ARM64.to_owned())]);
    assert_eq!(
        one.as_object().expect("object").keys().collect::<Vec<_>>(),
        ["linux-arm64"]
    );
    assert_eq!(one["linux-arm64"]["entries"][0]["digest"], ARM64);
}

/// Asserts the arguments of the `curl` that reads the CDN: HTTPS only, TLS 1.2, failure on an
/// HTTP error, five retries, the file and then the URL.
#[test]
fn curl_reads_over_https_with_retries() {
    assert_eq!(
        curl_args(Path::new("/d/Packages-amd64"), "https://x/Packages").join(" "),
        "--proto =https --tlsv1.2 --fail --silent --show-error --retry 5 --retry-delay 20 --retry-all-errors --max-time 60 --output /d/Packages-amd64 https://x/Packages"
    );
}
