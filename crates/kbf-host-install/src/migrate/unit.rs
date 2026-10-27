//! Reading the hand-placed unit of a node: the flags it gives `kbf`, the account it runs as and
//! the few settings the migration carries over.
//!
//! The reading is deliberately narrow. It understands what systemd does with the directives it
//! reads (a line ends with a backslash when it continues, `#` and `;` start a comment, an empty
//! assignment of a list empties it, the last assignment of a single value wins) and refuses
//! what it would have to guess: an `ExecStart=` with a prefix character, a quote, a variable,
//! a specifier or a shell operator is not one the migration carries, and the operator is told
//! which word stopped it.

/// What the migration reads of a hand-placed unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandPlaced {
    /// The program `ExecStart=` runs.
    pub program: String,
    /// Its arguments, the node's flags.
    pub args: Vec<String>,
    pub user: Option<String>,
    pub group: Option<String>,
    /// The settings carried as they are, in order: `SupplementaryGroups=`, `WorkingDirectory=`,
    /// `EnvironmentFile=` and `Environment=`.
    pub carried: Vec<(&'static str, String)>,
}

impl HandPlaced {
    /// The flags the packaged unit gives `/usr/bin/kbf` for a unit of the mode `mode`: the
    /// node's own, with the mode named by `--mode=` and by nothing else.
    ///
    /// `kbf` takes the mode as `--mode=<mode>` or, in its older spelling, as the first word
    /// after the program (`kbf storage-and-daemon --cold-store=...`), and refuses both
    /// together ("the mode is given twice") and the word anywhere but first ("unexpected
    /// argument"). A hand-placed unit may use the word, and the packaged unit names the mode with
    /// `--mode=`: a first word that is the mode is dropped, one that is another word is
    /// refused, as is a `--mode` of another mode. A unit that names no mode gets `--mode=`.
    ///
    /// # Errors
    /// What stops the move, in one line.
    pub fn flags(&self, mode: &str, unit: &str) -> Result<Vec<String>, String> {
        let mut flags = self.args.clone();
        if flags.first().is_some_and(|w| !w.starts_with('-')) {
            let word = flags.remove(0);
            if word != mode {
                return Err(format!(
                    "the unit runs the mode word {word:?}, which is not the mode of {unit}"
                ));
            }
        }
        let mut named = false;
        let mut words = flags.iter();
        while let Some(word) = words.next() {
            let given = match word.strip_prefix("--mode") {
                Some("") => words.next().map(String::as_str),
                Some(rest) if rest.starts_with('=') => rest.strip_prefix('='),
                _ => continue,
            };
            named = true;
            if given != Some(mode) {
                return Err(format!(
                    "the unit runs --mode={}, which is not the mode of {unit}",
                    given.unwrap_or_default()
                ));
            }
        }
        if !named {
            flags.insert(0, format!("--mode={mode}"));
        }
        Ok(flags)
    }
}

/// The directives carried as they are, besides the account.
const CARRIED: [&str; 4] = [
    "SupplementaryGroups",
    "WorkingDirectory",
    "EnvironmentFile",
    "Environment",
];

/// Whether `directive` takes a list (an empty assignment empties it).
fn is_list(directive: &str) -> bool {
    matches!(
        directive,
        "ExecStart" | "EnvironmentFile" | "Environment" | "SupplementaryGroups"
    )
}

/// The punctuation a word of a flag may have besides ASCII letters and digits, each safe in a
/// systemd `ExecStart=` word written without quotes:
///
/// - `_ . / : = @ + , ~ -` are in paths, addresses and `--flag=value` (`file:///var/lib/kbf/cold`,
///   `0.0.0.0:8981`); systemd reads none of them, and the prefix characters (`@ - : + !`) matter
///   only at the start of the whole command, which `parse` checks apart.
/// - `?` and `&` join the query of a cold-store URL
///   (`s3://bucket/prefix/?endpoint=http://127.0.0.1:9000&conditional-put=true`). systemd is not
///   a shell: neither starts a glob, a background job or a pipeline in an `ExecStart=` word.
///
/// Refused on purpose: whitespace (it splits words), quotes and `\` (systemd unquotes and
/// unescapes them), `$` (variable expansion), `%` (a specifier, written `%%` in a unit) and `;`
/// (a separator of commands).
const WORD_PUNCTUATION: &[u8] = b"_./:=@+,~-?&";

