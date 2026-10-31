//! What a provision run made, kept in the result directory (`provision-state.json`).
//!
//! `provision` may only change a bucket or an account it created itself. A store cannot say who
//! made a bucket, so the run writes down each bucket and account it makes, as it makes it, and
//! treats one that is already on the store and not written down as someone else's: the verb stops
//! before touching it. The same record makes a second `--apply` of the same run safe: its own
//! bucket and account are found again and kept, and a failed live-bucket probe removes only the
//! accounts listed here.
//!
//! A `--premade` run (see [`crate::provision::Supply`]) makes nothing, but writes down the
//! pre-made bucket and account once the probes have passed, with a `premade` marker, so the
//! teardown's guard finds them as it finds the ones a run made.
//!
//! The file names buckets and account ids, so it is private like `ops.json`; nothing prints it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::error::BenchError;
use crate::preflight::mc::Mc;

/// The file of the result directory the record is kept in.
pub const STATE_FILE: &str = "provision-state.json";

/// The names a run has made.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Made {
    buckets: BTreeSet<String>,
    accounts: BTreeSet<String>,
    /// Set when a `--premade` run wrote the record: the bucket and account were made by a held
    /// script. A record of a run that made its own carries no such key.
    #[serde(default, skip_serializing_if = "is_false")]
    premade: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The record of one run's provisioning.
#[derive(Debug)]
pub struct State {
    path: PathBuf,
    prefix: String,
    made: Made,
}

impl State {
    /// The record in `dir`, or an empty one if the run has made nothing yet. `prefix` is the
    /// run's test prefix: a name without it is never reported as made by the run, whatever the
    /// file says.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the file exists and cannot be read, [`BenchError::Ops`] if it is not
    /// a record (a record that cannot be read is not an empty one: the run would take its own
    /// buckets for foreign ones, or worse the other way round).
    pub fn load(dir: &Path, prefix: &str) -> Result<Self, BenchError> {
        let path = dir.join(STATE_FILE);
        let made = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|_| BenchError::Ops(format!("{STATE_FILE} is not a provision record")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Made::default(),
            Err(source) => return Err(BenchError::Io { path, source }),
        };
        Ok(Self {
            path,
            prefix: prefix.to_owned(),
            made,
        })
    }

    /// Whether the record was written by a `--premade` run.
    #[must_use]
    pub fn is_premade(&self) -> bool {
        self.made.premade
    }

    /// Whether the record names any bucket or account.
    #[must_use]
    pub fn has_entries(&self) -> bool {
        !self.made.buckets.is_empty() || !self.made.accounts.is_empty()
    }

    /// Whether this run made the bucket `name`.
    #[must_use]
    pub fn made_bucket(&self, name: &str) -> bool {
        name.starts_with(&self.prefix) && self.made.buckets.contains(name)
    }

    /// Whether this run made the account `id`.
    #[must_use]
    pub fn made_account(&self, id: &str) -> bool {
        id.starts_with(&self.prefix) && self.made.accounts.contains(id)
    }

    /// Writes the record down, atomically, so a crash leaves the old one or the new one.
    fn save(&self) -> Result<(), BenchError> {
        let io = |source| BenchError::Io {
            path: self.path.clone(),
            source,
        };
        let mut text = serde_json::to_string_pretty(&self.made).expect("a record serialises");
        text.push('\n');
        std::fs::create_dir_all(self.path.parent().unwrap_or(Path::new(""))).map_err(io)?;
        atomic::write(&self.path, text.as_bytes()).map_err(io)
    }

    /// Records that this run made the bucket `name`, and saves.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the record cannot be saved.
    pub fn record_bucket(&mut self, name: &str) -> Result<(), BenchError> {
        self.made.buckets.insert(name.to_owned());
        self.save()
    }

    /// Records that this run made the account `id`, and saves.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the record cannot be saved.
    pub fn record_account(&mut self, id: &str) -> Result<(), BenchError> {
        self.made.accounts.insert(id.to_owned());
        self.save()
    }

    /// Records the pre-made bucket and account of one role, marks the record `premade`, and
    /// saves. Called once the probes have passed, so the held teardown finds what to remove.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the record cannot be saved.
    pub fn record_premade(&mut self, bucket: &str, id: &str) -> Result<(), BenchError> {
        self.made.premade = true;
        self.made.buckets.insert(bucket.to_owned());
        self.made.accounts.insert(id.to_owned());
        self.save()
    }

    /// Removes the account `id` from the store, if this run made it and it carries the test
    /// prefix, and from the record. Any other account is left alone and the call is an error
    /// that runs no command.
    ///
    /// # Errors
    /// [`BenchError::Ops`] for an account this run did not make, or if the command fails;
    /// [`BenchError::Io`] if the record cannot be saved.
    pub fn remove_account(&mut self, mc: &Mc<'_>, id: &str) -> Result<(), BenchError> {
        if !self.made_account(id) {
            return Err(BenchError::Ops(
                "the account was not made by this run, or lacks the test prefix; it is left in place"
                    .to_owned(),
            ));
        }
        mc.remove_account(id)?;
        self.made.accounts.remove(id);
        self.save()
    }
}
