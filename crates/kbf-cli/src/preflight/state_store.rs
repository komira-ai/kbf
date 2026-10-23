//! The scheduler's state store: a frontend keeps its state in a state store, Postgres from
//! the next release, so a `kbf frontend` given no `--state-store` (and no `state-store` key
//! in `kbf.toml`) is refused here, before anything is bound or opened. `all-in-one` is given
//! the local Postgres by its parser when it names none, and the other modes hold no
//! scheduler state.

use super::Report;
use crate::Invocation;

const WHAT: &str = "--state-store is not set";
const WHY: &str = "a frontend keeps its scheduler state in a state store, Postgres from the \
                   next release";
const FIX: &str = "give '--state-store=postgres:///kbf?host=/run/postgresql&user=kbf-server', or \
                   the state-store key of kbf.toml (kbf-installer setup writes it)";

/// Refuses a frontend that names no state store.
pub(super) fn check(invocation: &Invocation, report: &mut Report) {
    let Invocation::Frontend(flags) = invocation else {
        return;
    };
    if flags.server.state_store.is_some() {
        return;
    }
    report.refuse(WHAT.to_owned(), WHY, FIX.to_owned());
}
