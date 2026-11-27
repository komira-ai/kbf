//! `cargo xtask farm-build` end to end: the verb is run as a process in a git repository of its own
//! (made below), with `--buck2` naming the stand-in (src/bin/xtask_fake/buck2.rs, linked as `buck2`),
//! which answers each buck2 call from files of the working directory, names the stand-in linked as
//! `buildrules` beside it as the command line it was asked to build, and records the calls it got
//! (`buck2.calls`; the stand-in `buildrules` records its own in `buildrules.calls`). Each buck2
//! command that talks to the farm reaches the stand-in buck2 through the stand-in linked as
//! `assert_remote`, which records its calls in `assert_remote.calls`. Each test reads
//! the verb's exit status and words, those calls, and for a stale table the patch and the job
//! summary it wrote. Nothing here reaches a network or a build farm.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kbf_testkit::dir::TestDir;
use sha2::Digest as _;
use xtask::farm::table::{PATCH_ARTIFACT, PATCH_FILE, summary_markdown};

const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// A git repository of its own in a scratch directory, holding a root manifest and a lock file.
struct Repo {
    /// Its working tree.
    dir: PathBuf,
    /// Removes the directory when the repository is dropped.
    _scratch: TestDir,
}

impl Repo {
    /// A fresh repository named `name`, its manifest and lock file committed.
    fn new(name: &str) -> Repo {
        let scratch = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-farm-build"),
            name,
        );
        let repo = Repo {
            dir: scratch.path().to_path_buf(),
            _scratch: scratch,
        };
        repo.git(&["init", "--quiet", "-b", "main"]);
        repo.write(
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = []\n",
        );
        repo.write("Cargo.lock", "version = 4\n");
        repo.commit("workspace");
        repo
    }

    /// Writes `text` to `rel`, creating its directory.
    fn write(&self, rel: &str, text: &str) {
        let path = self.dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, text).expect("write");
    }

    /// Runs git here and returns its trimmed stdout.
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
            ])
            .args(args)
            .current_dir(&self.dir)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout)
            .expect("utf-8")
            .trim()
            .to_owned()
    }

    /// Commits everything.
    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "--allow-empty", "-m", message]);
    }
}

/// What buck2 prints for a key that is set.
const SET: &str = r#"{"buildrules_re.linux_x86_64_properties":"OSFamily=Linux,ISA=x86-64"}"#;
/// A farm build's summary line.
const FARM: &str = "Commands: 474 (cached: 0, remote: 474, local: 0)";
/// The audit call.
const AUDIT: &str = "audit config buildrules_re.linux_x86_64_properties --style json";
/// The build of the command line.
const TOOL: &str = "build buildrules//cli:buildrules --show-full-output";
/// The calls of a run that got as far as the build.
const BUILT: [&str; 4] = [AUDIT, TOOL, "build //:ci //:sync_check", "kill"];
/// The calls of a run that stopped at the table.
const STOPPED: [&str; 3] = [AUDIT, TOOL, "kill"];
/// The targets of the lane, in the order the workflow gives them.
const TARGETS: [&str; 2] = ["//:ci", "//:sync_check"];
/// The run the handoff names.
const RUN: &str = "42";
/// The repository the handoff names.
const REPO: &str = "komira-ai/kbf";
/// The table of the stand-in, and its one other generated file.
const TABLE: &str = "buildrules.lock.bzl";
const ARCHIVES: &str = "third_party/buildrules/git_archives.tsv";
/// A dependency that gives the workspace a git source.
const GIT_DEP: &str =
    "\n[workspace.dependencies]\nr = { git = \"https://github.com/o/r\", rev = \"0123abc\" }\n";
/// The fix the real command line ends each of its lines with.
const FIX: &str = "run `buildrules sync` and commit the result";

/// A repository whose table is fresh and committed, the stand-ins in a directory beside it, and
/// a directory for the patch and the summary.
struct Rig {
    repo: Repo,
    bin: TestDir,
    out: TestDir,
}

