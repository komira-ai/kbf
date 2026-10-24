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

/// The characters a word of a flag may have: enough for `--cold-store=file:///var/lib/kbf/cold`,
/// `--listen=0.0.0.0:8981` and `--node-id st1`, and no character a shell or systemd would
/// read for itself.
fn plain_word(word: &str) -> bool {
    word.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_./:=@+,~-".contains(&b))
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
