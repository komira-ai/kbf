//! Tests of the interpreter budget's parts. Each says which defect it catches. The
//! whole scan over a planted tree is tested in `repo_lints.rs` (module `budget`).

use super::rust::{is_crate_src, is_dash_c, is_shell, shell_c_files};
use super::*;

fn found(key: &str) -> Found {
    Found {
        section: Section::Files,
        key: key.to_owned(),
        why: "a `.sh` file".to_owned(),
    }
}

#[test]
fn well_formed_rows_parse_in_their_sections() {
    // Catches: a header, comment or blank line read as a row; a row filed under the
    // wrong section; the same key refused across two sections.
    let text = "# budget\n\n[files]\na.sh\tCI\n[workflow]\nw.yml#j/s\tinstall\n\
                [src-sh-c]\ncrates/x/src/k.rs\tkeeper\na.sh\tsame key, other section\n";
    let (rows, problems) = parse(text);
    assert_eq!(problems, Vec::<String>::new());
    let got: Vec<(Section, &str, usize)> = rows
        .iter()
        .map(|r| (r.section, r.key.as_str(), r.line))
        .collect();
    assert_eq!(
        got,
        vec![
            (Section::Files, "a.sh", 4),
            (Section::Workflow, "w.yml#j/s", 6),
            (Section::SrcShC, "crates/x/src/k.rs", 8),
            (Section::SrcShC, "a.sh", 9),
        ]
    );
}

#[test]
fn malformed_and_duplicate_rows_are_refused() {
    // Catches: a malformed row skipped (so its file escapes the list), including a
    // row still in the old `<count>\t<key>\t<reason>` form; a second row for one key,
    // which would let a stale copy hide.
    for (text, want) in [
        ("a.sh\tCI\n", "line 1: a row before any section header"),
        ("[file]\n", "line 1: unknown section `[file]`"),
        (
            "[files]\n[files]\n",
            "line 2: the section [files] appears twice",
        ),
        (
            "[files]\na.sh\n",
            "line 2: a row needs two tab-separated fields (<key>, <reason>), found 1",
        ),
        (
            "[files]\n3\ta.sh\tCI\n",
            "line 2: a row needs two tab-separated fields (<key>, <reason>), found 3",
        ),
        (
            "[files]\na.sh CI\n",
            "line 2: a row needs two tab-separated fields (<key>, <reason>), found 1",
        ),
        (
            "[files]\na.sh\t\n",
            "line 2: a field is empty or has blanks around it",
        ),
        (
            "[files]\n\tCI\n",
            "line 2: a field is empty or has blanks around it",
        ),
        (
            "[files]\n a.sh\tCI\n",
            "line 2: a field is empty or has blanks around it",
        ),
        (
            "[files]\na.sh\tCI\r\n",
            "line 2: a field is empty or has blanks around it",
        ),
        (
            "[files]\na.sh\tCI\na.sh\tagain\n",
            "line 3: a second [files] row for `a.sh` (the first is line 2)",
        ),
    ] {
        let (_, problems) = parse(text);
        assert_eq!(problems, vec![want.to_owned()], "{text:?}");
    }
}

#[test]
fn a_matching_budget_passes() {
    // Catches: a check that refuses a tree whose every file has its row (the control
    // for the planted defects below).
    let budget = "[files]\na.sh\tCI\nb.py\tempty\n";
    assert_eq!(
        check(budget, &[found("a.sh"), found("b.py")]),
        Vec::<String>::new()
    );
}

#[test]
fn an_unlisted_file_is_refused() {
    // Catches (planted defect): a new `x.sh` with no row passing the lint.
    assert_eq!(
        check("[files]\n", &[found("x.sh")]),
        vec![format!(
            "`x.sh`: a `.sh` file, and {BUDGET} has no [files] row for it; port it to \
             Rust, or add the row `x.sh\\t<reason>`"
        )]
    );
}

