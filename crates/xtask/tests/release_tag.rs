//! `cargo xtask release tag` as the release workflow runs it, against `xtask-fake` as
//! `gh`: its `gh-api.json` holds what GitHub would answer, and `gh-args` what was asked, so
//! a test sees every write the verb made (the POSTs of a tag object and of a ref) and the
//! ones it must never make (a PATCH of a ref).

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::dir::TestDir;
use serde_json::{Map, Value, json};
use xtask::release::tag::{BOT_LOGIN, check_tagger};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const REPO: &str = "komira-ai/kbf";
const SHA: &str = "357494ac6b0fecfa065d6c8fa038cfca9ae9c13f";
const OTHER: &str = "0123456789abcdef0123456789abcdef01234567";
const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MESSAGE: &str = "{\"verdict\": \"green\"}\n";

struct Fx {
    root: PathBuf,
    /// Last field: the directory is removed with the fixture.
    _own: TestDir,
}

fn fx(name: &str) -> Fx {
    let own = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-release-tag"),
        name,
    );
    let root = own.to_path_buf();
    std::fs::create_dir_all(root.join("gh")).expect("gh dir");
    std::fs::hard_link(FAKE, root.join("gh/gh")).expect("link the fake as gh");
    std::fs::write(root.join("message"), MESSAGE).expect("message");
    Fx { root, _own: own }
}

fn refs_path() -> String {
    format!("repos/{REPO}/git/matching-refs/tags/v?per_page=100&page=1")
}

fn ref_path(tag: &str) -> String {
    format!("repos/{REPO}/git/ref/tags/{tag}")
}

impl Fx {
    fn gh(&self) -> PathBuf {
        self.root.join("gh/gh")
    }

    fn answers(&self) -> Map<String, Value> {
        let text = std::fs::read_to_string(self.root.join("gh/gh-api.json")).unwrap_or_default();
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn put(&self, entries: Vec<(String, Value)>) {
        let mut map = self.answers();
        map.extend(entries);
        std::fs::write(
            self.root.join("gh/gh-api.json"),
            Value::Object(map).to_string(),
        )
        .expect("gh-api.json");
    }

    /// The release tags GitHub lists, and `compares`, `(tag, commit, status)`.
    fn github(&self, tags: &[&str], compares: &[(&str, &str, &str)]) {
        let refs: Vec<Value> = tags
            .iter()
            .map(|t| json!({ "ref": format!("refs/tags/{t}") }))
            .collect();
        let mut entries = vec![(refs_path(), Value::Array(refs))];
        for (tag, commit, status) in compares {
            entries.push((
                format!("repos/{REPO}/compare/{tag}...{commit}"),
                json!({ "status": status }),
            ));
        }
        self.put(entries);
    }

    /// The tag `tag` already exists: annotated, naming `sha`, with `message`, by `tagger`.
    fn existing(&self, tag: &str, sha: &str, message: &str, tagger: &str) {
        self.put(vec![
            (
                ref_path(tag),
                json!({ "object": { "type": "tag", "sha": OID } }),
            ),
            (
                format!("repos/{REPO}/git/tags/{OID}"),
                json!({
                    "tag": tag,
                    "sha": OID,
                    "message": message,
                    "object": { "type": "commit", "sha": sha },
                    "tagger": { "name": tagger },
                }),
            ),
        ]);
    }

    fn write_file(&self, name: &str, text: &str) {
        std::fs::write(self.root.join("gh").join(name), text).expect("a fake knob");
    }

    fn gh_args(&self) -> String {
        std::fs::read_to_string(self.root.join("gh/gh-args")).unwrap_or_default()
    }

    /// The writes the fake saw: `gh api` lines with a method.
    fn writes(&self) -> Vec<String> {
        self.gh_args()
            .lines()
            .filter(|l| l.contains(" -X "))
            .map(str::to_owned)
            .collect()
    }

    fn tag_with(&self, sha: &str, tag: &str, extra: &[&str]) -> (Option<i32>, String, String) {
        let out = Command::new(XTASK)
            .args(["release", "tag", "--repo", REPO, "--sha", sha, "--tag", tag])
            .arg("--message-file")
            .arg(self.root.join("message"))
            .arg("--gh")
            .arg(self.gh())
            .args(extra)
            .env_remove("XTASK_FAKE_DEFECT")
            .output()
            .expect("run xtask");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (out.status.code(), text(&out.stdout), text(&out.stderr))
    }

    fn tag(&self, tag: &str) -> (Option<i32>, String, String) {
        self.tag_with(SHA, tag, &[])
    }

    /// The tag object the ref `tag` points at, as the fake stored it.
    fn stored(&self, tag: &str) -> Option<Value> {
        let answers = self.answers();
        let oid = answers.get(&ref_path(tag))?["object"]["sha"].as_str()?;
        answers
            .get(&format!("repos/{REPO}/git/tags/{oid}"))
            .cloned()
    }
}

/// GitHub holds v0.0.1 at a commit before `SHA`.
fn after_v1(fx: &Fx) {
    fx.github(&["v0.0.1"], &[("v0.0.1", SHA, "ahead")]);
}

#[test]
fn creates_an_annotated_tag_with_a_tag_object_then_a_ref_and_nothing_else() {
    // Catches: a tag made some other way (a lightweight ref, a PATCH), the wrong object or
    // message, or any write beyond the two POSTs.
    let fx = fx("create");
    after_v1(&fx);
    let (code, out, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "tag=v0.0.2\ncreated=true\n");
    let writes = fx.writes();
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert!(writes[0].starts_with(&format!("api repos/{REPO}/git/tags -X POST")));
    assert!(writes[1].starts_with(&format!("api repos/{REPO}/git/refs -X POST")));
    let stored = fx.stored("v0.0.2").expect("the tag exists");
    assert_eq!(stored["tag"], "v0.0.2");
    assert_eq!(stored["message"], MESSAGE);
    assert_eq!(stored["object"], json!({ "type": "commit", "sha": SHA }));
}

#[test]
fn the_first_release_is_tagged_when_no_release_tag_exists() {
    // Catches: Rule S refusing a repository with no tag.
    let fx = fx("first");
    fx.github(&[], &[]);
    let (code, out, err) = fx.tag("v0.0.1");
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("created=true"), "{out}");
}

