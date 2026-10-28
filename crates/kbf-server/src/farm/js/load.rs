//! The load a node reports in its heartbeat, as the js core takes it.

use kbf_types::WorkerId;
use kbf_types::sched::usage::NodeLoad;

use super::JsFarm;

/// The load `worker`'s heartbeat reported, as `js_sched` takes it in `Event::Heartbeat`
/// (`js_server_core::Beat::with_load`): placement by `--placement-load` weighs a node by
/// it. This server's heartbeat carries no load yet, so there is none; the conversion from
/// the heartbeat's `NodeLoad`, and the vetting of what it says, follow with it.
pub(crate) fn node_load(js: &JsFarm, worker: &WorkerId) -> Option<NodeLoad> {
    let _ = (js, worker);
    None
}
