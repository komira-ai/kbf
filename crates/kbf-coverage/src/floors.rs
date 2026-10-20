//! The `coverage-floors` file: the crates that are under 99% on lines or branches,
//! each with the floor it may not fall below.
//!
//! Each line is `<crate> <lines floor> <branches floor>`, separated by whitespace; a
//! floor is a percentage with two decimals (`97.43`), or `-` for a metric that has no
//! floor and so must reach 99.00% ([`Percent::TARGET`]). Blank lines and lines
//! starting with `#` are comments. A floor is below 99.00%, since a crate at 99% or
//! more needs none, and a line with both floors `-` is refused for the same reason.
//! A crate the file does not list has no floors.
//!
//! Floors are written as the percentage floored to hundredths, so the run that
//! measured one meets it exactly ([`crate::Counts::meets`]).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::Percent;
use crate::gate::Metric;

/// One crate's floors; `None` is the default, [`Percent::TARGET`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Floor {
    pub lines: Option<Percent>,
    pub branches: Option<Percent>,
}

impl Floor {
    /// The listed floor of `metric`.
    pub fn get(self, metric: Metric) -> Option<Percent> {
        match metric {
            Metric::Lines => self.lines,
            Metric::Branches => self.branches,
        }
    }

    /// The least `metric` may be: its floor, else 99.00%.
    pub fn threshold(self, metric: Metric) -> Percent {
        self.get(metric).unwrap_or(Percent::TARGET)
    }
}

/// Crate name to its floors.
pub type Floors = BTreeMap<String, Floor>;

/// Why a floors file was refused. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FloorsError {
    #[error("line {line}: expected `<crate> <lines floor> <branches floor>`, found {found} fields")]
    Fields { line: usize, found: usize },
    #[error("line {line}: not a percentage with two decimals or `-`: {value:?}")]
    Percent { line: usize, value: String },
    #[error("line {line}: {krate} is listed twice")]
    Duplicate { line: usize, krate: String },
    #[error("line {line}: {krate}'s floor {value} is not below 99.00; a crate at 99% needs none")]
    NotBelowTarget {
        line: usize,
        krate: String,
        value: Percent,
    },
    #[error("line {line}: {krate} has no floor; remove the line")]
    NoFloor { line: usize, krate: String },
}

fn value(line: usize, s: &str) -> Result<Option<Percent>, FloorsError> {
    if s == "-" {
        return Ok(None);
    }
    Percent::parse(s)
        .map(Some)
        .ok_or_else(|| FloorsError::Percent {
            line,
            value: s.to_owned(),
        })
}

/// Reads a floors file.
pub fn parse(text: &str) -> Result<Floors, FloorsError> {
    let mut out = Floors::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = raw.split_whitespace().collect();
        let [krate, lines, branches] = fields[..] else {
            return Err(FloorsError::Fields {
                line,
                found: fields.len(),
            });
        };
        let floor = Floor {
            lines: value(line, lines)?,
            branches: value(line, branches)?,
        };
        if floor == Floor::default() {
            return Err(FloorsError::NoFloor {
                line,
                krate: krate.to_owned(),
            });
        }
        for p in [floor.lines, floor.branches].into_iter().flatten() {
            if p >= Percent::TARGET {
                return Err(FloorsError::NotBelowTarget {
                    line,
                    krate: krate.to_owned(),
                    value: p,
                });
            }
        }
        if out.insert(krate.to_owned(), floor).is_some() {
            return Err(FloorsError::Duplicate {
                line,
                krate: krate.to_owned(),
            });
        }
    }
    Ok(out)
}