#[test]
fn appends_the_lines_to_the_output_file() {
    // Catches: GITHUB_OUTPUT never written, so later jobs read no tag.
    let fx = fx("output");
    after_v1(&fx);
    let file = fx.root.join("out");
    let (code, _, err) = fx.tag_with(SHA, "v0.0.2", &["--output", file.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        "tag=v0.0.2\ncreated=true\n"
    );
}

#[test]
fn a_superseded_commit_writes_nothing() {
    // Catches: a stale run tagging (the guard skipped or `behind` accepted): no POST at all.
    let fx = fx("superseded");
    fx.github(&["v0.0.5"], &[("v0.0.5", SHA, "behind")]);
    let (code, _, err) = fx.tag("v0.0.6");
    assert_eq!(code, Some(1));
    assert!(err.contains("superseded"), "{err}");
    assert!(fx.writes().is_empty(), "{:?}", fx.writes());
    assert!(fx.stored("v0.0.6").is_none());
}

#[test]
fn a_diverged_commit_writes_nothing() {
    // Catches: only `behind` refused; a commit on another line of history tags too.
    let fx = fx("diverged");
    fx.github(&["v0.0.5"], &[("v0.0.5", SHA, "diverged")]);
    let (code, _, err) = fx.tag("v0.0.6");
    assert_eq!(code, Some(1));
    assert!(err.contains("diverged"), "{err}");
    assert!(fx.writes().is_empty());
}

#[test]
fn a_tag_that_is_no_step_above_the_highest_writes_nothing() {
    // Catches: the --tag rule missing: Rule S alone passes this (the commit is ahead of the
    // highest tag), yet v0.0.3 is below v0.0.5.
    let fx = fx("stale-tag");
    fx.github(&["v0.0.5"], &[("v0.0.5", SHA, "ahead")]);
    let (code, _, err) = fx.tag("v0.0.3");
    assert_eq!(code, Some(1));
    assert!(err.contains("--tag v0.0.3"), "{err}");
    assert!(fx.writes().is_empty());
}

#[test]
fn a_rerun_reuses_the_tag_that_names_this_commit_and_message() {
    // Catches: a re-run failing on its own tag, or writing a second one.
    let fx = fx("reuse");
    fx.github(&["v0.0.2"], &[("v0.0.2", SHA, "identical")]);
    fx.existing("v0.0.2", SHA, MESSAGE, BOT_LOGIN);
    let (code, out, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "tag=v0.0.2\ncreated=false\n");
    assert!(fx.writes().is_empty());
}

#[test]
fn git_ending_the_message_with_a_line_end_is_the_same_message() {
    // Catches: a re-run refused because the stored message gained or lost a final newline.
    let fx = fx("newline");
    fx.github(&["v0.0.2"], &[("v0.0.2", SHA, "identical")]);
    fx.existing("v0.0.2", SHA, MESSAGE.trim_end(), BOT_LOGIN);
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(0), "{err}");
}

