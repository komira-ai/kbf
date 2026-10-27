//! Checked-in golden hashes of simulation traces.
//!
//! A simulation run is a function of its seed and its scenario, so the [`TraceHash`] of
//! a given scenario and seed is a constant. A golden file records those constants in
//! the repository, which turns "the two runs in this process agree" into "this run
//! agrees with the run before the change". A change that only moves code (a type to
//! another crate, a function to another module) must leave every hash as it is; a change
//! that alters a decision, or the trace text itself, moves them, and the author
//! regenerates them on purpose and says why in the pull request.
//!
//! One file can hold several families (one per test binary): each family checks and
//! regenerates only its own lines, named `<family>/<name>`.
//!
//! The file is text: comment lines start with `#`, then a line `format <n>` naming the
//! [`FORMAT_VERSION`] of the trace text, then one `<family>/<name> <64 hex digits>` line
//! per run, sorted.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;

use crate::{FORMAT_VERSION, TraceHash};

const HEADER: &str = "\
# Golden trace hashes: the SHA-256 of the full trace of each seeded simulation run.
#
# Do not edit by hand. A change to any hash means a simulation now makes a different
# decision or writes its trace differently. Regenerate them only on purpose, with the
# command the failing test prints, and justify each changed line in the pull request.
# A change that only moves code must leave every hash as it is.
";

/// A golden file and the family of runs one test binary keeps in it.
#[derive(Clone, Debug)]
pub struct GoldenFile {
    path: PathBuf,
    family: String,
    regenerate_with: String,
}

