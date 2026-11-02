//! `AGENTS.md` (the rules every change is written and reviewed against) and `CLAUDE.md`
//! (which imports it). The rules are pure functions of the texts, so each is tested
//! against an edited copy as well as against the real files.
//!
//! Catches:
//! - a shared core that drifted: the text between the two marker lines of `AGENTS.md` is,
//!   byte for byte, `tools/agents/shared_core.md` (the first line that differs is named,
//!   in both files);
//! - a copy edited along with it: `tools/agents/shared_core.sha256` is
//!   `<sha256 of the copy, lowercase hex>  shared_core.md` and one newline, the form
//!   `sha256sum` writes, and the digest is the copy's;
//! - markers that cannot delimit the core: each of the two is a line of its own, exactly
//!   once, the begin before the end;
//! - a `CLAUDE.md` that no longer imports `AGENTS.md`: a line that is exactly
//!   `@AGENTS.md`, outside a fenced code block of backticks or tildes (an import inside a
//!   fence, inside a sentence or with anything after it, a carriage return included, is
//!   not read as one).

use sha2::{Digest, Sha256};

const BEGIN: &str = "<!-- shared-core:begin -->";
const END: &str = "<!-- shared-core:end -->";
const IMPORT: &str = "@AGENTS.md";
const COPY: &str = "tools/agents/shared_core.md";
const HASH: &str = "tools/agents/shared_core.sha256";

/// The lines of `text` split at `\n` only, each with its 1-based number and the byte
/// offsets where it starts and where the text after its newline starts.
fn lines(text: &str) -> Vec<(usize, &str, usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut n = 1;
    while start < text.len() {
        let stop = text[start..].find('\n').map_or(text.len(), |i| start + i);
        let after = (stop + 1).min(text.len());
        out.push((n, &text[start..stop], start, after));
        start = stop + 1;
        n += 1;
    }
    out
}

