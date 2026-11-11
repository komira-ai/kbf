//! No workflow needs the Ubuntu archive: the Debian packages a job installs are pinned and
//! cached, so an outage of Canonical's names cannot fail a run.
//!
//! The rules, over every workflow in `.github/workflows`, parsed as YAML:
//!
//! - no `run:` command is `apt-get`, `apt` or `aptitude` (any command of any line, split at
//!   `&&`, `||`, `;` and `|`, after `sudo` and its options, `env` and `NAME=value`
//!   assignments), and no `run:` line names a Canonical host (`ubuntu.com`, `launchpad.net`,
//!   `canonical.com`);
//! - a package a job needs and the runner image lacks is pinned in `.github/debs/<set>.toml`
//!   and installed by four steps in a row, all under the same `if:` (or none):
//!   - `actions/cache/restore` with an `id:` and in its `with:` exactly the path
//!     `${{ runner.temp }}/debs/<set>` and the key
//!     `debs-<set>-${{ runner.arch }}-${{ hashFiles('.github/debs/<set>.toml') }}`: a new pin is
//!     a new key, and no `restore-keys` can hand back another pin's files;
//!   - with an `id:`, `cargo xtask debs --pins .github/debs/<set>.toml --dir
//!     "$RUNNER_TEMP/debs/<set>"`, one command, which checks every file and downloads only
//!     the missing ones;
//!   - `actions/cache/save`, run only when that fetch passed and the restore missed (`if:` the
//!     steps' condition, if any, `&& steps.<fetch>.outcome == 'success' &&
//!     steps.<restore>.outputs.cache-hit != 'true'`), with the same path and
//!     `${{ steps.<restore>.outputs.cache-primary-key }}`, so a failed fetch never saves a
//!     partial set under the key;
//!   - `sudo dpkg --install --recursive "$RUNNER_TEMP/debs/<set>"`, the only `dpkg` install
//!     a workflow may run;
//! - every `.github/debs/*.toml` is the set of some site, and every site's set has its file.
//!
//! `cargo xtask debs` refuses a directory holding a file no pin names, so the directory dpkg
//! installs is the pinned packages. The workflow lint (`workflows`) holds the actions to their
//! commit pins; the interpreter lint holds each `run:` to one command.
//!
//! What it misses: a package installed by a program a step runs or an image a step builds (the
//! step's own command line is all it reads: the deb install test's container,
//! `packaging/linux/test/ubuntu/Dockerfile`, runs apt inside it), an `apt-get` spelt through a
//! variable, a path (`/usr/bin/apt-get`), a wrapper (`command`, `exec`) or a script file, a
//! host named in another letter case or outside a `run:`, and composite actions.

use super::workflows::{self, Step};

/// Where the workflows are, relative to the root.
const WORKFLOW_DIR: &str = ".github/workflows/";
/// Where the pins files are, relative to the root.
const DEBS_DIR: &str = ".github/debs/";

/// The package managers a `run:` line may not call.
const APT: [&str; 3] = ["apt-get", "apt", "aptitude"];
/// The hosts a `run:` line may not name.
const CANONICAL: [&str; 3] = ["ubuntu.com", "launchpad.net", "canonical.com"];

/// One set of pinned packages a job installs.
#[derive(Debug, PartialEq, Eq)]
pub struct Site {
    /// The workflow's file name.
    pub file: String,
    /// The job's key.
    pub job: String,
    /// The set: `.github/debs/<set>.toml`.
    pub set: String,
}

/// A step's `if:`, without `${{ }}` around it.
fn condition(step: &Step) -> Option<&str> {
    step.condition.as_deref().map(|c| {
        c.trim()
            .strip_prefix("${{")
            .and_then(|c| c.strip_suffix("}}"))
            .unwrap_or(c)
            .trim()
    })
}

/// The words of `text`, quotes stripped.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| w.trim_matches(['"', '\'']).to_owned())
        .collect()
}

/// The commands of `line`, split at `&&`, `||`, `;` and `|`, each as its words from the program
/// on: without `sudo` and its options, `env`, and `NAME=value` assignments before it.
fn commands(line: &str) -> Vec<Vec<String>> {
    let line = line.replace("&&", ";").replace("||", ";").replace('|', ";");
    line.split(';')
        .map(|part| {
            let mut words = words(part);
            while let Some(w) = words.first() {
                // `sudo`'s options that take the next word as their value.
                let takes_value =
                    ["-u", "-g", "-h", "-p", "-C", "-D", "-R", "-T"].contains(&w.as_str());
                let dropped = w == "sudo"
                    || w == "env"
                    || w.starts_with('-')
                    || w.split_once('=').is_some_and(|(name, _)| {
                        !name.is_empty()
                            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    });
                if !dropped {
                    break;
                }
                words.remove(0);
                if takes_value && !words.is_empty() {
                    words.remove(0);
                }
            }
            words
        })
        .collect()
}

