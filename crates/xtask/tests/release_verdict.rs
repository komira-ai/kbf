//! `cargo xtask release verdict` and `release notes` against real git repositories: the
//! tags are made by `git tag`, so a lightweight tag is a ref to a commit and an
//! annotated one a tag object, as GitHub would hold them.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_gate::{
    CommitId, Floor, GateResult, Header, Limit, MinLoad, ResultDoc, Sample, SetHash, Stage, Status,
    Waiver, Window,
};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/gamma-verdict.json"
);
/// The release commit of [`repo`]: fixed dates and identity make git's hash a constant,
/// so the golden verdict can name it.
const RELEASE: &str = "357494ac6b0fecfa065d6c8fa038cfca9ae9c13f";
const TAG: &str = "v0.10.0";
/// What `SHA256SUMS` holds in every test, and its set hash.
const SUMS: &str = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881  a\n";
const SET: &str = "4a4b874a1e9776a52e38fd82a053825f309f46187a803a0fc0c6670c6670e768";
const DAY: u64 = 24 * 60 * 60;

struct Fx {
    root: PathBuf,
    /// The first commit, an ancestor of both.
    first: String,
    /// A commit on a side branch, not on main.
    side: String,
}

impl Fx {
    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn dist(&self) -> PathBuf {
        self.root.join("dist")
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.repo(), args, None)
    }

    /// Makes annotated tag `name` at `commit` with exactly `message`.
    fn annotate(&self, name: &str, commit: &str, message: &[u8]) {
        let file = self.root.join("message");
        std::fs::write(&file, message).unwrap();
        let file = file.to_str().unwrap();
        self.git(&["tag", "-a", name, "-F", file, "--cleanup=verbatim", commit]);
    }
}

fn git(dir: &Path, args: &[&str], stdin: Option<&str>) -> String {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .env("GIT_AUTHOR_DATE", "2026-10-18T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-10-18T00:00:00Z")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    drop(
        child
            .stdin
            .take()
            .map(|mut i| i.write_all(stdin.unwrap_or("").as_bytes())),
    );
    let out = child.wait_with_output().expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

/// A repository: main holds `first` and `release commit` (hash [`RELEASE`]); the branch
/// `side` holds `side` on top of `first`. Release files: `dist/SHA256SUMS`.
fn fx(name: &str) -> Fx {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("release-verdict")
        .join(name);
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(root.join("dist")).unwrap();
    std::fs::write(root.join("dist/SHA256SUMS"), SUMS).unwrap();
    git(
        &repo,
        &["init", "-q", "-b", "main", "--object-format=sha1"],
        None,
    );
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "first"],
        None,
    );
    let first = git(&repo, &["rev-parse", "HEAD"], None);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "release commit"],
        None,
    );
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"], None),
        RELEASE,
        "git hashes changed"
    );
    git(&repo, &["switch", "-q", "-c", "side", &first], None);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "side"],
        None,
    );
    let side = git(&repo, &["rev-parse", "HEAD"], None);
    git(&repo, &["switch", "-q", "main"], None);
    Fx { root, first, side }
}

/// A verdict that passes for [`TAG`], [`RELEASE`] and [`SET`].
fn good() -> ResultDoc {
    let header = Header {
        stage: Stage::Gamma,
        commit: CommitId::parse(RELEASE).unwrap(),
        set_hash: SetHash::parse(SET).unwrap(),
        run_id: "bake-0001".to_owned(),
        attempt: 1,
    };
    let mut d = ResultDoc::begin(header, 1_000);
    d.status = Status::Green;
    d.finished = Some(90_000);
    d.desired_generation = Some(7);
    d.deploy = Some(Window {
        start: 1_100,
        end: Some(1_900),
    });
    d.bake = Some(Window {
        start: 2_000,
        end: Some(2_000 + DAY),
    });
    let floor = |measured, required| Floor { measured, required };
    d.load = Some(MinLoad {
        actions: floor(1_000, 1_000),
        builds: floor(12, 10),
        hours: floor(3, 3),
    });
    d.gates = vec![GateResult::decide(
        "lost-blobs",
        Sample::Value(0.0),
        Limit::AtMost(0.0),
        2_000,
        2_000 + DAY,
    )];
    d.tag = Some(TAG.to_owned());
    d
}

