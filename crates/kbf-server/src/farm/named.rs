//! Answering a client that asks for an operation by its name, `operations/<id>`
//! (WaitExecution, GetOperation), on any frontend of a state store.
//!
//! The ids of a farm over a store are the store's, so a name means one operation on every
//! frontend and across restarts. A frontend answers a name:
//!
//! - **from its scheduler**, if the scheduler holds the operation (queued, leased on one of
//!   this frontend's nodes, or finished within the finished retention): through the channel
//!   of the first caller it holds for it, which is shown the changes as they happen;
//! - **from the store** otherwise: the `op/`, `queue/`, `done/` and `doneof/` rows say what
//!   the operation runs and where it stands (`Durable::named_operation`), and the channel of
//!   the operation is shown what each intake poll finds ([`Engine::follow_watched`]), by the
//!   look that `Farm::follow` makes for the callers of a retired frontend (`Progress::stage`). An
//!   operation is answered as long as its `done/` row is there, which is at least
//!   [`DONE_FLOOR`](super::DONE_FLOOR) after it finished.
//!
//! A farm with no state store has no ids that mean one operation across restarts (every
//! process numbers from 0), and answers no name of this form.

use kbf_front::{Stage, Ticket};
use kbf_types::OperationId;
use tokio::sync::watch;

use super::State;
use super::engine::{Adapter, Engine};
use super::handover::show;
use crate::machine::stored_name;

impl State {
    /// A ticket on the operation named `operations/<operation>`: `None` if the farm has no
    /// state store, or neither the scheduler nor the store has a trace of the operation (it
    /// never existed, or its result is past its retention). The store is read, under the
    /// lock, only for an operation the scheduler does not hold.
    pub(super) fn stored_ticket(&mut self, operation: OperationId) -> Option<Ticket> {
        self.engine.durable.as_ref()?;
        let held = self.held_ticket(operation);
        held.or_else(|| self.watch(operation))
    }

    /// A ticket on `operation` through the channel of the first caller the scheduler holds
    /// for it, if the scheduler holds it.
    fn held_ticket(&self, operation: OperationId) -> Option<Ticket> {
        let (first, waiter) = self.side.first_waiter(&self.engine.sched, operation)?;
        self.side.stages.get(&first).map(|stage| Ticket {
            name: stored_name(operation),
            instance: waiter.key.instance.clone(),
            action: waiter.key.action,
            stage: stage.subscribe(),
        })
    }

    /// A ticket on `operation` through a channel of its own, shown what the store says of
    /// the operation now and at each intake poll while a ticket holds it. `None` if the store
    /// has no trace of the operation, or cannot be read.
    fn watch(&mut self, operation: OperationId) -> Option<Ticket> {
        let mut looked = None;
        self.engine.durably("an operation asked for by name", |d| {
            looked = d.named_operation(operation)?;
            Ok(())
        });
        let (key, progress) = looked?;
        let stage = progress.stage().unwrap_or(Stage::Queued);
        let watched = &mut self.side.watched;
        let entry = watched.entry(operation);
        let channel = entry.or_insert_with(|| watch::channel(stage.clone()).0);
        show(channel, stage);
        Some(Ticket {
            name: stored_name(operation),
            instance: key.instance,
            action: key.action,
            stage: channel.subscribe(),
        })
    }
}

impl Engine {
    /// Shows each watched operation what the store says of it now. A watch nobody holds a
    /// ticket on any more is dropped, and one whose result has been shown is not looked at
    /// again: its tickets end with it. A look the store fails is made again at the next poll,
    /// and an operation the store has no trace of is shown nothing, so its tickets wait, as
    /// the callers of an operation the store lost do.
    pub(super) fn follow_watched(&mut self, a: &mut dyn Adapter) {
        for operation in a.watched() {
            let mut looked = None;
            self.durably("a watched operation", |d| {
                looked = Some(d.progress(operation)?);
                Ok(())
            });
            if let Some(stage) = looked.and_then(|progress| progress.stage()) {
                a.show_watched(operation, &stage);
            }
        }
    }
}
