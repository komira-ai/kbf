//! The interpreter budget: the shell, Python, awk, sed, Perl and Ruby this repository
//! still holds, each listed by path in [`BUDGET`]. The list says what exists, not how
//! big it is: editing a listed thing never touches the list.
//!
//! The scan finds three kinds of thing:
//! - a file whose extension is one of [`EXTENSIONS`], or whose first line is a `#!`
//!   line naming one of the interpreters [`is_interpreter`] knows, directly or through
//!   `env` or `busybox`. Its key is its path, listed under `[files]`.
//! - a workflow or composite-action step whose `run:` script is shell logic
//!   ([`shell_logic`]: more than one line, or a pipe, `&&`, `;`, `$(`, a backtick,
//!   `if`, `for` or `while`), named `<file>#<job>/<step>`. No row can list one: the scan
//!   refuses each, and [`check`] refuses any row under a `[workflow]` header, so its
//!   logic goes into Rust (a `cargo xtask` verb, or a flag of the program the step
//!   runs). A one-line command passes.
//! - a production Rust file under `crates/*/src/` that names a shell with a `-c` flag
//!   (module [`rust`] says how it reads the file). Its key is its path, listed under
//!   `[src-sh-c]`.
//!
//! [`check`] refuses a file or spawn the scan found with no row, a row the scan found
//! nothing for (a deleted or renamed file takes its row out in the same change), and
//! a malformed or duplicate row. Rows are `<key>\t<reason>`, under a `[files]` or
//! `[src-sh-c]` header; a line starting with `#` and a blank line are skipped.

pub mod rust;

use std::collections::BTreeMap;

// The file's format and parser are shared with `cargo xtask budget-diff`.
pub use xtask::budget::{BUDGET, Row, Section, parse};

/// File extensions (compared ignoring ASCII case) that make a file an interpreter
/// file whatever its first line says.
pub const EXTENSIONS: &[&str] = &["sh", "bash", "zsh", "ksh", "py", "awk", "sed", "pl", "rb"];

/// The interpreters a `#!` line may name (besides any `python*`).
const INTERPRETERS: &[&str] = &[
    "sh", "bash", "ash", "dash", "ksh", "zsh", "awk", "gawk", "mawk", "nawk", "sed", "perl", "ruby",
];

/// One thing the scan found: its section and key, and why the scan found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub section: Section,
    pub key: String,
    pub why: String,
}

/// The problems of a budget text against what the scan found. `found` holds each key
/// of a section once. Only existence is checked: a listed thing may change in any way.
/// The `[workflow]` section must stay empty: a row in it is refused whatever it names.
pub fn check(budget: &str, found: &[Found]) -> Vec<String> {
    let (rows, mut problems) = parse(budget);
    let (closed, rows): (Vec<Row>, Vec<Row>) = rows
        .into_iter()
        .partition(|r| r.section == Section::Workflow);
    for row in closed {
        problems.push(format!(
            "line {}: a [workflow] row for `{}`; the section stays empty, because no \
             `run:` step may hold shell logic: make the step one command and remove the row",
            row.line, row.key
        ));
    }
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
