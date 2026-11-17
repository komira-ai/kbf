//! An operation whose lease a draining frontend gave up because its job was never placed on a
//! host ([`Farm::unplaced`]): what its callers are shown while it waits for a worker.

use kbf_proto::google::rpc;
use kbf_proto::worker;
use tonic::Code;

use super::blocks_tests::{Racing, farm_over};
use super::intake_tests::{caller, register};
use super::*;

/// The infrastructure result of `lease`, as the executor reports a job it withdrew.
fn drained(lease: LeaseId) -> worker::Result {
    worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        status: Some(rpc::Status {
            code: Code::Unavailable as i32,
            message: "the frontend is draining and the job had not been placed on a host"
                .to_owned(),
            details: Vec::new(),
        }),
        ..worker::Result::default()
    }
}

/// Withdraws the only lease of `farm`, held on `node`, as a drain does, and returns the caller
/// of its operation after the farm has seen the lease given up.
async fn withdrawn_at_a_drain(farm: &Farm, node: &WorkerId) -> kbf_front::Ticket {
    let _inbox = register(farm, node.as_str());
    let ticket = caller(farm, 1);
    farm.tick().await;
    let views = farm.lease_views(None);
    let [view] = &views.leases[..] else {
        panic!("one lease expected");
    };
    let lease = view.lease;
    farm.close_intake();
    farm.unplaced(lease);
    farm.report(node, drained(lease)).await;
    farm.tick().await;
    farm.intake();
    farm.tick().await;
    ticket
}

/// Asserts that a caller whose operation was withdrawn at a drain keeps its stream while a worker
/// session of another frontend could run the operation (`WaitingKept`), and is not answered: Buck2
/// never re-attaches a stream that ends, so ending it fails the user's action, and the other
/// frontend takes the operation from the state store and answers it.
#[tokio::test(start_paused = true)]
async fn a_withdrawn_operation_another_frontend_could_run_keeps_its_caller() {
    let store = Racing::new();
    let (here, there) = (farm_over(&store), farm_over(&store));
    let _daemon = register(&there, "node-there");

    let ticket = withdrawn_at_a_drain(&here, &WorkerId::new("node-here")).await;

    let stage = ticket.stage.borrow().clone();
    assert!(matches!(stage, Stage::WaitingKept(_)), "{stage:?}");
}

/// Asserts that a caller whose operation was withdrawn at a drain, with no other frontend that
/// could run it, is shown a stage a drain ends (`Waiting`): nothing would ever place it, and the
/// drain would wait for it for ever.
#[tokio::test(start_paused = true)]
async fn a_withdrawn_operation_no_other_frontend_could_run_ends_its_stream_at_the_drain() {
    let store = Racing::new();
    let here = farm_over(&store);

    let ticket = withdrawn_at_a_drain(&here, &WorkerId::new("node-here")).await;

    let stage = ticket.stage.borrow().clone();
    assert!(matches!(stage, Stage::Waiting(_)), "{stage:?}");
}
