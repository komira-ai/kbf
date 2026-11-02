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
//! Each unit body is read with `kbf_unitfile`, the parser the package test
//! (`tests/package_units.rs`) reads the packaged units with, so the docs and the
//! units agree on what a unit says.
//!
//! The examples write host-specific values as placeholders (`<node id>`); [`argv`]
//! replaces each with a sample value from [`PLACEHOLDERS`], and a placeholder missing
//! from that table is an error rather than a word passed through.

use std::fmt;

use kbf_unitfile::UnitFile;

/// Sample values for the placeholders the deploy docs use in an `ExecStart=`.
pub const PLACEHOLDERS: &[(&str, &str)] = &[
    ("<server>", "storage-a.example"),
    ("<worker port>", "8981"),
    ("<node id>", "node-1"),
    ("<RAM in GiB minus 4 to 8>", "248"),
    ("<storage host address>", "192.0.2.10"),
    ("<storage host name>", "storage-a.example"),
    ("<node>", "node-1"),
    ("<member id>", "1"),
    ("<connection>", "cold"),
    ("<new endpoint URL>", "http://store-b.example"),
    ("<generation>", "7"),
];

/// Sample values for the shell variables the held script in a deploy doc passes to
/// `kbf cluster` (`"$NAME"` or `$NAME`); a variable missing from this table is an
/// error, as a placeholder is.
pub const SHELL_VARIABLES: &[(&str, &str)] = &[
    ("API", "127.0.0.1:8982"),
    ("TOKEN_FILE", "/etc/kbf/api-token"),
    ("CONNECTION", "cold"),
    ("ENDPOINT", "http://store-b.example"),
    ("EXPECT_GENERATION", "7"),
    ("FOR_SECS", "900"),
    ("node_id", "node-1"),
    ("member_id", "1"),
];

/// One example unit file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// The path the doc names, such as `/etc/systemd/system/kbf-server.service`.
    pub path: String,
    /// The `## ` heading the unit sits under (`""` before the first).
    pub section: String,
    /// The unit file in the block, read by `kbf_unitfile`.
    pub file: UnitFile,
}

impl Unit {
    /// The value of the last `key=` line in any section, as systemd reads a key given
    /// once.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.file.get(key)
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
    /// A shell variable [`SHELL_VARIABLES`] does not have, in a `kbf cluster` command.
    Variable(String),
    /// A unit body (its `ini` block at this line) that is not a unit file.
    Body(usize, kbf_unitfile::UnitError),
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
            UnitError::Variable(v) => {
                write!(
                    f,
                    "shell variable ${v} has no sample value in SHELL_VARIABLES"
                )
            }
            UnitError::Body(line, e) => write!(f, "the ini block at line {line}: {e}"),
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
        let body = lines[i + 1..i + 1 + len].join("\n");
        let file = UnitFile::parse(&body).map_err(|e| UnitError::Body(i + 1, e))?;
        out.push(Unit {
            path: path.to_owned(),
            section: section.to_owned(),
            file,
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

/// One `kbf cluster ...` command line of a doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterCommand {
    /// The 1-based line of the doc it starts on.
    pub line: usize,
    /// Its words, the first `kbf` and the second `cluster`, with placeholders and shell
    /// variables replaced.
    pub argv: Vec<String>,
}

/// Every `kbf cluster ...` command in the fenced blocks of `markdown`, in order: the text
/// from `kbf cluster` (at the start of a word or after a `/`, as in `/usr/bin/kbf`) to the
/// end of its line, a line ending in `\` joined with the next. Its `<placeholder>`s are
/// replaced from [`PLACEHOLDERS`] first; then it is cut at the first `)`, `|`, `;`, `&`,
/// `>`, `<` or backtick, so a command inside `$(...)` or before a pipe or a redirection is
/// read alone; then `$NAME` is replaced from [`SHELL_VARIABLES`] and double quotes are
/// dropped. A comment line (`#` first) is skipped.
pub fn cluster_commands(markdown: &str) -> Result<Vec<ClusterCommand>, UnitError> {
    let mut out = Vec::new();
    let mut fenced = false;
    let mut joined = String::new();
    let mut start = 0;
    for (i, line) in markdown.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            joined.clear();
            continue;
        }
        if !fenced || line.trim_start().starts_with('#') {
            continue;
        }
        if joined.is_empty() {
            start = i + 1;
        }
        match line.trim_end().strip_suffix('\\') {
            Some(head) => {
                joined.push_str(head);
                joined.push(' ');
                continue;
            }
            None => joined.push_str(line),
        }
        let logical = std::mem::take(&mut joined);
        for (at, _) in logical.match_indices("kbf cluster") {
            let before = logical[..at].chars().next_back();
            let after = logical[at + "kbf cluster".len()..].chars().next();
            if before.is_some_and(|c| !c.is_whitespace() && c != '/' && c != '(')
                || after.is_some_and(|c| !c.is_whitespace())
            {
                continue;
            }
            // Placeholders first: their own `<` and `>` are not a redirection.
            let rest = substitute(&logical[at..])?;
            let end = rest
                .find([')', '|', ';', '&', '>', '<', '`'])
                .unwrap_or(rest.len());
            let text = variables(&rest[..end])?;
            out.push(ClusterCommand {
                line: start,
                argv: text.split_whitespace().map(str::to_owned).collect(),
            });
        }
    }
    Ok(out)
}