impl Rig {
    /// A fresh rig named `name`, its workspace given a git dependency when `git`, with `answers`
    /// (file name, text) in the repository's working tree for the stand-in buck2.
    fn new(name: &str, git: bool, answers: &[(&str, &str)]) -> Rig {
        let repo = Repo::new(&format!("farm-{name}"));
        let bin = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("cli-farm-bin-{name}"));
        let out = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("cli-farm-out-{name}"));
        for program in ["buck2", "buildrules", "assert_remote"] {
            std::fs::hard_link(FAKE, bin.join(program)).expect("link the stand-in");
        }
        let rig = Rig { repo, bin, out };
        if git {
            rig.append_manifest(GIT_DEP);
        }
        rig.buildrules(&rig.repo.dir, "sync");
        std::fs::remove_file(rig.repo.dir.join("buildrules.calls")).expect("calls");
        rig.repo.commit("the table");
        for (file, text) in answers {
            rig.repo.write(file, text);
        }
        rig
    }

    /// Appends `text` to the root manifest.
    fn append_manifest(&self, text: &str) {
        let manifest = std::fs::read_to_string(self.repo.dir.join("Cargo.toml")).expect("read");
        self.repo.write("Cargo.toml", &format!("{manifest}{text}"));
    }

    /// The stand-in buck2.
    fn buck2(&self) -> PathBuf {
        self.bin.join("buck2")
    }

    /// The patch file the handoff names.
    fn patch(&self) -> PathBuf {
        self.out.join("patch").join(PATCH_FILE)
    }

    /// The summary file the handoff names.
    fn summary(&self) -> PathBuf {
        self.out.join("summary.md")
    }

    /// Asks the stand-in buildrules for `verb` in `cwd`: its exit status.
    fn buildrules(&self, cwd: &Path, verb: &str) -> Option<i32> {
        Command::new(self.bin.join("buildrules"))
            .arg(verb)
            .current_dir(cwd)
            .output()
            .expect("run the stand-in")
            .status
            .code()
    }

    /// Asks for a failure of the stand-in buildrules.
    fn defect(&self, defect: &str) {
        std::fs::write(self.bin.join("defect"), defect).expect("defect");
    }

    /// The farm build in the repository, with the handoff's flags.
    fn farm_build(&self) -> Output {
        self.farm_build_in(&self.repo.dir, &self.patch(), &self.summary())
    }

    /// The farm build in `cwd`, writing a stale table's fix to `patch` and `summary`.
    fn farm_build_in(&self, cwd: &Path, patch: &Path, summary: &Path) -> Output {
        xtask(cwd, &args(&self.buck2(), patch, summary))
    }
}

/// The flags of a farm build of `//:ci` and `//:sync_check`.
fn args(buck2: &Path, patch: &Path, summary: &Path) -> Vec<String> {
    args_for(&TARGETS, buck2, patch, summary)
}

/// The flags of a farm build of `targets`, one `--target` each.
fn args_for(targets: &[&str], buck2: &Path, patch: &Path, summary: &Path) -> Vec<String> {
    let path = |p: &Path| p.to_str().expect("utf-8").to_owned();
    let mut flags = vec!["farm-build".to_owned(), "--buck2".to_owned(), path(buck2)];
    // The stand-in assert_remote beside the stand-in buck2, which keeps its logs in their directory.
    let bin = buck2.parent().expect("the stand-ins' directory");
    flags.extend([
        "--assert-remote".to_owned(),
        path(&bin.join("assert_remote")),
        "--farm-config".to_owned(),
        path(&bin.join("farm.buckconfig")),
        "--log-dir".to_owned(),
        path(bin),
    ]);
    for target in targets {
        flags.extend(["--target".to_owned(), (*target).to_owned()]);
    }
    flags.extend(
        [
            "--patch",
            &path(patch),
            "--summary",
            &path(summary),
            "--repo",
            REPO,
            "--run-id",
            RUN,
        ]
        .map(str::to_owned),
    );
    flags
}

/// `xtask <args>` in `cwd`.
fn xtask(cwd: &Path, args: &[String]) -> Output {
    Command::new(XTASK)
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run xtask")
}

/// The calls a stand-in recorded in the file `name` of `dir`, one per line.
fn calls_in(dir: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(name))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The calls buck2 got in `dir`.
fn calls(dir: &Path) -> Vec<String> {
    calls_in(dir, "buck2.calls")
}

