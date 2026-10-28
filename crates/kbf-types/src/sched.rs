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
//! The work itself is named by a [`WorkKey`], a class and a dedup text, and a completed
//! attempt's stored result by a [`ResultRef`], an identity and a size; the build's
//! `ActionKey` and `Digest` convert to and from them at the edges. The test at the foot
//! of this file refuses a build name in the code of any file of the module.

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
    Answer, ControlRecord, Failure, FarmFault, FarmRun, FencePolicy, LeaseGrant, LostWhy,
    MemoryKill, MemoryRun, OperationId, Outcome, Refusal, RefusalRecord, Resources, ResultRecord,
    ResultRef, StartLease, WaiterId, Waiting, WorkKey, WorkerId,
};

#[cfg(test)]
mod tests {
    /// The names this module must not use, in lower case. A line is matched after
    /// lower-casing, so `Digest`, `digest::`, `REAPI`, `reapi`, `ActionKey` and
    /// `action_key` all count.
    const BUILD_NAMES: [&str; 4] = ["digest", "reapi", "actionkey", "action_key"];

    /// A line break stays a line break; any other character becomes `_`.
    fn blank(c: char) -> char {
        if c == '\n' { '\n' } else { '_' }
    }

    fn put(kept: &mut String, shape: &mut String, c: char, s: char) {
        kept.push(c);
        shape.push(s);
    }

    /// The length of the character literal that starts at `chars[i]` (a `'`), or `None`
    /// when it is a lifetime. `'x'` is 3 long; an escape such as `'\n'`, `'\''` or
    /// `'\u{7b}'` runs to the next quote after its second character.
    fn char_literal_len(chars: &[char], i: usize) -> Option<usize> {
        if chars.get(i + 1) == Some(&'\\') {
            chars
                .iter()
                .skip(i + 3)
                .position(|&x| x == '\'')
                .map(|p| p + 4)
        } else if chars.get(i + 2) == Some(&'\'') {
            Some(3)
        } else {
            None
        }
    }

