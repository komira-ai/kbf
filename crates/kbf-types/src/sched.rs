//! The generic scheduling vocabulary: QoS levels, lease identifiers and kinds, node
//! phases, farm time, the vocabulary of scheduled work (operations, workers, request
//! vectors, control-log records), the `StateMachine` trait and `Effect` enum, and the
//! version a binary reports and the cap two versions negotiate.
//!
//! Nothing here is specific to building: no REAPI, no content `Digest`. The
//! build-specific types (digest, image, release, enrollment, host role, object store and
//! operator APIs) stay in the crate root. Every item is also re-exported at the crate
//! root, so `kbf_types::LeaseId` and `kbf_types::sched::LeaseId` are the same type.
//!
//! One item still names a build type: `work` carries `ActionKey` (and with it a
//! `Digest`) in `StartLease` and `Outcome`. The test at the foot of this file lists
//! those lines and refuses any other.

mod lease;
mod phase;
mod qos;
mod state;
mod time;
mod version;
mod work;

pub use lease::{LeaseId, LeaseKind};
pub use phase::NodePhase;
pub use qos::{CustomQos, Qos, QosError};
pub use state::{Effect, StateMachine};
pub use time::FarmTime;
pub use version::{Supported, TooNew, VERSION_CAP, Version, VersionFault, understood};
pub use work::{
    ActionKey, Answer, ControlRecord, Failure, FarmFault, FarmRun, FencePolicy, LeaseGrant,
    LostWhy, MemoryKill, MemoryRun, OperationId, Outcome, Refusal, RefusalRecord, Resources,
    ResultRecord, StartLease, WaiterId, Waiting, WorkerId,
};

#[cfg(test)]
mod tests {
    /// The names this module must not use, in lower case. A line is matched after
    /// lower-casing, so `Digest`, `digest::`, `REAPI`, `reapi`, `ActionKey` and
    /// `action_key` all count.
    const BUILD_NAMES: [&str; 4] = ["digest", "reapi", "actionkey", "action_key"];

    /// Code lines of `work.rs` that still name a build type. A later change removes
    /// `ActionKey` from the scheduling types and empties this list.
    const ALLOWED: [&str; 5] = [
        "use crate::{Digest, LeaseId, LeaseKind};",
        "pub struct ActionKey {",
        "pub action: Digest,",
        "action_result: Digest,",
        "pub key: ActionKey,",
    ];

    /// `source` with `//` comments (doc comments included) and `/* */` comments
    /// (nested ones too) removed. Line breaks inside a block comment are kept so line
    /// structure survives. Text inside a string literal is left alone.
    fn strip_comments(source: &str) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        let mut block = 0_usize;
        let mut in_string = false;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            if block > 0 {
                if c == '*' && next == Some('/') {
                    block -= 1;
                    i += 2;
                } else if c == '/' && next == Some('*') {
                    block += 1;
                    i += 2;
                } else {
                    if c == '\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
            } else if in_string {
                out.push(c);
                if c == '\\' {
                    if let Some(escaped) = next {
                        out.push(escaped);
                        i += 1;
                    }
                } else if c == '"' {
                    in_string = false;
                }
                i += 1;
            } else if c == '/' && next == Some('/') {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            } else if c == '/' && next == Some('*') {
                block = 1;
                i += 2;
            } else if c == '\'' && chars.get(i + 2) == Some(&'\'') {
                // A one-character literal such as '"' must not open a string.
                out.extend(&chars[i..i + 3]);
                i += 3;
            } else {
                if c == '"' {
                    in_string = true;
                }
                out.push(c);
                i += 1;
            }
        }
        out
    }

    /// The trimmed, non-empty code lines of one source file: comments removed and every
    /// `#[cfg(test)]` item (a test module, tracked by its braces) left out. Code after a
    /// test module is still scanned.
    fn code_lines(source: &str) -> Vec<String> {
        let stripped = strip_comments(source);
        let mut lines = Vec::new();
        let mut skipping = false;
        let mut opened = false;
        let mut depth = 0_i64;
        for line in stripped.lines().map(str::trim) {
            if !skipping && line.starts_with("#[cfg(test)]") {
                skipping = true;
                opened = false;
                depth = 0;
            }
            if skipping {
                for c in line.chars() {
                    match c {
                        '{' => {
                            depth += 1;
                            opened = true;
                        }
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                // The item ends at its closing brace, or at `;` when it has no body.
                if (opened && depth <= 0) || (!opened && line.ends_with(';')) {
                    skipping = false;
                }
            } else if !line.is_empty() {
                lines.push(line.to_string());
            }
        }
        lines
    }

    fn offenders(source: &str) -> Vec<String> {
        code_lines(source)
            .into_iter()
            .filter(|line| {
                let lower = line.to_lowercase();
                BUILD_NAMES.iter().any(|name| lower.contains(name))
            })
            .collect()
    }

    /// The modules `sched.rs` declares with `mod name;`, sorted.
    fn declared_modules() -> Vec<String> {
        let mut names: Vec<String> = include_str!("sched.rs")
            .lines()
            .filter_map(|line| {
                line.strip_prefix("mod ")
                    .or_else(|| line.strip_prefix("pub mod "))
            })
            .filter_map(|rest| rest.strip_suffix(';'))
            .map(str::to_string)
            .collect();
        names.sort();
        names
    }

    /// The file stems present in `src/sched/`, sorted.
    fn files_present() -> Vec<String> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/sched");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("the sched directory is readable")
            .map(|entry| entry.expect("a directory entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .filter_map(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names
    }

    fn read(module: &str) -> String {
        let path = format!("{}/src/sched/{module}.rs", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn every_file_under_sched_is_a_declared_module() {
        assert_eq!(declared_modules(), files_present());
    }

    #[test]
    fn the_generic_files_name_no_build_type() {
        let modules = declared_modules();
        assert!(modules.iter().any(|m| m == "work"), "{modules:?}");
        for module in modules.iter().filter(|m| *m != "work") {
            assert_eq!(
                offenders(&read(module)),
                Vec::<String>::new(),
                "{module}.rs"
            );
        }
    }

    #[test]
    fn work_names_only_the_listed_build_lines() {
        let found = offenders(&read("work"));
        assert_eq!(found, ALLOWED);
    }

    #[test]
    fn the_scan_sees_every_spelling_in_code_and_skips_comments_and_test_modules() {
        let source = "\
// the REAPI name
/// a Digest in a doc comment
/* a Digest
   in a block */
let a = 1; /* ActionKey */
use crate::digest::Digest;
let b: reapi::Client;
let c: REAPI;
let d: ActionKey;
let e = action_key();
#[cfg(test)]
mod tests {
    fn t() {
        let f: Digest;
    }
}
let g: Digest;
let h = \"// kept: Digest\";
";
        assert_eq!(
            offenders(source),
            vec![
                "use crate::digest::Digest;",
                "let b: reapi::Client;",
                "let c: REAPI;",
                "let d: ActionKey;",
                "let e = action_key();",
                "let g: Digest;",
                "let h = \"// kept: Digest\";",
            ]
        );
    }

    #[test]
    fn the_scan_of_a_clean_source_is_empty() {
        let source = "pub struct Lease;\n// Digest\n/* REAPI */\n#[cfg(test)]\nmod tests {}\n";
        assert_eq!(offenders(source), Vec::<String>::new());
    }

    #[test]
    fn the_root_paths_are_the_sched_types() {
        let id: crate::sched::LeaseId = crate::LeaseId::new(1, 7);
        assert_eq!(id, crate::sched::LeaseId::new(1, 7));
    }
}