/// A tag that exists but is not what this run would make; the highest tag, so only the
/// existing-tag checks can refuse it.
fn refused(fx: &Fx, why: &str) {
    let before = fx.answers();
    let (code, out, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(1), "{out}");
    assert!(err.contains(why), "{err}");
    assert!(fx.writes().is_empty(), "{:?}", fx.writes());
    assert_eq!(fx.answers(), before, "the tag was changed");
}

#[test]
fn an_existing_tag_at_another_commit_fails_and_is_not_moved() {
    // Catches: reuse by name alone, or a PATCH that moves the tag to this commit.
    let fx = fx("wrong-sha");
    fx.github(&["v0.0.2"], &[("v0.0.2", SHA, "identical")]);
    fx.existing("v0.0.2", OTHER, MESSAGE, BOT_LOGIN);
    refused(&fx, &format!("not commit {SHA}"));
}

#[test]
fn an_existing_tag_with_another_message_fails_and_is_not_replaced() {
    // Catches: a tag accepted for its commit alone, carrying another verdict.
    let fx = fx("wrong-message");
    fx.github(&["v0.0.2"], &[("v0.0.2", SHA, "identical")]);
    fx.existing("v0.0.2", SHA, "{\"verdict\": \"red\"}\n", BOT_LOGIN);
    refused(&fx, "another message");
}

#[test]
fn an_existing_tag_by_another_tagger_fails() {
    // Catches: a tag a human made, with the right commit and message, being adopted.
    let fx = fx("wrong-tagger");
    fx.github(&["v0.0.2"], &[("v0.0.2", SHA, "identical")]);
    fx.existing("v0.0.2", SHA, MESSAGE, "someone");
    refused(&fx, "the tagger of the tag is \"someone\"");
}

#[test]
fn the_expected_tagger_is_a_flag() {
    // Catches: the default login hard-wired, so a different App identity cannot be used.
    let fx = fx("tagger-flag");
    fx.github(&["v0.0.2"], &[("v0.0.2", SHA, "identical")]);
    fx.existing("v0.0.2", SHA, MESSAGE, "other-app[bot]");
    let (code, _, err) = fx.tag_with(SHA, "v0.0.2", &["--tagger", "other-app[bot]"]);
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn a_lightweight_tag_of_that_name_fails() {
    // Catches: a bare ref to the commit passing as an annotated tag.
    let fx = fx("lightweight");
    fx.github(&["v0.0.2"], &[("v0.0.2", SHA, "identical")]);
    fx.put(vec![(
        ref_path("v0.0.2"),
        json!({ "object": { "type": "commit", "sha": SHA } }),
    )]);
    refused(&fx, "is lightweight");
}

#[test]
fn a_tag_object_named_otherwise_fails() {
    // Catches: the ref's name taken for the tag object's name.
    let fx = fx("object-name");
    fx.github(&["v0.0.2"], &[("v0.0.2", SHA, "identical")]);
    fx.existing("v0.0.2", SHA, MESSAGE, BOT_LOGIN);
    let mut stored = fx.answers()[&format!("repos/{REPO}/git/tags/{OID}")].clone();
    stored["tag"] = json!("v9.9.9");
    fx.put(vec![(format!("repos/{REPO}/git/tags/{OID}"), stored)]);
    refused(&fx, "tag object is named \"v9.9.9\"");
}

#[test]
fn a_ref_whose_tag_object_is_missing_fails() {
    // Catches: a dangling ref read as an absent tag, or as a reusable one.
    let fx = fx("dangling");
    fx.github(&["v0.0.2"], &[("v0.0.2", SHA, "identical")]);
    fx.put(vec![(
        ref_path("v0.0.2"),
        json!({ "object": { "type": "tag", "sha": OID } }),
    )]);
    refused(&fx, "which GitHub does not have");
}

#[test]
fn one_transient_error_is_retried_once() {
    // Catches: no retry: a single 502 failing the release.
    let fx = fx("retry");
    after_v1(&fx);
    fx.write_file("gh-fault", "fail-post:502");
    let (code, out, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("created=true"));
    assert_eq!(fx.writes().len(), 3, "{:?}", fx.writes());
    assert!(fx.stored("v0.0.2").is_some());
}

#[test]
fn two_transient_errors_fail_the_run_with_no_tag() {
    // Catches: a retry loop that never ends, or one that eventually forces the tag.
    let fx = fx("retry-twice");
    after_v1(&fx);
    fx.write_file("gh-fault", "fail-post:502:2");
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(1));
    assert!(err.contains("transient"), "{err}");
    assert_eq!(fx.writes().len(), 2, "{:?}", fx.writes());
    assert!(fx.stored("v0.0.2").is_none());
}

