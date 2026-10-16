//! The `coverage-baseline` file: one line per workspace crate with how many of its lines
//! and branches no test runs. CI fails when either count rises.
//!
//! Each line is `<crate> <lines missed> <branches missed>`, separated by whitespace; a
//! value is a count in decimal digits, or `-` for a crate with nothing to measure.
//! Blank lines and lines starting with `#` are comments. [`render`] writes the file,
//! [`rewrite`] changes its values in place (what `kbf-coverage tighten` uses, keeping
//! the comments), and [`parse`] reads what they write, refusing a crate listed twice.

use std::collections::BTreeMap;

use crate::ratchet::Metric;
use crate::{CrateCoverage, show};

/// One crate's recorded missed counts; `None` means nothing to measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub lines: Option<u64>,
    pub branches: Option<u64>,
}

impl Entry {
    /// The entry that records `c` as measured.
    pub fn of(c: CrateCoverage) -> Self {
        Self {
            lines: c.lines.missed(),
            branches: c.branches.missed(),
        }
    }

    /// The recorded value of `metric`.
    pub fn get(self, metric: Metric) -> Option<u64> {
        match metric {
            Metric::Lines => self.lines,
            Metric::Branches => self.branches,
        }
    }
}

/// Crate name to its recorded missed counts.
pub type Baseline = BTreeMap<String, Entry>;

/// Why a baseline file was refused. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BaselineError {
    #[error(
        "line {line}: expected `<crate> <lines missed> <branches missed>`, found {found} fields"
    )]
    Fields { line: usize, found: usize },
    #[error("line {line}: not a count of missed items or `-`: {value:?}")]
    Count { line: usize, value: String },
    #[error("line {line}: {krate} is listed twice")]
    Duplicate { line: usize, krate: String },
}

/// A count is decimal digits only, so `+1`, `-1` and `1.0` (an old percentage) are
/// refused rather than read as something the writer never wrote.
fn value(line: usize, s: &str) -> Result<Option<u64>, BaselineError> {
    if s == "-" {
        return Ok(None);
    }
    let bad = || BaselineError::Count {
        line,
        value: s.to_owned(),
    };
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    s.parse().map(Some).map_err(|_| bad())
}

/// Reads a baseline file.
pub fn parse(text: &str) -> Result<Baseline, BaselineError> {
    let mut out = Baseline::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = raw.split_whitespace().collect();
        let [krate, lines, branches] = fields[..] else {
            return Err(BaselineError::Fields {
                line,
                found: fields.len(),
            });
        };
        let entry = Entry {
            lines: value(line, lines)?,
            branches: value(line, branches)?,
        };
        if out.insert(krate.to_owned(), entry).is_some() {
            return Err(BaselineError::Duplicate {
                line,
                krate: krate.to_owned(),
            });
        }
    }
    Ok(out)
}

const HEADER: &str = "\
# Coverage ratchet, checked by the CI coverage job (crates/kbf-coverage). One line per
# workspace crate: how many of its lines and branches no test runs; `-` means the crate
# has nothing to measure. CI fails when a crate misses more than its line here, so a
# change covers the code it adds (or covers as much existing code as it leaves
# uncovered), and when it misses fewer, so the file never keeps slack. The target is 0
# everywhere. When coverage improves, running `kbf-coverage tighten` on the measured
# file the coverage job uploads is required (the failing job prints the command). It
# is not a bypass: it only lowers counts, and refuses any rise. The one hand edit is a
# crate whose code is no longer measured at all (tighten refuses it and says so): its
# `-` goes in by hand, in a reviewed pull request.
# crate lines-missed branches-missed
";

/// The name column's width: the longest crate name.
fn width(baseline: &Baseline) -> usize {
    baseline.keys().map(String::len).max().unwrap_or(0)
}

/// One crate's line, values right-aligned in six columns after the padded name.
fn entry_line(krate: &str, e: Entry, width: usize) -> String {
    format!(
        "{krate:<width$} {:>6} {:>6}\n",
        show(e.lines),
        show(e.branches)
    )
}

/// Writes a baseline file, crates in name order, values aligned.
pub fn render(baseline: &Baseline) -> String {
    let width = width(baseline);
    let mut out = String::from(HEADER);
    for (krate, e) in baseline {
        out.push_str(&entry_line(krate, *e, width));
    }
    out
}

/// The crate a line of the file names, or `None` for a comment or blank line.
fn crate_of(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    if raw.starts_with('#') {
        return None;
    }
    raw.split_whitespace().next()
}