/// The calls buildrules got in `dir`.
fn tool_calls(dir: &Path) -> Vec<String> {
    calls_in(dir, "buildrules.calls")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The sha256 of `text`, in hex.
fn sha256(text: &str) -> String {
    hex::encode(sha2::Sha256::digest(text))
}

/// The command the summary gives.
fn apply() -> String {
    format!(
        "gh run download {RUN} -R {REPO} -n {PATCH_ARTIFACT} -D buck-out/{PATCH_ARTIFACT}-{RUN} \
         && git apply buck-out/{PATCH_ARTIFACT}-{RUN}/{PATCH_FILE}"
    )
}

/// The log of a passing build.
const BUILD_LOG: &str = "[t] Waiting on 3 actions\n[t] Commands: 474 (cached: 0, remote: 474, local: 0)\nBUILD SUCCEEDED\n";
/// The answers of a passing build.
const PASSING: [(&str, &str); 2] = [
    ("buck2-audit.stdout", SET),
    ("buck2-build.stderr", BUILD_LOG),
];

#[test]
fn a_farm_build_with_a_fresh_table_prints_its_summary_line_and_stops_buck2_with_status_0() {
    // Catches: the build not run, run before the worker check or the table's check, its log not
    // passed on, its summary not printed, buck2's daemon left running, or a fresh table synced or
    // handed over as a patch.
    let d = Rig::new("ok", false, &PASSING);
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{FARM}\n"));
    assert_eq!(stderr(&o), BUILD_LOG);
    assert_eq!(calls(&d.repo.dir), BUILT);
    assert_eq!(tool_calls(&d.repo.dir), ["check"]);
    assert!(!d.patch().exists());
    assert!(!d.summary().exists());
}

/// The stale table of `d` handed over: the verb fails before the build with the stale inputs
/// `inputs` and the changed files `files` in its words and in the summary (after what the summary
/// held), and the patch it wrote, applied to a clone of the stale commit, makes the table fresh
/// and the farm build pass there.
fn hands_over(d: &Rig, inputs: &[String], files: &[&str]) {
    std::fs::create_dir_all(d.out.path()).expect("out");
    std::fs::write(d.summary(), "earlier\n").expect("summary");
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "");
    let files: Vec<String> = files.iter().map(|f| (*f).to_owned()).collect();
    assert_eq!(
        stderr(&o),
        format!(
            "buildrules: wrote {TABLE}\nxtask: the generated build table is stale: {}. \
             buildrules.lock.bzl, buildrules.bzl and third_party/buildrules/ are generated from \
             the Cargo files by `buildrules sync`, which this job ran at the pinned version; what \
             it changed ({}) is the artifact {PATCH_ARTIFACT} of this run, made against this \
             run's merge of the branch into main. Apply it in the branch's checkout, then commit \
             and push (if it does not apply, merge main first): {}\n",
            inputs.join("; "),
            files.join(", "),
            apply()
        )
    );
    assert_eq!(calls(&d.repo.dir), STOPPED);
    let buck2 = d.buck2();
    assert_eq!(
        tool_calls(&d.repo.dir),
        [
            "check".to_owned(),
            format!("sync --buck2 {}", buck2.display()),
            "check".to_owned()
        ]
    );
    assert_eq!(
        std::fs::read_to_string(d.summary()).expect("summary"),
        format!("earlier\n{}", summary_markdown(inputs, &files, &apply()))
    );

    // The patch, on the stale commit as the pull request's branch has it.
    let clone = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "cli-farm-clone");
    let tree = clone.join("tree");
    d.repo
        .git(&["clone", "--quiet", ".", tree.to_str().expect("utf-8")]);
    assert_eq!(d.buildrules(&tree, "check"), Some(1), "the clone is stale");
    let applied = Command::new("git")
        .arg("apply")
        .arg(d.patch())
        .current_dir(&tree)
        .output()
        .expect("git apply");
    assert!(applied.status.success(), "{applied:?}");
    assert_eq!(
        d.buildrules(&tree, "check"),
        Some(0),
        "the patch makes it fresh"
    );
    for (file, text) in PASSING {
        std::fs::write(tree.join(file), text).expect("an answer");
    }
    let o = d.farm_build_in(&tree, &clone.join("p"), &clone.join("s"));
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{FARM}\n"));
    assert_eq!(calls(&tree), BUILT);
}