#[test]
fn a_ref_whose_reply_was_lost_is_found_by_the_retry_and_reused() {
    // Catches: a retry that writes again (a second tag) or fails on its own earlier work,
    // for both ways the lost reply shows (a 5xx, and "Reference already exists").
    for lost in ["502", "422"] {
        let fx = fx(&format!("lost-{lost}"));
        after_v1(&fx);
        fx.write_file("gh-fault", &format!("lose-ref-{lost}"));
        let (code, out, err) = fx.tag("v0.0.2");
        assert_eq!(code, Some(0), "{lost}: {err}");
        assert!(out.contains("created=false"), "{lost}: {out}");
        assert_eq!(fx.writes().len(), 2, "{lost}: {:?}", fx.writes());
    }
}

#[test]
fn a_tag_another_run_made_first_is_read_and_refused_when_it_differs() {
    // Catches: a lost race answered by moving the tag, or by calling it ours.
    let fx = fx("race");
    after_v1(&fx);
    fx.write_file("gh-fault", "race");
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(1));
    assert!(err.contains("another message"), "{err}");
    assert_eq!(fx.writes().len(), 2, "{:?}", fx.writes());
    assert_eq!(fx.stored("v0.0.2").expect("theirs")["message"], "other\n");
}

#[test]
fn a_failure_that_is_not_transient_is_not_retried() {
    // Catches: every error retried; a 422 validation failure is the API refusing, not a blip.
    let fx = fx("not-transient");
    after_v1(&fx);
    fx.write_file("gh-fault", "refuse-ref");
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(1));
    assert!(err.contains("Validation Failed"), "{err}");
    assert!(!err.contains("transient"), "{err}");
    assert_eq!(fx.writes().len(), 2, "{:?}", fx.writes());
}

#[test]
fn a_rate_limited_write_is_retried() {
    // Catches: a retry on 429 missing (rate limited).
    let fx = fx("statuses");
    after_v1(&fx);
    fx.write_file("gh-fault", "fail-post:429");
    let (code, out, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("created=true"));
    assert_eq!(fx.writes().len(), 3, "{:?}", fx.writes());
}

#[test]
fn a_not_found_on_a_write_is_not_retried() {
    // Catches: a retry made for an answer that is no blip.
    let fx = fx("statuses-404");
    after_v1(&fx);
    fx.write_file("gh-fault", "fail-post:404");
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(1));
    assert!(
        err.contains("(HTTP 404)") && !err.contains("transient"),
        "{err}"
    );
    assert_eq!(fx.writes().len(), 1, "{:?}", fx.writes());
}

#[test]
fn a_read_that_fails_is_retried_once_then_fails_the_run() {
    // Catches: an unreachable API read as "no such tag", which would create one.
    let fx = fx("read-fails");
    after_v1(&fx);
    fx.write_file("gh-fault", "fail-get:502");
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(1));
    assert!(
        err.contains("(HTTP 502)") && err.contains("transient"),
        "{err}"
    );
    assert!(fx.writes().is_empty());
    let err = check_tagger(&fx.gh(), REPO, "v0.0.2", BOT_LOGIN).unwrap_err();
    assert!(err.to_string().contains("(HTTP 502)"), "{err}");
    fx.write_file("gh-fault", "fail-get:403");
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(1));
    assert!(
        err.contains("(HTTP 403)") && !err.contains("transient"),
        "{err}"
    );
}