/// A tag message: the verdict `doc` as the evaluator writes it.
fn msg(doc: &ResultDoc) -> Vec<u8> {
    doc.to_bytes().unwrap()
}

type Ran = (Option<i32>, String, String);

fn xtask(args: &[&str]) -> Ran {
    let out = Command::new(XTASK).args(args).output().expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

fn run(fx: &Fx, verb: &str, tag: &str, commit: &str, extra: &[&str]) -> Ran {
    let (dist, repo) = (fx.dist(), fx.repo());
    let mut args = vec![
        "release",
        verb,
        dist.to_str().unwrap(),
        "--repo-dir",
        repo.to_str().unwrap(),
        "--tag",
        tag,
        "--commit",
        commit,
        "--main-ref",
        "main",
    ];
    args.extend(extra);
    xtask(&args)
}

fn verdict(fx: &Fx, tag: &str, commit: &str) -> Ran {
    run(fx, "verdict", tag, commit, &[])
}

/// Tags [`RELEASE`] with `doc` and expects `release verdict` to refuse it with `why`.
fn refused(name: &str, doc: &ResultDoc, why: &str) {
    let fx = fx(name);
    fx.annotate(TAG, RELEASE, &msg(doc));
    let (code, out, err) = verdict(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(err.contains(why), "wanted `{why}` in:\n{err}");
}

fn accepted(name: &str, doc: &ResultDoc) {
    let fx = fx(name);
    fx.annotate(TAG, RELEASE, &msg(doc));
    let (code, out, err) = verdict(&fx, TAG, RELEASE);
    assert_eq!(code, Some(0), "{out}{err}");
}

#[test]
fn the_golden_verdict_bytes_are_what_the_gate_crate_writes() {
    // Catches: a serializer change that the evaluator and this check would both follow
    // (they share the type), so a verdict already in a tag stops parsing. `BLESS=1`
    // rewrites the file; the diff is then reviewed.
    let bytes = msg(&good());
    if std::env::var_os("BLESS").is_some() {
        std::fs::write(GOLDEN, &bytes).unwrap();
    }
    let golden = std::fs::read(GOLDEN).expect("golden file");
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        String::from_utf8_lossy(&golden)
    );
}

#[test]
fn the_golden_verdict_in_a_real_annotated_tag_passes_and_prints_the_set_hash() {
    // Catches: a check that cannot read what git stores (the message round trip through
    // `git tag -a` and `git cat-file tag`) or that prints another set hash than the files'.
    let fx = fx("golden");
    fx.annotate(TAG, RELEASE, &std::fs::read(GOLDEN).unwrap());
    assert_eq!(
        fx.git(&["cat-file", "-t", &format!("refs/tags/{TAG}")]),
        "tag"
    );
    let output = fx.root.join("out");
    let out = output.to_str().unwrap();
    let (code, stdout, err) = run(&fx, "verdict", TAG, RELEASE, &["--output", out]);
    assert_eq!(code, Some(0), "{err}");
    assert!(stdout.ends_with(&format!("set-hash={SET}\n")), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        format!("set-hash={SET}\n")
    );
}

#[test]
fn a_tag_with_no_message_has_no_verdict() {
    // Catches: an empty message read as OK.
    let fx = fx("empty");
    let object = format!("object {RELEASE}\ntype commit\ntag {TAG}\ntagger T <t@e.x> 0 +0000\n\n");
    let sha = git(&fx.repo(), &["mktag"], Some(&object));
    fx.git(&["update-ref", &format!("refs/tags/{TAG}"), &sha]);
    let (code, _, err) = verdict(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("no usable verdict"), "{err}");
}

#[test]
fn a_tag_whose_message_is_not_a_verdict_is_refused() {
    let fx = fx("prose");
    fx.annotate(TAG, RELEASE, b"release 0.10.0\n\nlooks fine\n");
    let (code, _, err) = verdict(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("no usable verdict"), "{err}");
}

#[test]
fn a_lightweight_tag_fails_because_it_cannot_carry_a_verdict() {
    // Catches: a check that takes the commit a lightweight tag points at for a tag
    // object. The fixture is a real lightweight tag: git says its object is a commit.
    let fx = fx("lightweight");
    fx.git(&["tag", TAG, RELEASE]);
    assert_eq!(
        fx.git(&["cat-file", "-t", &format!("refs/tags/{TAG}")]),
        "commit"
    );
    let (code, _, err) = verdict(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("lightweight"), "{err}");
}

#[test]
fn a_tag_that_is_not_there_fails() {
    let fx = fx("no-tag");
    let (code, _, err) = verdict(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{err}");
}

#[test]
fn a_tag_on_a_commit_not_on_main_fails_even_with_a_perfect_verdict() {
    // Catches: a verify that never asks whether the commit is main's. The verdict names
    // the side commit, the tag names it, the set matches: only main refuses it.
    let fx = fx("off-main");
    let mut d = good();
    d.commit = CommitId::parse(&fx.side).unwrap();
    fx.annotate(TAG, &fx.side, &msg(&d));
    let (code, _, err) = verdict(&fx, TAG, &fx.side);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("is not on main"), "{err}");
    // The same tag shape on a main commit passes (so the refusal above was the branch).
    let fx2 = fx_with_first_verdict("off-main-control");
    let (code, _, err) = verdict(&fx2, TAG, &fx2.first);
    assert_eq!(code, Some(0), "{err}");
}

/// A fixture whose tag is at `first` (on main), with a verdict naming it.
fn fx_with_first_verdict(name: &str) -> Fx {
    let fx = fx(name);
    let mut d = good();
    d.commit = CommitId::parse(&fx.first).unwrap();
    fx.annotate(TAG, &fx.first, &msg(&d));
    fx
}

#[test]
fn a_red_or_extended_verdict_fails_on_its_status_alone() {
    // Catches: a check that reads only the gates (all green here).
    for status in [Status::Red, Status::Extended, Status::Running] {
        let mut d = good();
        d.status = status;
        refused(&format!("status-{status:?}"), &d, "not green");
    }
}

#[test]
fn a_green_verdict_over_a_red_gate_fails() {
    // Catches: a check that reads only the top-level status. (The document parser refuses
    // this combination; the test pins that the refusal reaches the exit code.)
    let mut d = good();
    d.gates[0] = GateResult::decide("lost-blobs", Sample::Absent, Limit::AtMost(0.0), 1, 2);
    refused("red-gate", &d, "a gate is red");
}

#[test]
fn a_verdict_for_another_stage_fails() {
    let mut d = good();
    d.stage = Stage::Beta;
    refused("stage", &d, "not Gamma");
}

#[test]
fn a_verdict_naming_another_commit_fails() {
    // Catches: a skipped commit comparison.
    let fx = fx("other-commit");
    let mut d = good();
    d.commit = CommitId::parse(&fx.first).unwrap();
    fx.annotate(TAG, RELEASE, &msg(&d));
    let (code, _, err) = verdict(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("the verdict names commit"), "{err}");
}

#[test]
fn a_tag_naming_another_commit_than_the_release_fails() {
    // Catches: a check that trusts `--commit` and never reads what the tag points at:
    // the verdict, the main check and the set all agree with `--commit`.
    let fx = fx("tag-elsewhere");
    let mut d = good();
    d.commit = CommitId::parse(&fx.first).unwrap();
    fx.annotate(TAG, RELEASE, &msg(&d));
    let (code, _, err) = verdict(&fx, TAG, &fx.first);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("not commit"), "{err}");
}

