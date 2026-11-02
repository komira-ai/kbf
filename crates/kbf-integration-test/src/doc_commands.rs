//! The `kbf cluster ...` command lines in a doc's fenced blocks, read so
//! `tests/deploy_units.rs` can parse each one with `kbf`'s own parser
//! (`kbf_cli::parse_cluster`). A doc whose example or held script names a verb or a flag
//! `kbf cluster` does not have then fails a test instead of an operator.
//!
//! What is read: every fenced block (` ``` ` or `~~~`, any info string). A line ending in
//! `\` is joined with the next, and a line that holds the word `cluster` is split into
//! words as a shell does: single and double quotes, a backslash, `$NAME` and `${NAME}`
//! (from [`SHELL_VARIABLES`]), `<placeholder>` (from [`PLACEHOLDERS`]), and the control
//! operators (`|`, `||`, `&`, `&&`, `;`, `(`, `)`, `$(`) that end one command. A
//! redirection (`>file`, `2>&1`, `< file`) is taken out with its target, and the words
//! after it stay in the command, as the shell runs them. A command is the words from one
//! whose last path part is `kbf` followed by `cluster`; a `kbf cluster` inside a quoted
//! string (a line a script only prints) is not a command.
//!
//! What is refused, so that nothing escapes the check unseen: a placeholder or variable
//! with no sample value, a backtick, a quote or substitution left open, a continued line
//! with nothing after it, and a fence that never closes. A `<` followed by a letter is a
//! placeholder; write an input redirection with a space (`< file`).

use std::fmt;

use crate::deploy_units::PLACEHOLDERS;

/// Sample values for the shell variables a held script in a doc passes to `kbf cluster`.
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

/// One `kbf cluster ...` command of a doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterCommand {
    /// The 1-based line of the doc it starts on.
    pub line: usize,
    /// The info string of its fenced block (`text`, `shell`, or empty).
    pub info: String,
    /// Its words: `kbf`, `cluster`, then the rest, with values replaced and quotes removed.
    pub argv: Vec<String>,
}

/// Why a doc's commands cannot be read. Each names the 1-based line it is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// A `<placeholder>` [`PLACEHOLDERS`] does not have.
    Placeholder { line: usize, name: String },
    /// A shell variable [`SHELL_VARIABLES`] does not have (empty for a `$` followed by no
    /// name, such as `$1` or `$?`).
    Variable { line: usize, name: String },
    /// A line ending in `\` with no line after it in its block.
    Unfinished(usize),
    /// A quote, a `$(` or a `(` that the line does not close.
    Unclosed(usize),
    /// A fence that never closes.
    Fence(usize),
    /// Shell syntax this reader does not take apart.
    Unsupported { line: usize, what: &'static str },
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Placeholder { line, name } => write!(
                f,
                "line {line}: placeholder {name} has no sample value in PLACEHOLDERS"
            ),
            Self::Variable { line, name } => write!(
                f,
                "line {line}: shell variable ${name} has no sample value in SHELL_VARIABLES"
            ),
            Self::Unfinished(line) => write!(
                f,
                "line {line}: a command continued with `\\` has no next line in its block"
            ),
            Self::Unclosed(line) => write!(f, "line {line}: a quote or parenthesis never closes"),
            Self::Fence(line) => write!(f, "line {line}: the fenced block never closes"),
            Self::Unsupported { line, what } => write!(f, "line {line}: {what} is not read"),
        }
    }
}

impl std::error::Error for CommandError {}

/// Every `kbf cluster ...` command in the fenced blocks of `markdown`, in order.
pub fn cluster_commands(markdown: &str) -> Result<Vec<ClusterCommand>, CommandError> {
    let mut out = Vec::new();
    // The open fence: its line, its marker (three or more of one character) and its info.
    let mut fence: Option<(usize, String, String)> = None;
    let mut joined = String::new();
    let mut start = 0;
    for (i, line) in markdown.lines().enumerate() {
        let n = i + 1;
        let trimmed = line.trim();
        let Some((_, marker, info)) = &fence else {
            if let Some(marker) = fence_marker(trimmed) {
                let info = trimmed[marker.len()..].trim().to_owned();
                fence = Some((n, marker, info));
            }
            continue;
        };
        let closes = fence_marker(trimmed)
            .is_some_and(|m| m.len() == trimmed.len() && m.starts_with(marker.as_str()));
        if closes {
            if !joined.is_empty() {
                return Err(CommandError::Unfinished(start));
            }
            fence = None;
            continue;
        }
        if joined.is_empty() {
            if trimmed.starts_with('#') {
                continue;
            }
            start = n;
        }
        if let Some(head) = line.trim_end().strip_suffix('\\') {
            joined.push_str(head);
            joined.push(' ');
            continue;
        }
        joined.push_str(line);
        let logical = std::mem::take(&mut joined);
        if !logical.contains("cluster") {
            continue;
        }
        for argv in commands(&lex(&logical, start)?) {
            out.push(ClusterCommand {
                line: start,
                info: info.clone(),
                argv,
            });
        }
    }
    match fence {
        Some((line, _, _)) => Err(CommandError::Fence(line)),
        None => Ok(out),
    }
}