#[test]
fn an_answer_that_is_not_json_fails() {
    // Catches: garbage read as a tag id.
    let fx = fx("garbage");
    after_v1(&fx);
    fx.write_file("gh-fault", "garbage");
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(1));
    assert!(err.contains("not JSON"), "{err}");
    assert!(fx.writes().len() == 1);
}

#[test]
fn an_existing_tag_of_a_tree_with_this_id_fails() {
    // Catches: only the id compared: a tag object naming a non-commit with the same id.
    let fx = fx("tree");
    fx.github(&["v0.0.2"], &[("v0.0.2", SHA, "identical")]);
    fx.existing("v0.0.2", SHA, MESSAGE, BOT_LOGIN);
    let key = format!("repos/{REPO}/git/tags/{OID}");
    let mut stored = fx.answers()[&key].clone();
    stored["object"]["type"] = json!("tree");
    fx.put(vec![(key, stored)]);
    refused(&fx, "names tree");
}

#[test]
fn a_tag_object_answered_with_no_id_fails_before_any_ref() {
    // Catches: a ref made for an id that was never given.
    let fx = fx("no-sha");
    after_v1(&fx);
    fx.write_file("gh-fault", "no-sha");
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(1));
    assert!(err.contains("no sha"), "{err}");
    assert_eq!(fx.writes().len(), 1, "{:?}", fx.writes());
}

#[test]
fn a_tag_that_is_no_release_tag_is_refused_before_anything_is_asked() {
    // Catches: a suffix or probe tag created by the release path.
    let fx = fx("bad-tag");
    for bad in ["v0.0.2-rc1", "latest", "v1.2"] {
        let (code, _, err) = fx.tag(bad);
        assert_eq!(code, Some(2), "{bad}: {err}");
        assert!(err.contains("not a release tag"), "{err}");
    }
    assert_eq!(fx.gh_args(), "");
}

#[test]
fn a_commit_that_is_not_a_full_sha_is_refused() {
    // Catches: an abbreviation or a ref name tagged.
    let fx = fx("bad-sha");
    let (code, _, err) = fx.tag_with("main", "v0.0.2", &[]);
    assert_eq!(code, Some(2));
    assert!(err.contains("--sha"), "{err}");
    assert_eq!(fx.gh_args(), "");
}

#[test]
fn an_empty_or_missing_message_file_is_refused() {
    // Catches: a tag with no verdict.
    let fx = fx("no-message");
    after_v1(&fx);
    std::fs::write(fx.root.join("message"), " \n").unwrap();
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(2));
    assert!(err.contains("is empty"), "{err}");
    std::fs::remove_file(fx.root.join("message")).unwrap();
    let (code, _, err) = fx.tag("v0.0.2");
    assert_eq!(code, Some(1));
    assert!(err.contains("read"), "{err}");
    assert!(fx.writes().is_empty());
}

#[test]
fn the_tagger_check_reads_the_tag_and_compares_the_login() {
    // Catches: a check that passes any tagger, any tag, or a missing or lightweight one.
    let fx = fx("check-tagger");
    let gh = fx.gh();
    fx.existing("v0.0.2", SHA, MESSAGE, BOT_LOGIN);
    check_tagger(&gh, REPO, "v0.0.2", BOT_LOGIN).expect("the bot's tag");
    let err = check_tagger(&gh, REPO, "v0.0.2", "someone").unwrap_err();
    assert!(err.to_string().contains("not \"someone\""), "{err}");
    let err = check_tagger(&gh, REPO, "v0.0.9", BOT_LOGIN).unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
    fx.put(vec![(
        ref_path("v0.0.3"),
        json!({ "object": { "type": "commit", "sha": SHA } }),
    )]);
    let err = check_tagger(&gh, REPO, "v0.0.3", BOT_LOGIN).unwrap_err();
    assert!(err.to_string().contains("is lightweight"), "{err}");
}