/// The stale line of the stand-in for the file `input`, from the text `was` to the text `now`.
fn changed(input: &str, was: &str, now: &str) -> String {
    format!(
        "{input} changed since `buildrules sync` (recorded sha256 {}, now {})",
        sha256(was),
        sha256(now)
    )
}

#[test]
fn a_changed_manifest_hands_over_a_patch_that_changes_the_table_and_adds_a_file() {
    // Catches: a stale table built (or failed without a fix), sync not run or run without the
    // job's buck2, the second check skipped, a patch that leaves out a file sync made (git diff
    // without the new file in the index), the fix's words (STALE mark and `buildrules sync`
    // advice) left in the inputs, a summary that replaces what the job wrote before, a patch
    // that does not apply to the branch or leaves the table stale, and a patch that takes in a
    // change of the working tree that sync did not make (a tracked file outside the paths sync
    // writes, changed by an earlier step).
    let d = Rig::new("stale-add", false, &PASSING);
    let was = std::fs::read_to_string(d.repo.dir.join("Cargo.toml")).expect("read");
    d.append_manifest(GIT_DEP);
    d.repo.commit("a git dependency, the table not synced");
    d.repo.write("README.md", "changed by an earlier step\n");
    let now = format!("{was}{GIT_DEP}");
    hands_over(&d, &[changed("Cargo.toml", &was, &now)], &[TABLE, ARCHIVES]);
}

#[test]
fn a_removed_dependency_hands_over_a_patch_that_deletes_a_file() {
    // Catches: a patch that keeps a file sync removed, and one that cannot be applied for it.
    let d = Rig::new("stale-remove", true, &PASSING);
    let was = std::fs::read_to_string(d.repo.dir.join("Cargo.toml")).expect("read");
    let now = was.replace(GIT_DEP, "");
    d.repo.write("Cargo.toml", &now);
    d.repo
        .commit("the git dependency removed, the table not synced");
    hands_over(&d, &[changed("Cargo.toml", &was, &now)], &[TABLE, ARCHIVES]);
}

#[test]
fn a_generated_file_removed_by_hand_hands_over_a_patch_that_writes_it_again() {
    // Catches: a stale generated file (the inputs unchanged) not taken for a stale table, and a
    // patch that changes only files that were tracked.
    let d = Rig::new("stale-generated", true, &PASSING);
    d.repo.git(&["rm", "--quiet", ARCHIVES]);
    d.repo.commit("a generated file removed by hand");
    hands_over(
        &d,
        &[format!("{ARCHIVES} is not what sync writes")],
        &[ARCHIVES],
    );
}

#[test]
fn two_changed_inputs_are_both_named() {
    // Catches: only the first stale line of check taken as the stale inputs.
    let d = Rig::new("stale-two", false, &PASSING);
    let manifest = std::fs::read_to_string(d.repo.dir.join("Cargo.toml")).expect("read");
    let lock = std::fs::read_to_string(d.repo.dir.join("Cargo.lock")).expect("read");
    d.repo.write("Cargo.toml", &format!("{manifest}# a\n"));
    d.repo.write("Cargo.lock", &format!("{lock}# b\n"));
    d.repo.commit("both changed");
    hands_over(
        &d,
        &[
            changed("Cargo.toml", &manifest, &format!("{manifest}# a\n")),
            changed("Cargo.lock", &lock, &format!("{lock}# b\n")),
        ],
        &[TABLE],
    );
}

/// Makes the table of `d` stale: a comment in the root manifest, committed.
fn make_stale(d: &Rig) {
    d.append_manifest("# a comment\n");
    d.repo.commit("the manifest changed, the table not synced");
}

/// Asserts that the run `o` of `d` failed with `words` after the log `log`, before the build,
/// leaving no patch and no summary.
fn refused(d: &Rig, o: &Output, log: &str, words: &str) {
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(o), "");
    assert_eq!(stderr(o), format!("{log}xtask: {words}\n"));
    assert_eq!(calls(&d.repo.dir), STOPPED);
    assert!(!d.patch().exists());
    assert!(!d.summary().exists());
}

