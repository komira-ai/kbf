//! The systemd units in `docs/deploy/*.md`, read so tests can check them against the
//! code: `tests/deploy_units.rs` runs `kbf-server`'s flag parser over each server
//! unit's `ExecStart=`, and `kbf-node`'s tests run `kbf-daemon`'s over each daemon
//! unit's. A doc whose example names a flag the binary does not have, or gives one a
//! value it refuses, then fails a test instead of a deployment.
//!
//! What counts as a unit: a fenced block with the info string `ini` whose nearest
//! non-blank line above is `` `/etc/systemd/system/<file>`: ``. An `ini` block without
//! that line is an error, so an example cannot escape the check by losing its name.
//!
//! The examples write host-specific values as placeholders (`<node id>`); [`argv`]
//! replaces each with a sample value from [`PLACEHOLDERS`], and a placeholder missing
//! from that table is an error rather than a word passed through.

use std::fmt;

/// Sample values for the placeholders the deploy docs use in an `ExecStart=`.
pub const PLACEHOLDERS: &[(&str, &str)] = &[
    ("<server>", "storage-a.example"),
    ("<worker port>", "8981"),
    ("<node id>", "node-1"),
    ("<RAM in GiB minus 4 to 8>", "248"),
    ("<storage host address>", "192.0.2.10"),
    ("<storage host name>", "storage-a.example"),
];

/// One example unit file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// The path the doc names, such as `/etc/systemd/system/kbf-server.service`.
    pub path: String,
    /// The `## ` heading the unit sits under (`""` before the first).
    pub section: String,
    /// `Key=value` lines in order, `\` continuations joined, comments and section
    /// headers left out.
    pub directives: Vec<(String, String)>,
}

impl Unit {
    /// The value of the last `key=` line, as systemd reads a key given once.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.directives
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The `ExecStart=` command line split at whitespace, with its placeholders
    /// replaced ([`argv`]); `None` for a unit that starts nothing.
    pub fn exec_start(&self) -> Option<Result<Vec<String>, UnitError>> {
        self.get("ExecStart").map(argv)
    }
}

/// Why a doc's units cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitError {
    /// An `ini` block (at this 1-based line) without the path line above it.
    Unnamed(usize),
    /// An `ini` block (at this line) that never closes.
    Unclosed(usize),
    /// A placeholder [`PLACEHOLDERS`] does not have.
    Placeholder(String),
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitError::Unnamed(line) => write!(
                f,
                "line {line}: an ini block needs \"`/etc/systemd/system/<file>`:\" on the line above"
            ),
            UnitError::Unclosed(line) => write!(f, "line {line}: the ini block never closes"),
            UnitError::Placeholder(p) => {
                write!(f, "placeholder {p} has no sample value in PLACEHOLDERS")
            }
        }
    }
}

impl std::error::Error for UnitError {}

/// The units in `markdown`, in order.
pub fn units(markdown: &str) -> Result<Vec<Unit>, UnitError> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out = Vec::new();
    let mut section = "";
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(h) = line.strip_prefix("## ") {
            section = h.trim();
        }
        if line.trim() != "```ini" {
            i += 1;
            continue;
        }
        let path = unit_path(&lines[..i]).ok_or(UnitError::Unnamed(i + 1))?;
        let len = lines[i + 1..]
            .iter()
            .position(|l| l.trim() == "```")
            .ok_or(UnitError::Unclosed(i + 1))?;
        out.push(Unit {
            path: path.to_owned(),
            section: section.to_owned(),
            directives: directives(&lines[i + 1..i + 1 + len]),
        });
        i += len + 2;
    }
    Ok(out)
}

/// The path on the nearest non-blank line of `above`, if it is `` `<path>`: ``.
fn unit_path<'a>(above: &[&'a str]) -> Option<&'a str> {
    let line = above.iter().rev().find(|l| !l.trim().is_empty())?.trim();
    line.strip_prefix("`/etc/systemd/system/")
        .and_then(|rest| rest.strip_suffix("`:"))
        .map(|_| &line[1..line.len() - 2])
}

/// The `Key=value` lines of a unit's body.
fn directives(body: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pending = String::new();
    for line in body.iter().map(|l| l.trim()) {
        let skip = pending.is_empty() && (line.starts_with('#') || line.starts_with('['));
        if skip || line.is_empty() {
            continue;
        }
        match line.strip_suffix('\\') {
            Some(head) => pending.push_str(head),
            None => {
                pending.push_str(line);
                let (k, v) = pending.split_once('=').unwrap_or((pending.as_str(), ""));
                out.push((k.to_owned(), v.to_owned()));
                pending.clear();
            }
        }
    }
    out
}

/// A command line with each `<placeholder>` replaced with its sample value, then split
/// at whitespace (a placeholder may hold spaces; no sample value does).
pub fn argv(command: &str) -> Result<Vec<String>, UnitError> {
    let line = substitute(command)?;
    Ok(line.split_whitespace().map(str::to_owned).collect())
}

