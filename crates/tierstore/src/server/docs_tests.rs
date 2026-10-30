//! The operator guide (`docs/*.md`) held to the code.
//!
//! Every page of `crates/tierstore/docs/` is read, not only the guide's: a flag it names must be
//! one of the binary's (read from clap, `Cli::command().get_arguments()`), a `section.key` it
//! names must be in the config file's key table, a TOML block must load through the real loader
//! (or be marked `toml fragment`, which has only its keys checked), a metric name must occur in
//! the source, and a block marked `message` must quote text that the source holds. The other way
//! round, every flag of the binary and every key of the table must be named by some page.
//!
//! A section titled exactly `Not in a release yet` (a heading of any level; it runs to the next
//! heading of the same or a higher level) is skipped by the exists-in-code checks: it is where a
//! page describes what is on its way and not in the code yet. Everything else in a page is held
//! to the code.
//!
//! The checker is a function of the pages and the code, so the tests below run it on pages that
//! have been made wrong and see it say so.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::CommandFactory;

use super::cli::Cli;
use super::config::{KEYS, from_text};
use super::config_testing::DEFAULTS;

/// The title of the sections that the exists-in-code checks leave out.
const NOT_YET: &str = "Not in a release yet";

/// Flags of other programs that a page may name (an example of a podman or curl call).
const FOREIGN_FLAGS: &[&str] = &["--tls-verify"];

/// Flags of the binary the pages need not name.
const UNDOCUMENTED_FLAGS: &[&str] = &[];

/// Keys of the table the pages need not name.
const UNDOCUMENTED_KEYS: &[&str] = &[];

/// Pages that another change adds; a link to one of them is let stand until it exists.
const PENDING_PAGES: &[&str] = &["metrics.md"];

/// The tables of the file, and the keys of `[[policy]]`, which are not in the key table.
const SECTIONS: &[&str] = &[
    "server",
    "hot_store",
    "cold_store",
    "oci",
    "policy",
    "local_cache",
];
const POLICY_KEYS: &[&str] = &["policy.prefix", "policy.cache"];

/// One page.
struct Doc {
    name: String,
    text: String,
}

/// What the checker knows of the code.
struct Code {
    flags: BTreeSet<String>,
    keys: BTreeSet<String>,
    /// The non-test source of the crate with its comments and the backslashes of its escapes out.
    source: String,
}

/// A fenced block.
struct Block {
    info: String,
    body: String,
    line: usize,
}

/// What a page holds, outside its `Not in a release yet` sections.
#[derive(Default)]
struct Parsed {
    /// Text in single backticks, with its line.
    spans: Vec<(usize, String)>,
    blocks: Vec<Block>,
    /// The lines that are not in a fence.
    prose: Vec<(usize, String)>,
    /// The anchors of every heading, those of skipped sections too.
    slugs: Vec<String>,
}

/// The anchor of a heading: letters and digits lowercased, `_` kept, spaces and `-` as `-`.
/// The site's renderer makes the same ids.
fn slug(heading: &str) -> String {
    heading
        .chars()
        .filter_map(|c| match c {
            'a'..='z' | '0'..='9' | '_' => Some(c),
            'A'..='Z' => Some(c.to_ascii_lowercase()),
            ' ' | '-' => Some('-'),
            _ => None,
        })
        .collect()
}

fn parse(text: &str) -> Parsed {
    let mut out = Parsed::default();
    let mut skipping: Option<usize> = None;
    let mut fence: Option<Block> = None;
    for (at, line) in text.lines().enumerate() {
        let number = at + 1;
        if fence.is_some() {
            if line.starts_with("```") {
                let block = fence.take().expect("a fence is open");
                if skipping.is_none() {
                    out.blocks.push(block);
                }
            } else if let Some(block) = fence.as_mut() {
                block.body.push_str(line);
                block.body.push('\n');
            }
            continue;
        }
        if let Some(info) = line.strip_prefix("```") {
            fence = Some(Block {
                info: info.trim().to_owned(),
                body: String::new(),
                line: number,
            });
            continue;
        }
        let level = line.chars().take_while(|c| *c == '#').count();
        if level > 0 && line[level..].starts_with(' ') {
            let title = line[level..].trim();
            out.slugs.push(slug(title));
            if skipping.is_some_and(|depth| level <= depth) {
                skipping = None;
            }
            if title == NOT_YET && skipping.is_none() {
                skipping = Some(level);
            }
        }
        if skipping.is_some() {
            continue;
        }
        out.prose.push((number, line.to_owned()));
        for (index, part) in line.split('`').enumerate() {
            if index % 2 == 1 {
                out.spans.push((number, part.to_owned()));
            }
        }
    }
    out
}