    /// Two copies of `source` with `//` comments (doc comments included) and `/* */`
    /// comments (nested ones too) removed; line breaks inside a block comment are kept
    /// so line structure survives. The first copy leaves string and character
    /// literals as written. In the second every character inside a literal is `_`, so
    /// a brace in a literal cannot be mistaken for code. Both have the same lines.
    fn scan(source: &str) -> (String, String) {
        let chars: Vec<char> = source.chars().collect();
        let mut kept = String::with_capacity(source.len());
        let mut shape = String::with_capacity(source.len());
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
                        put(&mut kept, &mut shape, c, c);
                    }
                    i += 1;
                }
            } else if in_string {
                if c == '\\' {
                    for &x in &chars[i..(i + 2).min(chars.len())] {
                        put(&mut kept, &mut shape, x, blank(x));
                    }
                    i += 2;
                } else {
                    put(&mut kept, &mut shape, c, blank(c));
                    in_string = c != '"';
                    i += 1;
                }
            } else if c == '/' && next == Some('/') {
                while chars.get(i).is_some_and(|&x| x != '\n') {
                    i += 1;
                }
            } else if c == '/' && next == Some('*') {
                block = 1;
                i += 2;
            } else if let Some(len) = (c == '\'').then(|| char_literal_len(&chars, i)).flatten() {
                for &x in &chars[i..i + len] {
                    put(&mut kept, &mut shape, x, blank(x));
                }
                i += len;
            } else {
                in_string = c == '"';
                put(&mut kept, &mut shape, c, c);
                i += 1;
            }
        }
        (kept, shape)
    }

    /// The trimmed, non-empty code lines of one source file: comments removed and every
    /// `#[cfg(test)]` item (a test module, tracked by its braces outside literals) left
    /// out. Code after a test module is still scanned.
    fn code_lines(source: &str) -> Vec<String> {
        let (kept, shape) = scan(source);
        let mut lines = Vec::new();
        let mut skipping = false;
        let mut opened = false;
        let mut depth = 0_i64;
        for (line, shaped) in kept
            .lines()
            .map(str::trim)
            .zip(shape.lines().map(str::trim))
        {
            if !skipping && shaped.starts_with("#[cfg(test)]") {
                skipping = true;
                opened = false;
                depth = 0;
            }
            if skipping {
                let open = shaped.matches('{').count();
                let close = shaped.matches('}').count();
                depth += i64::try_from(open).expect("a line has few braces");
                depth -= i64::try_from(close).expect("a line has few braces");
                opened |= open > 0;
                // The item ends at its closing brace, or at `;` when it has no body.
                if (opened && depth <= 0) || (!opened && shaped.ends_with(';')) {
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

    /// The modules `source` declares with `mod name;` in code, sorted.
    fn declared_modules(source: &str) -> Vec<String> {
        let mut names: Vec<String> = code_lines(source)
            .iter()
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

    /// Whether `source` has a `#[path]` attribute in code, which would point a module
    /// at a file the directory listing does not show.
    fn has_path_attribute(source: &str) -> bool {
        code_lines(source)
            .iter()
            .any(|line| line.contains("#[path"))
    }

    /// What a directory listing of `(file name, is a directory)` entries contributes: the
    /// stem of each `.rs` file, and `name/` for each directory (a directory can hide
    /// `x/mod.rs` and is never a declared module). Other files are ignored. Sorted.
    fn module_names(entries: &[(&str, bool)]) -> Vec<String> {
        let mut names: Vec<String> = entries
            .iter()
            .filter_map(|&(name, is_dir)| {
                if is_dir {
                    Some(format!("{name}/"))
                } else {
                    name.strip_suffix(".rs").map(str::to_string)
                }
            })
            .collect();
        names.sort();
        names
    }

    /// The entries of `src/sched/`, as `module_names` takes them.
    fn files_present() -> Vec<String> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/sched");
        let listed: Vec<(String, bool)> = std::fs::read_dir(dir)
            .expect("the sched directory is readable")
            .map(|entry| entry.expect("a directory entry"))
            .map(|entry| {
                let is_dir = entry.file_type().expect("a file type").is_dir();
                (entry.file_name().to_string_lossy().into_owned(), is_dir)
            })
            .collect();
        let borrowed: Vec<(&str, bool)> = listed.iter().map(|(n, d)| (n.as_str(), *d)).collect();
        module_names(&borrowed)
    }

    fn read(module: &str) -> String {
        let path = format!("{}/src/sched/{module}.rs", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn every_file_under_sched_is_a_declared_module() {
        assert_eq!(declared_modules(include_str!("sched.rs")), files_present());
    }

    #[test]
    fn no_sched_file_redirects_a_module_with_a_path_attribute() {
        assert!(!has_path_attribute(include_str!("sched.rs")));
        for module in declared_modules(include_str!("sched.rs")) {
            assert!(!has_path_attribute(&read(&module)), "{module}.rs");
        }
    }

    #[test]
    fn sched_rs_names_no_build_type() {
        let offenders = offenders(include_str!("sched.rs"));
        assert!(offenders.is_empty(), "{offenders:?}");
    }

    #[test]
    fn the_generic_files_name_no_build_type() {
        let modules = declared_modules(include_str!("sched.rs"));
        assert!(modules.iter().any(|m| m == "work"), "{modules:?}");
        for module in &modules {
            assert_eq!(
                offenders(&read(module)),
                Vec::<String>::new(),
                "{module}.rs"
            );
        }
    }

    #[test]
    fn the_scan_sees_every_spelling_in_code_and_skips_comments_and_test_modules() {
        let source = "\
// the REAPI name
/// a Digest in a doc comment
/* a Digest
   in a block */
let a = 1; /* ActionKey */
let r = a / b;
use crate::digest::Digest;
let b: reapi::Client;
let c: REAPI;
let d: ActionKey;
let e = action_key();

#[cfg(test)]
mod tests {
    #[cfg(test)]
    fn t() {
        let f: Digest;
    }
}
#[cfg(test)]
use something::Digest;
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
    fn block_comments_nest() {
        let source = "/* a * b / c /* d */ Digest */\nlet z: u8;\n";
        assert_eq!(offenders(source), Vec::<String>::new());
        assert_eq!(code_lines(source), ["let z: u8;"]);
    }

    #[test]
    fn braces_in_literals_do_not_open_a_test_item() {
        let source = "#[cfg(test)]\nmod tests {\n    const A: &str = \"{{\";\n    const B: char = '{';\n}\nlet after: Digest;\n";
        assert_eq!(offenders(source), ["let after: Digest;"]);
    }

    #[test]
    fn braces_in_literals_do_not_close_a_test_item() {
        let source = "\
#[cfg(test)]
mod tests {
    const A: &str = \"}}\";
    const B: char = '}';
    const Q: char = '\\'';
    const E: char = '\\n';
    const S: &str = \"\\\"}\";
    const M: &str = \"{
}\";
    fn t<'a>(x: &'a str) { let f: Digest; }
}
let after: Digest;
";
        assert_eq!(offenders(source), ["let after: Digest;"]);
    }

    #[test]
    fn a_trailing_escape_or_unterminated_literal_ends_the_scan_quietly() {
        assert_eq!(code_lines("let s = \"abc\\"), ["let s = \"abc\\"]);
        assert_eq!(code_lines("let c = '\\"), ["let c = '\\"]);
    }

    #[test]
    fn module_names_list_rs_stems_and_directories_only() {
        let entries = [
            ("b.rs", false),
            ("x", true),
            ("note.txt", false),
            ("a.rs", false),
        ];
        assert_eq!(module_names(&entries), ["a", "b", "x/"]);
    }

    #[test]
    fn declared_modules_come_from_code_only() {
        let source = "mod a;\npub mod b;\n// mod c;\n/* mod e; */\nmod d {\n}\nuse x;\n";
        assert_eq!(declared_modules(source), ["a", "b"]);
    }

    #[test]
    fn a_path_attribute_is_found_in_code_and_not_in_comments() {
        assert!(has_path_attribute("#[path = \"x.rs\"]\nmod x;\n"));
        assert!(!has_path_attribute("// #[path = \"x.rs\"]\nmod x;\n"));
    }

    #[test]
    fn a_nested_directory_would_not_match_the_declared_modules() {
        let present = module_names(&[("work.rs", false), ("x", true)]);
        assert_ne!(declared_modules("mod work;\nmod x;\n"), present);
    }

    #[test]
    fn the_root_paths_are_the_sched_types() {
        let id: crate::sched::LeaseId = crate::LeaseId::new(1, 7);
        assert_eq!(id, crate::sched::LeaseId::new(1, 7));
    }
}