#[test]
fn a_verdict_naming_another_set_fails() {
    // Catches: a skipped set comparison (the files are not what gamma ran).
    let mut d = good();
    d.set_hash = SetHash::of_sums(b"other files");
    refused("other-set", &d, "the verdict names set");
}

#[test]
fn a_changed_sums_file_changes_the_set_and_fails() {
    // Catches: a set hash computed from something other than the SHA256SUMS bytes.
    let fx = fx("changed-sums");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    std::fs::write(fx.dist().join("SHA256SUMS"), format!("{SUMS}{SUMS}")).unwrap();
    let (code, _, err) = verdict(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("the verdict names set"), "{err}");
}

#[test]
fn a_verdict_naming_another_tag_fails() {
    // Catches: a skipped tag comparison (a verdict made for v0.9.0 reused for v0.10.0).
    let mut d = good();
    d.tag = Some("v0.9.0".to_owned());
    refused("other-tag", &d, "the verdict names tag");
    let mut d = good();
    d.tag = None;
    refused("doc-without-tag", &d, "the verdict names tag");
}

#[test]
fn a_tag_object_named_other_than_its_ref_fails() {
    // Catches: refs/tags/v0.10.0 made to point at another tag's object (the verdict then
    // names a tag the ref is not).
    let fx = fx("renamed");
    let mut d = good();
    d.tag = Some("v0.9.0".to_owned());
    fx.annotate("v0.9.0", RELEASE, &msg(&d));
    let sha = fx.git(&["rev-parse", "refs/tags/v0.9.0"]);
    fx.git(&["update-ref", &format!("refs/tags/{TAG}"), &sha]);
    let (code, _, err) = verdict(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("tag object is named"), "{err}");
}