/// The `--flag` words of `text`.
fn flags_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while let Some(offset) = text[at..].find("--") {
        let start = at + offset;
        let before = start.checked_sub(1).map(|i| bytes[i]);
        let mut end = start + 2;
        while end < bytes.len()
            && (bytes[end].is_ascii_lowercase()
                || bytes[end].is_ascii_digit()
                || bytes[end] == b'-')
        {
            end += 1;
        }
        let word = text[start..end].trim_end_matches('-');
        let lone = before.is_none_or(|b| !b.is_ascii_alphanumeric() && b != b'-');
        if lone && word.len() > 2 && word.as_bytes()[2].is_ascii_lowercase() {
            found.push(word.to_owned());
        }
        at = end.max(start + 2);
    }
    found
}

/// The `tierstore_...` words of `text` that are metric names.
fn metrics_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = text[at..].find("tierstore_") {
        let start = at + offset;
        let rest = &text[start..];
        let len = rest
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(rest.len());
        let word = rest[..len].trim_end_matches('_');
        if word.len() > "tierstore_".len() {
            found.push(word.to_owned());
        }
        at = start + len.max(1);
    }
    found
}

/// The targets of the `[text](target)` links of a line.
fn links_in(line: &str) -> Vec<String> {
    line.split("](")
        .skip(1)
        .filter_map(|rest| rest.split_once(')').map(|(target, _)| target.to_owned()))
        .collect()
}

/// Every problem the pages have against the code, each naming the page and the line.
fn check(docs: &[Doc], code: &Code) -> Vec<String> {
    let mut problems = Vec::new();
    let parsed: Vec<(&Doc, Parsed)> = docs.iter().map(|doc| (doc, parse(&doc.text))).collect();
    let mut named_flags = BTreeSet::new();
    let mut named_keys = BTreeSet::new();
    let known_key = |key: &str| code.keys.contains(key) || POLICY_KEYS.contains(&key);
    for (doc, page) in &parsed {
        let name = &doc.name;
        let mut seen = BTreeSet::new();
        for anchor in &page.slugs {
            if *anchor != slug(NOT_YET) && !seen.insert(anchor) {
                problems.push(format!("{name}: two headings have the anchor #{anchor}"));
            }
        }
        let flag_text = page
            .spans
            .iter()
            .map(|(line, text)| (*line, text.as_str()))
            .chain(page.blocks.iter().map(|b| (b.line, b.body.as_str())));
        for (line, text) in flag_text {
            for flag in flags_in(text) {
                let real = code.flags.contains(&flag) || flag == "--help" || flag == "--version";
                if real {
                    named_flags.insert(flag);
                } else if !FOREIGN_FLAGS.contains(&flag.as_str()) {
                    problems.push(format!("{name}:{line}: {flag} is not a flag of tierstore"));
                }
            }
        }
        for (line, span) in &page.spans {
            let Some((table, key)) = span.split_once('.') else {
                continue;
            };
            let shaped = key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && !key.is_empty();
            if SECTIONS.contains(&table) && shaped {
                if known_key(span) {
                    named_keys.insert(span.clone());
                } else {
                    problems.push(format!("{name}:{line}: {span} is not a key of the file"));
                }
            }
        }
        for block in &page.blocks {
            let at = format!("{name}:{}", block.line);
            if block.info == "toml" || block.info == "toml fragment" {
                match toml::from_str::<toml::Table>(&block.body) {
                    Err(why) => {
                        problems.push(format!("{at}: the TOML does not parse: {}", why.message()))
                    }
                    Ok(table) => {
                        for key in toml_keys(&table) {
                            if known_key(&key) {
                                named_keys.insert(key);
                            } else {
                                problems.push(format!("{at}: {key} is not a key of the file"));
                            }
                        }
                    }
                }
                if block.info == "toml"
                    && let Err(why) = from_text(&block.body, DEFAULTS)
                {
                    problems.push(format!("{at}: the loader refuses the file: {why}"));
                }
            } else if block.info == "message" {
                for line in block.body.lines().filter(|l| !l.trim().is_empty()) {
                    quoted(&at, line, code, &mut problems);
                }
            }
        }
        let metric_text = page
            .prose
            .iter()
            .map(|(_, text)| text.as_str())
            .chain(page.blocks.iter().map(|b| b.body.as_str()));
        for text in metric_text {
            for metric in metrics_in(text) {
                if !code.source.contains(&metric) {
                    problems.push(format!("{name}: the metric {metric} is not in the source"));
                }
            }
        }
        for (line, text) in &page.prose {
            for target in links_in(text) {
                link(&format!("{name}:{line}"), &target, &parsed, &mut problems);
            }
        }
    }
    let missing_flags = code.flags.iter().filter(|f| !named_flags.contains(*f));
    for flag in missing_flags.filter(|f| !UNDOCUMENTED_FLAGS.contains(&f.as_str())) {
        problems.push(format!("no page names the flag {flag}"));
    }
    let missing_keys = code.keys.iter().filter(|k| !named_keys.contains(*k));
    for key in missing_keys.filter(|k| !UNDOCUMENTED_KEYS.contains(&k.as_str())) {
        problems.push(format!("no page names the key {key}"));
    }
    problems
}

