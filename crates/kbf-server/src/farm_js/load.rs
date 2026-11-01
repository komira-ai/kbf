//! The load of the store at a start, with the checks `js-store` leaves to its embedder.
//!
//! `js-store` reads the rows the scheduler needs and does not know what the work is: the dedup
//! text of a floor row is any text, and the name a key floor gives its size summary any name.
//! This server's reader refuses or skips what is not the build server's own, and [`load`] does
//! the same:
//!
//! - a `floor/` row whose dedup text is not an action's digest is refused
//!   ([`StateError::Corrupt`], naming the row);
//! - a key floor whose size summary name does not parse is kept without the name (the action
//!   starts cold), and the row is logged.
//!
//! A request needs no check of its own: the codec reads only requests for actions.

use js_store::{Rows, StateError, Stored, keys};
use kbf_estimator::Key;
use kbf_types::{Digest, DigestFunction};

use crate::farm::durable::estimate;

/// What the store holds, as the farm server reads it.
#[derive(Debug)]
pub struct Loaded {
    /// The rows [`Rows::load`] read, checked. Its `key_names` is empty: the names are
    /// parsed in `summaries`.
    pub stored: Stored,
    /// The size summary each key floor decays with, for those whose row names one that
    /// parses: the size key and the summary's key.
    pub summaries: Vec<(u64, Key)>,
}

/// What `rows` holds, after the checks above.
///
/// # Errors
/// What [`Rows::load`] returns, and [`StateError::Corrupt`] for a floor row the farm server's
/// reader refuses.
pub fn load(rows: &Rows) -> Result<Loaded, StateError> {
    let mut stored = rows.load()?;
    for floor in &stored.floors {
        if Digest::parse(DigestFunction::Sha256, &floor.key.dedup).is_err() {
            return Err(StateError::Corrupt {
                key: keys::floor(&floor.key),
                why: "not a digest".to_owned(),
            });
        }
    }
    let mut summaries = Vec::new();
    for (floor, name) in std::mem::take(&mut stored.key_names) {
        match estimate::parse_key(&name) {
            Some(key) => summaries.push((floor, key)),
            None => {
                let row = keys::key_floor(floor);
                tracing::warn!(%row, "a key floor names a size summary that does not parse");
            }
        }
    }
    Ok(Loaded { stored, summaries })
}