/// Whether `word` is one a drop-in can carry unchanged: enough for
/// `--cold-store=file:///var/lib/kbf/cold`, `--listen=0.0.0.0:8981` and `--node-id st1`, and
/// no character systemd would read for itself.
fn plain_word(word: &str) -> bool {
    word.bytes()
        .all(|b| b.is_ascii_alphanumeric() || WORD_PUNCTUATION.contains(&b))
}

/// Join the lines the file continues with a backslash into the lines systemd reads, dropping
/// comments and empty lines.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut open = String::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if open.is_empty() && (trimmed.is_empty() || trimmed.starts_with(['#', ';'])) {
            continue;
        }
        match trimmed.strip_suffix('\\') {
            Some(part) => {
                open.push_str(part);
                open.push(' ');
            }
            None => {
                open.push_str(trimmed);
                lines.push(std::mem::take(&mut open));
            }
        }
    }
    if !open.is_empty() {
        lines.push(open);
    }
    lines
}

/// Read the unit `text`; `releases` is the directory the program must be under (the
/// installer's `releases/`, which holds `current/kbf`).
///
/// # Errors
/// What stops the migration, in one line.
pub fn parse(text: &str, releases: &str) -> Result<HandPlaced, String> {
    let mut in_service = false;
    let mut exec: Vec<String> = Vec::new();
    let mut user = None;
    let mut group = None;
    let mut carried: Vec<(&'static str, String)> = Vec::new();
    for line in logical_lines(text) {
        if let Some(section) = line.strip_prefix('[') {
            in_service = section.trim_end_matches(']') == "Service";
            continue;
        }
        if !in_service {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "ExecStart" if value.is_empty() => exec.clear(),
            "ExecStart" => exec.push(value.to_owned()),
            "User" => user = (!value.is_empty()).then(|| value.to_owned()),
            "Group" => group = (!value.is_empty()).then(|| value.to_owned()),
            other => {
                if let Some(name) = CARRIED.iter().find(|c| **c == other) {
                    if is_list(name) && value.is_empty() {
                        carried.retain(|(k, _)| k != name);
                    } else if is_list(name) {
                        carried.push((name, value.to_owned()));
                    } else {
                        carried.retain(|(k, _)| k != name);
                        if !value.is_empty() {
                            carried.push((name, value.to_owned()));
                        }
                    }
                }
            }
        }
    }
    let [command] = exec.as_slice() else {
        return Err(format!(
            "the unit has {} ExecStart= lines, not one",
            exec.len()
        ));
    };
    if command.starts_with(['@', '-', ':', '+', '!']) {
        return Err("ExecStart= has a prefix character (@ - : + !)".to_owned());
    }
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or_default();
    let under_releases = program.strip_prefix(releases).is_some_and(|rest| {
        rest.ends_with("/kbf")
            && rest
                .strip_prefix('/')
                .is_some_and(|rest| rest.split('/').all(|part| !matches!(part, "" | "." | "..")))
    });
    if !under_releases {
        return Err(format!(
            "ExecStart= runs {program}, not the kbf of a release under {releases}"
        ));
    }
    let args: Vec<String> = words.map(str::to_owned).collect();
    if let Some(odd) = args.iter().find(|w| !plain_word(w)) {
        return Err(format!(
            "the flag {odd:?} has a character the migration does not carry (quotes, $, %, ; and the like)"
        ));
    }
    Ok(HandPlaced {
        program: program.to_owned(),
        args,
        user,
        group,
        carried,
    })
}

#[cfg(test)]
#[path = "unit_tests.rs"]
mod tests;
