//! Memory and CPUs booked by action category
//! (`docs/platform-properties.md#memory-by-category`,
//! `docs/platform-properties.md#cpus-by-category`).
//!
//! An action that names no `kbf-book-mem-gib` books 1 GiB, and one that names no
//! `kbf-book-cpus` books one core. A client may say what kind of action it is with the
//! environment variable [`CATEGORY_ENV`] in its `Command` (Buck2's action `category`,
//! or any name the build chooses); the server's `--book-mem-gib-by-category` table
//! ([`MemoryByCategory`]) and `--book-cpus-by-category` table ([`CpusByCategory`]) then
//! name the memory and the cores that kind of action books instead. The front reads the
//! category ([`category`]); the dispatcher books it, no more than the largest node that
//! could run the action holds.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use kbf_proto::reapi::Command;

/// The `Command` environment variable that names an action's category.
pub const CATEGORY_ENV: &str = "KBF_ACTION_CATEGORY";

/// One GiB, the unit of the memory table.
const GIB: u64 = 1 << 30;

/// One core in thousandths of a core, the unit of the CPU table.
const CORE_MILLIS: u64 = 1_000;

/// The category `command` names in [`CATEGORY_ENV`]: the first such variable's value,
/// `None` when it names none or an empty one. The name is compared exactly.
#[must_use]
pub fn category(command: &Command) -> Option<String> {
    command
        .environment_variables
        .iter()
        .find(|v| v.name == CATEGORY_ENV)
        .map(|v| v.value.clone())
        .filter(|v| !v.is_empty())
}

/// The memory, in bytes, each action category books when the action names no size:
/// `--book-mem-gib-by-category`, written `<category>=<GiB>,...`. Empty by default, so
/// every action books 1 GiB unless it names a size.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryByCategory(BTreeMap<String, u64>);

impl MemoryByCategory {
    /// The bytes `category` books, if the table names it.
    #[must_use]
    pub fn get(&self, category: &str) -> Option<u64> {
        self.0.get(category).copied()
    }
}

/// The CPUs, in thousandths of a core, each action category books when the action names
/// no size: `--book-cpus-by-category`, written `<category>=<cores>,...`. Empty by
/// default, so every action books one core unless it names a size.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpusByCategory(BTreeMap<String, u64>);

impl CpusByCategory {
    /// The thousandths of a core `category` books, if the table names it.
    #[must_use]
    pub fn get(&self, category: &str) -> Option<u64> {
        self.0.get(category).copied()
    }
}

/// Why a `--book-mem-gib-by-category` or `--book-cpus-by-category` value is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CategoryTableError {
    /// An entry is not `<category>=<size>`.
    #[error("{0:?} is not <category>=<size>")]
    Entry(String),
    /// A category is empty or holds whitespace.
    #[error("{0:?} is not a category name: one or more characters, none of them whitespace")]
    Name(String),
    /// A size is not a whole number of `unit` (GiB or cores) from 1 to the most a
    /// booking holds.
    #[error("{category}={value:?}: not a whole number of {unit} from 1 to {max} in plain digits")]
    Size {
        category: String,
        value: String,
        unit: &'static str,
        max: u64,
    },
    /// A category is named twice.
    #[error("{0:?} is named twice")]
    Repeated(String),
}

/// Reads `<category>=<n>` entries separated by commas into `category -> n * scale`
/// (`unit` names what `n` counts in an error). The empty string is the empty table. A
/// size is written as `kbf-book-mem-gib` is: plain digits, no sign, no leading zero, at
/// least 1.
fn read_table(
    text: &str,
    scale: u64,
    unit: &'static str,
) -> Result<BTreeMap<String, u64>, CategoryTableError> {
    let mut table = BTreeMap::new();
    for entry in text.split(',').filter(|_| !text.is_empty()) {
        let (name, value) = entry
            .split_once('=')
            .ok_or_else(|| CategoryTableError::Entry(entry.to_owned()))?;
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(CategoryTableError::Name(name.to_owned()));
        }
        let size = Some(value)
            .filter(|v| v.starts_with(|c: char| ('1'..='9').contains(&c)))
            .and_then(|v| v.parse::<u64>().ok())
            .and_then(|n| n.checked_mul(scale))
            .ok_or_else(|| CategoryTableError::Size {
                category: name.to_owned(),
                value: value.to_owned(),
                unit,
                max: u64::MAX / scale,
            })?;
        if table.insert(name.to_owned(), size).is_some() {
            return Err(CategoryTableError::Repeated(name.to_owned()));
        }
    }
    Ok(table)
}

/// `table` as the flags take it, each size in units of `scale`.
fn write_table(table: &BTreeMap<String, u64>, scale: u64, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let entries: Vec<String> = table
        .iter()
        .map(|(name, size)| format!("{name}={}", size / scale))
        .collect();
    f.write_str(&entries.join(","))
}

impl FromStr for MemoryByCategory {
    type Err = CategoryTableError;

    /// Reads `<category>=<GiB>` entries separated by commas.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        read_table(text, GIB, "GiB").map(Self)
    }
}

impl fmt::Display for MemoryByCategory {
    /// The table as `--book-mem-gib-by-category` takes it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_table(&self.0, GIB, f)
    }
}

impl FromStr for CpusByCategory {
    type Err = CategoryTableError;

    /// Reads `<category>=<cores>` entries separated by commas.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        read_table(text, CORE_MILLIS, "cores").map(Self)
    }
}

