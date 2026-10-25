//! A wrapper that fails or delays the calls of another [`Backend`], for tests.

use std::collections::{BTreeMap, VecDeque};
use std::iter::repeat_n;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{Backend, ByteRange, Error, Listed, Meta, PartBody};

/// The kinds of call a [`FaultyBackend`] tells apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    /// [`Backend::put`].
    Put,
    /// [`Backend::put_if_match`].
    PutIfMatch,
    /// [`Backend::get`].
    Get,
    /// [`Backend::head`].
    Head,
    /// [`Backend::delete`].
    Delete,
    /// [`Backend::list`].
    List,
    /// [`Backend::create_upload`].
    CreateUpload,
    /// [`Backend::upload_part`].
    UploadPart,
    /// [`Backend::complete_upload`].
    CompleteUpload,
    /// [`Backend::abort_upload`].
    AbortUpload,
}

/// The error of a call that fails because its backend is down or was told to fail it.
fn unavailable(op: Op) -> Error {
    Error::Unavailable(format!("{op:?} failed on purpose"))
}

/// An error for the next call of a kind on a key with a given prefix.
struct Rule {
    op: Op,
    prefix: String,
    /// Taken by the call that fails; a rule without one is used up.
    error: Option<Error>,
}

impl Rule {
    fn applies(&self, op: Op, key: &str) -> bool {
        let matched = (self.op, key.starts_with(&self.prefix), self.error.is_some());
        matched == (op, true, true)
    }
}

#[derive(Default)]
struct Plan {
    /// The errors the next calls of a kind fail with, one for each, the first one first.
    fail: BTreeMap<Op, VecDeque<Error>>,
    /// The errors for single calls chosen by their key.
    keyed: Vec<Rule>,
    /// How long a call of a kind waits before it runs.
    delay: BTreeMap<Op, Duration>,
    /// Whether every call fails.
    down: bool,
    /// How many calls of a kind were made, failed or not.
    calls: BTreeMap<Op, u64>,
    /// The prefix of every list call, failed or not, in the order of the calls.
    listed: Vec<String>,
}

/// Passes every call to the backend it wraps, unless it was told to fail or delay it.
///
/// A failed call returns [`Error::Unavailable`] without reaching the wrapped backend.
pub struct FaultyBackend {
    inner: Arc<dyn Backend>,
    plan: Mutex<Plan>,
}

impl FaultyBackend {
    /// A wrapper that passes everything through.
    #[must_use]
    pub fn new(inner: Arc<dyn Backend>) -> Self {
        Self {
            inner,
            plan: Mutex::new(Plan::default()),
        }
    }

    /// Fails the next `count` calls of kind `op` with [`Error::Unavailable`], in place of the
    /// failures planned for that kind before.
    pub fn fail_next(&self, op: Op, count: u32) {
        let errors: VecDeque<Error> = repeat_n(unavailable(op), count as usize).collect();
        self.plan().fail.insert(op, errors);
    }

    /// Fails one more call of kind `op`, after those already planned to fail, with `error`.
    pub fn fail_next_with(&self, op: Op, error: Error) {
        self.plan().fail.entry(op).or_default().push_back(error);
    }

    /// Fails the next call of kind `op` on a key that starts with `key_prefix` with `error`
    /// (for a list call, the key is the prefix listed). Other calls, and later calls on such
    /// keys, are not affected.
    pub fn fail_next_on(&self, op: Op, key_prefix: &str, error: Error) {
        let rule = Rule {
            op,
            prefix: key_prefix.to_owned(),
            error: Some(error),
        };
        self.plan().keyed.push(rule);
    }

    /// Fails every call while `down`, whatever its kind.
    pub fn set_down(&self, down: bool) {
        self.plan().down = down;
    }

    /// Makes every call of kind `op` wait `delay` (tokio time) before it runs, or stops
    /// doing so for `None`.
    pub fn set_delay(&self, op: Op, delay: Option<Duration>) {
        let mut plan = self.plan();
        match delay {
            Some(delay) => plan.delay.insert(op, delay),
            None => plan.delay.remove(&op),
        };
    }

    /// How many calls of kind `op` were made, the failed ones included.
    #[must_use]
    pub fn calls(&self, op: Op) -> u64 {
        self.plan().calls.get(&op).copied().unwrap_or(0)
    }

    /// The prefix of every list call made, the failed ones included, in the order of the
    /// calls. A paged listing is a list call too.
    #[must_use]
    pub fn listed_prefixes(&self) -> Vec<String> {
        self.plan().listed.clone()
    }

    fn plan(&self) -> MutexGuard<'_, Plan> {
        // No method panics while holding the lock, so a poisoned plan is still whole.
        self.plan.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts the call, and says how long it is to wait and with what error it is to fail, if
    /// it is to. A planned failure is used up by the call even when the backend is down.
    fn arrive(&self, op: Op, key: &str) -> (Option<Duration>, Option<Error>) {
        let mut plan = self.plan();
        *plan.calls.entry(op).or_insert(0) += 1;
        let delay = plan.delay.get(&op).copied();
        let injected = match plan.fail.get_mut(&op) {
            Some(planned) => planned.pop_front(),
            None => None,
        };
        let keyed = plan
            .keyed
            .iter_mut()
            .find(|rule| rule.applies(op, key))
            .and_then(|rule| rule.error.take());
        let error = if plan.down {
            Some(unavailable(op))
        } else {
            injected.or(keyed)
        };
        (delay, error)
    }

    async fn enter(&self, op: Op, key: &str) -> Result<(), Error> {
        let (delay, error) = self.arrive(op, key);
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl Backend for FaultyBackend {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.enter(Op::Put, key).await?;
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.enter(Op::PutIfMatch, key).await?;
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.enter(Op::Get, key).await?;
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.enter(Op::Head, key).await?;
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.enter(Op::Delete, key).await?;
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.plan().listed.push(prefix.to_owned());
        self.enter(Op::List, prefix).await?;
        self.inner.list(prefix).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.enter(Op::CreateUpload, key).await?;
        self.inner.create_upload(key).await
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.enter(Op::UploadPart, key).await?;
        self.inner.upload_part(key, id, number, body).await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.enter(Op::CompleteUpload, key).await?;
        self.inner.complete_upload(key, id, parts).await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        self.enter(Op::AbortUpload, key).await?;
        self.inner.abort_upload(key, id).await
    }
}
