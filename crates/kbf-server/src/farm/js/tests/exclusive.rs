//! `--exclusive-actions` at the farm: a request that owns a whole worker (what the front
//! sends with the flag on) books the worker's full capacity and runs alone, on the legacy
//! core and the js core alike; a request that does not is placed beside others as before.

use kbf_front::{Dispatch as _, Finished, Stage, Submission, Ticket};
use kbf_proto::worker::server_message::Message;
use kbf_proto::worker::{self, MemoryKill};
use tonic::Code;

use super::hot::{backend, farm_on, join, killed, linux, node, ok, request};
use crate::SchedulerCore;
use crate::farm::Farm;

const CORES: u64 = 4_000;
const MEMORY: u64 = 8 << 30;

fn submit(farm: &Farm, tag: u8, whole_worker: bool) -> Ticket {
    farm.submit(Submission {
        request: kbf_sched::Request {
            whole_worker,
            ..request(tag)
        },
        call: None,
        described: kbf_front::Described::default(),
        evidence: Default::default(),
        max_wall: None,
    })
    .expect("queued")
}

/// The `Start`s among `sent`, as the CPUs and memory each books.
fn starts(sent: Vec<Message>) -> Vec<(u64, u64)> {
    sent.into_iter()
        .filter_map(|message| match message {
            Message::Start(start) => Some((start.millicpus, start.memory_bytes)),
            _ => None,
        })
        .collect()
}

fn lease_of(sent: Vec<Message>) -> worker::LeaseId {
    sent.into_iter()
        .find_map(|message| match message {
            Message::Start(start) => start.lease_id,
            _ => None,
        })
        .expect("a Start")
}

const CORES_UNDER_TEST: [SchedulerCore; 2] = [SchedulerCore::Legacy, SchedulerCore::Js];

/// Catches: an ordinary request (`whole_worker` false, the flag off) changed by the new
/// request field: two actions booking one core and 1 GiB each both start on a four-core
/// worker at once and book what they asked, so the setting off is today's placement.
#[tokio::test(start_paused = true)]
async fn without_the_setting_two_actions_share_a_worker() {
    for core in CORES_UNDER_TEST {
        let farm = farm_on(core, &backend());
        let mut joined = join(&farm, "n1", linux());
        let _a = submit(&farm, 1, false);
        let _b = submit(&farm, 2, false);
        farm.tick().await;
        assert_eq!(
            starts(joined.sent()),
            [(1_000, 1 << 30), (1_000, 1 << 30)],
            "{core:?}"
        );
    }
}

/// Catches: an exclusive request booked by its own numbers (one core, 1 GiB) instead of
/// the worker's, a second one placed beside the first on a multi-CPU worker, a second that
/// is not placed once the first ends, and a worker that stays held after its lease ends.
/// The first Start books the worker's four cores and 8 GiB; the second waits, with no
/// Start, until the first's result is taken, and then books the same.
#[tokio::test(start_paused = true)]
async fn an_exclusive_action_books_the_whole_worker_and_the_next_waits_for_it() {
    for core in CORES_UNDER_TEST {
        let farm = farm_on(core, &backend());
        let name = node("n1");
        let mut joined = join(&farm, "n1", linux());
        let _first = submit(&farm, 1, true);
        let second = submit(&farm, 2, true);
        farm.tick().await;
        let sent = joined.sent();
        let lease = lease_of(sent.clone());
        assert_eq!(starts(sent), [(CORES, MEMORY)], "{core:?}: first");
        farm.tick().await;
        assert!(starts(joined.sent()).is_empty(), "{core:?}: second waits");
        assert!(
            matches!(&*second.stage.borrow(), Stage::Queued),
            "{core:?}: second is queued"
        );

        farm.report(&name, ok(&lease, None)).await.expect("an ack");
        farm.tick().await;
        assert_eq!(starts(joined.sent()), [(CORES, MEMORY)], "{core:?}: second");
    }
}

/// Catches: an ordinary request placed beside an exclusive lease (the worker must hold
/// nothing else), and an exclusive request placed on a worker that already runs an
/// ordinary action. An ordinary action runs first; the exclusive one waits until it
/// ends.
#[tokio::test(start_paused = true)]
async fn an_exclusive_action_and_an_ordinary_one_do_not_share_a_worker() {
    for core in CORES_UNDER_TEST {
        let farm = farm_on(core, &backend());
        let name = node("n1");
        let mut joined = join(&farm, "n1", linux());
        let _ordinary = submit(&farm, 1, false);
        let _exclusive = submit(&farm, 2, true);
        farm.tick().await;
        let sent = joined.sent();
        let lease = lease_of(sent.clone());
        assert_eq!(starts(sent), [(1_000, 1 << 30)], "{core:?}: ordinary only");
        farm.report(&name, ok(&lease, None)).await.expect("an ack");
        farm.tick().await;
        assert_eq!(
            starts(joined.sent()),
            [(CORES, MEMORY)],
            "{core:?}: exclusive"
        );
        let _late = submit(&farm, 3, false);
        farm.tick().await;
        assert!(starts(joined.sent()).is_empty(), "{core:?}: not beside it");
    }
}

/// Catches: the memory ladder climbing for an exclusive action (there is no larger
/// booking than the worker): an own-limit kill finishes the operation out of memory
/// (`FAILED_PRECONDITION`, as a kill at the largest booking) at once, with no second Start.
#[tokio::test(start_paused = true)]
async fn an_own_limit_kill_of_an_exclusive_action_is_final() {
    for core in CORES_UNDER_TEST {
        let farm = farm_on(core, &backend());
        let mut joined = join(&farm, "n1", linux());
        let ticket = submit(&farm, 1, true);
        farm.tick().await;
        let lease = lease_of(joined.sent());
        let ack = farm
            .report(&node("n1"), killed(&lease, MemoryKill::OwnLimit))
            .await
            .expect("an ack");
        assert!(ack.accepted, "{core:?}: {ack:?}");
        farm.tick().await;
        assert!(starts(joined.sent()).is_empty(), "{core:?}: no rerun");
        match &*ticket.stage.borrow() {
            Stage::Done(Finished::Failed(status)) => {
                assert_eq!(
                    status.code,
                    Code::FailedPrecondition as i32,
                    "{core:?}: {status:?}"
                );
            }
            other => panic!("{core:?}: not finished out of memory: {other:?}"),
        }
    }
}