#[test]
fn a_stale_row_is_refused() {
    // Catches (planted defect): a row left behind for a deleted file, so a later file
    // of that name would be listed without review.
    assert_eq!(
        check("[files]\ngone.sh\tCI\n", &[]),
        vec![
            "line 2: the [files] row for `gone.sh` names nothing the scan found (deleted, \
             renamed, or no longer counted); remove the row"
                .to_owned()
        ]
    );
}

#[test]
fn a_row_matches_only_its_own_section() {
    // Catches: a production `sh -c` cleared by a [files] row of the same key.
    let mut spawn = found("crates/k/src/k.rs");
    spawn.section = Section::SrcShC;
    let problems = check("[files]\ncrates/k/src/k.rs\tCI\n", &[spawn]);
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].contains("no [src-sh-c] row"), "{problems:?}");
    assert!(
        problems[1].contains("the [files] row for `crates/k/src/k.rs`"),
        "{problems:?}"
    );
}

#[test]
fn interpreter_files_are_found_by_extension_or_first_line() {
    // Catches (one planted defect among them: an extensionless
    // `#!/usr/bin/env python3`): an interpreter file missed because it has no
    // extension, an upper-case one, or a `#!` line through `env` options or
    // `busybox`; and a Rust inner attribute or a compiled tool's `#!` taken for one.
    for (path, bytes, want) in [
        ("tools/x.sh", "echo\n", Some("a `.sh` file")),
        ("tools/X.PY", "", Some("a `.PY` file")),
        ("t/cov.awk", "{}\n", Some("a `.awk` file")),
        ("t/fix.sed", "s/a/b/\n", Some("a `.sed` file")),
        ("t/a.rb", "", Some("a `.rb` file")),
        ("t/a.pl", "", Some("a `.pl` file")),
        ("t/a.zsh", "", Some("a `.zsh` file")),
        ("t/a.ksh", "", Some("a `.ksh` file")),
        ("t/a.bash", "", Some("a `.bash` file")),
        (
            "tools/probe",
            "#!/usr/bin/env python3\nprint(1)\n",
            Some("its `#!` line runs `python3`"),
        ),
        ("t/fakecmd", "#!/bin/sh\n", Some("its `#!` line runs `sh`")),
        (
            "t/a",
            "#! /bin/bash -eu\n",
            Some("its `#!` line runs `bash`"),
        ),
        (
            "t/b",
            "#!/usr/bin/env -S bash -e\n",
            Some("its `#!` line runs `bash`"),
        ),
        (
            "t/c",
            "#!/usr/bin/env -u HOME LC_ALL=C python3.12\n",
            Some("its `#!` line runs `python3.12`"),
        ),
        (
            "t/d",
            "#!/bin/busybox ash\n",
            Some("its `#!` line runs `ash`"),
        ),
        (
            "t/e",
            "#!/usr/bin/env busybox sh\n",
            Some("its `#!` line runs `sh`"),
        ),
        (
            "t/f",
            "#!/usr/bin/awk -f\n",
            Some("its `#!` line runs `awk`"),
        ),
        (
            "t/g",
            "#!/usr/bin/perl\r\n",
            Some("its `#!` line runs `perl`"),
        ),
        (
            "t/h",
            "#!/usr/bin/env ruby\n",
            Some("its `#!` line runs `ruby`"),
        ),
        ("t/i", "#!/bin/sed -f\n", Some("its `#!` line runs `sed`")),
        ("t/j", "#!/bin/dash\n", Some("its `#!` line runs `dash`")),
        ("t/k", "#!/bin/zsh\n", Some("its `#!` line runs `zsh`")),
        ("t/l", "#!/bin/ksh\n", Some("its `#!` line runs `ksh`")),
        ("crates/x/src/lib.rs", "#![allow(dead_code)]\n", None),
        ("t/tool", "#!/usr/bin/env cargo\n", None),
        ("t/env-only", "#!/usr/bin/env\n", None),
        ("t/notes", "echo hi\n#!/bin/sh\n", None),
        ("t/a.shx", "", None),
        ("sh/README.md", "", None),
        ("t/a.sh.txt", "", None),
        ("t/shell", "", None),
    ] {
        assert_eq!(
            interpreter_file(path, bytes.as_bytes()).as_deref(),
            want,
            "{path}: {bytes:?}"
        );
    }
}

