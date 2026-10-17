//! The subordinate id ranges of `/etc/subuid` and `/etc/subgid` (`subuid(5)`): reading
//! them, and choosing the block `package postinst` gives the `kbf` user.
//!
//! The rule is the one place the host scripts kept state (a range picked by hand to avoid
//! a neighbour's): the first block of [`BLOCK`] ids at or above [`FLOOR`] that overlaps no
//! range of any owner. A range of the user's own that is large enough stands; a smaller
//! one is refused and never widened, because widening it would move ids under files
//! already mapped to it.

use std::fmt;

/// Ids in the block `kbf` gets: what rootless `crun` maps for a container's users.
pub const BLOCK: u64 = 65536;

/// The first id a new block may start at, above the 100000 that `useradd` gives a login
/// user and the ranges container tools hand out.
pub const FLOOR: u64 = 200_000;

/// The ids stop here: a range ends at or below `2^32 - 1`, which is `(uid_t)-1`.
const LIMIT: u64 = 4_294_967_295;

/// One line of the file: `owner:start:count` (the owner is a user name or a uid).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    pub owner: String,
    pub start: u64,
    pub count: u64,
}

impl Range {
    /// The first id after the range.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.start + self.count
    }

    fn overlaps(&self, start: u64, count: u64) -> bool {
        self.start < start + count && start < self.end()
    }

    /// `start-last`, as `usermod --add-subuids` takes it.
    #[must_use]
    pub fn span(start: u64, count: u64) -> String {
        format!("{start}-{}", start + count - 1)
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.owner, self.start, self.count)
    }
}

/// Why the ranges could not be read, or no block could be chosen.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SubIdError {
    #[error("{file} line {line} is not owner:start:count: {text:?}")]
    Line {
        file: String,
        line: usize,
        text: String,
    },
    #[error(
        "{file} gives {range} only {} ids, fewer than the {BLOCK} that rootless crun maps: \
         widening it would move ids under files already mapped to it, so edit {file} by hand \
         (usermod --del-subuids / --add-subuids, or --del-subgids / --add-subgids)",
        .range.count
    )]
    Smaller { file: String, range: Range },
    #[error("{file} leaves no free block of {BLOCK} ids below 2^32")]
    Exhausted { file: String },
}

/// The ranges in `text`, the contents of `file` (named in errors). Blank lines and `#`
/// comments are skipped; any other line must be `owner:start:count` with a count above
/// zero and the range inside the id space. A line that is not is refused rather than
/// skipped: a range this reader missed would be one a new block could overlap.
pub fn parse(file: &str, text: &str) -> Result<Vec<Range>, SubIdError> {
    let mut ranges = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let bad = || SubIdError::Line {
            file: file.to_owned(),
            line: i + 1,
            text: line.to_owned(),
        };
        let fields: Vec<&str> = t.split(':').collect();
        let [owner, start, count] = fields[..] else {
            return Err(bad());
        };
        let (Ok(start), Ok(count)) = (start.parse::<u64>(), count.parse::<u64>()) else {
            return Err(bad());
        };
        if owner.is_empty() || count == 0 || start + count > LIMIT {
            return Err(bad());
        }
        ranges.push(Range {
            owner: owner.to_owned(),
            start,
            count,
        });
    }
    Ok(ranges)
}

/// What `kbf` needs from one of the two files.
#[derive(Debug, PartialEq, Eq)]
pub enum Need {
    /// The user has a range of at least [`BLOCK`] ids already.
    Present,
    /// Give the user the block starting here.
    Add(u64),
}

/// What `user` (and, when it exists, its `uid`) needs from the ranges of `file`.
pub fn decide(
    file: &str,
    ranges: &[Range],
    user: &str,
    uid: Option<u32>,
) -> Result<Need, SubIdError> {
    let uid = uid.map(|u| u.to_string());
    let mine: Vec<&Range> = ranges
        .iter()
        .filter(|r| r.owner == user || Some(&r.owner) == uid.as_ref())
        .collect();
    if mine.iter().any(|r| r.count >= BLOCK) {
        return Ok(Need::Present);
    }
    if let Some(range) = mine.into_iter().max_by_key(|r| r.count) {
        return Err(SubIdError::Smaller {
            file: file.to_owned(),
            range: range.clone(),
        });
    }
    first_free(ranges, FLOOR, BLOCK)
        .map(Need::Add)
        .ok_or_else(|| SubIdError::Exhausted {
            file: file.to_owned(),
        })
}

/// The lowest start at or above `floor` of `count` ids that overlaps none of `ranges`;
/// `None` when the id space runs out first.
#[must_use]
pub fn first_free(ranges: &[Range], floor: u64, count: u64) -> Option<u64> {
    let mut sorted: Vec<&Range> = ranges.iter().collect();
    sorted.sort_by_key(|r| (r.start, r.count));
    let mut start = floor;
    for r in sorted {
        if r.overlaps(start, count) {
            start = r.end();
        }
    }
    (start + count <= LIMIT).then_some(start)
}

#[cfg(test)]
#[path = "subid_tests.rs"]
mod tests;