/// `text` with each `$NAME` replaced from [`SHELL_VARIABLES`] and double quotes dropped.
fn variables(text: &str) -> Result<String, UnitError> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        let name_len = rest[dollar + 1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len() - dollar - 1);
        let name = &rest[dollar + 1..dollar + 1 + name_len];
        let value = SHELL_VARIABLES
            .iter()
            .find(|(v, _)| *v == name)
            .ok_or_else(|| UnitError::Variable(name.to_owned()))?
            .1;
        out.push_str(&rest[..dollar]);
        out.push_str(value);
        rest = &rest[dollar + 1 + name_len..];
    }
    out.push_str(rest);
    Ok(out.replace('"', ""))
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
        // Catches: a unit read without its path or section, or its body not handed whole
        // to the unit parser (`kbf_unitfile` tests the parsing itself).
        let units = units(DOC).expect("units");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].path, "/etc/systemd/system/a.slice");
        assert_eq!(units[0].section, "");
        assert_eq!(units[0].get("MemoryMin"), Some("1G"));
        assert_eq!(units[0].file.directives().len(), 1);
        let b = &units[1];
        assert_eq!(b.section, "Second");
        assert_eq!(b.get("Environment"), Some("X=1"));
        assert_eq!(b.get("Nothing"), None);
        assert_eq!(b.file.directives().len(), 4);
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
        // A body the unit parser refuses is an error naming the block, not a unit with
        // its garbled line dropped.
        let garbled = "`/etc/systemd/system/a.service`:\n```ini\n[Service]\nMemoryMin 1G\n```\n";
        let e = units(garbled).err().map(|e| e.to_string());
        assert_eq!(
            e.as_deref(),
            Some("the ini block at line 2: line 2: not [Section], a comment or Key=value")
        );
    }

    fn argvs(doc: &str) -> Vec<(usize, String)> {
        cluster_commands(doc)
            .expect("commands")
            .into_iter()
            .map(|c| (c.line, c.argv.join(" ")))
            .collect()
    }

    #[test]
    fn cluster_commands_are_read_from_fenced_blocks_only_with_values_replaced() {
        // Catches: a command in prose read as a command line, a placeholder or a shell
        // variable passed through, a continued line read as two commands, a command
        // after a path prefix missed, a comment line read, a block's state carried past
        // its closing fence.
        let doc = "\
Run `kbf cluster status` first.

```text
kbf cluster drain <node> --token-file /etc/kbf/api-token
sudo /usr/bin/kbf cluster object-store repoint --connection <connection> \\
    --endpoint <new endpoint URL> --expect-generation <generation>
# kbf cluster not-a-command
```

```bash
run kbf cluster --api \"$API\" undrain \"$node_id\" --token-file \"$TOKEN_FILE\"
x=\"$(kbf cluster status | jq .nodes)\"; kbf cluster member $member_id --state active 2>&1
mykbf cluster status
kbf clusters
kbf cluster
```
";
        assert_eq!(
            argvs(doc),
            [
                (
                    4,
                    "kbf cluster drain node-1 --token-file /etc/kbf/api-token".to_owned()
                ),
                (
                    5,
                    "kbf cluster object-store repoint --connection cold --endpoint \
                     http://store-b.example --expect-generation 7"
                        .to_owned()
                ),
                (
                    11,
                    "kbf cluster --api 127.0.0.1:8982 undrain node-1 --token-file \
                     /etc/kbf/api-token"
                        .to_owned()
                ),
                (12, "kbf cluster status".to_owned()),
                (12, "kbf cluster member 1 --state active 2".to_owned()),
                (15, "kbf cluster".to_owned()),
            ]
        );
    }

    #[test]
    fn a_continued_line_left_open_by_its_fence_is_dropped() {
        // Catches: the half line before a closing fence glued to the next block's first
        // command.
        let doc = "```\nkbf cluster drain \\\n```\n```\nkbf cluster undrain n\n```\n";
        assert_eq!(argvs(doc), [(5, "kbf cluster undrain n".to_owned())]);
    }

    #[test]
    fn an_unknown_placeholder_or_shell_variable_in_a_command_is_an_error() {
        // Catches: a value with no sample reaching the parser as text, where it might
        // parse by luck and hide a wrong flag after it.
        assert_eq!(
            cluster_commands("```\nkbf cluster drain <who>\n```\n"),
            Err(UnitError::Placeholder("<who>".into()))
        );
        assert_eq!(
            cluster_commands("```\nkbf cluster drain \"$WHO\"\n```\n"),
            Err(UnitError::Variable("WHO".into()))
        );
        assert_eq!(
            cluster_commands("```\nkbf cluster drain $\n```\n"),
            Err(UnitError::Variable(String::new()))
        );
        assert_eq!(
            UnitError::Variable("WHO".into()).to_string(),
            "shell variable $WHO has no sample value in SHELL_VARIABLES"
        );
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