#[test]
fn shell_logic_in_a_run_script_is_named() {
    // Catches: a pipe, chain, loop, condition or substitution passing as a one-line
    // command, and a plain command (with `--if-present` or a path holding `for`)
    // refused as logic.
    for (script, want) in [
        ("cargo test --workspace --locked", None),
        ("cargo test --workspace --locked\n", None),
        ("bash tools/ci/x.sh dist/for/a --if-present", None),
        ("cargo run -- --wait-ready\n\n", None),
        ("a\nb\n", Some("more than one line")),
        ("cargo llvm-cov | tee x", Some("a pipe or `||`")),
        ("a || true", Some("a pipe or `||`")),
        ("sw_vers && uname -m", Some("`&&`")),
        ("a; b", Some("`;`")),
        ("echo $(date)", Some("`$(`")),
        ("echo `date`", Some("a backtick")),
        ("if true then x fi", Some("`if`")),
        ("for x in a b do y done", Some("`for`")),
        ("while true do x done", Some("`while`")),
    ] {
        assert_eq!(shell_logic(script), want, "{script:?}");
    }
    assert_eq!(script_lines("\n  a\n  b\n\n"), 2);
}

fn src(files: &[(&str, &str)]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
        .collect()
}

fn spawns(files: &[(&str, &str)]) -> Vec<String> {
    let (found, problems) = shell_c_files(&src(files));
    assert_eq!(problems, Vec::<String>::new());
    found
}

const LIB: &str = "crates/x/src/lib.rs";

#[test]
fn a_production_sh_c_is_found() {
    // Catches (planted defect): `Command::new("sh").arg("-c")` in production code
    // passing the lint; the keeper's shape (the shell a `Path` in one function, `-c`
    // an `.arg` in another) and a macro's argument list missed; and a file with two
    // spawns reported twice (its one row is per file).
    let planted = r#"fn f() { std::process::Command::new("sh").arg("-c").arg("x"); }"#;
    assert_eq!(spawns(&[(LIB, planted)]), vec![LIB.to_owned()]);
    let keeper = r#"
        pub fn watch() { with(std::path::Path::new("/bin/sh")) }
        fn with(shell: &std::path::Path) { std::process::Command::new(shell).arg("-c"); }
    "#;
    assert_eq!(spawns(&[(LIB, keeper)]), vec![LIB.to_owned()]);
    let argv = r#"fn f() -> Vec<&'static str> { vec!["/bin/bash", "-ec", "x"] }"#;
    assert_eq!(spawns(&[(LIB, argv)]), vec![LIB.to_owned()]);
    let not_test = r#"#[cfg(not(test))] fn f() { g("bash", "-c") }"#;
    assert_eq!(spawns(&[(LIB, not_test)]), vec![LIB.to_owned()]);
    let any = r#"#[cfg(any(test, unix))] fn f() { g("zsh", "-c") }"#;
    assert_eq!(spawns(&[(LIB, any)]), vec![LIB.to_owned()]);
    let two = r#"fn f() { g("sh", "-c"); g("/bin/sh", "-c") }"#;
    assert_eq!(spawns(&[(LIB, two)]), vec![LIB.to_owned()]);
}

#[test]
fn a_shell_name_or_flag_alone_is_not_a_spawn() {
    // Catches: a file refused for naming a shell it never gives `-c` (a path
    // resolver's table), or for a `-c` that is no shell's; and a doc comment or
    // attribute read as code.
    for text in [
        r#"fn f() { resolve("sh") }"#,
        r#"fn f() { g("-c") }"#,
        r#"/// Runs `sh -c`.
           #[doc = "sh"] fn f() { g("-c") }"#,
        r#"fn f() { g("bin/sh", "-c"); g("-cp") }"#,
    ] {
        assert_eq!(spawns(&[(LIB, text)]), Vec::<String>::new(), "{text}");
    }
}