/// The `with:` input `key` of `step`.
fn input<'a>(step: &'a Step, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// `step` names its position for a message: `step <n>` (1-based) and its name, if any.
fn label(steps: &[Step], i: usize) -> String {
    match &steps[i].name {
        Some(name) => format!("step {} ({name})", i + 1),
        None => format!("step {}", i + 1),
    }
}

/// The sites of `workflows` (`(file name, text)`) and every way they break the rules, each
/// problem as `<file>: <job>: <step>: <why>`.
pub fn scan(workflows: &[(String, String)]) -> (Vec<Site>, Vec<String>) {
    let mut sites = Vec::new();
    let mut problems = Vec::new();
    for (file, text) in workflows {
        let jobs = match workflows::job_names(text) {
            Ok(jobs) => jobs,
            Err(e) => {
                problems.push(format!("{file}: {e}"));
                continue;
            }
        };
        for job in jobs {
            let steps = match workflows::job_steps(text, &job) {
                Ok(steps) => steps,
                // A job that calls a reusable workflow (`uses:`) has no steps of its own.
                Err(_) if workflows::job_uses(text, &job).ok().flatten().is_some() => continue,
                Err(e) => {
                    problems.push(format!("{file}: {job}: {e}"));
                    continue;
                }
            };
            let mut installs = Vec::new();
            for (i, step) in steps.iter().enumerate() {
                if step
                    .run
                    .as_deref()
                    .is_some_and(|r| r.contains("xtask debs"))
                {
                    installs.push(i + 2);
                    match site(&steps, i) {
                        Ok(set) => sites.push(Site {
                            file: file.clone(),
                            job: job.clone(),
                            set,
                        }),
                        Err(why) => {
                            problems.push(format!("{file}: {job}: {}: {why}", label(&steps, i)));
                        }
                    }
                }
            }
            for (i, step) in steps.iter().enumerate() {
                let mut fail = |why: String| {
                    problems.push(format!("{file}: {job}: {}: {why}", label(&steps, i)))
                };
                for line in step.run.as_deref().unwrap_or_default().lines() {
                    for words in commands(line) {
                        let first = words.first().map_or("", String::as_str);
                        if APT.contains(&first) {
                            fail(format!(
                                "runs {first}: pin the packages in {DEBS_DIR} and install them \
                                 with `cargo xtask debs`"
                            ));
                        }
                        let installs_deb = words.iter().any(|w| {
                            w == "--install"
                                || w == "--unpack"
                                || (w.starts_with('-')
                                    && !w.starts_with("--")
                                    && w.contains(['i', 'x']))
                        });
                        if first == "dpkg" && installs_deb && !installs.contains(&i) {
                            fail(
                                "a dpkg install that is not the last of the four debs steps"
                                    .to_owned(),
                            );
                        }
                    }
                    if let Some(host) = CANONICAL.iter().find(|h| line.contains(**h)) {
                        fail(format!("names the Canonical host {host}"));
                    }
                }
            }
        }
    }
    (sites, problems)
}

/// The set of the `xtask debs` at `steps[i]`, once it and the steps around it keep the rule.
fn site(steps: &[Step], i: usize) -> Result<String, String> {
    let debs = &steps[i];
    let run = debs.run.as_deref().unwrap_or_default();
    for chain in ["\n", "&&", "||", ";", "|", "`", "$(", ">", "${{"] {
        if run.trim_end().contains(chain) {
            return Err(format!(
                "the debs step is not one command ({chain:?} in it)"
            ));
        }
    }
    let words = words(run);
    let set = match words
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["cargo", "xtask", "debs", "--pins", pins, "--dir", dir] => {
            let set = pins
                .strip_prefix(DEBS_DIR)
                .and_then(|p| p.strip_suffix(".toml"))
                .filter(|s| {
                    !s.is_empty()
                        && s.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                })
                .ok_or_else(|| format!("the --pins {pins:?} is not {DEBS_DIR}<set>.toml"))?;
            if *dir != format!("$RUNNER_TEMP/debs/{set}") {
                return Err(format!(
                    "the --dir {dir:?} is not \"$RUNNER_TEMP/debs/{set}\""
                ));
            }
            set.to_owned()
        }
        _ => {
            return Err(
                "the debs step is not `cargo xtask debs --pins <file> --dir <dir>`".to_owned(),
            );
        }
    };
    let fetch = debs.id.as_deref().ok_or("the debs step has no `id:`")?;
    let cond = condition(debs);
    let path = format!("${{{{ runner.temp }}}}/debs/{set}");

    let restore = i
        .checked_sub(1)
        .map(|r| &steps[r])
        .ok_or("no actions/cache/restore step right before the debs step")?;
    if !restore
        .uses
        .as_deref()
        .is_some_and(|u| u.starts_with("actions/cache/restore@"))
    {
        return Err("the step before the debs step is not actions/cache/restore".to_owned());
    }
    if condition(restore) != cond {
        return Err(format!(
            "the restore's `if:` is {:?}, not the debs step's {cond:?}",
            condition(restore)
        ));
    }
    let id = restore.id.as_deref().ok_or("the restore has no `id:`")?;
    let key =
        format!("debs-{set}-${{{{ runner.arch }}}}-${{{{ hashFiles('{DEBS_DIR}{set}.toml') }}}}");
    expect_with(restore, "restore", &path, &key)?;

    let save = steps
        .get(i + 1)
        .ok_or("no actions/cache/save step right after the debs step")?;
    if !save
        .uses
        .as_deref()
        .is_some_and(|u| u.starts_with("actions/cache/save@"))
    {
        return Err("the step after the debs step is not actions/cache/save".to_owned());
    }
    let only =
        format!("steps.{fetch}.outcome == 'success' && steps.{id}.outputs.cache-hit != 'true'");
    let want = cond.map_or(only.clone(), |c| format!("{c} && {only}"));
    if condition(save) != Some(want.as_str()) {
        return Err(format!(
            "the save's `if:` is {:?}, not {want:?}",
            condition(save)
        ));
    }
    let primary = format!("${{{{ steps.{id}.outputs.cache-primary-key }}}}");
    expect_with(save, "save", &path, &primary)?;

    let install = steps
        .get(i + 2)
        .ok_or("no dpkg install step right after the save")?;
    let want = format!("sudo dpkg --install --recursive \"$RUNNER_TEMP/debs/{set}\"");
    if install.run.as_deref().map(str::trim_end) != Some(want.as_str())
        || condition(install) != cond
    {
        return Err(format!(
            "the step after the save is not `{want}` under the debs step's `if:`"
        ));
    }
    Ok(set)
}