#[test]
fn a_failed_sync_fails_with_its_output_and_hands_over_nothing() {
    // Catches: a failed sync taken for a fix, its words hidden, or a patch of a half-written
    // table handed over.
    let d = Rig::new("sync-fails", false, &PASSING);
    make_stale(&d);
    d.defect("sync-fails");
    let o = d.farm_build();
    refused(
        &d,
        &o,
        "buildrules: sync: CargoFailed\n",
        "the generated build table is stale, and `buildrules sync` failed (exit status: 1); its \
         output is above",
    );
    assert_eq!(tool_calls(&d.repo.dir).len(), 2);
}

#[test]
fn a_sync_that_leaves_the_table_stale_fails_with_what_check_still_says() {
    // Catches: a patch handed over without checking that it makes the table fresh.
    let d = Rig::new("sync-no-fix", false, &PASSING);
    let was = std::fs::read_to_string(d.repo.dir.join("Cargo.toml")).expect("read");
    make_stale(&d);
    d.defect("sync-writes-nothing");
    let o = d.farm_build();
    let line = changed("Cargo.toml", &was, &format!("{was}# a comment\n"));
    refused(
        &d,
        &o,
        &format!("buildrules: wrote {TABLE}\n"),
        &format!(
            "`buildrules sync` ran, and `buildrules check` still fails (exit status: 1): \
             buildrules: STALE: {line}: {FIX}"
        ),
    );
    assert_eq!(tool_calls(&d.repo.dir).len(), 3);
}

#[test]
fn a_missing_generated_file_without_a_stale_mark_hands_over_a_patch_that_adds_it() {
    // Catches: a problem check prints without `STALE:` (a generated file deleted) taken for one
    // sync cannot fix, and a patch that leaves out a new file at the repository's root.
    let d = Rig::new("no-table", false, &PASSING);
    d.repo.git(&["rm", "--quiet", TABLE]);
    d.repo.commit("no table");
    hands_over(&d, &[format!("{TABLE} is missing")], &[TABLE]);
}

#[test]
fn a_check_that_cannot_judge_the_table_fails_with_its_words_and_runs_no_sync() {
    // Catches: a check that could not run (a usage error, status 2) taken for a fresh table or a
    // stale one, and check's words hidden.
    let d = Rig::new("check-usage", false, &PASSING);
    d.defect("check-usage");
    let o = d.farm_build();
    refused(
        &d,
        &o,
        "",
        "`buildrules check` could not judge the table (exit status: 2): buildrules: check: \
         unknown argument --nope",
    );
    assert_eq!(tool_calls(&d.repo.dir), ["check"]);
}

#[test]
fn a_command_line_that_cannot_be_built_named_or_run_fails_before_the_check() {
    // Catches: a failed build of the command line read as a fresh table, an output line of
    // another target or with no path taken for it, and a command line that cannot start taken
    // for a passing check.
    let mut answers = PASSING.to_vec();
    answers.extend([
        ("buck2-tool.status", "3"),
        ("buck2-tool.stdout", ""),
        ("buck2-tool.stderr", "no cell buildrules\n"),
    ]);
    let d = Rig::new("tool-fails", false, &answers);
    let o = d.farm_build();
    refused(
        &d,
        &o,
        "",
        "`buck2 build buildrules//cli:buildrules` failed (exit status: 3): no cell buildrules",
    );
    assert_eq!(tool_calls(&d.repo.dir), Vec::<String>::new());

    for (name, stdout) in [
        ("tool-none", ""),
        ("tool-other", "buildrules//cli:other /x/other\n"),
        ("tool-no-path", "buildrules//cli:buildrules \n"),
    ] {
        let mut answers = PASSING.to_vec();
        answers.push(("buck2-tool.stdout", stdout));
        let d = Rig::new(name, false, &answers);
        let o = d.farm_build();
        refused(
            &d,
            &o,
            "",
            &format!(
                "`buck2 build buildrules//cli:buildrules --show-full-output` named no output of \
                 buildrules//cli:buildrules: {stdout:?}"
            ),
        );
    }

    let mut answers = PASSING.to_vec();
    answers.push((
        "buck2-tool.stdout",
        "buildrules//cli:buildrules /nonexistent/buildrules\n",
    ));
    let d = Rig::new("tool-missing", false, &answers);
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).starts_with("xtask: cannot run \"/nonexistent/buildrules\": "),
        "{}",
        stderr(&o)
    );
    assert_eq!(calls(&d.repo.dir), STOPPED);
}