#[test]
fn test_code_is_left_out() {
    // Catches: a test's `sh -c` (the tests keep sh on purpose for now) refused as
    // production, in an inline test module, a test-only function of an impl, a
    // `cfg(all(test, ...))` item or a file whose inner attribute makes it a test.
    for text in [
        r#"#[cfg(test)] mod tests { fn f() { g("/bin/sh", "-c") } }"#,
        r#"struct K; impl K { #[cfg(test)] fn f() { g("sh", "-c") } fn h() {} }"#,
        r#"trait T { #[cfg(test)] fn f() { g("sh", "-c") } }"#,
        r#"#[cfg(all(test, target_os = "linux"))] fn f() { g("sh", "-c") }"#,
        r#"fn f() { #[cfg(test)] let x = g("sh", "-c"); }"#,
        "#![cfg(test)]\nfn f() { g(\"sh\", \"-c\") }",
    ] {
        assert_eq!(spawns(&[(LIB, text)]), Vec::<String>::new(), "{text}");
    }
}

#[test]
fn a_file_reached_through_a_test_module_is_left_out() {
    // Catches: `src/a/tests.rs` (declared `#[cfg(test)] mod tests;` in `src/a.rs`)
    // counted as production; and the same for a `#[path]` file, a module nested in
    // a test file, and a test module inside an inline module; while a production
    // module's file still counts.
    let spawn = r#"fn f() { g("sh", "-c") }"#;
    let files = [
        (
            "crates/x/src/lib.rs",
            "mod a;\nmod b;\nmod c { #[cfg(test)] mod t; }",
        ),
        ("crates/x/src/a.rs", "#[cfg(test)]\nmod tests;"),
        ("crates/x/src/a/tests.rs", "mod deeper;"),
        ("crates/x/src/a/tests/deeper.rs", spawn),
        (
            "crates/x/src/b.rs",
            "#[cfg(test)]\n#[path = \"b_tests.rs\"]\nmod tests;\nmod prod;",
        ),
        ("crates/x/src/b_tests.rs", spawn),
        ("crates/x/src/b/prod.rs", spawn),
        ("crates/x/src/c/t.rs", spawn),
        ("crates/x/src/bin/tool.rs", "#[cfg(test)] mod tool_tests;"),
        ("crates/x/src/bin/tool_tests.rs", spawn),
    ];
    assert_eq!(spawns(&files), vec!["crates/x/src/b/prod.rs".to_owned()]);
}

#[test]
fn a_file_that_does_not_parse_is_refused() {
    // Catches: a source the parser cannot read skipped, so its `sh -c` escapes.
    let (found, problems) = shell_c_files(&src(&[(LIB, "fn f( {")]));
    assert_eq!(found, Vec::<String>::new());
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].starts_with(&format!("{LIB}: not read as Rust")),
        "{problems:?}"
    );
}

#[test]
fn shells_flags_and_crate_sources_are_told_apart() {
    // Catches: a shell missed by its path form, a relative path or a longer name
    // taken for one; a `-c` flag missed in a cluster; a file outside a crate's `src/`
    // scanned (tests, examples, build scripts) or one inside missed.
    for s in [
        "sh",
        "/bin/sh",
        "/usr/bin/bash",
        "dash",
        "/bin/zsh",
        "ksh",
        "ash",
    ] {
        assert!(is_shell(s), "{s}");
    }
    for s in ["shell", "bin/sh", "./sh", "/bin/fish", "", "sh "] {
        assert!(!is_shell(s), "{s}");
    }
    for s in ["-c", "-ec", "-lc", "-xec"] {
        assert!(is_dash_c(s), "{s}");
    }
    for s in ["c", "-", "--c", "-cp", "-c1", "--command"] {
        assert!(!is_dash_c(s), "{s}");
    }
    for p in ["crates/a/src/lib.rs", "crates/a/src/x/y.rs"] {
        assert!(is_crate_src(p), "{p}");
    }
    for p in [
        "crates/a/tests/x.rs",
        "crates/a/build.rs",
        "crates/a/src/x.txt",
        "crates//src/lib.rs",
        "tools/src/lib.rs",
    ] {
        assert!(!is_crate_src(p), "{p}");
    }
}