#[test]
fn a_bake_one_second_short_of_24_hours_fails_and_24_hours_passes() {
    // Catches: `>` against 24 h (exactly 24 h fails) and `>=` against less.
    let mut d = good();
    d.bake = Some(Window {
        start: 2_000,
        end: Some(2_000 + DAY - 1),
    });
    refused("bake-short", &d, "less than");
    let mut d = good();
    d.bake = Some(Window {
        start: 2_000,
        end: Some(2_000 + DAY),
    });
    accepted("bake-exact", &d);
    let mut d = good();
    d.bake = Some(Window {
        start: 2_000,
        end: None,
    });
    refused("bake-open", &d, "no finished bake");
    let mut d = good();
    d.bake = None;
    refused("bake-none", &d, "no finished bake");
}

#[test]
fn thin_load_needs_a_recorded_waiver() {
    // Catches: a check that never reads the load, or one that reads a waiver of anything.
    let thin = |d: &mut ResultDoc| {
        d.load.as_mut().unwrap().actions.measured = 999;
    };
    let mut d = good();
    thin(&mut d);
    refused("thin", &d, "minimum load was not met");
    let mut d = good();
    thin(&mut d);
    d.waivers = vec![Waiver::new("lost-blobs", "op", "because").unwrap()];
    refused("thin-other-waiver", &d, "minimum load was not met");
    let mut d = good();
    thin(&mut d);
    d.waivers = vec![Waiver::new("min-load", "op", "holiday week").unwrap()];
    accepted("thin-waived", &d);
    let mut d = good();
    d.load = None;
    refused("no-load", &d, "minimum load was not met");
}

#[test]
fn a_waiver_with_no_reason_is_not_a_record() {
    // Catches: a waiver accepted without who or why. The fixture is written by hand,
    // because the type refuses to build one.
    let fx = fx("blank-waiver");
    let mut d = good();
    d.waivers = vec![Waiver::new("min-load", "op", "x").unwrap()];
    let text = String::from_utf8(msg(&d))
        .unwrap()
        .replace("\"reason\": \"x\"", "\"reason\": \"\"");
    fx.annotate(TAG, RELEASE, text.as_bytes());
    let (code, _, err) = verdict(&fx, TAG, RELEASE);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("names no operator or no reason"), "{err}");
}