/// `text` with each `<...>` in it replaced from [`PLACEHOLDERS`].
fn substitute(text: &str) -> Result<String, UnitError> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let close = rest[open..].find('>').map_or(rest.len(), |c| open + c + 1);
        let name = &rest[open..close];
        let value = PLACEHOLDERS
            .iter()
            .find(|(p, _)| *p == name)
            .ok_or_else(|| UnitError::Placeholder(name.to_owned()))?
            .1;
        out.push_str(&rest[..open]);
        out.push_str(value);
        rest = &rest[close..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The lines of every fenced block whose info string is `info` exactly.
pub fn fenced<'a>(markdown: &'a str, info: &str) -> Vec<Vec<&'a str>> {
    let mut out = Vec::new();
    let mut lines = markdown.lines();
    while let Some(line) = lines.next() {
        if line.trim() == format!("```{info}") {
            out.push(lines.by_ref().take_while(|l| l.trim() != "```").collect());
        }
    }
    out
}

/// The cells of the first pipe table after the line `heading`, header and separator
/// rows left out; empty when there is none.
pub fn table_after(markdown: &str, heading: &str) -> Vec<Vec<String>> {
    markdown
        .lines()
        .skip_while(|l| l.trim() != heading)
        .skip_while(|l| !l.trim_start().starts_with('|'))
        .take_while(|l| l.trim_start().starts_with('|'))
        .skip(2)
        .map(|row| {
            let row = row.trim().trim_start_matches('|').trim_end_matches('|');
            row.split('|').map(|c| c.trim().to_owned()).collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
# Title

`/etc/systemd/system/a.slice`:

```ini
[Slice]
MemoryMin=1G
```

## Second

`/etc/systemd/system/b.service`:

```ini
[Service]
# a comment
Environment=X=1
MemoryMin=2G
MemoryMin=3G
ExecStart=/usr/local/bin/b \\
    --node-id=<node id> \\
    --server=https://<server>:<worker port>
```
";

    #[test]
    fn units_are_named_split_into_directives_and_placed_under_their_section() {
        // Catches: a unit read without its path or section, a comment or `[Service]`
        // header taken as a directive, a continuation not joined, `Environment=X=1`
        // split at the wrong `=`.
        let units = units(DOC).expect("units");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].path, "/etc/systemd/system/a.slice");
        assert_eq!(units[0].section, "");
        assert_eq!(units[0].directives, [("MemoryMin".into(), "1G".into())]);
        let b = &units[1];
        assert_eq!(b.section, "Second");
        assert_eq!(b.get("Environment"), Some("X=1"));
        assert_eq!(b.get("Nothing"), None);
        assert_eq!(b.directives.len(), 4);
        assert_eq!(units[0].exec_start(), None);
    }

    #[test]
    fn the_last_line_for_a_key_wins() {
        // Catches: `get` reading the first `MemoryMin=` where systemd reads the last.
        assert_eq!(units(DOC).expect("units")[1].get("MemoryMin"), Some("3G"));
    }

    #[test]
    fn exec_start_is_split_with_placeholders_replaced() {
        // Catches: a placeholder passed through, or two in one word not both replaced.
        let argv = units(DOC).expect("units")[1]
            .exec_start()
            .expect("ExecStart");
        assert_eq!(
            argv.expect("argv"),
            [
                "/usr/local/bin/b",
                "--node-id=node-1",
                "--server=https://storage-a.example:8981"
            ]
        );
    }

    #[test]
    fn an_unknown_placeholder_is_an_error() {
        // Catches: a placeholder with no sample value reaching the flag parser as text
        // (or a `<` with no `>` being dropped).
        assert_eq!(
            argv("--a=<what>"),
            Err(UnitError::Placeholder("<what>".into()))
        );
        assert_eq!(
            argv("--a=<open"),
            Err(UnitError::Placeholder("<open".into()))
        );
        let e = UnitError::Placeholder("<what>".into()).to_string();
        assert!(e.contains("<what>"), "{e}");
    }

    #[test]
    fn an_unnamed_or_unclosed_ini_block_is_an_error() {
        // Catches: an example that loses its path line, or its closing fence, and so
        // is never checked.
        let unnamed = "Some text:\n\n```ini\nA=1\n```\n";
        assert_eq!(units(unnamed), Err(UnitError::Unnamed(3)));
        assert_eq!(units("```ini\n```\n"), Err(UnitError::Unnamed(1)));
        let unclosed = "`/etc/systemd/system/a.service`:\n```ini\nA=1\n";
        assert_eq!(units(unclosed), Err(UnitError::Unclosed(2)));
        assert!(UnitError::Unnamed(3).to_string().contains("line 3"));
        assert!(UnitError::Unclosed(2).to_string().contains("never closes"));
    }

    #[test]
    fn fenced_blocks_are_picked_by_their_exact_info_string() {
        // Catches: a block picked by the first word of its info string only.
        let doc = "```text a\n--x\n--y\n```\n```text\n--z\n```\n";
        assert_eq!(fenced(doc, "text a"), [vec!["--x", "--y"]]);
        assert_eq!(fenced(doc, "text"), [vec!["--z"]]);
    }

    #[test]
    fn the_table_after_a_heading_is_read_without_its_header() {
        // Catches: the header or separator row read as data, or the first table in the
        // doc read instead of the one after the heading.
        let doc = "| x | y |\n|---|---|\n| 0 | 0 |\n### H\n\ntext\n| a | b |\n|---|---|\n\
                   | one | 1 |\n| two | 2 |\nafter\n| z | 9 |\n";
        assert_eq!(
            table_after(doc, "### H"),
            [vec!["one", "1"], vec!["two", "2"]]
        );
        assert!(table_after(doc, "### None").is_empty());
    }
}
