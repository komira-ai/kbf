//! Debian version ordering, as `dpkg --compare-versions` does it (deb-version(7)).
//!
//! A version is `[epoch:]upstream[-revision]`. The epoch is compared as a number; the
//! upstream version and the revision are compared by [`verrevcmp`]: alternating runs of
//! non-digits (compared character by character, `~` before the end of the text, the end
//! before a letter, a letter before any other character) and digits (compared as numbers).

use std::cmp::Ordering;

/// A version that does not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DebVersionError {
    #[error("empty version")]
    Empty,
    #[error("a character outside the Debian version alphabet")]
    BadChar,
    #[error("the epoch is not a number")]
    BadEpoch,
    #[error("the upstream version is empty")]
    EmptyUpstream,
    #[error("the upstream version does not start with a digit")]
    UpstreamNotDigit,
    #[error("the revision is empty")]
    EmptyRevision,
}

/// A parsed Debian version. Equality is the ordering's: `1.0` and `01.0` are equal.
#[derive(Debug, Clone)]
pub struct DebVersion {
    epoch: u64,
    upstream: String,
    revision: String,
}

impl DebVersion {
    /// Parse `text`.
    ///
    /// # Errors
    /// [`DebVersionError`] for text dpkg would refuse as a version.
    pub fn parse(text: &str) -> Result<Self, DebVersionError> {
        if text.is_empty() {
            return Err(DebVersionError::Empty);
        }
        if !text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+~:-".contains(&b))
        {
            return Err(DebVersionError::BadChar);
        }
        let (epoch, rest) = match text.split_once(':') {
            Some((epoch, rest)) => {
                let digits = !epoch.is_empty() && epoch.bytes().all(|b| b.is_ascii_digit());
                let epoch = epoch.parse().ok().filter(|_| digits);
                (epoch.ok_or(DebVersionError::BadEpoch)?, rest)
            }
            None => (0, text),
        };
        // The revision starts after the last hyphen.
        let (upstream, revision) = rest.rsplit_once('-').unwrap_or((rest, ""));
        if upstream.is_empty() {
            return Err(DebVersionError::EmptyUpstream);
        }
        if !upstream.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(DebVersionError::UpstreamNotDigit);
        }
        if revision.is_empty() && upstream.len() != rest.len() {
            return Err(DebVersionError::EmptyRevision);
        }
        Ok(Self {
            epoch,
            upstream: upstream.to_owned(),
            revision: revision.to_owned(),
        })
    }
}

/// The weight of a character in a run of non-digits: `~` is below the end of the text
/// (0), the end is below a letter, a letter is below any other character. `None` is the
/// end of the text and a digit.
fn order(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(b'~') => -1,
        Some(c) if c.is_ascii_alphabetic() => i32::from(c),
        Some(c) => i32::from(c) + 256,
    }
}

/// The first byte of `text` when it is not a digit.
fn non_digit(text: &[u8]) -> Option<u8> {
    text.first().copied().filter(|c| !c.is_ascii_digit())
}

/// The length of the digit run at the start of `text`.
fn digit_run(text: &[u8]) -> usize {
    text.iter().take_while(|c| c.is_ascii_digit()).count()
}

/// `digits` without its leading zeros.
fn without_zeros(digits: &[u8]) -> &[u8] {
    digits
        .get(digits.iter().take_while(|&&c| c == b'0').count()..)
        .unwrap_or_default()
}

/// dpkg's `verrevcmp`.
fn verrevcmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    while !a.is_empty() || !b.is_empty() {
        while non_digit(a).is_some() || non_digit(b).is_some() {
            let unequal = order(non_digit(a)).cmp(&order(non_digit(b)));
            if unequal != Ordering::Equal {
                return unequal;
            }
            // Equal weights: both are the same non-digit (a weight of 0 never keeps
            // this loop going), so both advance.
            a = a.get(1..).unwrap_or_default();
            b = b.get(1..).unwrap_or_default();
        }
        // A run of digits is a number: without leading zeros the longer is the greater,
        // and the same length is decided by the first digit that differs.
        let (run_a, run_b) = (digit_run(a), digit_run(b));
        let (num_a, num_b) = (
            without_zeros(a.get(..run_a).unwrap_or_default()),
            without_zeros(b.get(..run_b).unwrap_or_default()),
        );
        let unequal = num_a.len().cmp(&num_b.len()).then_with(|| num_a.cmp(num_b));
        if unequal != Ordering::Equal {
            return unequal;
        }
        a = a.get(run_a..).unwrap_or_default();
        b = b.get(run_b..).unwrap_or_default();
    }
    Ordering::Equal
}

impl Ord for DebVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| verrevcmp(&self.upstream, &other.upstream))
            .then_with(|| verrevcmp(&self.revision, &other.revision))
    }
}

impl PartialOrd for DebVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for DebVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for DebVersion {}

#[cfg(test)]
mod tests;