pub(crate) const HEADER: &str = "\
# Coverage floors, checked by the CI test job (crates/kbf-coverage). The pull-request
# gate is 99.00% of lines and 99.00% of branches in every crate, measured on the fast
# tests. A crate under 99% on either is listed here with the floor it may not fall
# below, as a percentage floored to hundredths; `-` means that metric has no floor and
# must reach 99.00%. A crate at 99% or more on both is not listed.
#
# A floor may only rise. `cargo run -p kbf-coverage -- raise` lifts floors to a
# measurement (a line is dropped once both metrics reach 99%) and never adds or lowers
# one, and CI fails a pull request whose floors are lower than its base's. Each listed
# crate has an open issue to reach 99%.
# crate lines-floor branches-floor
";

/// Writes a floors file, crates in name order, values aligned.
pub fn render(floors: &Floors) -> String {
    let width = floors.keys().map(String::len).max().unwrap_or(0);
    let show = |p: Option<Percent>| crate::show(p);
    let mut out = String::from(HEADER);
    for (krate, f) in floors {
        let _ = writeln!(
            out,
            "{krate:<width$} {:>6} {:>6}",
            show(f.lines),
            show(f.branches)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Option<Percent> {
        Some(Percent::parse(text).expect(text))
    }

    /// Catches: a writer and reader that disagree, so floors `raise` wrote would not
    /// read back as the same values, or the header's comments would be read as data.
    #[test]
    fn render_then_parse_round_trips() {
        let mut f = Floors::new();
        f.insert(
            "kbf-state".into(),
            Floor {
                lines: p("72.58"),
                branches: p("77.48"),
            },
        );
        f.insert(
            "kbf-it".into(),
            Floor {
                lines: None,
                branches: p("97.43"),
            },
        );
        let text = render(&f);
        assert!(text.contains("kbf-it         -  97.43\n"), "{text}");
        assert_eq!(parse(&text), Ok(f));
        assert_eq!(parse(&render(&Floors::new())), Ok(Floors::new()));
    }

    /// Catches: a malformed file taken for a floor, in particular a floor that would
    /// weaken the gate (at or above 99, which `max` would not need) or that sets none.
    #[test]
    fn malformed_files_are_refused_with_the_line() {
        let err = |text: &str| parse(text).expect_err(text);
        assert_eq!(
            err("# c\nkbf-a 90.00"),
            FloorsError::Fields { line: 2, found: 2 }
        );
        assert_eq!(
            err("kbf-a 90.00 - x"),
            FloorsError::Fields { line: 1, found: 4 }
        );
        for bad in ["90", "90.5", "x", "100.01", "+9.00"] {
            assert_eq!(
                err(&format!("kbf-a {bad} -")),
                FloorsError::Percent {
                    line: 1,
                    value: bad.into()
                }
            );
        }
        assert_eq!(
            err("kbf-a 90.00 90.00\nkbf-a 91.00 -"),
            FloorsError::Duplicate {
                line: 2,
                krate: "kbf-a".into()
            }
        );
        for at in ["99.00", "99.01", "100.00"] {
            let e = err(&format!("kbf-a - {at}"));
            assert_eq!(
                e,
                FloorsError::NotBelowTarget {
                    line: 1,
                    krate: "kbf-a".into(),
                    value: Percent::parse(at).expect("percent")
                }
            );
            assert!(e.to_string().contains("not below 99.00"), "{e}");
        }
        assert_eq!(
            err("\n\nkbf-a - -"),
            FloorsError::NoFloor {
                line: 3,
                krate: "kbf-a".into()
            }
        );
        // 98.99 is the highest floor.
        assert!(parse("kbf-a 98.99 -").is_ok());
    }

    /// Catches: the default threshold wrong, or a metric read from the other field.
    #[test]
    fn threshold_is_the_floor_else_99() {
        let f = Floor {
            lines: p("50.00"),
            branches: None,
        };
        assert_eq!(f.get(Metric::Lines), p("50.00"));
        assert_eq!(f.get(Metric::Branches), None);
        assert_eq!(f.threshold(Metric::Lines).to_string(), "50.00");
        assert_eq!(f.threshold(Metric::Branches).to_string(), "99.00");
        assert_eq!(Floor::default().threshold(Metric::Lines), Percent::TARGET);
    }
}
