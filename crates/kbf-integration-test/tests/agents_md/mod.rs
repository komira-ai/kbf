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
//!   `@AGENTS.md` in a paragraph of the file read as CommonMark (by `pulldown-cmark`), so
//!   not inside a fenced or indented code block or a code span (which may run over several
//!   lines, and which an escaped backtick does not open), and not in a sentence, a heading
//!   or a quote, or with anything after it on its line (CR LF and a lone CR end a line, as
//!   LF does).

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

/// Whether `claude`, read as CommonMark, has a paragraph line that is exactly `@AGENTS.md`:
/// plain text, with nothing else on its line of the source, so not inside a code block, a
/// code span, a heading or an HTML block, and not quoted, indented or followed by anything.
fn imports_agents(claude: &str) -> bool {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let text = fence_tails_trimmed(claude);
    let b = text.as_bytes();
    // CommonMark's line endings: LF, CR LF and a lone CR.
    let whole_line = |r: &std::ops::Range<usize>| {
        (r.start == 0 || matches!(b[r.start - 1], b'\n' | b'\r'))
            && (r.end == b.len() || matches!(b[r.end], b'\n' | b'\r'))
    };
    let mut in_paragraph = false;
    for (event, range) in Parser::new(&text).into_offset_iter() {
        match event {
            Event::Start(Tag::Paragraph) => in_paragraph = true,
            Event::End(TagEnd::Paragraph) => in_paragraph = false,
            Event::Text(t) if in_paragraph && &*t == IMPORT && whole_line(&range) => return true,
            _ => {}
        }
    }
    false
}

/// `text` with the spaces and tabs removed from the end of every fence line (up to three
/// spaces, then three or more backticks or tildes, then only spaces and tabs). CommonMark
/// lets a closing fence end in spaces and tabs, but pulldown-cmark 0.13 does not close a
/// fence on a tab; without this, a later fence line would close the block in its place.
/// The other lines, and every line ending, are kept as they are.
fn fence_tails_trimmed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        let (line, after) = rest.split_at(end);
        let eol = if after.starts_with("\r\n") {
            2
        } else {
            usize::from(!after.is_empty())
        };
        let body = line.trim_end_matches([' ', '\t']);
        let t = body.trim_start_matches(' ');
        let run = t.chars().next().map_or(0, |c| {
            if c == '`' || c == '~' {
                t.chars().take_while(|&x| x == c).count()
            } else {
                0
            }
        });
        let is_fence_line = body.len() - t.len() <= 3 && run >= 3 && run == t.len();
        out.push_str(if is_fence_line { body } else { line });
        out.push_str(&after[..eol]);
        rest = &after[eol..];
    }
    out
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
        // A closing fence may end in spaces and tabs.
        "```\n```  \n@AGENTS.md\n",
        "~~~\n~~~   \n@AGENTS.md\n",
        "```\n```\t\n@AGENTS.md\n",
        "```\n``` \t \n@AGENTS.md\n",
        "~~~\n~~~\t\t\r\n@AGENTS.md\r\n",
        // CR LF and a lone CR end a line as LF does.
        "@AGENTS.md\r\n",
        "x\r@AGENTS.md\r",
        "```\r\n@x\r\n```\r\n@AGENTS.md\r\n",
        // After a matched code span, the import counts.
        "``\nx\n``\n@AGENTS.md\n",
        "``\nx\n``\n\n@AGENTS.md\n",
        // Runs of different lengths do not match: the opener is plain text.
        "``\n@AGENTS.md\n`\n",
        "``\n@AGENTS.md\n```x\n",
        // A span opener with no closer in its paragraph is plain text, and a blank line
        // or a fence ends the paragraph.
        "``\n@AGENTS.md\n",
        "``\n\n@AGENTS.md\n``\n",
        "``\n~~~\n~~~\n@AGENTS.md\n``\n",
        // An escaped backtick is text and opens nothing.
        "\\`\n@AGENTS.md\n`\n",
        "\\`\n@AGENTS.md\n",
        // A heading's backtick is in the heading: it cannot pair with one in the next
        // paragraph, and a heading interrupts a paragraph with an unmatched backtick.
        "# `\n@AGENTS.md\n`\n",
        "`\n# x\n@AGENTS.md\n`\n",
    ] {
        assert!(imports_agents(good), "{good:?}");
    }
    for bad in [
        "",
        "\n",
        "AGENTS.md\n",
        "@AGENTS.md \n",
        " @AGENTS.md\n",
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
        // A closing fence ending in a tab closes, so the next fence opens a block that
        // holds the import.
        "```\n```\t\n```\n@AGENTS.md\n",
        "~~~\n~~~ \t\n~~~\n@AGENTS.md\n",
        // A closing fence followed by anything but spaces and tabs does not close.
        "```\n```\u{a0}\n@AGENTS.md\n",
        "```\n```\u{3000}\n@AGENTS.md\n",
        // An inline code span over several lines holds the import.
        "``\n@AGENTS.md\n``\n",
        "`\n@AGENTS.md\n`\n",
        "see ``a\n@AGENTS.md\nb`` here\n",
        // An escaped backtick does not open a span; the backtick after it does.
        "\\``\n@AGENTS.md\n`\n",
        // A heading, a quote or an HTML block is not a paragraph.
        "@AGENTS.md\n===\n",
        "# @AGENTS.md\n",
        "> @AGENTS.md\n",
        "<!--\n@AGENTS.md\n-->\n",
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

/// Catches the fence-tail trim reaching past fence lines: only a line that is a fence run
/// (indented at most three spaces) loses its trailing spaces and tabs, and every line
/// ending (LF, CR LF, a lone CR) and every other line is kept byte for byte.
#[test]
fn only_fence_lines_lose_their_trailing_spaces_and_tabs() {
    for (input, want) in [
        ("```\t\n", "```\n"),
        ("   ~~~ \t\r\n", "   ~~~\r\n"),
        ("````\t\r", "````\r"),
        ("```\t", "```"),
        ("x\n```  \nx", "x\n```\nx"),
        ("@AGENTS.md \t\n", "@AGENTS.md \t\n"),
        ("    ```\t\n", "    ```\t\n"),
        ("``\t\n", "``\t\n"),
        ("```x\t\n", "```x\t\n"),
        ("`~~\t\n", "`~~\t\n"),
        ("```\u{a0}\n", "```\u{a0}\n"),
        ("", ""),
        ("\n\r\n\r", "\n\r\n\r"),
    ] {
        assert_eq!(fence_tails_trimmed(input), want, "{input:?}");
    }
}