/// Rewrites the baseline file `text` so it records `baseline`, keeping its comment
/// and blank lines where they are. The crate lines come out in name order, in
/// [`render`]'s format (so a file [`render`] wrote changes only where a value, or the
/// name column's width, does), at the places the file's crate lines held: a crate the
/// file lacks goes before the first line whose crate sorts after it, or after the
/// last crate line, and a crate `baseline` lacks is dropped. `text` must have parsed.
pub fn rewrite(text: &str, baseline: &Baseline) -> String {
    let width = width(baseline);
    let lines: Vec<&str> = text.lines().collect();
    let last = lines.iter().rposition(|raw| crate_of(raw).is_some());
    let mut pending = baseline.iter().peekable();
    let mut out = String::new();
    for (index, raw) in lines.into_iter().enumerate() {
        match crate_of(raw) {
            None => {
                out.push_str(raw);
                out.push('\n');
            }
            Some(krate) => {
                while let Some((k, e)) = pending.next_if(|(k, _)| k.as_str() <= krate) {
                    out.push_str(&entry_line(k, *e, width));
                }
            }
        }
        if Some(index) == last {
            for (k, e) in pending.by_ref() {
                out.push_str(&entry_line(k, *e, width));
            }
        }
    }
    // A file without crate lines: the crates go after its comments.
    for (k, e) in pending {
        out.push_str(&entry_line(k, *e, width));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Counts;

    /// Catches: a writer and reader that disagree, so a baseline CI wrote would not
    /// read back as the same values (or would lose a crate).
    #[test]
    fn render_then_parse_round_trips() {
        let mut b = Baseline::new();
        b.insert(
            "kbf-types".into(),
            Entry::of(CrateCoverage {
                lines: Counts::new(8, 7).expect("valid"),
                branches: Counts::default(),
            }),
        );
        b.insert(
            "a".into(),
            Entry {
                lines: Some(0),
                branches: Some(12_345_678),
            },
        );
        let text = render(&b);
        assert!(text.starts_with("# Coverage ratchet"), "{text}");
        assert!(text.contains("\nkbf-types      1      -\n"), "{text}");
        assert!(text.contains("\na              0 12345678\n"), "{text}");
        assert_eq!(parse(&text), Ok(b));
        assert_eq!(parse(&render(&Baseline::new())), Ok(Baseline::new()));
    }

    /// Catches: comments or blank lines read as crates, or surrounding whitespace
    /// rejected.
    #[test]
    fn skips_comments_and_blank_lines() {
        let got = parse("# c\n\n   \n  # indented\n  x\t17  -  \n").expect("valid");
        assert_eq!(got.len(), 1);
        assert_eq!(
            got["x"],
            Entry {
                lines: Some(17),
                branches: None
            }
        );
    }

    /// Catches: a malformed line skipped, which would drop a crate from the ratchet; a
    /// value in another form (a signed number, or a percentage from an older file) read
    /// as a count; or a crate listed twice with the second value silently winning.
    #[test]
    fn refuses_malformed_lines() {
        assert_eq!(
            parse("x 1\n"),
            Err(BaselineError::Fields { line: 1, found: 2 })
        );
        assert_eq!(
            parse("# h\nx 1 2 3\n"),
            Err(BaselineError::Fields { line: 2, found: 4 })
        );
        // The last value is all digits but overflows a u64.
        for bad in ["97.50", "+1", "-1", "--", "1e3", "18446744073709551616"] {
            assert_eq!(
                parse(&format!("x - {bad}\n")),
                Err(BaselineError::Count {
                    line: 1,
                    value: bad.into()
                }),
                "{bad}"
            );
        }
        assert_eq!(
            parse("x 97.50 -\n"),
            Err(BaselineError::Count {
                line: 1,
                value: "97.50".into()
            })
        );
        assert_eq!(
            parse("x - -\nx - -\n"),
            Err(BaselineError::Duplicate {
                line: 2,
                krate: "x".into()
            })
        );
        let messages: Vec<String> = ["x\n", "x 1.00 -\n", "x - -\nx - -\n"]
            .iter()
            .map(|t| parse(t).expect_err(t).to_string())
            .collect();
        assert_eq!(
            messages,
            [
                "line 1: expected `<crate> <lines missed> <branches missed>`, found 1 fields",
                "line 1: not a count of missed items or `-`: \"1.00\"",
                "line 2: x is listed twice",
            ]
        );
    }

    /// Catches: a rewrite that loses or moves a comment, leaves a crate out of name
    /// order, keeps a crate the new baseline drops, puts a new crate somewhere other
    /// than its sorted place, or aligns columns differently from [`render`].
    #[test]
    fn rewrite_keeps_comments_and_places_crates_in_order() {
        let text = "# head\n# more\nbb 1 2\n\n# between\ndd   4 -\nff 6 6\n# tail\n";
        let new: Baseline = [
            ("a", Some(0), None),
            ("bb", Some(1), Some(1)),
            ("cc", None, None),
            ("dd", Some(3), None),
            ("gg", Some(0), Some(0)),
        ]
        .iter()
        .map(|(k, l, b)| {
            (
                (*k).to_owned(),
                Entry {
                    lines: *l,
                    branches: *b,
                },
            )
        })
        .collect();
        assert_eq!(
            rewrite(text, &new),
            "# head\n# more\n\
             a       0      -\n\
             bb      1      1\n\
             \n# between\n\
             cc      -      -\n\
             dd      3      -\n\
             gg      0      0\n\
             # tail\n"
        );
        assert_eq!(parse(&rewrite(text, &new)), Ok(new.clone()));
        // A file whose crate lines are out of order comes out in order, each crate once.
        assert_eq!(
            rewrite("z 1 1\na 1 1\n", &new),
            render(&new)[HEADER.len()..]
        );
        // A file with no crate lines: the crates follow its comments.
        assert_eq!(
            rewrite("# only\n", &new),
            format!("# only\n{}", &render(&new)[HEADER.len()..])
        );
    }

    /// Catches: [`rewrite`] in another format than [`render`] (so a file CI wrote
    /// would change where no value did), and the checked-in file drifting from that
    /// format or from [`HEADER`]: `tighten` keeps its header, CI's measured file
    /// carries this one, and the two must read the same.
    #[test]
    fn the_checked_in_file_is_in_render_format() {
        let text = include_str!("../../../coverage-baseline");
        let parsed = parse(text).expect("the checked-in baseline parses");
        assert_eq!(render(&parsed), text);
        assert_eq!(rewrite(text, &parsed), text);
        let mut b = parsed;
        b.insert(
            "a-name-longer-than-any-crate".into(),
            Entry {
                lines: Some(0),
                branches: None,
            },
        );
        assert_eq!(rewrite(&render(&b), &b), render(&b));
    }
}
