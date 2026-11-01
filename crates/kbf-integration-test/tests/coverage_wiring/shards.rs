//! The coverage run is split: the matrix job `test-shard` runs each core's fast tests in parts
//! (`nextest --partition count:<part>/<parts>`), and the job `test` builds the same instrumented
//! binaries, waits for every shard's raw profiles, makes ONE report of them all and gates it.
//! The gate is the same gate only while the split loses nothing, and nothing but this reads the
//! workflow for that. The rules, each a way the 99% per crate would be measured over less than
//! the whole set of tests (a crate could then fail, or, worse, pass on a number that left tests
//! out) or the required check would pass over a red shard:
//!
//! - **Each shard partitions the whole set exactly once.** For each core (`legacy`, `js`), the
//!   matrix lists parts 1 to N, each once, and every entry says `parts: N`; every entry of a core
//!   runs the same packages (`args`), and a shard's nextest command is `--partition
//!   count:$PART/$PARTS` of `$ARGS` with the branch coverage and the `pr` profile and no filter.
//!   Both cores are in the matrix, the legacy one on the whole workspace and the js one on the
//!   `farm-core-js` build.
//! - **A shard's profiles are its own and arrive.** Their file names carry the core and the part
//!   (the process id alone repeats between machines); the shard's last step uploads them, with no
//!   `if:`, so the job has succeeded exactly when they are up.
//! - **A shard has the disk it needs.** The matrix entry's `min_free_gb`, which the early disk
//!   check refuses below, is at least the core's measured need ([`CORES`]), so a runner that is
//!   too short stops there and not with tests failing for want of room. The test step does not
//!   pass when there are no tests (`--no-tests=pass`), so a part that ran nothing is red.
//! - **A shard is found by its name.** The job is named `test-shard (<core>-<part>-of-<parts>)`,
//!   and `test` asks for each artifact with that text (`--artifact <name>=<text>`), so that it
//!   counts an artifact only when the shard's job of the latest attempt made it (a re-run of
//!   the run leaves the earlier attempt's artifacts there).
//! - **`test` takes every shard.** It builds each core's instrumented tests (the `args` of the
//!   matrix), drops the profiles that listing them wrote, waits for the artifact of every shard of
//!   the matrix (and fails when a shard failed), downloads them flat into the one directory the
//!   report merges, refuses a directory in which a shard's profiles are missing, reports, and
//!   only then gates. None of those steps has an `if:`.
//! - **The check names stay.** The required checks are `lint` and `test`: the jobs have those
//!   ids and no `name:`; `test` has no job-level `if:` (a skipped required check passes) and may
//!   read the run's jobs and artifacts (`actions: read`).

use super::{flag, input, words};
use crate::workflows;

/// The matrix job.
const SHARD: &str = "test-shard";
/// The gate job, and the other required check.
const GATE: &str = "test";
const LINT: &str = "lint";
/// The cores, and what a core's `args` must hold.
/// The least `min_free_gb` of a core: its measured disk use (legacy about 67 GB: the build 59,
/// the store floor 7.5; js about 50) and a margin.
const CORES: [(&str, &str, usize); 2] = [
    ("legacy", "--workspace", 75),
    ("js", "--features kbf-server/farm-core-js", 55),
];
/// The directory a shard merges its raw profiles in, and the `test` job merges the shards'
/// merged profiles into: the one `llvm-cov report` reads.
const PROFILES: &str = "target/llvm-cov-target";
/// Where `test` downloads the shards' merged profiles (a few MB each, not the raw GB).
const DOWNLOADS: &str = "target/shard-profdata";
/// What the shards name their artifacts, and the gate downloads.
const ARTIFACT: &str = "profdata-${{ matrix.core }}-${{ matrix.part }}";
/// The text of a shard job's name that tells it from the others, and the whole name.
const JOB_KEY: &str = "(${{ matrix.core }}-${{ matrix.part }}-of-${{ matrix.parts }})";
const JOB_NAME: &str = "test-shard (${{ matrix.core }}-${{ matrix.part }}-of-${{ matrix.parts }})";
const PATTERN: &str = "profdata-*";
/// What a shard's profile file names begin with.
const FILE_PREFIX: &str = "kbf-${{ matrix.core }}-${{ matrix.part }}-";

/// One entry of the matrix.
struct Entry(Vec<(String, String)>);