/// The `table.key` of every key of a TOML table of the file, `[[policy]]` included.
fn toml_keys(table: &toml::Table) -> Vec<String> {
    let mut keys = Vec::new();
    for (section, value) in table {
        let tables: Vec<&toml::Table> = match value {
            toml::Value::Table(t) => vec![t],
            toml::Value::Array(items) => items.iter().filter_map(toml::Value::as_table).collect(),
            _ => Vec::new(),
        };
        if tables.is_empty() {
            keys.push(section.clone());
        }
        for inner in tables {
            if inner.is_empty() {
                keys.push(format!("{section}."));
            }
            keys.extend(inner.keys().map(|key| format!("{section}.{key}")));
        }
    }
    keys
}

/// A line of a `message` block: the text between its `<placeholders>` is in the source.
fn quoted(at: &str, line: &str, code: &Code, problems: &mut Vec<String>) {
    let mut rest = line;
    let mut parts = Vec::new();
    while let Some((before, after)) = rest.split_once('<') {
        parts.push(before);
        rest = after.split_once('>').map_or("", |(_, tail)| tail);
    }
    parts.push(rest);
    let edge = |c: char| c == ':' || c.is_whitespace();
    let checked = parts
        .iter()
        .map(|p| p.trim_matches(edge))
        .filter(|p| p.len() >= 6);
    for part in checked {
        if !code.source.contains(part) {
            problems.push(format!("{at}: the message {part:?} is not in the source"));
        }
    }
}

/// A link of a page: another page of the docs, with an anchor that its headings make, or an
/// address on the web (not checked here).
fn link(at: &str, target: &str, parsed: &[(&Doc, Parsed)], problems: &mut Vec<String>) {
    if target.contains("://") {
        return;
    }
    let (file, anchor) = target
        .split_once('#')
        .map_or((target, None), |(f, a)| (f, Some(a)));
    let Some((_, page)) = parsed.iter().find(|(doc, _)| doc.name == file) else {
        if !PENDING_PAGES.contains(&file) {
            problems.push(format!("{at}: the link {target} names no page of docs/"));
        }
        return;
    };
    if let Some(anchor) = anchor
        && !page.slugs.iter().any(|s| s == anchor)
    {
        problems.push(format!(
            "{at}: the link {target} names no heading of {file}"
        ));
    }
}

fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs")
}

fn read_docs() -> Vec<Doc> {
    let mut docs = Vec::new();
    let entries = std::fs::read_dir(docs_dir()).expect("crates/tierstore/docs exists");
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        if path.extension().is_some_and(|e| e == "md") {
            let name = path
                .file_name()
                .expect("a name")
                .to_string_lossy()
                .into_owned();
            let text = std::fs::read_to_string(&path).expect("a readable page");
            docs.push(Doc { name, text });
        }
    }
    docs.sort_by(|a, b| a.name.cmp(&b.name));
    docs
}

/// The non-test `.rs` files under `dir`, comment lines out.
fn source_of(dir: &Path, out: &mut String) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("src is readable")
        .map(|e| e.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            source_of(&path, out);
        } else if name.ends_with(".rs")
            && !name.ends_with("tests.rs")
            && !name.ends_with("testing.rs")
        {
            let text = std::fs::read_to_string(&path).expect("a readable source file");
            for line in text.lines().filter(|l| !l.trim_start().starts_with("//")) {
                out.push_str(&line.replace('\\', ""));
                out.push('\n');
            }
        }
    }
}