/// The marker of a fence line: three or more backticks or tildes at its start.
fn fence_marker(trimmed: &str) -> Option<String> {
    let first = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let marker: String = trimmed.chars().take_while(|c| *c == first).collect();
    (marker.len() >= 3).then_some(marker)
}

/// A piece of a lexed line.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    /// A word, its quotes removed and its values replaced.
    Word(String),
    /// A control operator: the command before it ends.
    Control,
    /// A redirection that takes the next word as its target.
    Redirect,
}

/// Where the lexer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Context {
    /// Unquoted, at the line's own level.
    Root,
    /// Unquoted, inside `(` or `$(`; `in_quotes` says whether `$(` was met inside `"`.
    Nested { in_quotes: bool },
    /// Inside double quotes.
    Quoted,
}

/// A word being built.
#[derive(Default)]
struct Word {
    text: String,
    /// Whether any of it was quoted or replaced, so an empty `""` is still a word and
    /// `"2">` is not a descriptor.
    quoted: bool,
    started: bool,
}

impl Word {
    fn push(&mut self, c: char) {
        self.text.push(c);
        self.started = true;
    }

    fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
        self.started = true;
        self.quoted = true;
    }

    fn flush(&mut self, out: &mut Vec<Token>) {
        if self.started {
            out.push(Token::Word(std::mem::take(&mut self.text)));
        }
        *self = Word::default();
    }
}

/// `line` (doc line `n`) split into tokens.
fn lex(line: &str, n: usize) -> Result<Vec<Token>, CommandError> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut stack = vec![Context::Root];
    let mut word = Word::default();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let context = *stack.last().unwrap_or(&Context::Root);
        if c == '`' {
            return Err(CommandError::Unsupported {
                line: n,
                what: "a backtick",
            });
        }
        if c == '$' {
            if next == Some('(') {
                word.flush(&mut out);
                out.push(Token::Control);
                stack.push(Context::Nested {
                    in_quotes: context == Context::Quoted,
                });
                i += 2;
                continue;
            }
            let (value, used) = variable(&chars[i + 1..], n)?;
            word.push_str(value);
            i += 1 + used;
            continue;
        }
        if c == '<' && next.is_some_and(|c| c.is_ascii_alphabetic()) {
            let (value, used) = placeholder(&chars[i..], n)?;
            word.push_str(value);
            i += used;
            continue;
        }
        if context == Context::Quoted {
            match c {
                '"' => {
                    stack.pop();
                }
                '\\' if next.is_some_and(|c| matches!(c, '$' | '"' | '\\' | '`')) => {
                    word.push(next.unwrap_or(c));
                    i += 1;
                }
                _ => word.push(c),
            }
            i += 1;
            continue;
        }
        match c {
            ' ' | '\t' => word.flush(&mut out),
            '#' if !word.started => break,
            '\'' => {
                let len = chars[i + 1..]
                    .iter()
                    .position(|c| *c == '\'')
                    .ok_or(CommandError::Unclosed(n))?;
                let text: String = chars[i + 1..i + 1 + len].iter().collect();
                word.push_str(&text);
                i += len + 1;
            }
            '"' => {
                word.quoted = true;
                word.started = true;
                stack.push(Context::Quoted);
            }
            // A line never ends in `\`: a continued line is joined before it is lexed.
            '\\' => {
                word.push(next.unwrap_or(c));
                word.quoted = true;
                i += 1;
            }
            '>' | '<' => {
                i += redirect(&mut word, &chars[i..], &mut out);
                continue;
            }
            '&' if next == Some('>') => {
                i += redirect(&mut word, &chars[i..], &mut out);
                continue;
            }
            '|' | '&' | ';' => {
                word.flush(&mut out);
                out.push(Token::Control);
                if next == Some(c) {
                    i += 1;
                }
            }
            '(' => {
                word.flush(&mut out);
                out.push(Token::Control);
                stack.push(Context::Nested { in_quotes: false });
            }
            ')' => {
                word.flush(&mut out);
                out.push(Token::Control);
                if let Some(Context::Nested { .. }) = stack.last() {
                    stack.pop();
                }
            }
            _ => word.push(c),
        }
        i += 1;
    }
    if stack.len() > 1 {
        return Err(CommandError::Unclosed(n));
    }
    word.flush(&mut out);
    Ok(out)
}

