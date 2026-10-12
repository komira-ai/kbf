//! Fakes of the macOS tools kbf runs, chosen by the name the program is run by (the
//! base name of argv0): `xcrun`, `xcodebuild` and `sandbox-exec`. A test links the
//! program under one of those names where the code under test looks for the tool.
//!
//! Each run reads `<link>.rules`, where `<link>` is argv0 (looked up on `PATH` when it
//! has no `/`) or, when there is no rules file beside it, the first link along its
//! chain of symbolic links that has one; so one rules file serves every link made to
//! one fake. No rules file anywhere along the chain fails the run. Before answering,
//! the run appends one line to `<link>.log`: argv0 and the arguments, joined by
//! spaces.
//!
//! A rules file holds one rule per line, three fields separated by tabs; empty lines
//! and lines starting with `#` are skipped:
//!
//! ```text
//! <arguments>\t<environment>\t<steps>
//! ```
//!
//! - `<arguments>` matches the arguments joined by spaces; `*` in it matches any run
//!   of characters, and nothing else is special.
//! - `<environment>` is `-`, which matches always, or `NAME=VALUE`, which matches when
//!   the variable's value (empty when unset) matches `VALUE`, `*` as above.
//! - `<steps>` are [`crate::act`] steps, split at spaces, with `$1` to `$9` (the
//!   arguments), `$*` (all of them, joined by spaces), `$$` (the fake's process id, so
//!   each run can say something new) and `${NAME}` (the variable, empty when unset)
//!   replaced in each word.
//!
//! The first matching rule's steps run. When they end without `exit`, or no rule
//! matches, the tool's own ending follows:
//! - `xcrun` and `xcodebuild` exit 0 after a matching rule; with none, they print
//!   `unknown: <arguments>` on stderr and exit 64, as a real one does for an option it
//!   does not know.
//! - `sandbox-exec` skips its options (`-f`, `-n`, `-p` and `-D`, each with a value)
//!   and replaces itself with the command after them, so the command runs unsandboxed
//!   with the same process id; with no command it exits 2. Each `-D NAME=VALUE` (a
//!   profile parameter to the real one) is put in the command's environment, so a fake
//!   the command runs can tell it ran under `sandbox-exec`, and with which values; a
//!   `-D` value without `=` exits 2.

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

/// A tool this program can stand in for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Xcrun,
    Xcodebuild,
    SandboxExec,
}

impl Tool {
    /// The tool argv0's base name names, if any.
    #[must_use]
    pub fn named(argv0: &str) -> Option<Self> {
        match Path::new(argv0).file_name()?.to_str()? {
            "xcrun" => Some(Self::Xcrun),
            "xcodebuild" => Some(Self::Xcodebuild),
            "sandbox-exec" => Some(Self::SandboxExec),
            _ => None,
        }
    }

    /// The tool's name, as run.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Xcrun => "xcrun",
            Self::Xcodebuild => "xcodebuild",
            Self::SandboxExec => "sandbox-exec",
        }
    }
}

/// Answers as `tool`, run by `argv0` with `args`; returns the exit status.
pub(crate) fn run(tool: Tool, argv0: &str, args: &[String]) -> Result<u8, Error> {
    let link = rules_link(argv0)?;
    let mut line = vec![argv0.to_owned()];
    line.extend(args.iter().cloned());
    crate::act::append(&with_suffix(&link, ".log"), &line.join(" "))?;
    let rules = std::fs::read_to_string(with_suffix(&link, ".rules"))
        .map_err(crate::failed(format!("read {}.rules", link.display())))?;
    let joined = args.join(" ");
    let mut matched = false;
    for rule in rules.lines() {
        if rule.is_empty() || rule.starts_with('#') {
            continue;
        }
        let Some([pattern, environment, steps]) = fields(rule) else {
            return Err(Error::Usage(format!("bad rule: {rule:?}")));
        };
        if !glob(pattern, &joined) || !environment_matches(environment)? {
            continue;
        }
        let words: Vec<String> = steps.split(' ').map(|w| expand(w, args)).collect();
        if let Some(code) = crate::act::perform(&crate::act::parse(&words)?)? {
            return Ok(code);
        }
        matched = true;
        break;
    }
    match tool {
        Tool::Xcrun | Tool::Xcodebuild if matched => Ok(0),
        Tool::Xcrun | Tool::Xcodebuild => {
            crate::write_line(std::io::stderr(), &format!("unknown: {joined}"))?;
            Ok(64)
        }
        Tool::SandboxExec => exec_command(args),
    }
}

/// The three tab-separated fields of `rule`.
fn fields(rule: &str) -> Option<[&str; 3]> {
    let mut parts = rule.split('\t');
    let fields = [parts.next()?, parts.next()?, parts.next()?];
    parts.next().is_none().then_some(fields)
}

/// Whether `condition` (`-` or `NAME=VALUE`) holds in this process's environment.
fn environment_matches(condition: &str) -> Result<bool, Error> {
    if condition == "-" {
        return Ok(true);
    }
    let (name, pattern) = condition
        .split_once('=')
        .ok_or_else(|| Error::Usage(format!("bad environment condition: {condition:?}")))?;
    let value = std::env::var_os(name).unwrap_or_default();
    Ok(glob(pattern, &value.to_string_lossy()))
}

/// Whether `text` matches `pattern`, where `*` matches any run of characters.
fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let middle: Vec<&str> = parts.collect();
    let Some((last, middle)) = middle.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