#[test]
fn a_patch_that_git_cannot_make_fails_and_hands_over_nothing() {
    // Catches: a failed git taken for an empty patch, and its words hidden.
    let d = Rig::new("git-fails", false, &PASSING);
    make_stale(&d);
    let o = Command::new(XTASK)
        .current_dir(&d.repo.dir)
        .args(args(&d.buck2(), &d.patch(), &d.summary()))
        .env("GIT_DIR", d.out.join("no-repository"))
        .output()
        .expect("run xtask");
    assert_eq!(o.status.code(), Some(1));
    let words = stderr(&o);
    let start = "buildrules: wrote buildrules.lock.bzl\nxtask: `git ls-files --others \
                 --exclude-standard -z -- BUCK Cargo.lock buildrules.bzl buildrules.lock.bzl \
                 third_party/buildrules toolchains` failed (exit status: 128): fatal: ";
    assert!(words.starts_with(start), "{words}");
    assert!(!d.patch().exists());
    assert!(!d.summary().exists());
}

#[test]
fn a_patch_or_summary_that_cannot_be_written_fails() {
    // Catches: a fix the job could not write reported as handed over.
    let d = Rig::new("patch-unwritable", false, &PASSING);
    make_stale(&d);
    std::fs::write(d.out.join("patch"), "a file, not a directory").expect("block");
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(1));
    let patch = d.patch();
    assert_eq!(
        stderr(&o),
        format!(
            "buildrules: wrote {TABLE}\nxtask: cannot write {patch:?}: File exists (os error 17)\n"
        )
    );
    assert!(!d.summary().exists());

    let d = Rig::new("summary-unwritable", false, &PASSING);
    make_stale(&d);
    let o = d.farm_build_in(&d.repo.dir, &d.patch(), d.out.path());
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        format!(
            "buildrules: wrote {TABLE}\nxtask: cannot write {:?}: Is a directory (os error 21)\n",
            d.out.path()
        )
    );
    assert!(d.patch().exists());
}

#[test]
fn a_build_with_actions_on_this_machine_fails_with_status_1() {
    // Catches: a build that ran an action on the runner taken for a farm build.
    let line = "Commands: 5 (cached: 2, remote: 1, local: 2)";
    let log = format!("{line}\nBUILD SUCCEEDED\n");
    let d = Rig::new(
        "local",
        false,
        &[("buck2-audit.stdout", SET), ("buck2-build.stderr", &log)],
    );
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "");
    assert_eq!(
        stderr(&o),
        format!(
            "{log}xtask: `{line}`: actions ran on this machine, not on the farm (local must be 0)\n"
        )
    );
    assert_eq!(calls(&d.repo.dir), BUILT);
}

#[test]
fn a_build_with_no_summary_line_fails_with_status_1() {
    // Catches: a build whose summary is missing taken for one that ran nothing locally.
    let d = Rig::new(
        "no-summary",
        false,
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-build.stderr", "BUILD SUCCEEDED\n"),
        ],
    );
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        "BUILD SUCCEEDED\nxtask: the build printed no summary line (`Commands: N (cached: N, remote: N, local: N)`)\n"
    );
    assert_eq!(calls(&d.repo.dir), BUILT);
}

#[test]
fn a_failed_build_fails_with_status_1_shows_its_log_and_still_stops_buck2() {
    // Catches: a failed build read as a pass because its summary says `local: 0`, its log (the
    // failing action's words) hidden, or the daemon left running after a failure.
    let log = format!("Action failed: test of kbf-types\n{FARM}\nBUILD FAILED\n");
    let d = Rig::new(
        "failed",
        false,
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-build.stderr", &log),
            ("buck2-build.status", "3"),
        ],
    );
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "");
    assert_eq!(
        stderr(&o),
        format!("{log}xtask: `buck2 build //:ci //:sync_check` failed (exit status: 3)\n")
    );
    assert_eq!(calls(&d.repo.dir), BUILT);
}