fn code() -> Code {
    let command = Cli::command();
    let flags = command
        .get_arguments()
        .filter_map(|arg| arg.get_long().map(|long| format!("--{long}")))
        .collect();
    let keys = KEYS
        .iter()
        .flat_map(|(_, keys)| keys.iter().map(|key| (*key).to_owned()))
        .collect();
    let mut source = String::new();
    source_of(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut source,
    );
    Code {
        flags,
        keys,
        source,
    }
}

/// The pages with `change` made to the one named `name`; the text it replaces must be in it.
fn changed(name: &str, from: &str, to: &str) -> Vec<Doc> {
    let mut docs = read_docs();
    let doc = docs
        .iter_mut()
        .find(|d| d.name == name)
        .expect("the page exists");
    assert!(doc.text.contains(from), "{name} has no {from:?}");
    doc.text = doc.text.replacen(from, to, 1);
    docs
}

/// The pages without the one named `name`.
fn without_page(name: &str) -> Vec<Doc> {
    let mut docs = read_docs();
    let before = docs.len();
    docs.retain(|d| d.name != name);
    assert_eq!(docs.len() + 1, before, "the page {name} exists");
    docs
}

/// The pages with every `from` taken out.
fn without(from: &str) -> Vec<Doc> {
    let mut docs = read_docs();
    assert!(
        docs.iter().any(|d| d.text.contains(from)),
        "no page has {from:?}"
    );
    for doc in &mut docs {
        doc.text = doc.text.replace(from, "");
    }
    docs
}

/// The pages with `extra` added at the end of the one named `name`, under a heading of its own
/// (the page may end in a `Not in a release yet` section, which the checks skip).
fn with_appended(name: &str, extra: &str) -> Vec<Doc> {
    let mut docs = read_docs();
    let doc = docs
        .iter_mut()
        .find(|d| d.name == name)
        .expect("the page exists");
    doc.text.push_str("\n## Appended\n\n");
    doc.text.push_str(extra);
    docs
}

fn said(problems: &[String], needle: &str) -> bool {
    problems.iter().any(|p| p.contains(needle))
}

/// Asserts that the pages of `docs/` hold no flag, key, TOML file, metric, link or quoted message
/// that the code does not have, and leave no flag or key of the code unnamed. Catches a page that
/// was not updated when a flag was renamed, a key added or a message reworded.
#[test]
fn the_pages_say_what_the_code_has() {
    let docs = read_docs();
    assert!(
        docs.iter().any(|d| d.name == "configuration.md"),
        "the guide's pages are in docs/"
    );
    assert_eq!(check(&docs, &code()), Vec::<String>::new());
}

/// Asserts that a flag that is not the binary's is found, in a span and in a block.
#[test]
fn a_flag_that_is_not_the_binarys_is_found() {
    let wrong = changed("configuration.md", "`--listen`", "`--listn`");
    let problems = check(&wrong, &code());
    assert!(said(&problems, "--listn is not a flag"), "{problems:?}");
    // `--listen` is named in a span and in a block of this page, and in no other page.
    let problems = check(&without_page("configuration.md"), &code());
    assert!(
        said(&problems, "no page names the flag --listen"),
        "{problems:?}"
    );
    let block = with_appended("operations.md", "\n```sh\ntierstore --no-such-flag\n```\n");
    assert!(said(
        &check(&block, &code()),
        "--no-such-flag is not a flag"
    ));
}

/// Asserts that a key that the table does not have is found, in a span and in a TOML block, and
/// that a key the pages leave out is found.
#[test]
fn a_key_that_is_not_in_the_table_is_found() {
    let span = changed("configuration.md", "`server.listen`", "`server.listn`");
    let problems = check(&span, &code());
    assert!(said(&problems, "server.listn is not a key"), "{problems:?}");
    // `server.listen` is named in the table and in the TOML blocks of this page, and in no other.
    let problems = check(&without_page("configuration.md"), &code());
    assert!(
        said(&problems, "no page names the key server.listen"),
        "{problems:?}"
    );
    let block = with_appended(
        "configuration.md",
        "\n```toml fragment\n[hot_store]\nbudgett = 1\n```\n",
    );
    assert!(said(
        &check(&block, &code()),
        "hot_store.budgett is not a key"
    ));
}

/// Asserts that a flag and a key of the code that no page names are found.
#[test]
fn a_flag_or_key_no_page_names_is_found() {
    let problems = check(&without("--oci-prefix"), &code());
    assert!(
        said(&problems, "no page names the flag --oci-prefix"),
        "{problems:?}"
    );
    let problems = check(&without("`server.presign_max_expires`"), &code());
    assert!(
        said(
            &problems,
            "no page names the key server.presign_max_expires"
        ),
        "{problems:?}"
    );
}

