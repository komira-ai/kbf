//! The interpreter budget: the shell, Python, awk, sed, Perl and Ruby this repository
//! still holds, each listed by path in [`BUDGET`]. The list says what exists, not how
//! big it is: editing a listed thing never touches the list.
//!
//! The scan finds three kinds of thing, one section of the budget file each:
//! - `[files]`: a file whose extension is one of [`EXTENSIONS`], or whose first line is
//!   a `#!` line naming one of the interpreters [`is_interpreter`] knows, directly or
//!   through `env` or `busybox`. Its key is its path.
//! - `[workflow]`: a workflow or composite-action step whose `run:` script is shell
//!   logic ([`shell_logic`]: more than one line, or a pipe, `&&`, `;`, `$(`, a
//!   backtick, `if`, `for` or `while`). Its key is `<file>#<job>/<step>`. A one-line
//!   command passes without a row.
//! - `[src-sh-c]`: a production Rust file under `crates/*/src/` that names a shell
//!   with a `-c` flag (module [`rust`] says how it reads the file). Its key is its path.
//!
//! [`check`] refuses a thing the scan found with no row, a row the scan found nothing
//! for (a deleted or renamed file takes its row out in the same change), and
//! a malformed or duplicate row. Rows are `<key>\t<reason>`, under a `[files]`,
//! `[workflow]` or `[src-sh-c]` header; a line starting with `#` and a blank line are
//! skipped.

pub mod rust;

use std::collections::BTreeMap;

/// The budget file, relative to the repository root.
pub const BUDGET: &str = "tools/interpreter_budget.tsv";

/// File extensions (compared ignoring ASCII case) that make a file an interpreter
/// file whatever its first line says.
pub const EXTENSIONS: &[&str] = &["sh", "bash", "zsh", "ksh", "py", "awk", "sed", "pl", "rb"];

/// The interpreters a `#!` line may name (besides any `python*`).
const INTERPRETERS: &[&str] = &[
    "sh", "bash", "ash", "dash", "ksh", "zsh", "awk", "gawk", "mawk", "nawk", "sed", "perl", "ruby",
];

/// One section of the budget file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Files,
    Workflow,
    SrcShC,
}

impl Section {
    const ALL: [Section; 3] = [Section::Files, Section::Workflow, Section::SrcShC];

    pub fn header(self) -> &'static str {
        match self {
            Section::Files => "[files]",
            Section::Workflow => "[workflow]",
            Section::SrcShC => "[src-sh-c]",
        }
    }
}

/// One row of the budget file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub section: Section,
    pub key: String,
    /// The 1-based line of the budget file that holds the row.
    pub line: usize,
}

/// The rows of a budget text, and one problem per line that is not a well-formed row,
/// header, comment or blank line, or that repeats a key of its section.
pub fn parse(text: &str) -> (Vec<Row>, Vec<String>) {
    let mut rows: Vec<Row> = Vec::new();
    let mut problems = Vec::new();
    let mut section = None;
    let mut seen_headers = Vec::new();
    let mut first_at: BTreeMap<(Section, String), usize> = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        let n = at + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            match Section::ALL.into_iter().find(|s| s.header() == line) {
                Some(s) if seen_headers.contains(&s) => {
                    problems.push(format!("line {n}: the section {line} appears twice"));
                }
                Some(s) => {
                    seen_headers.push(s);
                    section = Some(s);
                }
                None => problems.push(format!("line {n}: unknown section `{line}`")),
            }
            continue;
        }
        let Some(s) = section else {
            problems.push(format!("line {n}: a row before any section header"));
            continue;
        };
        let fields: Vec<&str> = line.split('\t').collect();
        let [key, reason] = fields[..] else {
            problems.push(format!(
                "line {n}: a row needs two tab-separated fields (<key>, <reason>), found {}",
                fields.len()
            ));
            continue;
        };
        if [key, reason].iter().any(|f| f.is_empty() || f.trim() != *f) {
            problems.push(format!(
                "line {n}: a field is empty or has blanks around it"
            ));
            continue;
        }
        match first_at.get(&(s, key.to_owned())) {
            Some(first) => problems.push(format!(
                "line {n}: a second {} row for `{key}` (the first is line {first})",
                s.header()
            )),
            None => {
                first_at.insert((s, key.to_owned()), n);
                rows.push(Row {
                    section: s,
                    key: key.to_owned(),
                    line: n,
                });
            }
        }
    }
    (rows, problems)
}