impl Entry {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// `template` with each `${{ matrix.<key> }}` replaced by the entry's value.
    fn expand(&self, template: &str) -> String {
        self.0.iter().fold(template.to_owned(), |text, (k, v)| {
            text.replace(&format!("${{{{ matrix.{k} }}}}"), v)
        })
    }
}

fn entries(workflow: &str) -> Result<Vec<Entry>, String> {
    Ok(workflows::job_matrix(workflow, SHARD)?
        .into_iter()
        .map(Entry)
        .collect())
}

/// The shards' `--artifact` values (the artifact name, `=`, the text of the job's name) and
/// profile prefixes the matrix makes, in matrix order.
fn expected(entries: &[Entry]) -> (Vec<String>, Vec<String>) {
    (
        entries
            .iter()
            .map(|e| e.expand(&format!("{ARTIFACT}={JOB_KEY}")))
            .collect(),
        entries.iter().map(|e| e.expand(FILE_PREFIX)).collect(),
    )
}

/// The matrix runs each part of each core's tests once.
fn partition_rules(entries: &[Entry]) -> Result<(), String> {
    if let Some(e) = entries
        .iter()
        .find(|e| !CORES.iter().any(|(c, _, _)| e.get("core") == Some(*c)))
    {
        return Err(format!(
            "a matrix entry names the core {:?}, which is neither `legacy` nor `js`",
            e.get("core")
        ));
    }
    for (core, needs, _) in CORES {
        let mine: Vec<&Entry> = entries
            .iter()
            .filter(|e| e.get("core") == Some(core))
            .collect();
        let Some(first) = mine.first() else {
            return Err(format!(
                "the matrix of `{SHARD}` has no `{core}` entry, so that core's tests would not run"
            ));
        };
        let number = |e: &Entry, key: &str| e.get(key).and_then(|v| v.parse::<usize>().ok());
        let Some(parts) = number(first, "parts") else {
            return Err(format!("a `{core}` entry has no numeric `parts`"));
        };
        let mut got = Vec::new();
        for e in &mine {
            if number(e, "parts") != Some(parts) {
                return Err(format!(
                    "the `{core}` entries do not all say `parts: {parts}`, so their parts are of different splits"
                ));
            }
            let Some(part) = number(e, "part") else {
                return Err(format!("a `{core}` entry has no numeric `part`"));
            };
            got.push(part);
        }
        got.sort_unstable();
        if got != (1..=parts).collect::<Vec<_>>() {
            return Err(format!(
                "the `{core}` entries are parts {got:?} of {parts}, not each of 1 to {parts} once, so some tests would run twice or not at all"
            ));
        }
        if mine.iter().any(|e| e.get("args") != first.get("args")) {
            return Err(format!(
                "the `{core}` entries do not run the same packages (`args`), so the parts are not parts of one set of tests"
            ));
        }
        for e in &mine {
            let least = CORES
                .iter()
                .find(|(c, _, _)| *c == core)
                .map_or(0, |(_, _, gb)| *gb);
            match number(e, "min_free_gb") {
                None => {
                    return Err(format!("a `{core}` entry has no numeric `min_free_gb`"));
                }
                Some(gb) if gb < least => {
                    return Err(format!(
                        "a `{core}` entry says `min_free_gb: {gb}`, under the {least} its build and tests need, so the early disk check would pass a runner that is too short"
                    ));
                }
                Some(_) => {}
            }
        }
        if !first.get("args").is_some_and(|a| a.contains(needs)) {
            return Err(format!(
                "the `args` of the `{core}` entries do not contain `{needs}`"
            ));
        }
    }
    Ok(())
}

