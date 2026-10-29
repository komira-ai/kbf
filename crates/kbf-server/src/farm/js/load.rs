//! The load a node reports in its heartbeat, as the js core takes it.

use kbf_types::WorkerId;
use kbf_types::sched::usage::NodeLoad;

use super::JsFarm;

/// The load `worker`'s heartbeat reported, as `js_sched` takes it in `Event::Heartbeat`
/// (`js_server_core::Beat::with_load`): placement by `--placement-load` weighs a node by
/// it. None for now: the conversion from the heartbeat's reported load, and the vetting of
/// what it says, follow in their own change, which also keeps the load the node view shows
/// (the farm's `note_node_load` returns early on this core).
pub(crate) fn node_load(js: &JsFarm, worker: &WorkerId) -> Option<NodeLoad> {
    let _ = (js, worker);
    None
}