/// One thing the scan found: its section and key, and why the scan found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub section: Section,
    pub key: String,
    pub why: String,
}

/// The problems of a budget text against what the scan found. `found` holds each key
/// of a section once. Only existence is checked: a listed thing may change in any way.
pub fn check(budget: &str, found: &[Found]) -> Vec<String> {
    let (rows, mut problems) = parse(budget);
    let mut by_key: BTreeMap<(Section, &str), &Row> = BTreeMap::new();
    for row in &rows {
        by_key.insert((row.section, row.key.as_str()), row);
    }
    let mut matched = Vec::new();
    for f in found {
        let s = f.section;
        match by_key.get(&(s, f.key.as_str())) {
            None => problems.push(format!(
                "`{}`: {}, and {BUDGET} has no {} row for it; port it to Rust, or add \
                 the row `{}\\t<reason>`",
                f.key,
                f.why,
                s.header(),
                f.key
            )),
            Some(row) => matched.push(row.line),
        }
    }
    for row in rows.iter().filter(|r| !matched.contains(&r.line)) {
        problems.push(format!(
            "line {}: the {} row for `{}` names nothing the scan found (deleted, \
             renamed, or no longer counted); remove the row",
            row.line,
            row.section.header(),
            row.key
        ));
    }
    problems
}

/// Why the file at `path` with these bytes is an interpreter file, or `None`.
pub fn interpreter_file(path: &str, bytes: &[u8]) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if let Some((_, ext)) = name.rsplit_once('.')
        && EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext))
    {
        return Some(format!("a `.{ext}` file"));
    }
    shebang_interpreter(bytes)
        .filter(|i| is_interpreter(i))
        .map(|i| format!("its `#!` line runs `{i}`"))
}

/// Whether a `#!` line naming the program `name` (a base name) runs an interpreter.
pub fn is_interpreter(name: &str) -> bool {
    INTERPRETERS.contains(&name) || name.starts_with("python")
}

/// The base name of the program a `#!` first line runs, looking through `env` (its
/// options and `NAME=value` words) and `busybox` (its applet). `None` when the first
/// line is not a `#!` line or names nothing. (A Rust `#![...]` inner attribute reads
/// as the program `[...]`, which is no interpreter.)
pub fn shebang_interpreter(bytes: &[u8]) -> Option<&str> {
    let first = bytes.split(|&b| b == b'\n').next()?;
    let rest = first.strip_prefix(b"#!")?;
    let rest = std::str::from_utf8(rest).ok()?;
    let mut words = rest.split_ascii_whitespace();
    let mut program = words.next()?;
    loop {
        let name = program.rsplit('/').next().unwrap_or(program);
        match name {
            "env" => {
                program = loop {
                    let w = words.next()?;
                    if matches!(w, "-u" | "-C" | "--unset" | "--chdir") {
                        words.next();
                    } else if !(w.starts_with('-') || w.contains('=')) {
                        break w;
                    }
                };
            }
            "busybox" => program = words.next()?,
            _ => return Some(name),
        }
    }
}

/// The number of lines of a `run:` script, blank lines at either end left out.
pub fn script_lines(script: &str) -> usize {
    script.trim().lines().count()
}

/// What makes a `run:` script shell logic rather than one command, or `None` for a
/// one-line command.
pub fn shell_logic(script: &str) -> Option<&'static str> {
    if script_lines(script) > 1 {
        return Some("more than one line");
    }
    for (token, what) in [
        ("|", "a pipe or `||`"),
        ("&&", "`&&`"),
        (";", "`;`"),
        ("$(", "`$(`"),
        ("`", "a backtick"),
    ] {
        if script.contains(token) {
            return Some(what);
        }
    }
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/');
    script.split(|c: char| !word(c)).find_map(|w| match w {
        "if" => Some("`if`"),
        "for" => Some("`for`"),
        "while" => Some("`while`"),
        _ => None,
    })
}

#[cfg(test)]
mod tests;
