//! Debian control-file syntax: fields (`Name: value`, continuation lines start with a space) and
//! stanzas separated by blank lines. The same text is the `control` file inside a package and a
//! stanza of an apt `Packages` file.

use crate::channels::Error;

/// One field: its name, and the text after `Name: `; a multi-line value keeps its continuation
/// lines (each starts with a space), joined by `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The field name.
    pub name: String,
    /// The field value, continuation lines joined by `\n`.
    pub value: String,
}

impl Field {
    /// A field of `name` holding `value`.
    pub fn new(name: &str, value: &str) -> Field {
        Field {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }
}

/// A name is letters, digits and `-`; a value is one or more lines with no NUL and no carriage
/// return, every line after the first a continuation, and no trailing newline: a value cannot
/// smuggle in a second field.
fn check(field: &Field) -> Result<(), Error> {
    let name_ok = !field.name.is_empty()
        && field
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let value = &field.value;
    let value_ok = !value.is_empty()
        && !value.bytes().any(|b| b == 0 || b == b'\r')
        && !value.ends_with('\n')
        && value.split('\n').skip(1).all(|l| l.starts_with(' '));
    if name_ok && value_ok {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "control field {:?} is not a field a control file can hold",
            field.name
        )))
    }
}

/// `Name: value\n` for each field.
///
/// # Errors
/// A field that [`check`] refuses.
pub fn render(fields: &[Field]) -> Result<String, Error> {
    let mut out = String::new();
    for field in fields {
        check(field)?;
        out.push_str(&format!("{}: {}\n", field.name, field.value));
    }
    Ok(out)
}

/// The fields of ONE stanza (no blank line inside).
///
/// # Errors
/// A continuation line before any field, or a line without a colon.
pub fn parse(text: &str) -> Result<Vec<Field>, Error> {
    let mut out: Vec<Field> = Vec::new();
    for line in text.split('\n').filter(|l| !l.is_empty()) {
        if line.starts_with([' ', '\t']) {
            let Some(last) = out.last_mut() else {
                return Err(Error::Failed(format!(
                    "control text starts with a continuation line: {line:?}"
                )));
            };
            last.value.push('\n');
            last.value.push_str(line);
        } else {
            let Some((name, value)) = line.split_once(':') else {
                return Err(Error::Failed(format!(
                    "control line without a colon: {line:?}"
                )));
            };
            out.push(Field::new(name, value.trim_start_matches(' ')));
        }
    }
    Ok(out)
}

/// The value of the field `name` (case sensitive), if there is one.
pub fn get<'a>(fields: &'a [Field], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|f| f.name == name)
        .map(|f| f.value.as_str())
}

/// Splits a file of stanzas (blank-line separated) into the text of each, every one ending in
/// `\n` unless the file does not.
pub fn split_stanzas(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut at = 0;
    while at < text.len() {
        let end = text[at..].find('\n').map_or(text.len(), |p| at + p);
        if end == at {
            if at > start {
                out.push(&text[start..at]);
            }
            start = end + 1;
        }
        at = end + 1;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

#[cfg(test)]
mod tests;
