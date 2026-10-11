//! Strict `@KEY@` substitution for the files a kit renders.
//!
//! A placeholder is `@`, an upper-case letter, upper-case letters, digits or `_`, then
//! `@`. Rendering refuses, rather than writes:
//! - a placeholder with no value (a template that gained a key its kit does not set
//!   would otherwise ship `@KEY@` to the host);
//! - an `@` that does not start a placeholder (a typo such as `@KBF_ETC/ca.pem` would
//!   otherwise pass through as text);
//! - a [`Value::Lines`] placeholder that is not alone on its line.
//!
//! A [`Value::Lines`] placeholder alone on a line becomes that many lines, none when it
//! is empty, so a variable number of flags fits a `\`-continued command line. Values
//! are inserted once and never scanned again, so a value may hold an `@` (an image
//! digest does).

use std::collections::BTreeMap;

/// What a placeholder becomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// Text inside a line.
    Text(String),
    /// Whole lines, in place of a line holding only the placeholder.
    Lines(Vec<String>),
}

/// The values a template is rendered with, by key.
pub type Values = BTreeMap<&'static str, Value>;

/// Why a template could not be rendered. Line numbers count from 1.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("{template}:{line}: placeholder @{key}@ has no value")]
    Unknown {
        template: String,
        line: usize,
        key: String,
    },
    #[error("{template}:{line}: an @ that starts no @KEY@ placeholder")]
    Stray { template: String, line: usize },
    #[error("{template}:{line}: @{key}@ stands for whole lines and must be alone on its line")]
    NotAlone {
        template: String,
        line: usize,
        key: String,
    },
}

/// `text`, the template called `name` (for errors), with every placeholder replaced
/// from `values`.
pub fn render(name: &str, text: &str, values: &Values) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        let at = |key: &str| (name.to_owned(), i + 1, key.to_owned());
        if let Some(key) = alone(line)
            && let Some(Value::Lines(lines)) = values.get(key)
        {
            for l in lines {
                out.push_str(l);
                out.push('\n');
            }
            continue;
        }
        let mut rest = line;
        while let Some(open) = rest.find('@') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let Some(key) = after.split_once('@').map(|(k, _)| k).filter(|k| is_key(k)) else {
                let (template, line, _) = at("");
                return Err(TemplateError::Stray { template, line });
            };
            match values.get(key) {
                Some(Value::Text(v)) => out.push_str(v),
                Some(Value::Lines(_)) => {
                    let (template, line, key) = at(key);
                    return Err(TemplateError::NotAlone {
                        template,
                        line,
                        key,
                    });
                }
                None => {
                    let (template, line, key) = at(key);
                    return Err(TemplateError::Unknown {
                        template,
                        line,
                        key,
                    });
                }
            }
            rest = &after[key.len() + 1..];
        }
        out.push_str(rest);
        out.push('\n');
    }
    Ok(out)
}

/// The placeholders left in rendered text, such as `@KBF_UID@`, in order.
pub fn leftovers(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('@') {
        let after = &rest[open + 1..];
        match after.split_once('@') {
            Some((k, _)) if is_key(k) => {
                found.push(&rest[open..open + k.len() + 2]);
                rest = &after[k.len() + 1..];
            }
            _ => rest = after,
        }
    }
    found
}

/// The key of a line that holds one placeholder and nothing else.
fn alone(line: &str) -> Option<&str> {
    line.strip_prefix('@')
        .and_then(|l| l.strip_suffix('@'))
        .filter(|k| is_key(k))
}

fn is_key(k: &str) -> bool {
    let mut chars = k.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Values {
        BTreeMap::from([
            ("ETC", Value::Text("/etc/kbf".into())),
            ("IMAGE", Value::Text("docker://r@sha256:ab".into())),
            (
                "ARGS",
                Value::Lines(vec!["  --a \\".into(), "  --b \\".into()]),
            ),
            ("NONE", Value::Lines(Vec::new())),
        ])
    }

    /// Catches a placeholder left in place, two on one line not both replaced, a value
    /// scanned again after it is inserted (the `@` in an image digest would then read
    /// as a placeholder), and whole-line values not expanded or not dropped when empty.
    #[test]
    fn every_placeholder_is_replaced_once() {
        let text = "\
# at @ETC@
ExecStart=/bin/x \\
@ARGS@
@NONE@
  --ca=@ETC@/ca.pem --image=@IMAGE@ --key=@ETC@/k
";
        assert_eq!(
            render("t", text, &values()),
            Ok("\
# at /etc/kbf
ExecStart=/bin/x \\
  --a \\
  --b \\
  --ca=/etc/kbf/ca.pem --image=docker://r@sha256:ab --key=/etc/kbf/k
"
            .to_owned())
        );
    }

    /// Catches a template whose placeholder has no value being rendered with the
    /// placeholder left in (a leftover reaching the host), a stray `@` passed through,
    /// and a whole-line value pasted into the middle of a line.
    #[test]
    fn an_unknown_placeholder_a_stray_at_or_lines_inline_is_refused() {
        let refused = |text: &str| {
            render("kbf.slice", text, &values())
                .err()
                .map(|e| e.to_string())
        };
        let cases = [
            (
                "A=1\nB=@KBF_UID@\n",
                "kbf.slice:2: placeholder @KBF_UID@ has no value",
            ),
            (
                "A=@ETC/ca.pem\n",
                "kbf.slice:1: an @ that starts no @KEY@ placeholder",
            ),
            (
                "A=@etc@\n",
                "kbf.slice:1: an @ that starts no @KEY@ placeholder",
            ),
            (
                "A=user@\n",
                "kbf.slice:1: an @ that starts no @KEY@ placeholder",
            ),
            (
                "A=x @ARGS@\n",
                "kbf.slice:1: @ARGS@ stands for whole lines and must be alone on its line",
            ),
        ];
        for (text, want) in cases {
            assert_eq!(refused(text).as_deref(), Some(want), "{text:?}");
        }
        assert_eq!(
            refused("@UNSET@\n").as_deref(),
            Some("kbf.slice:1: placeholder @UNSET@ has no value")
        );
    }

    /// Catches a leftover missed at the end of a line or next to another, and an `@`
    /// inside a value (an image digest, an email) reported as a placeholder.
    #[test]
    fn leftovers_finds_each_placeholder_and_nothing_else() {
        let text = "a=@KBF_UID@\nb=r@sha256:ab x@ y=@A@@B_2@ @lower@ @\n";
        assert_eq!(leftovers(text), ["@KBF_UID@", "@A@", "@B_2@"]);
        assert!(leftovers("--image=docker://r@sha256:ab\n").is_empty());
    }
}
