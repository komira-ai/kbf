//! Strict `@KEY@` substitution for the files the kit renders.
//!
//! A placeholder is `@`, an upper-case letter, upper-case letters, digits or `_`, then
//! `@`. Rendering refuses, rather than writes:
//! - a placeholder with no value (a template that gained a key the kit does not set
//!   would otherwise ship `@KEY@` to the host);
//! - an `@` that does not start a placeholder (a typo such as `@ETC/ca.pem` would
//!   otherwise pass through as text).
//!
//! Values are inserted once and never scanned again, so a value may hold an `@`.

use std::collections::BTreeMap;

/// The values a template is rendered with, by key.
pub type Values = BTreeMap<&'static str, String>;

/// Why a template could not be rendered. Line numbers count from 1.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// A placeholder with no value.
    #[error("{template}:{line}: placeholder @{key}@ has no value")]
    Unknown {
        /// The template's name.
        template: String,
        /// The line.
        line: usize,
        /// The placeholder's key.
        key: String,
    },
    /// An `@` that starts no placeholder.
    #[error("{template}:{line}: an @ that starts no @KEY@ placeholder")]
    Stray {
        /// The template's name.
        template: String,
        /// The line.
        line: usize,
    },
}

/// `text`, the template called `name` (for errors), with every placeholder replaced
/// from `values`. Every line of the output ends in a newline.
pub fn render(name: &str, text: &str, values: &Values) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        let mut rest = line;
        while let Some(open) = rest.find('@') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let key = after.split_once('@').map(|(k, _)| k).filter(|k| is_key(k));
            let Some(key) = key else {
                return Err(TemplateError::Stray {
                    template: name.to_owned(),
                    line: i + 1,
                });
            };
            let Some(value) = values.get(key) else {
                return Err(TemplateError::Unknown {
                    template: name.to_owned(),
                    line: i + 1,
                    key: key.to_owned(),
                });
            };
            out.push_str(value);
            rest = &after[key.len() + 1..];
        }
        out.push_str(rest);
        out.push('\n');
    }
    Ok(out)
}

fn is_key(k: &str) -> bool {
    let mut chars = k.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}
