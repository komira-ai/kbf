//! The end of a GitHub list read page by page (`?per_page=100&page=N`).
//!
//! A list ends at its first short page (fewer than [`PER_PAGE`] entries, none at all
//! included). Two answers end the read with an error instead, so a loop never asks for
//! pages without end and spends the API's rate limit:
//!
//! - a full page equal to the page before it: the endpoint ignores `page=` and gives the
//!   same answer each time (GitHub's `git/matching-refs` does, which is why the release
//!   tags are read in one call);
//! - a full page at page [`MAX_PAGES`]: more entries than any list xtask reads holds.

use crate::Error;

/// Entries GitHub lists on one page, the most it allows.
pub const PER_PAGE: usize = 100;

/// The most pages one list is read for: 10,000 entries.
pub const MAX_PAGES: usize = 100;

/// Where a page-by-page read of the list at `what` stands.
#[derive(Debug)]
pub struct Pages<T> {
    what: String,
    read: usize,
    previous: Vec<T>,
}

impl<T: PartialEq + Clone> Pages<T> {
    /// A read of the list at `what` (the path without its paging, named in errors).
    pub fn new(what: impl Into<String>) -> Pages<T> {
        Pages {
            what: what.into(),
            read: 0,
            previous: Vec::new(),
        }
    }

    /// The number of the page to read next, from 1.
    pub fn number(&self) -> usize {
        self.read + 1
    }

    /// Takes the page just read, `got`: whether another page follows. A short page is the
    /// last; a full page that repeats the one before it, or a full page [`MAX_PAGES`], is an
    /// error.
    pub fn more(&mut self, got: &[T]) -> Result<bool, Error> {
        self.read += 1;
        let (what, n) = (&self.what, self.read);
        if got.len() < PER_PAGE {
            return Ok(false);
        }
        if got == self.previous.as_slice() {
            return Err(Error::Failed(format!(
                "gh api {what}: page {n} is page {} again: the endpoint does not page \
                 with `page=`, so the list has no end to read to",
                n - 1
            )));
        }
        if n == MAX_PAGES {
            return Err(Error::Failed(format!(
                "gh api {what}: page {n} is still full ({PER_PAGE} entries), the most pages \
                 read of one list: stopped rather than read on"
            )));
        }
        self.previous = got.to_vec();
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