/// `step`'s `with:` is exactly `path` and `key`.
fn expect_with(step: &Step, what: &str, path: &str, key: &str) -> Result<(), String> {
    let mut keys: Vec<&str> = step.with.iter().map(|(k, _)| k.as_str()).collect();
    keys.sort_unstable();
    if keys != ["key", "path"] {
        return Err(format!(
            "the {what}'s `with:` has {keys:?}, not exactly path and key"
        ));
    }
    if input(step, "path") != Some(path) {
        return Err(format!(
            "the {what}'s path is {:?}, not {path:?}",
            input(step, "path")
        ));
    }
    if input(step, "key") != Some(key) {
        return Err(format!(
            "the {what}'s key is {:?}, not {key:?}",
            input(step, "key")
        ));
    }
    Ok(())
}

/// Every problem of `workflows` and the pins files `pins` (paths relative to the root): the
/// rules of [`scan`], a pins file no site installs, and a site whose pins file is missing.
pub fn problems(workflows: &[(String, String)], pins: &[String]) -> Vec<String> {
    let (sites, mut problems) = scan(workflows);
    for file in pins {
        let set = file
            .strip_prefix(DEBS_DIR)
            .and_then(|f| f.strip_suffix(".toml"))
            .unwrap_or(file);
        if !sites.iter().any(|s| s.set == set) {
            problems.push(format!("{file}: no workflow installs this set"));
        }
    }
    for site in &sites {
        let file = format!("{DEBS_DIR}{}.toml", site.set);
        if !pins.contains(&file) {
            problems.push(format!(
                "{}: {}: the pins file {file} does not exist",
                site.file, site.job
            ));
        }
    }
    problems
}

/// The repository's workflows `(file name, text)` and pins files.
fn real_files() -> (Vec<(String, String)>, Vec<String>) {
    let root = super::repo_root();
    let listed = super::listed_files(&root);
    let workflows: Vec<(String, String)> = listed
        .iter()
        .filter(|f| super::is_workflow(f))
        .filter_map(|f| {
            let text = std::fs::read_to_string(root.join(f)).ok()?;
            Some((f.strip_prefix(WORKFLOW_DIR)?.to_owned(), text))
        })
        .collect();
    let pins: Vec<String> = listed
        .into_iter()
        .filter(|f| f.starts_with(DEBS_DIR) && f.ends_with(".toml"))
        .collect();
    (workflows, pins)
}

/// Catches a workflow that runs apt or names a Canonical host, a pinned set installed any way
/// but restore, fetch, save on a miss and dpkg, and a pins file no job installs.
#[test]
fn no_workflow_needs_the_ubuntu_archive() {
    let (workflows, pins) = real_files();
    assert!(
        workflows.len() > 10 && workflows.iter().any(|(f, _)| f == "ci.yml"),
        "the workflows were not found; the lint would check nothing: {:?}",
        workflows.iter().map(|(f, _)| f).collect::<Vec<_>>()
    );
    assert_eq!(problems(&workflows, &pins), Vec::<String>::new());
    let (sites, _) = scan(&workflows);
    let sites: Vec<(&str, &str, &str)> = sites
        .iter()
        .map(|s| (s.file.as_str(), s.job.as_str(), s.set.as_str()))
        .collect();
    assert_eq!(
        sites,
        [
            ("artifacts.yml", "linux-x86_64", "musl-tools"),
            ("artifacts.yml", "linux-arm64", "musl-tools"),
            ("ci.yml", "lint", "musl-tools"),
        ]
    );
    assert_eq!(pins, [".github/debs/musl-tools.toml"]);
}

#[cfg(test)]
mod tests;
