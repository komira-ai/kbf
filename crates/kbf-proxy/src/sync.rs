//! The primitives the slot state machine is built from, behind one name so the same code runs
//! under the `loom` model checker (`--cfg kbf_loom`, `tools/ci/proxy-loom.sh`) and under
//! std.
//!
//! Only `slot.rs` takes its atomics from here. Everything else in the crate (hold queue,
//! pool, serve) is plain std and tokio and is not model-checked.

#[cfg(not(kbf_loom))]
pub(crate) use std::sync::atomic;

#[cfg(kbf_loom)]
pub(crate) use loom::sync::atomic;

/// The hold doorbell: a number tasks sleep on, written only while the epoch moves.
///
/// Under std it is a tokio `watch` channel. Under loom it is a mutex around the number: loom
/// cannot see inside tokio, and a std lock taken around a loom atomic would deadlock the
/// model.
#[cfg(not(kbf_loom))]
#[derive(Debug)]
pub(crate) struct Doorbell(tokio::sync::watch::Sender<u64>);

#[cfg(not(kbf_loom))]
impl Doorbell {
    pub(crate) fn new() -> Self {
        Self(tokio::sync::watch::channel(0).0)
    }

    /// Writes `next()` as the new value and wakes every sleeper. `next` runs inside the
    /// channel's lock, so two rings write their numbers in the order they took them.
    pub(crate) fn ring(&self, next: impl FnOnce() -> u64) {
        self.0.send_modify(|value| *value = next());
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.0.subscribe()
    }
}

#[cfg(kbf_loom)]
#[derive(Debug)]
pub(crate) struct Doorbell(loom::sync::Mutex<u64>);

#[cfg(kbf_loom)]
impl Doorbell {
    pub(crate) fn new() -> Self {
        Self(loom::sync::Mutex::new(0))
    }

    pub(crate) fn ring(&self, next: impl FnOnce() -> u64) {
        // The lock first, then the epoch: `*lock() = next()` would run `next` before the lock.
        let mut value = self.0.lock().expect("the model does not poison");
        *value = next();
    }

    /// The number a sleeper would read now.
    pub(crate) fn value(&self) -> u64 {
        *self.0.lock().expect("the model does not poison")
    }
}