impl GoldenFile {
    /// The lines named `<family>/...` of the file at `path`. `regenerate_with` is the
    /// command line that rewrites them, quoted in the failure message.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, family: &str, regenerate_with: &str) -> Self {
        Self {
            path: path.into(),
            family: family.to_owned(),
            regenerate_with: regenerate_with.to_owned(),
        }
    }

    /// Panics, with the way to regenerate, unless `actual` is exactly the family's lines
    /// in the file.
    ///
    /// # Panics
    ///
    /// If the hashes differ, or the file is unreadable (see [`GoldenFile::check`]).
    pub fn assert_matches(&self, actual: &[(String, TraceHash)]) {
        if let Err(message) = self.check(actual) {
            panic!("{message}");
        }
    }

    /// Compares `actual` (name and hash of each run) with the family's lines in the
    /// file.
    ///
    /// # Errors
    ///
    /// A message naming every changed, new and removed run and how to regenerate, if
    /// they differ; or why the file could not be read.
    pub fn check(&self, actual: &[(String, TraceHash)]) -> Result<(), String> {
        let golden = self.read().map_err(|why| self.explain(&why))?;
        let prefix = format!("{}/", self.family);
        let actual: BTreeMap<String, String> = actual
            .iter()
            .map(|(name, hash)| (self.key(name), hash.to_string()))
            .collect();
        let mut problems = Vec::new();
        for (key, now) in &actual {
            match golden.get(key) {
                Some(then) if then == now => {}
                Some(then) => problems.push(format!("  changed  {key}: golden {then}, now {now}")),
                None => problems.push(format!("  new      {key}: not in the file, now {now}")),
            }
        }
        for key in golden.keys().filter(|k| k.starts_with(&prefix)) {
            if !actual.contains_key(key) {
                problems.push(format!("  removed  {key}: in the file, no longer run"));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(self.explain(&problems.join("\n")))
        }
    }

    /// Rewrites the family's lines in the file to `actual`, keeping every other
    /// family's, and creating the file if it does not exist.
    ///
    /// # Errors
    ///
    /// If the file exists but cannot be read or parsed, or cannot be written.
    pub fn regenerate(&self, actual: &[(String, TraceHash)]) -> io::Result<()> {
        let mut all = match self.read() {
            Ok(all) => all,
            Err(_) if !self.path.exists() => BTreeMap::new(),
            Err(why) => return Err(io::Error::new(io::ErrorKind::InvalidData, why)),
        };
        let prefix = format!("{}/", self.family);
        all.retain(|key, _| !key.starts_with(&prefix));
        for (name, hash) in actual {
            all.insert(self.key(name), hash.to_string());
        }
        let mut text = String::from(HEADER);
        let _ = writeln!(text, "format {FORMAT_VERSION}");
        for (key, hash) in &all {
            let _ = writeln!(text, "{key} {hash}");
        }
        self.path
            .parent()
            .into_iter()
            .try_for_each(std::fs::create_dir_all)?;
        std::fs::write(&self.path, text)
    }

    fn key(&self, name: &str) -> String {
        assert!(
            !name.is_empty() && !name.contains(char::is_whitespace),
            "a golden run name has no whitespace: {name:?}"
        );
        format!("{}/{name}", self.family)
    }

    /// Every line of the file, by key.
    fn read(&self) -> Result<BTreeMap<String, String>, String> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("cannot read {}: {e}", self.path.display()))?;
        let mut format = None;
        let mut lines = BTreeMap::new();
        for line in text
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            let fields: Vec<&str> = line.split(' ').collect();
            match fields[..] {
                ["format", n] => format = n.parse::<u32>().ok(),
                [key, hash] if hash.len() == 64 => {
                    lines.insert(key.to_owned(), hash.to_owned());
                }
                _ => {
                    return Err(format!(
                        "{}: cannot read the line {line:?}",
                        self.path.display()
                    ));
                }
            }
        }
        if format != Some(FORMAT_VERSION) {
            return Err(format!(
                "{} is in trace format {format:?}; the code writes format {FORMAT_VERSION}",
                self.path.display()
            ));
        }
        Ok(lines)
    }

    fn explain(&self, what: &str) -> String {
        format!(
            "golden trace hashes for `{family}` do not match {path}:\n{what}\n\n\
             These hashes pin what the simulations decide and how their traces are written. \
             A changed hash means a run now makes a different decision or writes a different \
             trace line. If that is intended, regenerate the hashes on purpose with\n\n    \
             {command}\n\n\
             and justify every changed line in the pull request description. A change that \
             only moves code, with no change of behaviour, must leave every hash as it is.",
            family = self.family,
            path = self.path.display(),
            command = self.regenerate_with,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Trace;

    fn hash(line: &str) -> TraceHash {
        let mut t = Trace::new("golden-test");
        t.record(line);
        t.hash()
    }

    fn runs(lines: &[(&str, &str)]) -> Vec<(String, TraceHash)> {
        lines
            .iter()
            .map(|(name, line)| ((*name).to_owned(), hash(line)))
            .collect()
    }

    /// A file path of its own in the temporary directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("kbf-sim-golden-{}", std::process::id()));
            Self(dir.join(name))
        }

        fn file(&self, family: &str) -> GoldenFile {
            GoldenFile::new(&self.0, family, "cargo test regenerate")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Catches: a regenerated file the checker then rejects, or a check that passes
    /// when a hash differs.
    #[test]
    fn what_regenerate_writes_check_accepts_and_a_changed_line_fails() {
        let scratch = Scratch::new("round-trip.txt");
        let file = scratch.file("f");
        file.regenerate(&runs(&[("a", "one"), ("b", "two")]))
            .unwrap();
        assert_eq!(file.check(&runs(&[("a", "one"), ("b", "two")])), Ok(()));
        file.assert_matches(&runs(&[("b", "two"), ("a", "one")]));

        let err = file
            .check(&runs(&[("a", "one"), ("b", "CHANGED")]))
            .unwrap_err();
        assert!(err.contains("changed  f/b: golden"), "{err}");
        assert!(!err.contains("f/a"), "{err}");
        assert!(err.contains("cargo test regenerate"), "{err}");
        assert!(
            err.contains("justify every changed line in the pull request"),
            "{err}"
        );
    }

    /// Catches: a run added to a scenario list, or dropped from it, passing unnoticed.
    #[test]
    fn new_and_removed_runs_are_named() {
        let scratch = Scratch::new("new-removed.txt");
        let file = scratch.file("f");
        file.regenerate(&runs(&[("a", "one"), ("b", "two")]))
            .unwrap();
        let err = file
            .check(&runs(&[("a", "one"), ("c", "three")]))
            .unwrap_err();
        assert!(err.contains("new      f/c: not in the file"), "{err}");
        assert!(
            err.contains("removed  f/b: in the file, no longer run"),
            "{err}"
        );
    }

    /// Catches: one family's regeneration deleting or rewriting another family's lines,
    /// and one family's check failing on another family's lines.
    #[test]
    fn families_share_a_file_without_touching_each_other() {
        let scratch = Scratch::new("families.txt");
        let (one, two) = (scratch.file("one"), scratch.file("two"));
        one.regenerate(&runs(&[("a", "x")])).unwrap();
        two.regenerate(&runs(&[("a", "y")])).unwrap();
        one.regenerate(&runs(&[("a", "x2"), ("b", "z")])).unwrap();
        assert_eq!(one.check(&runs(&[("a", "x2"), ("b", "z")])), Ok(()));
        assert_eq!(two.check(&runs(&[("a", "y")])), Ok(()));
        let text = std::fs::read_to_string(&scratch.0).unwrap();
        let keys: Vec<&str> = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with("format"))
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        assert_eq!(keys, ["one/a", "one/b", "two/a"], "sorted, each once");
    }

    /// Catches: a missing file reading as an empty one (a check that passes with no
    /// golden values), or `regenerate` failing to create a first file.
    #[test]
    fn a_missing_file_fails_the_check_and_is_created_by_regenerate() {
        let scratch = Scratch::new("missing.txt");
        let file = scratch.file("f");
        let err = file.check(&runs(&[("a", "one")])).unwrap_err();
        assert!(err.contains("cannot read"), "{err}");
        assert!(err.contains("cargo test regenerate"), "{err}");
        file.regenerate(&runs(&[("a", "one")])).unwrap();
        assert_eq!(file.check(&runs(&[("a", "one")])), Ok(()));
    }

    /// Catches: a file in another trace format version, or damaged, passing as current
    /// (it would compare hashes of a different text), and a damaged file overwritten
    /// by regeneration without notice.
    #[test]
    fn a_stale_format_or_damaged_line_is_refused() {
        let scratch = Scratch::new("damaged.txt");
        let file = scratch.file("f");
        let good = "a".repeat(64);
        std::fs::create_dir_all(scratch.0.parent().unwrap()).unwrap();

        std::fs::write(&scratch.0, format!("format 0\nf/a {good}\n")).unwrap();
        let err = file.check(&runs(&[("a", "one")])).unwrap_err();
        assert!(err.contains("trace format Some(0)"), "{err}");

        std::fs::write(&scratch.0, format!("f/a {good}\n")).unwrap();
        let err = file.check(&runs(&[("a", "one")])).unwrap_err();
        assert!(err.contains("trace format None"), "{err}");

        std::fs::write(&scratch.0, "format 1\nf/a nothex\n").unwrap();
        let err = file.check(&runs(&[("a", "one")])).unwrap_err();
        assert!(err.contains("cannot read the line"), "{err}");
        let refused = file.regenerate(&runs(&[("a", "one")])).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidData);
    }

    /// Catches: a run name with whitespace, which would split its line in two.
    #[test]
    #[should_panic(expected = "no whitespace")]
    fn a_name_with_a_space_is_refused() {
        let scratch = Scratch::new("space.txt");
        let _ = scratch.file("f").regenerate(&runs(&[("a b", "one")]));
    }

    /// Catches: `assert_matches` returning on a mismatch.
    #[test]
    #[should_panic(expected = "do not match")]
    fn assert_matches_panics_on_a_difference() {
        let scratch = Scratch::new("panics.txt");
        let file = scratch.file("f");
        file.regenerate(&runs(&[("a", "one")])).unwrap();
        file.assert_matches(&runs(&[("a", "two")]));
    }
}