/// Asserts that a TOML block that the loader refuses is found, and that a block marked as a
/// fragment is not loaded (a fragment is not a whole file).
#[test]
fn a_toml_file_that_does_not_load_is_found() {
    let file = "\n```toml\n[server]\nlisten = \"192.0.2.10:9000\"\n```\n";
    let problems = check(&with_appended("configuration.md", file), &code());
    assert!(
        said(&problems, "the loader refuses the file"),
        "{problems:?}"
    );
    let fragment = file.replace("```toml\n", "```toml fragment\n");
    let problems = check(&with_appended("configuration.md", &fragment), &code());
    assert!(!said(&problems, "the loader refuses"), "{problems:?}");
}

/// Asserts that a metric name the source does not have is found.
#[test]
fn a_metric_that_is_not_exported_is_found() {
    let extra = "\nThe counter `tierstore_no_such_total` counts nothing.\n";
    let problems = check(&with_appended("operations.md", extra), &code());
    assert!(
        said(&problems, "tierstore_no_such_total is not in the source"),
        "{problems:?}"
    );
}

/// Asserts that a quoted message the source does not hold is found, and that one it holds is not.
#[test]
fn a_quoted_message_that_is_not_in_the_source_is_found() {
    let wrong = "\n```message\ntierstore: this text is nowhere in the program\n```\n";
    let problems = check(&with_appended("troubleshooting.md", wrong), &code());
    assert!(said(&problems, "is not in the source"), "{problems:?}");
    let right = "\n```message\n--low-water must be below --high-water\n```\n";
    let problems = check(&with_appended("troubleshooting.md", right), &code());
    assert!(!said(&problems, "the message"), "{problems:?}");
}

/// Asserts that a link to a page or a heading that is not there is found, and that a link to a
/// page another change adds is let stand.
#[test]
fn a_link_that_names_nothing_is_found() {
    let bad_page = with_appended("overview.md", "\nSee [x](nope.md).\n");
    assert!(said(&check(&bad_page, &code()), "nope.md names no page"));
    let bad_anchor = with_appended("overview.md", "\nSee [x](configuration.md#nope).\n");
    assert!(said(
        &check(&bad_anchor, &code()),
        "names no heading of configuration.md"
    ));
    let pending = with_appended("overview.md", "\nSee [x](metrics.md).\n");
    assert!(!said(&check(&pending, &code()), "metrics.md names no page"));
}

/// Asserts that a `Not in a release yet` section is left out of the checks to its end, that the
/// next heading of the same level is checked again, and that a deeper heading stays inside it.
#[test]
fn a_not_in_a_release_yet_section_is_skipped_to_its_end() {
    let wrong = "`--not-built-yet`, `server.not_built`, `tierstore_not_built_total`";
    let inside = format!("\n## {NOT_YET}\n\n{wrong}\n\n### Deeper\n\n{wrong}\n");
    assert_eq!(
        check(&with_appended("overview.md", &inside), &code()),
        Vec::<String>::new()
    );
    let after = format!("{inside}\n## Checked again\n\n{wrong}\n");
    let problems = check(&with_appended("overview.md", &after), &code());
    assert!(
        said(&problems, "--not-built-yet is not a flag"),
        "{problems:?}"
    );
    assert!(
        said(&problems, "server.not_built is not a key"),
        "{problems:?}"
    );
    assert!(
        said(&problems, "tierstore_not_built_total is not in the source"),
        "{problems:?}"
    );
    let other_title = with_appended(
        "overview.md",
        &format!("\n## Not in a release\n\n{wrong}\n"),
    );
    assert!(said(&check(&other_title, &code()), "--not-built-yet"));
}

/// Asserts that the word scanners read what they are meant to: a flag after a space, a quote or
/// `(`, not the `--` of prose or the middle of a word, and a metric stopped by `{`.
#[test]
fn the_scanners_read_flags_and_metrics() {
    assert_eq!(
        flags_in("run --a-b=1 (--c) x--d \"--e\" -- f"),
        ["--a-b", "--c", "--e"]
    );
    assert_eq!(
        metrics_in("tierstore_x_total{a} and tierstore_* and tierstore_y."),
        ["tierstore_x_total", "tierstore_y"]
    );
    assert_eq!(
        slug("`[hot_store]`: The Start-up"),
        "hot_store-the-start-up"
    );
}
