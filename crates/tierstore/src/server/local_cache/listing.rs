//! The listings of a cache: the upstream's, without the names a storage node keeps for itself.
//!
//! The keys under `_journal/` and `_tiering/` (a storage node's journal and its bookkeeping) are
//! left out of every listing, as a storage node leaves them out of its own. When the upstream is
//! a bucket that holds them, a client of the cache never sees them. A page that the filter cuts
//! short is made up from the next page of the upstream, because a caller takes a short page for
//! the last.
//!
//! Two things keep the cost of the filter down. A listing whose prefix is inside a reserved
//! prefix is empty and asks the upstream nothing. And when a page of the upstream ends in a
//! reserved range, the next page is asked for after the end of the range (`_journal/~`, which
//! sorts after every key under `_journal/` that the S3 backend can name, and before every key
//! that is not under `_journal/` and sorts after one), so a range of any size costs about one call
//! and not one for each thousand keys in it. The filter still applies to what comes back, so a
//! reserved key that sorts after the jump is left out as well; the jump never passes a key that
//! is not reserved.

use crate::journal::{JOURNAL_PREFIX, TIERING_PREFIX, is_reserved};
use crate::{Error, Listed, OpenUpload, UploadsAfter};

use super::Inner;

/// The most pages of the upstream one listing reads to make up for the names it leaves out.
pub(super) const LISTING_CALLS: u32 = 100;

/// The key to continue a listing after, when the last key the upstream gave is `last`: the end of
/// the reserved range `last` is in, or `last` itself if it is in none or already past that end.
fn continue_after(last: &str) -> String {
    for prefix in [JOURNAL_PREFIX, TIERING_PREFIX] {
        if last.starts_with(prefix) {
            let end = format!("{prefix}~");
            return if end.as_str() > last {
                end
            } else {
                last.to_owned()
            };
        }
    }
    last.to_owned()
}

impl Inner {
    /// Every object under `prefix` that is not under a reserved prefix.
    pub(super) async fn list_data(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        if is_reserved(prefix) {
            return Ok(Vec::new());
        }
        let mut found = self.upstream.list(prefix).await?;
        found.retain(|object| !is_reserved(&object.key));
        Ok(found)
    }

    /// A page of [`list_data`](Self::list_data): at most `max_keys` objects after `start_after`.
    pub(super) async fn list_data_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let mut found: Vec<Listed> = Vec::new();
        if is_reserved(prefix) {
            return Ok(found);
        }
        let mut after = start_after.map(str::to_owned);
        let mut calls = 0;
        while found.len() < max_keys {
            calls += 1;
            if calls > LISTING_CALLS {
                let text = format!("the upstream's listing took over {LISTING_CALLS} calls");
                return Err(Error::Unavailable(text));
            }
            let want = max_keys - found.len();
            let page = self
                .upstream
                .list_page(prefix, after.as_deref(), want)
                .await?;
            let full = page.len() >= want;
            after = page.last().map(|object| continue_after(&object.key));
            found.extend(page.into_iter().filter(|object| !is_reserved(&object.key)));
            if !full {
                break;
            }
        }
        Ok(found)
    }

    /// The open uploads under `prefix` whose key is not under a reserved prefix: at most
    /// `max_uploads` of them after the marker `after`.
    pub(super) async fn list_data_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let mut found: Vec<OpenUpload> = Vec::new();
        if is_reserved(prefix) {
            return Ok(found);
        }
        let mut after = after.map(|(key, id)| (key.to_owned(), id.map(str::to_owned)));
        let mut calls = 0;
        while found.len() < max_uploads {
            calls += 1;
            if calls > LISTING_CALLS {
                let text =
                    format!("the upstream's listing of uploads took over {LISTING_CALLS} calls");
                return Err(Error::Unavailable(text));
            }
            let want = max_uploads - found.len();
            let marker = after
                .as_ref()
                .map(|(key, id)| (key.as_str(), id.as_deref()));
            let page = self.upstream.list_uploads(prefix, marker, want).await?;
            let last = page.last().map(|up| (up.key.clone(), Some(up.id.clone())));
            let full = page.len() == want;
            found.extend(page.into_iter().filter(|up| !is_reserved(&up.key)));
            match last {
                Some(last) if full => {
                    // A full page that ends where the last one did would be asked for for ever.
                    if after.as_ref().is_some_and(|before| last <= *before) {
                        let text = "the upstream's listing of uploads did not advance";
                        return Err(Error::Unavailable(text.to_owned()));
                    }
                    let (key, id) = last;
                    let end = continue_after(&key);
                    // Past the end of a reserved range, every upload of the range is behind.
                    after = Some(if end == key { (key, id) } else { (end, None) });
                }
                _ => break,
            }
        }
        Ok(found)
    }
}
