use super::*;

fn file(p: &str) -> Pattern {
    Pattern::File(p.to_owned())
}

fn tree(p: &str) -> Pattern {
    Pattern::Tree(p.to_owned())
}

fn paths(list: &[&str]) -> Vec<String> {
    list.iter().map(|p| (*p).to_owned()).collect()
}

#[test]
fn a_file_lists_its_files_and_trees_in_order_and_skips_comments_and_blank_lines() {
    // Catches: a comment or a blank line read as a pattern (a blank pattern matches nothing, a
    // `#` line could match a file named so), or the two forms told apart wrongly.
    let text = "# a comment\n\ncrates/a/**\nCargo.lock\n  # not a comment: spaces first\n";
    assert_eq!(
        parse(text),
        Err(vec![
            "line 5: `  # not a comment: spaces first` has spaces at its start or end".to_owned()
        ])
    );
    let text = "# a comment\n\ncrates/a/**\nCargo.lock\n.github/workflows/x.yml\n";
    assert_eq!(
        parse(text),
        Ok(vec![
            tree("crates/a"),
            file("Cargo.lock"),
            file(".github/workflows/x.yml")
        ])
    );
}

#[test]
fn every_other_spelling_is_refused_with_its_line() {
    // Catches: a glob this reader does not evaluate (`*.rs`, `a/**/b`) taken literally, so it
    // matches nothing and the macOS jobs never run for the paths it was meant to name; and a
    // path that can never equal a path git prints (`/a`, `a/`, `./a`, `a//b`, `a/../b`, `a\b`).
    for (line, why) in [
        (" crates/a/**", "has spaces at its start or end"),
        ("crates/a/** ", "has spaces at its start or end"),
        (
            "crates\\a/**",
            "holds a backslash; parts are separated by `/`",
        ),
        ("crates/*.rs", "holds a `*` that is not the final `/**`"),
        ("crates/**/x.rs", "holds a `*` that is not the final `/**`"),
        ("crates/a/*", "holds a `*` that is not the final `/**`"),
        ("crates/a**", "holds a `*` that is not the final `/**`"),
        ("**", "holds a `*` that is not the final `/**`"),
        ("crates/a/**/**", "holds a `*` that is not the final `/**`"),
        (
            "/crates/a/**",
            "starts with `/`; patterns are relative to the root",
        ),
        (
            "crates/a/",
            "ends with `/`; a directory is written `<dir>/**`",
        ),
        (
            "crates/a//**",
            "ends with `/`; a directory is written `<dir>/**`",
        ),
        ("/**", "has an empty, `.` or `..` part"),
        ("crates//a.rs", "has an empty, `.` or `..` part"),
        ("./crates/a.rs", "has an empty, `.` or `..` part"),
        ("crates/../a.rs", "has an empty, `.` or `..` part"),
        (".", "has an empty, `.` or `..` part"),
    ] {
        let text = format!("ok.rs\n{line}\n");
        assert_eq!(
            parse(&text),
            Err(vec![format!("line 2: `{line}` {why}")]),
            "{line:?}"
        );
    }
}

#[test]
fn a_repeated_pattern_and_every_bad_line_are_each_reported() {
    // Catches: a parser that stops at the first problem (a fix then meets the next one a run
    // later), or that accepts a pattern twice (one copy edited, the other left stale).
    let text = "crates/a/**\nx.rs\ncrates/a/**\n/y\n";
    assert_eq!(
        parse(text),
        Err(vec![
            "line 3: `crates/a/**` is listed twice".to_owned(),
            "line 4: `/y` starts with `/`; patterns are relative to the root".to_owned(),
        ])
    );
}

#[test]
fn a_file_with_no_pattern_is_refused() {
    // Catches: an empty or comment-only file read as "nothing matches", which would skip the
    // macOS jobs on every pull request without a word.
    for text in ["", "\n\n", "# only a comment\n"] {
        assert_eq!(
            parse(text),
            Err(vec![
                "the file lists no pattern, so it would match nothing".to_owned()
            ]),
            "{text:?}"
        );
    }
}

#[test]
fn a_tree_matches_what_is_under_it_and_a_file_only_itself() {
    // Catches: a prefix match that takes a sibling with a longer name (`crates/kbf-node2`) for
    // the directory `crates/kbf-node`, and a file pattern that matches by prefix.
    let t = tree("crates/kbf-node");
    assert!(t.matches("crates/kbf-node/src/lib.rs"));
    assert!(t.matches("crates/kbf-node/Cargo.toml"));
    assert!(!t.matches("crates/kbf-node2/src/lib.rs"));
    assert!(!t.matches("crates/kbf-node"));
    assert!(!t.matches("crates/kbf"));
    assert!(!t.matches("x/crates/kbf-node/a.rs"));
    let f = file("Cargo.lock");
    assert!(f.matches("Cargo.lock"));
    assert!(!f.matches("Cargo.lock.orig"));
    assert!(!f.matches("crates/x/Cargo.lock"));
    assert_eq!(t.spelling(), "crates/kbf-node/**");
    assert_eq!(f.spelling(), "Cargo.lock");
}

#[test]
fn each_matched_path_is_reported_with_the_first_pattern_it_matches() {
    // Catches: a report that loses the order of the paths, names the wrong pattern, or lists
    // a path that matched nothing.
    let patterns = vec![tree("crates/a"), file("crates/a/x.rs"), file("ci.yml")];
    let changed = paths(&["docs/a.md", "crates/a/x.rs", "ci.yml"]);
    let found = matched(&patterns, &changed);
    assert_eq!(
        found,
        vec![("crates/a/x.rs", &patterns[0]), ("ci.yml", &patterns[2])]
    );
    assert_eq!(
        report("macos", &found),
        vec![
            "macos=true",
            "changed: crates/a/x.rs (matches crates/a/**)",
            "changed: ci.yml (matches ci.yml)",
        ]
    );
}

#[test]
fn no_match_and_no_change_both_say_false() {
    // Catches: `true` for a pull request that changed only paths outside the surface, or for
    // one that changed nothing at all.
    let patterns = vec![tree("crates/a")];
    for changed in [paths(&["docs/a.md", "crates/ab/x.rs"]), paths(&[])] {
        let found = matched(&patterns, &changed);
        assert!(found.is_empty(), "{changed:?}");
        assert_eq!(report("macos", &found), vec!["macos=false"]);
    }
}