#[test]
fn no_worker_property_set_fails_before_the_build_with_status_1() {
    // Catches: a build started with no farm to run on (buck2 would then fail at analysis, or a
    // later change could fall back to this machine): the key unset (buck2's `{}`, the stand-in's
    // answer when it has no file), set to nothing, and set to blanks.
    for (name, audit) in [
        ("unset", None),
        (
            "empty",
            Some(r#"{"buildrules_re.linux_x86_64_properties":""}"#),
        ),
        (
            "blank",
            Some(r#"{"buildrules_re.linux_x86_64_properties":"  "}"#),
        ),
    ] {
        let answers: Vec<(&str, &str)> = audit
            .map(|text| ("buck2-audit.stdout", text))
            .into_iter()
            .collect();
        let d = Rig::new(&format!("workers-{name}"), false, &answers);
        let o = d.farm_build();
        assert_eq!(o.status.code(), Some(1), "{name}");
        assert_eq!(stdout(&o), "", "{name}");
        assert_eq!(
            stderr(&o),
            "xtask: buildrules_re.linux_x86_64_properties is empty: `farm-config` wrote no worker \
             properties, so buck2 has no farm to build on\n",
            "{name}"
        );
        assert_eq!(calls(&d.repo.dir), [AUDIT, "kill"], "{name}");
        assert_eq!(tool_calls(&d.repo.dir), Vec::<String>::new(), "{name}");
    }
}

#[test]
fn a_failed_audit_fails_before_the_build_with_status_1() {
    // Catches: an audit that failed (no .buckconfig, a broken cell) read as a set key, though it
    // printed one.
    let d = Rig::new(
        "audit-fails",
        false,
        &[
            ("buck2-audit.stdout", SET),
            ("buck2-audit.stderr", "no .buckconfig\n"),
            ("buck2-audit.status", "3"),
        ],
    );
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        stderr(&o),
        "xtask: `buck2 audit config` failed (exit status: 3): no .buckconfig\n"
    );
    assert_eq!(calls(&d.repo.dir), [AUDIT, "kill"]);
}

#[test]
fn a_buck2_that_cannot_start_fails_with_status_1_and_the_flags_are_required() {
    // Catches: a missing buck2 taken for a passing build, and a verb that runs without its
    // target, its buck2 or any of the handoff's flags.
    let d = Rig::new("no-buck2", false, &[]);
    let o = xtask(
        &d.repo.dir,
        &args(Path::new("/nonexistent/buck2"), &d.patch(), &d.summary()),
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).starts_with("xtask: cannot run \"/nonexistent/buck2\": "),
        "{}",
        stderr(&o)
    );
    // One target, so that leaving out its flag leaves none.
    let all = args_for(&["//:ci"], &d.buck2(), &d.patch(), &d.summary());
    // Each flag and its value, left out in turn.
    for flag in (1..all.len()).step_by(2) {
        let mut without = all.clone();
        without.drain(flag..flag + 2);
        let o = xtask(&d.repo.dir, &without);
        assert_eq!(o.status.code(), Some(2), "{without:?}");
        assert!(stderr(&o).contains("required"), "{}", stderr(&o));
    }
    assert_eq!(calls(&d.repo.dir), Vec::<String>::new());
}

/// What the stand-in assert_remote prints for a run its watchdog ended (`xtask_fake/buck2.rs`).
const CUT: &str = "\
buck2-farm: watchdog: the connection to the farm from local port 41022 has heard nothing from the farm for 5 min 1 s, past the bound of 5 min 0 s: the farm has closed it and buck2 would wait on it for ever; ending buck2 (SIGINT)
buck2-farm: stopping buck2's daemon, which holds the dead connection (`buck2 kill`)
buck2-farm: the first run hung on a dead connection to the farm and was ended; waiting for the farm's port to answer, up to 10 min 0 s
";

/// The calls assert_remote got in `dir`.
fn watched_calls(dir: &Path) -> Vec<String> {
    calls_in(dir, "assert_remote.calls")
}