/// The `test-shard` job meets the rules; an error says how it does not.
pub fn shard_wiring(workflow: &str) -> Result<(), String> {
    let entries = entries(workflow)?;
    partition_rules(&entries)?;
    if workflows::job_name(workflow, SHARD)?.as_deref() != Some(JOB_NAME) {
        return Err(format!(
            "the job `{SHARD}` is not named `{JOB_NAME}`, which `test` finds each shard's job by"
        ));
    }
    let env = workflows::job_env(workflow, SHARD)?;
    let var = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    for (name, want) in [
        ("PART", "${{ matrix.part }}"),
        ("PARTS", "${{ matrix.parts }}"),
        ("ARGS", "${{ matrix.args }}"),
        ("MIN_FREE_GB", "${{ matrix.min_free_gb }}"),
    ] {
        if var(name) != Some(want) {
            return Err(format!("the job `{SHARD}` does not set `{name}: {want}`"));
        }
    }
    let profile = var("LLVM_PROFILE_FILE_NAME").unwrap_or_default();
    if !profile.starts_with(FILE_PREFIX) || !profile.ends_with(".profraw") {
        return Err(format!(
            "LLVM_PROFILE_FILE_NAME is `{profile}`, which does not begin `{FILE_PREFIX}` (the core and the part) and end `.profraw`, so the shards' profiles could overwrite one another"
        ));
    }
    let steps = workflows::job_steps(workflow, SHARD)?;
    let disk = steps.iter().find(|s| {
        s.run
            .as_deref()
            .is_some_and(|r| r.contains("xtask runner-disk"))
    });
    let disk_words = disk.and_then(|s| s.run.as_deref()).map(words);
    if disk.is_none_or(|s| s.conditional)
        || disk_words.as_deref().and_then(|w| flag(w, "--min-free-gb")) != Some("$MIN_FREE_GB")
    {
        return Err(format!(
            "the job `{SHARD}` has no unconditional step `xtask runner-disk ... --min-free-gb \"$MIN_FREE_GB\"`, so the early disk check would not use the matrix's `min_free_gb`"
        ));
    }
    let run = steps
        .iter()
        .find(|s| {
            s.run
                .as_deref()
                .is_some_and(|r| r.contains("llvm-cov nextest"))
        })
        .ok_or_else(|| format!("the job `{SHARD}` has no step running `llvm-cov nextest`"))?;
    let text = run.run.as_deref().unwrap_or_default();
    let w = words(text);
    for need in [
        "$ARGS",
        "--partition=count:$PART/$PARTS",
        "--branch",
        "--no-report",
        "--no-fail-fast",
    ] {
        if !w.contains(&need) {
            return Err(format!("the test step of `{SHARD}` lacks `{need}`: {text}"));
        }
    }
    if w.iter()
        .any(|x| x.starts_with("--no-tests") && *x != "--no-tests=fail")
    {
        return Err(format!(
            "the test step of `{SHARD}` passes when no test runs (`--no-tests`), so a part that ran nothing would be green"
        ));
    }
    if flag(&w, "--profile") != Some("pr") {
        return Err(format!(
            "the test step of `{SHARD}` does not run `--profile pr`"
        ));
    }
    if w.iter()
        .any(|x| matches!(*x, "-E" | "--filterset") || x.starts_with("--filterset="))
    {
        return Err(format!(
            "the test step of `{SHARD}` filters the tests, which would leave some out of every part"
        ));
    }
    if run.conditional {
        return Err(format!(
            "the test step of `{SHARD}` has an `if:` or `continue-on-error`, so a shard could pass without running its tests"
        ));
    }
    let merge_at = steps.iter().position(|s| {
        s.run
            .as_deref()
            .is_some_and(|r| r.contains("xtask merge-profiles"))
    });
    let Some(merge_at) = merge_at else {
        return Err(format!(
            "the job `{SHARD}` has no step `cargo xtask merge-profiles` that merges its raw profiles, so the raw profiles (GB) would be what `test` downloads"
        ));
    };
    let merge = &steps[merge_at];
    let merge_text = merge.run.as_deref().unwrap_or_default();
    if merge.conditional {
        return Err(format!(
            "the merge step of `{SHARD}` has an `if:` or `continue-on-error`, so a shard could upload without its profiles merged"
        ));
    }
    // Text, not words: the prefix and the name hold `${{ matrix.<key> }}`, which has spaces.
    let wants = [
        format!("--dir {PROFILES} "),
        "--extension profraw ".to_owned(),
        format!("--prefix {FILE_PREFIX} "),
        format!("--out {PROFILES}/{FILE_PREFIX}merged.profdata"),
    ];
    if wants.iter().any(|w| !merge_text.contains(w.as_str())) {
        return Err(format!(
            "the merge step of `{SHARD}` must merge the shard's own `{FILE_PREFIX}*.profraw` files of `{PROFILES}` into `{PROFILES}/{FILE_PREFIX}merged.profdata`"
        ));
    }
    let tests_at = steps.iter().position(|s| std::ptr::eq(s, run));
    if merge_at + 1 >= steps.len() || tests_at.is_none_or(|t| merge_at < t) {
        return Err(format!(
            "the merge step of `{SHARD}` does not come after the tests and before the upload"
        ));
    }
    let last = steps.last().ok_or("the shard job has no steps")?;
    let uploads = last
        .uses
        .as_deref()
        .is_some_and(|u| u.starts_with("actions/upload-artifact@"));
    if !uploads || last.conditional {
        return Err(format!(
            "the last step of `{SHARD}` must be the upload of its raw profiles with no `if:`, so that a shard has succeeded exactly when its profiles are up"
        ));
    }
    if input(last, "name") != Some(ARTIFACT) {
        return Err(format!("the upload step's `name` is not `{ARTIFACT}`"));
    }
    if !input(last, "path").is_some_and(|p| {
        p.starts_with(&format!("{PROFILES}/{FILE_PREFIX}")) && p.ends_with(".profdata")
    }) {
        return Err(format!(
            "the upload step's `path` is not the shard's own merged profile, `{PROFILES}/{FILE_PREFIX}merged.profdata`"
        ));
    }
    if input(last, "overwrite") != Some("true") {
        return Err("the upload step lacks `overwrite: true`, so a re-run of the run fails on the names its first run uploaded".to_owned());
    }
    if input(last, "if-no-files-found") != Some("error") {
        return Err("the upload step lacks `if-no-files-found: error`, so a shard with no profile would pass".to_owned());
    }
    Ok(())
}