/// `word` with `$1`..`$9`, `$*` and `${NAME}` replaced.
fn expand(word: &str, args: &[String]) -> String {
    let mut out = String::new();
    let mut rest = word;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        if let Some(digit) = rest
            .chars()
            .next()
            .and_then(|c| c.to_digit(10))
            .filter(|d| *d > 0)
        {
            let index = usize::try_from(digit).unwrap_or(usize::MAX);
            out.push_str(args.get(index - 1).map_or("", String::as_str));
            rest = &rest[1..];
        } else if let Some(after) = rest.strip_prefix('$') {
            out.push_str(&std::process::id().to_string());
            rest = after;
        } else if let Some(after) = rest.strip_prefix('*') {
            out.push_str(&args.join(" "));
            rest = after;
        } else if let Some((name, after)) = rest.strip_prefix('{').and_then(|r| r.split_once('}')) {
            out.push_str(&std::env::var_os(name).unwrap_or_default().to_string_lossy());
            rest = after;
        } else {
            out.push('$');
        }
    }
    out.push_str(rest);
    out
}

/// The path along argv0's links with a rules file beside it.
fn rules_link(argv0: &str) -> Result<PathBuf, Error> {
    let mut link = if argv0.contains('/') {
        PathBuf::from(argv0)
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(argv0))
            .find(|path| path.exists())
            .unwrap_or_else(|| PathBuf::from(argv0))
    };
    // A bound on the chain, as the kernel bounds it, so a loop of links ends.
    for _ in 0..40 {
        if Path::new(&with_suffix(&link, ".rules")).exists() {
            return Ok(link);
        }
        let Ok(target) = std::fs::read_link(&link) else {
            break;
        };
        link = link
            .parent()
            .map_or_else(PathBuf::new, Path::to_path_buf)
            .join(target);
    }
    Err(Error::Failed(format!(
        "no .rules file beside {argv0} or along its links"
    )))
}

fn with_suffix(path: &Path, suffix: &str) -> String {
    format!("{}{suffix}", path.display())
}

/// Replaces this process with the command after `sandbox-exec`'s options in `args`,
/// each `-D NAME=VALUE` put in its environment.
fn exec_command(args: &[String]) -> Result<u8, Error> {
    let mut rest = args;
    let mut params = Vec::new();
    while let [option, value, after @ ..] = rest {
        if !["-f", "-n", "-p", "-D"].contains(&option.as_str()) {
            break;
        }
        if option == "-D" {
            params.push(
                value
                    .split_once('=')
                    .ok_or_else(|| Error::Usage(format!("-D {value:?} is not NAME=VALUE")))?,
            );
        }
        rest = after;
    }
    let Some((program, program_args)) = rest.split_first() else {
        return Err(Error::Usage(
            "usage: sandbox-exec [-f file] [-n name] [-p profile] [-D key=value] command..."
                .to_owned(),
        ));
    };
    let error = Command::new(program).args(program_args).envs(params).exec();
    Err(Error::Failed(format!("exec {program}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a `*` that does not match an empty run, a pattern matched as a prefix
    /// or a suffix only, and parts allowed to overlap.
    #[test]
    fn globs_match_whole_text() {
        for (pattern, text, want) in [
            ("--find clang", "--find clang", true),
            ("--find clang", "--find clang2", false),
            ("--find clang", "x--find clang", false),
            ("*", "", true),
            ("*", "anything", true),
            ("--find *", "--find metal", true),
            (
                "*/Xcode_good.app/Contents/Developer",
                "/A/Xcode_good.app/Contents/Developer",
                true,
            ),
            (
                "*/Xcode_good.app/Contents/Developer",
                "/A/Xcode_bad.app/Contents/Developer",
                false,
            ),
            ("a*b*c", "a-b-c", true),
            ("a*b*c", "a-c-b", false),
            ("a*x*c", "abc", false),
            ("ab*ba", "aba", false),
            ("a**", "a", true),
            ("", "", true),
            ("", "x", false),
        ] {
            assert_eq!(glob(pattern, text), want, "{pattern:?} on {text:?}");
        }
    }

    /// Catches an argument taken off by one, `$*` not joined, `${NAME}` not read, `$$`
    /// not the process id, a `$0` or a lone `$` dropped or replaced.
    #[test]
    fn words_expand() {
        let args = ["--find".to_owned(), "clang".to_owned()];
        let path = std::env::var("PATH").expect("a PATH");
        let tool = format!("{path}/usr/bin/clang");
        let pid = format!("[{}:1]", std::process::id());
        for (word, want) in [
            ("[$$:1]", pid.as_str()),
            ("$2", "clang"),
            ("$1-$2", "--find-clang"),
            ("$3", ""),
            ("$*", "--find clang"),
            ("${PATH}/usr/bin/$2", tool.as_str()),
            ("${KBF_TESTKIT_UNSET}x", "x"),
            ("$0", "$0"),
            ("cost$", "cost$"),
            ("${open", "${open"),
            ("plain", "plain"),
        ] {
            assert_eq!(expand(word, &args), want, "{word:?}");
        }
    }

    /// Catches a tool taken for another, and a name matched on more than the base name.
    #[test]
    fn tools_are_named_by_argv0s_base_name() {
        for tool in [Tool::Xcrun, Tool::Xcodebuild, Tool::SandboxExec] {
            assert_eq!(Tool::named(tool.name()), Some(tool));
            assert_eq!(Tool::named(&format!("/a/b/{}", tool.name())), Some(tool));
        }
        for other in ["kbf-act", "/a/xcrun/kbf-act", "xcrun2", "", "/"] {
            assert_eq!(Tool::named(other), None, "{other:?}");
        }
    }
}