/// Ends `word` at the redirection that starts `rest` (a word of digits right before it is
/// its descriptor, not a word), pushes the redirection, and returns its length.
fn redirect(word: &mut Word, rest: &[char], out: &mut Vec<Token>) -> usize {
    if !word.quoted && !word.text.is_empty() && word.text.bytes().all(|b| b.is_ascii_digit()) {
        *word = Word::default();
    } else {
        word.flush(out);
    }
    let (len, takes_target) = redirection(rest);
    if takes_target {
        out.push(Token::Redirect);
    }
    len
}

/// How many characters the redirection operator at the start of `rest` takes (`>`, `>>`,
/// `>|`, `<`, `<<`, `<<<`, `<>`, `&>`, `&>>`). A `>&N` or `>&-` names a descriptor, not a
/// file: it takes no target, so the next word stays in the command. Returns the length and
/// whether the next word is its target.
fn redirection(rest: &[char]) -> (usize, bool) {
    let mut len = rest
        .iter()
        .take_while(|c| matches!(c, '>' | '<' | '|' | '&'))
        .count();
    let fd = rest[len..]
        .iter()
        .take_while(|c| c.is_ascii_digit() || **c == '-')
        .count();
    if rest[..len].ends_with(&['&']) && fd > 0 {
        len += fd;
        return (len, false);
    }
    (len, true)
}

/// The value of the variable whose name starts `rest` (after its `$`), and how many
/// characters the name takes (`NAME` or `{NAME}`).
fn variable(rest: &[char], n: usize) -> Result<(&'static str, usize), CommandError> {
    let braced = rest.first() == Some(&'{');
    let from = usize::from(braced);
    let len = rest[from..]
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
        .count();
    let name: String = rest[from..from + len].iter().collect();
    let closed = !braced || rest.get(from + len) == Some(&'}');
    let found = SHELL_VARIABLES
        .iter()
        .find(|(v, _)| *v == name)
        .filter(|_| closed && !name.starts_with(|c: char| c.is_ascii_digit()));
    match found {
        Some((_, value)) => Ok((value, from + len + usize::from(braced))),
        None => Err(CommandError::Variable { line: n, name }),
    }
}

/// The value of the placeholder that starts `rest` (`<name>`), and how many characters
/// it takes.
fn placeholder(rest: &[char], n: usize) -> Result<(&'static str, usize), CommandError> {
    let len = rest
        .iter()
        .position(|c| *c == '>')
        .map_or(rest.len(), |p| p + 1);
    let name: String = rest[..len].iter().collect();
    PLACEHOLDERS
        .iter()
        .find(|(p, _)| *p == name)
        .map(|(_, value)| (*value, len))
        .ok_or(CommandError::Placeholder { line: n, name })
}

/// The `kbf cluster` commands among `tokens`: each simple command (the words between two
/// control operators, redirections and their targets taken out) from its `kbf` word on.
fn commands(tokens: &[Token]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut target = false;
    for token in tokens.iter().chain([&Token::Control]) {
        match token {
            Token::Redirect => target = true,
            Token::Word(_) if target => target = false,
            Token::Word(w) => words.push(w.clone()),
            Token::Control => {
                target = false;
                let at = words
                    .windows(2)
                    .position(|w| w[0].rsplit('/').next() == Some("kbf") && w[1] == "cluster");
                if let Some(at) = at {
                    let mut argv = words.split_off(at);
                    argv[0] = "kbf".to_owned();
                    out.push(argv);
                }
                words.clear();
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "doc_commands_tests.rs"]
mod tests;
