//! Placeholder: the slot that serves is always the old one.

use super::super::model::{Intent, Slot};

#[allow(dead_code)]
pub(super) fn active_after(intent: &Intent, _new_holds: impl FnOnce() -> bool) -> Slot {
    intent.old_slot
}