#[test]
fn both_farm_builds_run_under_assert_remote_with_their_logs_the_summary_and_the_farm_config() {
    // Catches: a buck2 command that talks to the farm run outside the watchdog (the build of the
    // command line or the build), one given another log, job summary or farm config than the
    // verb's, a command passed on with its words changed, and the audit or the daemon's stop
    // (which reach no farm) sent through it.
    let d = Rig::new("watched", false, &PASSING);
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let bin = d.bin.path();
    let flags = format!(
        "--summary {} --farm-config {} -- {}",
        d.summary().display(),
        bin.join("farm.buckconfig").display(),
        d.buck2().display()
    );
    assert_eq!(
        watched_calls(&d.repo.dir),
        [
            format!(
                "run --log {} {flags} {TOOL}",
                bin.join("buck2-farm-tool.log").display()
            ),
            format!(
                "run --log {} {flags} build //:ci //:sync_check",
                bin.join("buck2-farm.log").display()
            ),
        ]
    );
    assert_eq!(calls(&d.repo.dir), BUILT);
}

#[test]
fn a_run_the_watchdog_ended_and_ran_again_passes_on_the_run_after_it_with_its_words_in_the_log() {
    // Catches: the watchdog's lines kept out of the job's log, a build whose run again passed
    // taken for a failure, or the summary of the run it ended read in place of the last run's.
    let d = Rig::new("watchdog-cut", false, &PASSING);
    d.defect("watchdog-cut");
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), format!("{FARM}\n"));
    assert_eq!(stderr(&o), format!("{CUT}{BUILD_LOG}"));
    assert_eq!(
        calls(&d.repo.dir),
        [
            AUDIT,
            "kill",
            TOOL,
            "kill",
            "build //:ci //:sync_check",
            "kill"
        ]
    );
}

#[test]
fn a_build_whose_every_run_hung_fails_with_status_1_and_still_stops_buck2() {
    // Catches: assert_remote's failure after its last run read as a pass (its log holds no
    // failed action and no summary), its words hidden, or the daemon left running after it.
    let d = Rig::new("watchdog-gives-up", false, &PASSING);
    d.defect("watchdog-gives-up");
    let o = d.farm_build();
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(stdout(&o), "");
    assert_eq!(
        stderr(&o),
        "buck2-farm: the third run hung on a dead connection to the farm too; not running it \
         again\nxtask: `buck2 build //:ci //:sync_check` failed (exit status: 1)\n"
    );
    assert_eq!(calls(&d.repo.dir), [AUDIT, TOOL, "kill", "kill"]);
}

#[test]
fn an_assert_remote_that_cannot_start_fails_before_the_check_and_stops_buck2() {
    // Catches: a missing assert_remote taken for a fresh table or a passing build, or the build
    // run without it.
    let d = Rig::new("no-assert-remote", false, &PASSING);
    let mut flags = args(&d.buck2(), &d.patch(), &d.summary());
    let at = flags
        .iter()
        .position(|f| f == "--assert-remote")
        .expect("the flag");
    flags[at + 1] = "/nonexistent/assert_remote".to_owned();
    let o = xtask(&d.repo.dir, &flags);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).starts_with("xtask: cannot run \"/nonexistent/assert_remote\": "),
        "{}",
        stderr(&o)
    );
    assert_eq!(calls(&d.repo.dir), [AUDIT, "kill"]);
    assert_eq!(tool_calls(&d.repo.dir), Vec::<String>::new());
}

#[test]
fn the_stand_in_assert_remote_refuses_a_call_of_another_shape() {
    // Catches: a stand-in that runs a command the verb never gave it in that shape (no `run`, a
    // missing flag, no `--`), which would let a test pass on a call the real tool refuses.
    let d = Rig::new("stand-in-usage", false, &[]);
    let tool = d.bin.join("assert_remote");
    for args in [
        vec!["run", "--log", "l", "--summary", "s", "--farm-config", "c"],
        vec![
            "check",
            "--log",
            "l",
            "--summary",
            "s",
            "--farm-config",
            "c",
            "--",
            "buck2",
        ],
        vec!["run", "--log", "l", "--farm-config", "c", "--", "buck2"],
        vec![
            "run",
            "--log",
            "l",
            "--summary",
            "s",
            "--farm-config",
            "c",
            "--",
        ],
    ] {
        let o = Command::new(&tool)
            .args(&args)
            .current_dir(&d.repo.dir)
            .output()
            .expect("run the stand-in");
        assert_eq!(o.status.code(), Some(2), "{args:?}");
    }
    assert_eq!(calls(&d.repo.dir), Vec::<String>::new());
}