impl fmt::Display for CpusByCategory {
    /// The table as `--book-cpus-by-category` takes it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_table(&self.0, CORE_MILLIS, f)
    }
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::command::EnvironmentVariable;

    use super::*;

    fn env(vars: &[(&str, &str)]) -> Command {
        Command {
            environment_variables: vars
                .iter()
                .map(|(name, value)| EnvironmentVariable {
                    name: (*name).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
            ..Default::default()
        }
    }

    /// Catches: the category read from another variable, or from the variable in
    /// another case; an empty value taken as a category; and a second variable of the
    /// name read instead of the first.
    #[test]
    fn the_category_is_the_first_variable_of_its_exact_name() {
        assert_eq!(category(&env(&[])), None);
        assert_eq!(
            category(&env(&[("PATH", "/bin"), (CATEGORY_ENV, "rustc")])),
            Some("rustc".to_owned())
        );
        assert_eq!(category(&env(&[("kbf_action_category", "rustc")])), None);
        assert_eq!(category(&env(&[(CATEGORY_ENV, "")])), None);
        assert_eq!(
            category(&env(&[(CATEGORY_ENV, "a"), (CATEGORY_ENV, "b")])),
            Some("a".to_owned())
        );
    }

    /// Catches: a size read in GiB as bytes (or not at all), an empty flag refused
    /// instead of read as the empty table, and the table written back in another form
    /// than it is read.
    #[test]
    fn a_table_is_read_in_gib_and_written_back_as_read() {
        let table: MemoryByCategory = "mojo_build_test=4,rustc=6".parse().expect("a table");
        assert_eq!(table.get("mojo_build_test"), Some(4 * GIB));
        assert_eq!(table.get("rustc"), Some(6 * GIB));
        assert_eq!(table.get("c_compile"), None);
        assert_eq!(table.to_string(), "mojo_build_test=4,rustc=6");
        let empty: MemoryByCategory = "".parse().expect("the empty table");
        assert_eq!(empty, MemoryByCategory::default());
        assert_eq!(empty.to_string(), "");
    }

    /// Catches: a booking of zero, a second spelling of a size (`+4`, `04`: one
    /// booking under two names), a size that overflows, a nameless or spaced
    /// category, an entry with no size, and a category named twice accepted.
    #[test]
    fn a_bad_table_is_refused() {
        let too_much = (u64::MAX / GIB + 1).to_string();
        let max = u64::MAX / GIB;
        let size = |value: &str| CategoryTableError::Size {
            category: "a".to_owned(),
            value: value.to_owned(),
            unit: "GiB",
            max,
        };
        for (text, want) in [
            ("a=0".to_owned(), size("0")),
            ("a=+4".to_owned(), size("+4")),
            ("a=04".to_owned(), size("04")),
            ("a=1.5".to_owned(), size("1.5")),
            ("a=".to_owned(), size("")),
            (format!("a={too_much}"), size(&too_much)),
            ("=4".to_owned(), CategoryTableError::Name(String::new())),
            (
                "a b=4".to_owned(),
                CategoryTableError::Name("a b".to_owned()),
            ),
            ("a".to_owned(), CategoryTableError::Entry("a".to_owned())),
            ("a=4,".to_owned(), CategoryTableError::Entry(String::new())),
            (
                "a=4,a=8".to_owned(),
                CategoryTableError::Repeated("a".to_owned()),
            ),
        ] {
            assert_eq!(
                text.parse::<MemoryByCategory>(),
                Err(want.clone()),
                "{text}"
            );
            assert!(!want.to_string().is_empty());
        }
        assert!(format!("a={max}").parse::<MemoryByCategory>().is_ok());
    }

    /// Catches: a core count read as thousandths (or as GiB), so `mojo_build_test=4`
    /// would book 4 millicores or 4 GiB; the table written back in another form than it
    /// is read; the empty flag refused; and a bad size or a repeated category accepted
    /// (the refusal must name cores, not GiB).
    #[test]
    fn a_cpu_table_is_read_in_cores_and_written_back_as_read() {
        let table: CpusByCategory = "mojo_build_test=4,rustc=2".parse().expect("a table");
        assert_eq!(table.get("mojo_build_test"), Some(4_000));
        assert_eq!(table.get("rustc"), Some(2_000));
        assert_eq!(table.get("c_compile"), None);
        assert_eq!(table.to_string(), "mojo_build_test=4,rustc=2");
        let empty: CpusByCategory = "".parse().expect("the empty table");
        assert_eq!(empty, CpusByCategory::default());
        assert_eq!(empty.to_string(), "");
        let max = u64::MAX / 1_000;
        let size = |value: &str| CategoryTableError::Size {
            category: "a".to_owned(),
            value: value.to_owned(),
            unit: "cores",
            max,
        };
        for (text, want) in [
            ("a=0", size("0")),
            ("a=+4", size("+4")),
            ("a=04", size("04")),
            ("a=1.5", size("1.5")),
            ("a=", size("")),
            ("a=4,a=8", CategoryTableError::Repeated("a".to_owned())),
        ] {
            assert_eq!(text.parse::<CpusByCategory>(), Err(want), "{text}");
        }
        assert!(
            size("0").to_string().contains("cores"),
            "the refusal names its unit"
        );
        assert!(format!("a={max}").parse::<CpusByCategory>().is_ok());
        assert!(format!("a={}", max + 1).parse::<CpusByCategory>().is_err());
    }
}
