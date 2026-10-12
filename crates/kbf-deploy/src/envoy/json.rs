//! The JSON the `envoy` kit writes, printed byte for byte as Python's
//! `json.dumps(value, indent=2, sort_keys=True)` prints it, plus a final newline.
//!
//! The bytes matter: the Deployment's pod template carries the sha256 of the Envoy
//! config, so any change in how the config is printed (key order, spacing, escapes)
//! changes the hash and restarts every Envoy pod on the next apply. The kit's first
//! renderer was a Python script, and its output is the golden the tests compare with.
//!
//! - Object keys are sorted by code point (a [`BTreeMap`] of `String`s sorts by UTF-8
//!   bytes, which is the same order).
//! - Two-space indent, `,` at line ends and `": "` between a key and its value; an
//!   empty object or list is `{}` or `[]` on one line.
//! - Numbers are integers only, so no float format can drift.
//! - Strings escape `"` and `\`, use `\n`, `\r`, `\t`, `\b` and `\f`, and write every
//!   other character outside space to `~` as `\uXXXX` (UTF-16 surrogate pairs above
//!   U+FFFF), as Python's default `ensure_ascii=True` does.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A JSON value with no floats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Json {
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    /// An object from `(key, value)` pairs.
    pub fn object<const N: usize>(pairs: [(&str, Json); N]) -> Json {
        Json::Object(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }

    /// A string.
    pub fn str(s: &str) -> Json {
        Json::Str(s.to_owned())
    }

    /// The value as a file: printed, then a newline.
    pub fn to_file_text(&self) -> String {
        let mut out = String::new();
        self.print(&mut out, 0);
        out.push('\n');
        out
    }

    fn print(&self, out: &mut String, depth: usize) {
        match self {
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Json::Str(s) => quote(out, s),
            Json::List(items) if items.is_empty() => out.push_str("[]"),
            Json::List(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    separate(out, i, depth + 1);
                    item.print(out, depth + 1);
                }
                newline(out, depth);
                out.push(']');
            }
            Json::Object(map) if map.is_empty() => out.push_str("{}"),
            Json::Object(map) => {
                out.push('{');
                for (i, (key, value)) in map.iter().enumerate() {
                    separate(out, i, depth + 1);
                    quote(out, key);
                    out.push_str(": ");
                    value.print(out, depth + 1);
                }
                newline(out, depth);
                out.push('}');
            }
        }
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::str(s)
    }
}

impl From<i64> for Json {
    fn from(n: i64) -> Json {
        Json::Int(n)
    }
}

impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}

impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(items: Vec<T>) -> Json {
        Json::List(items.into_iter().map(Into::into).collect())
    }
}

/// Before the `i`th item of a container: `,` after the one before, then a new line.
fn separate(out: &mut String, i: usize, depth: usize) {
    if i > 0 {
        out.push(',');
    }
    newline(out, depth);
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn quote(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a printer that keeps insertion order instead of sorting keys, indents or
    /// separates differently from Python, prints an empty container across lines, or
    /// escapes a character Python's `ensure_ascii` escapes otherwise (DEL, a non-ASCII
    /// letter, a character above U+FFFF as one escape instead of a surrogate pair). The
    /// expected text is `json.dumps(v, indent=2, sort_keys=True)` of the same value, as
    /// Python 3 printed it.
    #[test]
    fn values_print_as_python_json_dumps_with_sorted_keys_prints_them() {
        let s = "a\"b\\c/\n\r\t\u{8}\u{c}\u{1}\u{1f}\u{7f}\u{e9}\u{2028}\u{1F600} ~";
        let v = Json::object([
            ("s", Json::str(s)),
            ("e", Json::object([])),
            ("l", Json::List(Vec::new())),
            ("n", vec![0i64, -1, 65535].into()),
            ("t", true.into()),
            ("f", false.into()),
            ("B", 1i64.into()),
            (
                "a",
                Json::List(vec![Json::object([
                    ("z", 1i64.into()),
                    ("y", Json::List(Vec::new())),
                ])]),
            ),
        ]);
        let want = r#"{
  "B": 1,
  "a": [
    {
      "y": [],
      "z": 1
    }
  ],
  "e": {},
  "f": false,
  "l": [],
  "n": [
    0,
    -1,
    65535
  ],
  "s": "a\"b\\c/\n\r\t\b\f\u0001\u001f\u007f\u00e9\u2028\ud83d\ude00 ~",
  "t": true
}
"#;
        assert_eq!(v.to_file_text(), want);
    }

    /// Catches a top-level scalar or empty value printed with an indent or without its
    /// final newline.
    #[test]
    fn a_scalar_or_empty_value_is_one_line() {
        assert_eq!(Json::from("x").to_file_text(), "\"x\"\n");
        assert_eq!(Json::Int(-7).to_file_text(), "-7\n");
        assert_eq!(Json::object([]).to_file_text(), "{}\n");
        assert_eq!(Json::List(Vec::new()).to_file_text(), "[]\n");
    }
}