/// The values of every `flag` in `words`.
fn values<'a>(words: &[&'a str], flag: &str) -> Vec<&'a str> {
    words
        .windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1])
        .collect()
}

/// `a` and `b` hold the same names, once each, in any order.
fn same(a: &[&str], b: &[String]) -> bool {
    let mut a: Vec<&str> = a.to_vec();
    let mut b: Vec<&str> = b.iter().map(String::as_str).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b && a.windows(2).all(|w| w[0] != w[1])
}

/// The `test` job takes every shard's profiles and gates the one report.
pub fn gate_wiring(workflow: &str) -> Result<(), String> {
    let entries = entries(workflow)?;
    let (artifacts, prefixes) = expected(&entries);
    let steps = workflows::job_steps(workflow, GATE)?;
    let find = |pred: &dyn Fn(&workflows::Step) -> bool| steps.iter().position(pred);
    let run = |s: &workflows::Step| s.run.clone().unwrap_or_default();
    let mut builds = Vec::new();
    for (core, _, _) in CORES {
        let args = entries
            .iter()
            .find(|e| e.get("core") == Some(core))
            .and_then(|e| e.get("args"))
            .ok_or_else(|| format!("the matrix has no `args` for the `{core}` core"))?;
        let at = find(&|s| {
            let r = run(s);
            r.contains("llvm-cov nextest")
                && r.contains(args)
                && r.contains("--no-report")
                && r.contains("--branch")
                && r.contains("--no-tests=pass")
        })
        .ok_or_else(|| {
            format!("the job `{GATE}` has no step that builds the instrumented tests of the `{core}` core (`llvm-cov nextest {args} ... --no-tests=pass`)")
        })?;
        builds.push(at);
    }
    let clean = find(&|s| run(s).contains("llvm-cov clean --profraw-only"))
        .ok_or_else(|| format!("the job `{GATE}` has no step that drops the profiles the builds' test listing wrote (`llvm-cov clean --profraw-only`), which would be merged into the report"))?;
    let wait = find(&|s| run(s).contains("xtask wait-artifacts")).ok_or_else(|| {
        format!("the job `{GATE}` has no step running `cargo xtask wait-artifacts`")
    })?;
    let download = find(&|s| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/download-artifact@"))
    })
    .ok_or_else(|| {
        format!("the job `{GATE}` has no step that downloads the shards' raw profiles")
    })?;
    let complete = find(&|s| run(s).contains("xtask profiles-complete")).ok_or_else(|| {
        format!("the job `{GATE}` has no step running `cargo xtask profiles-complete`")
    })?;
    let merge = find(&|s| run(s).contains("xtask merge-profiles")).ok_or_else(|| {
        format!("the job `{GATE}` has no step running `cargo xtask merge-profiles` on the shards' profiles")
    })?;
    let report = find(&|s| run(s).contains("llvm-cov report"))
        .ok_or_else(|| format!("the job `{GATE}` has no step running `llvm-cov report`"))?;
    let gate = find(&|s| {
        let r = run(s);
        r.contains("-p kbf-coverage ") && !r.contains("no-lowering")
    })
    .ok_or_else(|| format!("the job `{GATE}` has no step running the gate (`-p kbf-coverage`)"))?;
    let after_builds = builds.iter().all(|b| *b < clean);
    if !(after_builds
        && clean < download
        && wait < download
        && download < complete
        && complete < merge
        && merge < report
        && report < gate)
    {
        return Err("the steps of `test` are not in the order build, drop listing profiles (before the download), wait, download, check the profiles, merge them, report, gate".to_owned());
    }
    for (what, i) in [
        ("build", builds[0]),
        ("build", builds[1]),
        ("profile-dropping", clean),
        ("wait", wait),
        ("download", download),
        ("profiles-complete", complete),
        ("profile-merging", merge),
        ("report", report),
    ] {
        if steps[i].conditional {
            return Err(format!(
                "the {what} step of `{GATE}` has an `if:` or `continue-on-error`, so the gate could pass without it"
            ));
        }
    }
    let wait_text = run(&steps[wait]);
    let w = words(&wait_text);
    if flag(&w, "--job-prefix") != Some(SHARD) {
        return Err(format!(
            "the wait step does not pass `--job-prefix {SHARD}`, so a failed shard would not end it"
        ));
    }
    if !same(&values(&w, "--artifact"), &artifacts) {
        return Err(format!(
            "the wait step's `--artifact` names are not the shards' artifacts {artifacts:?} (one for each matrix entry)"
        ));
    }
    if input(&steps[download], "pattern") != Some(PATTERN)
        || artifacts.iter().any(|a| !a.starts_with("profdata-"))
    {
        return Err(format!(
            "the download step's `pattern` is not `{PATTERN}`, which the shards' artifact names begin with"
        ));
    }
    if input(&steps[download], "path") != Some(DOWNLOADS)
        || input(&steps[download], "merge-multiple") != Some("true")
    {
        return Err(format!(
            "the download step must put the profiles flat in `{DOWNLOADS}` (`merge-multiple: true`)"
        ));
    }
    let complete_text = run(&steps[complete]);
    let w = words(&complete_text);
    if flag(&w, "--dir") != Some(DOWNLOADS) || flag(&w, "--extension") != Some("profdata") {
        return Err(format!(
            "the profiles-complete step does not check `--dir {DOWNLOADS} --extension profdata`"
        ));
    }
    if !same(&values(&w, "--prefix"), &prefixes) {
        return Err(format!(
            "the profiles-complete step's `--prefix` values are not the shards' file prefixes {prefixes:?} (one for each matrix entry)"
        ));
    }
    let merge_text = run(&steps[merge]);
    let w = words(&merge_text);
    if flag(&w, "--dir") != Some(DOWNLOADS)
        || flag(&w, "--extension") != Some("profdata")
        || flag(&w, "--workspace-profile-in") != Some(PROFILES)
    {
        return Err(format!(
            "the merge step of `{GATE}` must merge the `.profdata` files of `{DOWNLOADS}` into the profile `llvm-cov report` reads (`--workspace-profile-in {PROFILES}`)"
        ));
    }
    Ok(())
}

/// The required checks, `lint` and `test`, keep their names and cannot be skipped.
pub fn check_names(workflow: &str) -> Result<(), String> {
    let names = workflows::job_names(workflow)?;
    for job in [LINT, GATE] {
        if !names.iter().any(|n| n == job) {
            return Err(format!("the workflow has no job `{job}`, a required check"));
        }
        if workflows::job_has_key(workflow, job, "name")? {
            return Err(format!(
                "the job `{job}` has a `name:`, so its check is no longer called `{job}`"
            ));
        }
    }
    if workflows::job_has_key(workflow, GATE, "if")? {
        return Err(format!(
            "the job `{GATE}` has an `if:`, so it can be skipped, and a skipped required check passes"
        ));
    }
    let scopes = workflows::job_permission_levels(workflow, GATE)?;
    if !scopes.iter().any(|(s, l)| s == "actions" && l == "read") {
        return Err(format!(
            "the job `{GATE}` lacks `actions: read`, which `wait-artifacts` needs to list the run's jobs and artifacts"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod rules;
