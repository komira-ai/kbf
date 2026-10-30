//! The listings of a cache: the upstream's, without the names a storage node keeps for itself.
//!
//! The keys under `_journal/` and `_tiering/` (a storage node's journal and its bookkeeping) are
//! left out of every listing, as a storage node leaves them out of its own. When the upstream is
//! a bucket that holds them, a client of the cache never sees them. A page that the filter cuts
//! short is made up from the next page of the upstream, because a caller takes a short page for
//! the last.

use crate::journal::is_reserved;
use crate::{Error, Listed, OpenUpload, UploadsAfter};

use super::Inner;

/// The most pages of the upstream one listing reads to make up for the names it leaves out.
const LISTING_CALLS: u32 = 100;

impl Inner {
    /// Every object under `prefix` that is not under a reserved prefix.
    pub(super) async fn list_data(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
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
            after = page.last().map(|object| object.key.clone());
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
        let mut after = after.map(|(key, id)| (key.to_owned(), id.map(str::to_owned)));
        let mut found: Vec<OpenUpload> = Vec::new();
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
                    after = Some(last);
                }
                _ => break,
            }
        }
        Ok(found)
    }
}
