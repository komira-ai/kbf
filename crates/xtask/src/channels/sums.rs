//! The lines of a `SHA256SUMS` file, as `sha256sum` writes them: `<64 hex digits>  <file name>`.

use std::path::Path;

use super::Error;
use crate::fetch::Sha256;

/// The file name of the release's sums file.
pub const SUMS: &str = "SHA256SUMS";

/// One line of a sums file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The digest of the file.
    pub sha256: Sha256,
    /// The file's name, with no directory.
    pub name: String,
}

/// The lines of the sums file `text`, or one problem per line that is off the form.
///
/// # Errors
/// One problem per line that is not `<sha256>  <file name>` and a newline.
pub fn parse(text: &str) -> Result<Vec<Line>, Vec<String>> {
    let mut lines = Vec::new();
    let mut problems = Vec::new();
    let mut rest = text;
    let mut number = 0;
    while !rest.is_empty() {
        number += 1;
        let (line, after) = rest.split_once('\n').unwrap_or((rest, ""));
        let ended = rest.len() > line.len();
        rest = after;
        let parsed = line
            .split_once("  ")
            .filter(|_| ended)
            .and_then(|(hex, name)| Some((hex.parse::<Sha256>().ok()?, name)))
            .filter(|(_, name)| !name.is_empty() && !name.contains('/'));
        match parsed {
            Some((sha256, name)) => lines.push(Line {
                sha256,
                name: name.to_owned(),
            }),
            None => problems.push(format!(
                "line {number}: {line:?} is not `<sha256>  <file name>` and a newline"
            )),
        }
    }
    if problems.is_empty() {
        Ok(lines)
    } else {
        Err(problems)
    }
}

/// The lines of the sums file at `path`; a missing, malformed or empty file is an error.
pub(crate) fn read_sums(path: &Path) -> Result<Vec<Line>, Error> {
    let text =
        std::fs::read_to_string(path).map_err(Error::spawn(format!("read {}", path.display())))?;
    let lines = parse(&text).map_err(|problems| {
        Error::Refused(
            problems
                .into_iter()
                .map(|p| format!("{}: {p}", path.display()))
                .collect(),
        )
    })?;
    if lines.is_empty() {
        return Err(Error::Failed(format!("{} lists no file", path.display())));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests;