#[test]
fn problems_are_all_reported_not_just_the_first() {
    let fx = fx("many");
    let mut d = good();
    d.commit = CommitId::parse(&fx.first).unwrap();
    d.set_hash = SetHash::of_sums(b"x");
    fx.annotate(TAG, RELEASE, &msg(&d));
    let (_, _, err) = verdict(&fx, TAG, RELEASE);
    assert!(
        err.contains("names commit") && err.contains("names set"),
        "{err}"
    );
}

/// A verdict-bearing release tag `v0.10.0` and an older `v0.9.0` on the first commit.
fn with_notes(name: &str, change: impl FnOnce(&mut ResultDoc)) -> Fx {
    let fx = fx(name);
    fx.git(&["tag", "v0.9.0", &fx.first]);
    let mut d = good();
    change(&mut d);
    fx.annotate(TAG, RELEASE, &msg(&d));
    fx
}

fn notes_of(fx: &Fx) -> String {
    let out = fx.root.join("notes.md");
    let (code, stdout, err) = run(fx, "notes", TAG, RELEASE, &["--out", out.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{stdout}{err}");
    std::fs::read_to_string(out).unwrap()
}

#[test]
fn the_notes_list_the_commits_since_the_previous_release_by_version_order() {
    // Catches: a previous release taken by name order (v0.10.0 sorts before v0.9.0, so
    // the notes would call this the first release) or a range that starts at the root.
    let notes = notes_of(&with_notes("notes-commits", |_| {}));
    assert!(notes.starts_with("# v0.10.0\n"), "{notes}");
    assert!(notes.contains("## Commits since v0.9.0\n"), "{notes}");
    assert!(notes.contains(" release commit\n"), "{notes}");
    assert!(!notes.contains(" first\n"), "{notes}");
}

#[test]
fn the_first_release_lists_all_its_history() {
    let fx = fx("notes-first");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let notes = notes_of(&fx);
    assert!(
        notes.contains("## Commits (the first release)\n"),
        "{notes}"
    );
    assert!(
        notes.contains(" first\n") && notes.contains(" release commit\n"),
        "{notes}"
    );
}

#[test]
fn the_notes_list_what_is_not_reversible_and_say_so_when_nothing_is() {
    // Catches: a notes writer that omits the section (the offer's warning must reach the
    // people who click approve).
    let notes = notes_of(&with_notes("notes-irreversible", |d| {
        d.not_reversible = vec!["raises the node log version".to_owned()];
    }));
    assert!(
        notes.contains("## Not reversible\n\n- raises the node log version\n"),
        "{notes}"
    );
    let notes = notes_of(&with_notes("notes-reversible", |_| {}));
    assert!(
        notes.contains(
            "## Not reversible\n\nNothing in this release is flagged as not reversible.\n"
        ),
        "{notes}"
    );
}

#[test]
fn the_notes_carry_the_verdict_summary_the_escapes_and_the_withdrawn() {
    let notes = notes_of(&with_notes("notes-summary", |d| {
        d.escapes = vec![
            "handover with a build in flight".to_owned(),
            "two".to_owned(),
        ];
        d.withdrawn = vec!["v0.9.0".to_owned()];
        d.waivers = vec![Waiver::new("min-load", "op", "holiday week").unwrap()];
        d.gates[0] = GateResult::decide("lost-blobs", Sample::Value(0.0), Limit::AtMost(0.0), 1, 2);
    }));
    assert!(notes.contains("- bake: 24h 00m\n"), "{notes}");
    assert!(notes.contains("actions 1000 (needs 1000)"), "{notes}");
    assert!(notes.contains("- gate lost-blobs: 0\n"), "{notes}");
    assert!(
        notes.contains("- waived min-load by op: holiday week\n"),
        "{notes}"
    );
    assert!(notes.contains("2 scenario(s) only gamma found."), "{notes}");
    assert!(notes.contains("## Withdrawn\n\n- v0.9.0\n"), "{notes}");
}

#[test]
fn no_notes_are_written_for_a_verdict_that_fails() {
    // Catches: notes made from an unchecked tag message.
    let fx = with_notes("notes-red", |d| d.status = Status::Red);
    let out = fx.root.join("notes.md");
    let (code, _, err) = run(
        &fx,
        "notes",
        TAG,
        RELEASE,
        &["--out", out.to_str().unwrap()],
    );
    assert_eq!(code, Some(1), "{err}");
    assert!(!out.exists());
}

#[test]
fn a_warning_gate_is_reported_and_does_not_fail_the_verdict() {
    // Catches: a soft gate (latency between 1.5 and 3 times) read as red, or lost from the notes.
    let notes = notes_of(&with_notes("notes-warn", |d| {
        d.gates.push(GateResult::decide(
            "latency-p99",
            Sample::Value(1.8),
            Limit::Band {
                warn: 1.5,
                red: 3.0,
            },
            1,
            2,
        ));
    }));
    assert!(
        notes.contains("- gate latency-p99: 1.8 (warning)\n"),
        "{notes}"
    );
}

#[test]
fn a_gate_with_no_measurement_reads_absent_in_the_notes() {
    // An absent series is red, so the verdict cannot be green; the notes helper is
    // reached through a waiver-free green document whose gate measured nothing finite.
    let mut d = good();
    d.gates[0] = GateResult::decide(
        "lost-blobs",
        Sample::Value(f64::INFINITY),
        Limit::AtMost(f64::MAX),
        1,
        2,
    );
    d.gates[0].verdict = kbf_gate::Verdict::Green;
    d.gates[0].measured = None;
    let fx = fx("notes-absent");
    fx.annotate(TAG, RELEASE, &msg(&d));
    assert!(notes_of(&fx).contains("- gate lost-blobs: absent\n"));
}

#[test]
fn a_long_history_is_cut_and_counted() {
    // Catches: notes that list every commit of a first release.
    let fx = fx("notes-long");
    for i in 0..205 {
        fx.git(&["commit", "-q", "--allow-empty", "-m", &format!("c{i}")]);
    }
    let head = fx.git(&["rev-parse", "HEAD"]);
    let mut d = good();
    d.commit = CommitId::parse(&head).unwrap();
    fx.annotate(TAG, &head, &msg(&d));
    let out = fx.root.join("notes.md");
    let (code, o, err) = run(&fx, "notes", TAG, &head, &["--out", out.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{o}{err}");
    let notes = std::fs::read_to_string(out).unwrap();
    // 205 + first + release commit = 207 commits; 200 listed.
    assert!(notes.contains("- and 7 more\n"), "{notes}");
    assert!(!notes.contains(" c0\n"), "{notes}");
}

#[test]
fn a_tag_outside_the_release_names_is_a_first_release() {
    // `previous` finds no place for a tag that `v*` does not list.
    let fx = fx("notes-rc");
    let mut d = good();
    d.tag = Some("rc1".to_owned());
    fx.annotate("rc1", RELEASE, &msg(&d));
    let out = fx.root.join("notes.md");
    let (code, o, err) = run(
        &fx,
        "notes",
        "rc1",
        RELEASE,
        &["--out", out.to_str().unwrap()],
    );
    assert_eq!(code, Some(0), "{o}{err}");
    assert!(
        std::fs::read_to_string(out)
            .unwrap()
            .contains("(the first release)")
    );
}

#[test]
fn an_unknown_main_ref_is_an_error_not_a_no() {
    // Catches: a missing origin/main read as "the commit is not on main" (or as on it).
    let fx = fx("no-main");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let (dist, repo) = (fx.dist(), fx.repo());
    let (code, _, err) = xtask(&[
        "release",
        "verdict",
        dist.to_str().unwrap(),
        "--repo-dir",
        repo.to_str().unwrap(),
        "--tag",
        TAG,
        "--commit",
        RELEASE,
        "--main-ref",
        "refs/remotes/nowhere/main",
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("merge-base"), "{err}");
}

#[test]
fn a_commit_that_is_not_a_full_sha_is_a_usage_error() {
    let fx = fx("short-commit");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let (code, _, err) = verdict(&fx, TAG, "abc");
    assert_eq!(code, Some(2), "{err}");
}