/// The shared core of `agents` (the text after the begin line, up to the end line) and
/// the line it starts on.
fn core(agents: &str) -> Result<(&str, usize), Vec<String>> {
    let all = lines(agents);
    let find = |m: &str| -> Vec<(usize, usize, usize)> {
        all.iter()
            .filter(|l| l.1 == m)
            .map(|l| (l.0, l.2, l.3))
            .collect()
    };
    let (b, e) = (find(BEGIN), find(END));
    let mut errs = Vec::new();
    for (m, found) in [(BEGIN, &b), (END, &e)] {
        if found.len() != 1 {
            errs.push(format!(
                "AGENTS.md: the line `{m}` is there {} times, not once",
                found.len()
            ));
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    let ((bl, _, b_after), (el, e_start, _)) = (b[0], e[0]);
    if e_start < b_after {
        return Err(vec![format!(
            "AGENTS.md:{el}: the end of the shared core comes before its begin (line {bl})"
        )]);
    }
    Ok((&agents[b_after..e_start], bl + 1))
}

/// The first line where `got` (the core of AGENTS.md, starting on its line `first`) and
/// `want` (the copy) differ, named in both files.
fn same_core(got: &str, first: usize, want: &str) -> Result<(), String> {
    if got == want {
        return Ok(());
    }
    let (g, w) = (lines(got), lines(want));
    let mut i = 0;
    while i < g.len() && i < w.len() && g[i].1 == w[i].1 {
        i += 1;
    }
    let show = |l: Option<&(usize, &str, usize, usize)>| {
        l.map_or("the end of the text".to_owned(), |l| format!("`{}`", l.1))
    };
    if i == g.len() && i == w.len() {
        // Every line is the same: the two differ only in a newline at the very end.
        return Err(format!(
            "AGENTS.md:{}: the shared core and {COPY} differ in the newline that ends them",
            first + i
        ));
    }
    Err(format!(
        "AGENTS.md:{}: the shared core differs from {COPY}:{}: AGENTS.md has {}, the copy has {}",
        first + i,
        i + 1,
        show(g.get(i)),
        show(w.get(i)),
    ))
}

/// The hash file is `<sha256 of copy>  shared_core.md\n`, exactly.
fn hash_matches(copy: &[u8], hash_file: &str) -> Result<(), String> {
    let want = format!("{}  shared_core.md\n", hex::encode(Sha256::digest(copy)));
    if hash_file == want {
        Ok(())
    } else {
        Err(format!(
            "{HASH} is {hash_file:?}, not {want:?} (`sha256sum shared_core.md` in tools/agents)"
        ))
    }
}

/// Whether `claude` has a line that is exactly `@AGENTS.md` outside a fenced code block.
fn imports_agents(claude: &str) -> bool {
    // The open fence: its character and length; a closing fence is at least as long.
    let mut fence: Option<(char, usize)> = None;
    for (_, line, _, _) in lines(claude) {
        let t = line.trim_start_matches(' ');
        let lead = line.len() - t.len();
        let ch = t.chars().next();
        let run = ch.map_or(0, |c| t.chars().take_while(|&x| x == c).count());
        // A backtick fence's info string holds no backtick (that line is inline code).
        let is_fence = lead <= 3
            && matches!(ch, Some('`' | '~'))
            && run >= 3
            && !(ch == Some('`') && t[run..].contains('`'));
        match (fence, is_fence) {
            (None, true) => fence = ch.map(|c| (c, run)),
            (Some((c, n)), true) if ch == Some(c) && run >= n && t[run..].trim().is_empty() => {
                fence = None
            }
            (None, false) if line == IMPORT => return true,
            _ => {}
        }
    }
    false
}

fn read(rel: &str) -> String {
    let root = super::repo_root();
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Catches the real AGENTS.md drifting from its copy, the copy from its hash, and
/// CLAUDE.md no longer importing AGENTS.md.
#[test]
fn agents_md_core_is_the_hashed_copy_and_claude_md_imports_it() {
    let agents = read("AGENTS.md");
    let copy = read(COPY);
    let (got, first) = core(&agents).unwrap_or_else(|e| panic!("{}", e.join("\n")));
    assert!(
        got.lines().count() > 40,
        "the shared core is only {} lines",
        got.lines().count()
    );
    assert_eq!(same_core(got, first, &copy), Ok(()));
    assert_eq!(hash_matches(copy.as_bytes(), &read(HASH)), Ok(()));
    assert!(
        imports_agents(&read("CLAUDE.md")),
        "CLAUDE.md has no line `{IMPORT}`"
    );
}

fn agents_with(core_text: &str) -> String {
    format!("# AGENTS.md\n\nintro\n\n{BEGIN}\n{core_text}{END}\n\n## 2. Rust\n")
}

const CORE: &str = "## 1. Shared core\n\n- T1 BLOCK. one\n- T2 SHOULD. two\n";

/// Catches a core read from the wrong place: the text between the markers, and the line
/// after the begin marker as its first line.
#[test]
fn core_is_the_text_between_the_marker_lines() {
    assert_eq!(core(&agents_with(CORE)), Ok((CORE, 6)));
    // An empty core, the markers on adjacent lines.
    assert_eq!(core(&agents_with("")), Ok(("", 6)));
    // The end marker as the file's last line, with no newline after it.
    let text = format!("{BEGIN}\nx\n{END}");
    assert_eq!(core(&text), Ok(("x\n", 2)));
}

/// Catches a marker that is missing, repeated, out of order, or not a line of its own.
#[test]
fn markers_that_cannot_delimit_the_core_are_refused() {
    let twice = |m: &str| format!("AGENTS.md: the line `{m}` is there 2 times, not once");
    let none = |m: &str| format!("AGENTS.md: the line `{m}` is there 0 times, not once");
    assert_eq!(core("no markers\n"), Err(vec![none(BEGIN), none(END)]));
    assert_eq!(core(&format!("{BEGIN}\nx\n")), Err(vec![none(END)]));
    assert_eq!(
        core(&format!("{BEGIN}\n{BEGIN}\nx\n{END}\n")),
        Err(vec![twice(BEGIN)])
    );
    assert_eq!(
        core(&format!("{BEGIN}\nx\n{END}\n{END}\n")),
        Err(vec![twice(END)])
    );
    assert_eq!(
        core(&format!("{END}\nx\n{BEGIN}\n")),
        Err(vec![
            "AGENTS.md:1: the end of the shared core comes before its begin (line 3)".to_owned()
        ])
    );
    // A marker with text around it, or a carriage return, is not a marker line.
    for bad in [
        format!(" {BEGIN}"),
        format!("{BEGIN} "),
        format!("{BEGIN}\r"),
        format!("see {BEGIN}"),
    ] {
        assert_eq!(
            core(&format!("{bad}\nx\n{END}\n")),
            Err(vec![none(BEGIN)]),
            "{bad:?}"
        );
    }
}

/// Catches an edited core: a changed, added or removed line is named by its line in
/// AGENTS.md and in the copy, and a difference only in the final newline is refused too.
#[test]
fn a_core_that_differs_from_the_copy_names_the_first_differing_line() {
    assert_eq!(same_core(CORE, 6, CORE), Ok(()));
    let changed = CORE.replace("two", "2");
    assert_eq!(
        same_core(&changed, 6, CORE),
        Err(format!(
            "AGENTS.md:9: the shared core differs from {COPY}:4: AGENTS.md has `- T2 SHOULD. 2`, the copy has `- T2 SHOULD. two`"
        ))
    );
    let added = format!("{CORE}- T3 BLOCK. three\n");
    assert_eq!(
        same_core(&added, 6, CORE),
        Err(format!(
            "AGENTS.md:10: the shared core differs from {COPY}:5: AGENTS.md has `- T3 BLOCK. three`, the copy has the end of the text"
        ))
    );
    assert_eq!(
        same_core(CORE, 6, &added),
        Err(format!(
            "AGENTS.md:10: the shared core differs from {COPY}:5: AGENTS.md has the end of the text, the copy has `- T3 BLOCK. three`"
        ))
    );
    let crlf = CORE.replacen('\n', "\r\n", 1);
    assert_eq!(
        same_core(&crlf, 6, CORE),
        Err(format!(
            "AGENTS.md:6: the shared core differs from {COPY}:1: AGENTS.md has `## 1. Shared core\r`, the copy has `## 1. Shared core`"
        ))
    );
    assert_eq!(
        same_core(CORE.trim_end_matches('\n'), 6, CORE),
        Err(format!(
            "AGENTS.md:10: the shared core and {COPY} differ in the newline that ends them"
        ))
    );
}

/// Catches a copy edited without its hash, and a hash file in any form but the one
/// `sha256sum` writes.
#[test]
fn the_hash_file_is_the_copys_sha256sum_line() {
    let copy = CORE.as_bytes();
    let hex = hex::encode(Sha256::digest(copy));
    let good = format!("{hex}  shared_core.md\n");
    assert_eq!(hash_matches(copy, &good), Ok(()));
    for bad in [
        // The copy edited: the hash of the text before.
        format!("{}  shared_core.md\n", hex::encode(Sha256::digest(b"x"))),
        format!("{}  shared_core.md\n", hex.to_uppercase()),
        format!("{hex}  shared_core.md"),
        format!("{hex}  shared_core.md\n\n"),
        format!("{hex} shared_core.md\n"),
        format!("{hex}  tools/agents/shared_core.md\n"),
        format!("{hex}\n"),
        String::new(),
    ] {
        assert_eq!(
            hash_matches(copy, &bad),
            Err(format!(
                "{HASH} is {bad:?}, not {good:?} (`sha256sum shared_core.md` in tools/agents)"
            )),
            "{bad:?}"
        );
    }
}

/// Catches CLAUDE.md losing its import: only a line that is exactly `@AGENTS.md`, outside
/// a fenced code block, counts.
#[test]
fn claude_md_imports_agents_md_only_by_a_line_of_its_own_outside_a_fence() {
    for good in [
        "@AGENTS.md\n",
        "@AGENTS.md",
        "# Notes\n\nkbf specifics\n\n@AGENTS.md\n",
        "```\ncode\n```\n@AGENTS.md\n",
        "~~~~\n```\n~~~~\n@AGENTS.md\n",
        "````\n```\n````\n@AGENTS.md\n",
    ] {
        assert!(imports_agents(good), "{good:?}");
    }
    for bad in [
        "",
        "\n",
        "AGENTS.md\n",
        "@AGENTS.md \n",
        " @AGENTS.md\n",
        "@AGENTS.md\r\n",
        "@agents.md\n",
        "read @AGENTS.md first\n",
        "@AGENTS.md.bak\n",
        "```\n@AGENTS.md\n```\n",
        "~~~\n@AGENTS.md\n~~~\n",
        "```md\n@AGENTS.md\n```\n",
        // A shorter or different fence does not close the open one.
        "````\n```\n@AGENTS.md\n````\n",
        "```\n~~~\n@AGENTS.md\n```\n",
        // A fence line with text after it does not close.
        "```\n``` x\n@AGENTS.md\n",
        // A fence never closed holds the rest of the file.
        "```\n@AGENTS.md\n",
    ] {
        assert!(!imports_agents(bad), "{bad:?}");
    }
}

/// Catches a fence misread: four spaces of indent (an indented code block), a run of two,
/// or backticks followed by a backtick is not a fence, so an import after it counts; up to
/// three spaces of indent is one.
#[test]
fn a_fence_is_indented_at_most_three_spaces() {
    assert!(imports_agents("    ```\n@AGENTS.md\n"));
    assert!(!imports_agents("   ```\n@AGENTS.md\n"));
    // Two backticks are not a fence.
    assert!(imports_agents("``\n@AGENTS.md\n"));
    // Nor is a run of backticks with a backtick after it, which is inline code; a tilde
    // fence's info string may hold one.
    assert!(imports_agents("```a`b\n@AGENTS.md\n"));
    assert!(!imports_agents("~~~a`b\n@AGENTS.md\n"));
}
