//! Tests of the lease minutes collector: readings are scripted, no stream is up and no
//! clock is read, so every time here is a number the test chose.

use kbf_proto::LoadDrops;
use kbf_proto::worker::NodeLoad;

use super::*;
use crate::runtime::{LeaseReading, LeaseUsage};

const S: u64 = 1_000_000;

/// `lease` started at `started_s` seconds, read at `at_s`: half a CPU since its start (500
/// thousandths), a working set of `at_s` bytes.
fn lease(term: u64, seq: u64, started_s: u64, at_s: u64) -> LeaseReading {
    LeaseReading {
        lease: LeaseId::new(term, seq),
        started_usec: started_s * S,
        usage: Some(LeaseUsage {
            cpu_usage_usec: (at_s - started_s) * S / 2,
            working_set_bytes: at_s,
        }),
    }
}

fn reading(at_s: u64, leases: Vec<LeaseReading>) -> NodeReading {
    NodeReading {
        load: NodeLoad {
            mono_usec: at_s * S,
            ..NodeLoad::default()
        },
        leases,
    }
}

/// The reading at `at_s` of lease (1, 1), which started at 0.
fn steady(at_s: u64) -> NodeReading {
    reading(at_s, vec![lease(1, 1, 0, at_s)])
}

/// Heartbeats numbered from `first_seq`, one per time in `at`, each reading `steady`.
fn beats(
    lm: &mut LeaseMinutes,
    first_seq: u64,
    at: impl IntoIterator<Item = u64>,
) -> Vec<Attached> {
    at.into_iter()
        .zip(first_seq..)
        .map(|(at_s, seq)| lm.beat(seq, &steady(at_s)))
        .collect()
}

fn ten_seconds() -> LeaseMinutes {
    let mut lm = LeaseMinutes::new();
    lm.new_stream(Duration::from_secs(10));
    lm
}

type Minute = (u64, u64, u32, u32, u32, u64);

/// (term, seq, minute, mean, peak, working set) of each minute.
fn minutes_of(attached: &Attached) -> Vec<Minute> {
    attached
        .minutes
        .iter()
        .map(|m| {
            let lease = m.lease.as_ref().expect("a lease");
            (
                lease.term,
                lease.seq,
                m.minute,
                m.cpu_mean_millis,
                m.cpu_peak_window_millis,
                m.ws_max_bytes,
            )
        })
        .collect()
}

/// (term, seq, start) of each clock.
fn clocks_of(attached: &Attached) -> Vec<(u64, u64, u64)> {
    attached
        .clocks
        .iter()
        .map(|c| {
            let lease = c.lease.as_ref().expect("a lease");
            (lease.term, lease.seq, c.started_mono_usec)
        })
        .collect()
}

/// Asserts that a minute closes into the heartbeat after the one whose reading ended it:
/// readings every ten seconds from 0 to 60 close minute 0 at the seventh, with half a CPU
/// as mean and as peak and the largest working set of the minute (the reading at 60 s is
/// minute 1's).
#[test]
fn a_minute_closes_into_the_beat_after_it_ends() {
    let mut lm = ten_seconds();
    let out = beats(&mut lm, 1, (0..=60).step_by(10));
    for early in &out[..6] {
        assert!(early.minutes.is_empty());
        assert_eq!(clocks_of(early), [(1, 1, 0)]);
    }
    assert_eq!(minutes_of(&out[6]), [(1, 1, 0, 500, 500, 50)]);
    assert_eq!(lm.in_flight(), Some(7));
}

/// Asserts that a hundred leases, some unreadable, have their clocks listed oldest start
/// first with ties by lease and cut at 96, that `covered` follows the same order and cut, and
/// that a covered lease the reading does not list goes last.
#[test]
fn the_clocks_list_every_running_lease_oldest_first_up_to_96() {
    let mut lm = ten_seconds();
    let mut leases = Vec::new();
    for i in 0..100u64 {
        let mut one = lease(1, i, i % 7, 100);
        if i % 10 == 0 {
            one.usage = None;
        }
        leases.push(one);
    }
    let mut seen = reading(100, leases);
    seen.load.covered = (0..100u64)
        .rev()
        .map(|seq| worker::LeaseId { term: 1, seq })
        .collect();
    let out = lm.beat(1, &seen);

    let mut want: Vec<(u64, u64, u64)> = (0..100u64).map(|i| (1, i, (i % 7) * S)).collect();
    want.sort_by_key(|&(term, seq, started)| (started, term, seq));
    want.truncate(96);
    assert_eq!(clocks_of(&out), want);
    assert!(
        out.clocks
            .iter()
            .any(|c| c.lease.as_ref().map(|l| l.seq) == Some(0)),
        "an unreadable lease keeps its clock"
    );
    let covered: Vec<_> = out.covered.iter().map(|l| (l.term, l.seq)).collect();
    let in_clock_order: Vec<_> = want.iter().map(|&(term, seq, _)| (term, seq)).collect();
    assert_eq!(covered, in_clock_order);
}

/// Asserts that a covered lease the reading does not list sorts after the ones it lists.
#[test]
fn a_covered_lease_the_reading_does_not_list_goes_last() {
    let mut lm = ten_seconds();
    let mut seen = steady(10);
    seen.load.covered = vec![
        worker::LeaseId { term: 9, seq: 9 },
        worker::LeaseId { term: 1, seq: 1 },
    ];
    let out = lm.beat(1, &seen);
    let covered: Vec<_> = out.covered.iter().map(|l| (l.term, l.seq)).collect();
    assert_eq!(covered, [(1, 1), (9, 9)]);
}

/// Asserts that one batch is in flight until acknowledged: the heartbeats after the one that
/// carried minute 0 carry nothing while minute 1 closes underneath, and after the
/// acknowledgement the next one carries minute 1 alone.
#[test]
fn one_batch_is_in_flight_until_acked() {
    let mut lm = ten_seconds();
    let first = beats(&mut lm, 1, (0..=60).step_by(10));
    assert_eq!(minutes_of(&first[6]).len(), 1);
    let waiting = beats(&mut lm, 8, (70..=120).step_by(10));
    assert!(waiting.iter().all(|beat| beat.minutes.is_empty()));
    assert_eq!(lm.in_flight(), Some(7));
    assert_eq!(lm.pending(), 1, "minute 1 waits");

    lm.acked(7);
    assert_eq!(lm.in_flight(), None);
    let next = beats(&mut lm, 14, [130]);
    assert_eq!(minutes_of(&next[0]), [(1, 1, 1, 500, 500, 110)]);
}

/// Asserts that an acknowledgement covers the earlier heartbeats and not the later ones: the
/// batch sent at 7 is freed by the acknowledgement of 7 and of 9, and kept by that of 6.
#[test]
fn an_ack_covers_earlier_beats() {
    for (acked, freed) in [(6, false), (7, true), (9, true)] {
        let mut lm = ten_seconds();
        beats(&mut lm, 1, (0..=60).step_by(10));
        assert_eq!(lm.in_flight(), Some(7));
        lm.acked(acked);
        assert_eq!(lm.in_flight().is_none(), freed, "acked {acked}");
    }
}

/// Asserts that a stream that ends with a batch unacknowledged sends it again once: the
/// first heartbeat of the next stream carries it, an acknowledgement frees it, and the one
/// after carries nothing.
#[test]
fn a_new_stream_sends_the_unacked_batch_again_once() {
    let mut lm = ten_seconds();
    beats(&mut lm, 1, (0..=60).step_by(10));
    lm.new_stream(Duration::from_secs(10));
    assert_eq!(lm.in_flight(), None);
    let again = beats(&mut lm, 1, [70]);
    assert_eq!(minutes_of(&again[0]), [(1, 1, 0, 500, 500, 50)]);
    lm.acked(1);
    let after = beats(&mut lm, 2, [80]);
    assert!(after[0].minutes.is_empty());
}

/// Asserts that a kept minute goes out before a newer minute of its lease: minute 0 is in
/// flight when minute 1 closes, then the stream ends and both wait; the first heartbeat of
/// the next stream carries minute 0 alone, and minute 1 follows the acknowledgement.
#[test]
fn a_kept_minute_does_not_come_after_a_newer_one_of_its_lease() {
    let mut lm = ten_seconds();
    beats(&mut lm, 1, (0..=140).step_by(10));
    lm.new_stream(Duration::from_secs(10));
    let first = beats(&mut lm, 1, [150]);
    let minutes: Vec<_> = minutes_of(&first[0]).iter().map(|m| (m.1, m.2)).collect();
    assert_eq!(minutes, [(1, 0)]);
    lm.acked(1);
    let second = beats(&mut lm, 2, [160]);
    let minutes: Vec<_> = minutes_of(&second[0]).iter().map(|m| (m.1, m.2)).collect();
    assert_eq!(minutes, [(1, 1)]);
}

/// Asserts that a stream that ends on a full unacknowledged batch, with every lease's next
/// minute already closed and waiting, loses no minute and sends none out of order: 70 leases
/// close minute 0 (64 go out, 6 wait), minute 1 closes for all 70 under the batch in flight,
/// the stream ends, and over the next stream's heartbeats each lease's minutes reach the wire
/// as 0 then 1, once each, 140 in all (so nothing is lost if the first batch never arrived).
/// If the first batch did arrive, the minutes it carried are sent once more (the receiver
/// merges a repeat) and every lease's minutes still never go down.
#[test]
fn a_full_batch_unacked_at_stream_end_leaves_every_lease_in_order() {
    let mut lm = ten_seconds();
    let all = |at_s: u64| reading(at_s, (0..70u64).map(|i| lease(1, i, 0, at_s)).collect());
    let mut before: Vec<(u64, u64)> = Vec::new();
    let mut wire: Vec<(u64, u64)> = Vec::new();

    for (n, at_s) in (0..=50u64).step_by(10).enumerate() {
        assert!(lm.beat(n as u64 + 1, &all(at_s)).minutes.is_empty());
    }
    let first = lm.beat(7, &all(60));
    assert_eq!(first.minutes.len(), MAX_LOAD_MINUTES);
    before.extend(minutes_of(&first).iter().map(|m| (m.1, u64::from(m.2))));
    // Minute 1 closes at 120; the heartbeats at 130 and 140 still run under the batch in
    // flight, so the close time of the batch is long past when the stream ends.
    for (n, at_s) in (70..=140u64).step_by(10).enumerate() {
        assert!(lm.beat(n as u64 + 8, &all(at_s)).minutes.is_empty());
    }
    assert_eq!(lm.in_flight(), Some(7));

    lm.new_stream(Duration::from_secs(10));
    for (n, at_s) in (150..=170u64).step_by(10).enumerate() {
        let seq = n as u64 + 1;
        let out = lm.beat(seq, &all(at_s));
        assert!(out.minutes.len() <= MAX_LOAD_MINUTES);
        wire.extend(minutes_of(&out).iter().map(|m| (m.1, u64::from(m.2))));
        lm.acked(seq);
    }

    assert_eq!(wire.len(), 140, "every minute once: {wire:?}");
    for lease in 0..70u64 {
        let of_lease: Vec<_> = wire
            .iter()
            .filter(|&&(l, _)| l == lease)
            .map(|&(_, minute)| minute)
            .collect();
        assert_eq!(of_lease, [0, 1], "lease {lease}");

        let delivered: Vec<_> = before
            .iter()
            .chain(&wire)
            .filter(|&&(l, _)| l == lease)
            .map(|&(_, minute)| minute)
            .collect();
        assert!(delivered.is_sorted(), "lease {lease}: {delivered:?}");
    }
}

/// Asserts that a batch left unacknowledged by two streams in a row is held again each time and
/// sent once per stream, alone, with the time it was first taken kept: minute 0 goes out on
/// the first stream, again on the second and the third, and only after its acknowledgement
/// does minute 1 (closed and waiting the whole time) follow, once. Each lease's minutes reach
/// the wire ascending. Then the same on a third stream that begins past the age limit counted
/// from the first take (but within it counted from the second send) drops the batch and counts
/// it as aged out, so minute 1 goes out and minute 0 does not.
#[test]
fn a_held_batch_unacked_again_is_held_again_once() {
    let minute_ids = |out: &Attached| -> Vec<(u64, u32)> {
        minutes_of(out).iter().map(|m| (m.1, m.2)).collect()
    };
    let first_take = 60 * S;
    let limit_s = MAX_UNSENT_AGE_USEC / S;

    let mut lm = ten_seconds();
    let mut wire: Vec<(u64, u32)> = Vec::new();
    let first = beats(&mut lm, 1, (0..=140).step_by(10));
    for out in &first {
        wire.extend(minute_ids(out));
    }
    assert_eq!(wire, [(1, 0)], "minute 0 on the first stream");
    assert_eq!(lm.in_flight(), Some(7));

    lm.new_stream(Duration::from_secs(10));
    let second = lm.beat(1, &steady(150));
    assert_eq!(minute_ids(&second), [(1, 0)]);
    wire.extend(minute_ids(&second));
    assert_eq!(lm.in_flight(), Some(1));

    lm.new_stream(Duration::from_secs(10));
    assert_eq!(lm.in_flight(), None);
    let third = lm.beat(1, &steady(160));
    assert_eq!(minute_ids(&third), [(1, 0)], "alone, though minute 1 waits");
    wire.extend(minute_ids(&third));
    let kept = lm.in_flight.as_ref().expect("the batch is in flight again");
    assert_eq!(
        kept.batch.taken_usec, first_take,
        "ages from the first take"
    );
    lm.acked(1);

    let after = lm.beat(2, &steady(170));
    assert_eq!(minute_ids(&after), [(1, 1)]);
    wire.extend(minute_ids(&after));
    lm.acked(2);
    let rest = beats(&mut lm, 3, [175]);
    assert!(
        rest.iter().all(|out| out.minutes.is_empty()),
        "nothing repeats"
    );
    assert_eq!(wire, [(1, 0), (1, 0), (1, 0), (1, 1)]);
    assert!(wire.is_sorted());
    assert_eq!(lm.counters().aged_out, 0);

    // Past the limit from the first take, within it from the second send (at 150 s).
    let mut lm = ten_seconds();
    beats(&mut lm, 1, (0..=140).step_by(10));
    lm.new_stream(Duration::from_secs(10));
    lm.beat(1, &steady(150));
    lm.new_stream(Duration::from_secs(10));
    let late = lm.beat(1, &steady(60 + limit_s + 1));
    // The long gap also closes the open minute, so minute 2 may ride with minute 1.
    let late = minute_ids(&late);
    assert_eq!(late.first(), Some(&(1, 1)), "minute 1 goes out first");
    assert!(!late.contains(&(1, 0)), "minute 0 was dropped: {late:?}");
    assert_eq!(lm.counters().aged_out, 1);
}

/// Asserts that a lease that ends while its minutes are in a held batch sends its last minute
/// after them: minute 0 is held at the stream end, the lease ends (its open minute 1 closes)
/// before the next stream's first heartbeat, which carries the held minute 0 alone, and the
/// acknowledged one after carries the final minute 1, each once and ascending.
#[test]
fn a_lease_that_ends_while_its_minutes_are_held_sends_its_last_minute_after_them() {
    let mut lm = ten_seconds();
    beats(
        &mut lm,
        1,
        [0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 105],
    );
    assert_eq!(lm.in_flight(), Some(7));

    lm.new_stream(Duration::from_secs(10));
    lm.lease_ended(LeaseId::new(1, 1));
    let first = lm.beat(1, &reading(115, Vec::new()));
    assert_eq!(minutes_of(&first), [(1, 1, 0, 500, 500, 50)], "held, alone");
    lm.acked(1);
    let second = lm.beat(2, &reading(125, Vec::new()));
    assert_eq!(minutes_of(&second), [(1, 1, 1, 500, 500, 105)]);
    lm.acked(2);
    let third = lm.beat(3, &reading(135, Vec::new()));
    assert!(third.minutes.is_empty(), "each once");
}

/// Asserts the age limit of a kept batch: one the next stream's first heartbeat finds ten
/// minutes old (to the second) is still sent, and one a second older is dropped and counted
/// as aged out.
#[test]
fn a_kept_batch_ages_out_like_a_pending_minute() {
    let limit_s = MAX_UNSENT_AGE_USEC / S;
    for (late_s, kept) in [(60 + limit_s, true), (60 + limit_s + 1, false)] {
        let mut lm = ten_seconds();
        beats(&mut lm, 1, (0..=60).step_by(10));
        assert_eq!(lm.in_flight(), Some(7));
        lm.new_stream(Duration::from_secs(10));
        let out = beats(&mut lm, 1, [late_s]);
        assert_eq!(out[0].minutes.len(), usize::from(kept), "at {late_s} s");
        assert_eq!(lm.counters().aged_out, u64::from(!kept), "at {late_s} s");
        assert_eq!(lm.in_flight().is_some(), kept);
    }
}

/// Asserts that a lease that ends after a span long enough for a minute hands over its
/// last, partial minute on the next heartbeat, that one that ends too soon has none, and that
/// an unknown lease is ignored.
#[test]
fn a_lease_that_ends_hands_over_its_last_minute() {
    let mut lm = ten_seconds();
    beats(&mut lm, 1, [0, 10, 20, 30, 40, 45]);
    lm.lease_ended(LeaseId::new(1, 1));
    let out = lm.beat(7, &reading(55, Vec::new()));
    assert_eq!(minutes_of(&out), [(1, 1, 0, 500, 500, 45)]);
    assert!(out.clocks.is_empty());

    let mut lm = ten_seconds();
    beats(&mut lm, 1, [0, 10, 20]);
    lm.lease_ended(LeaseId::new(1, 1));
    let out = lm.beat(4, &reading(30, Vec::new()));
    assert!(out.minutes.is_empty(), "twenty seconds is no minute");

    lm.lease_ended(LeaseId::new(9, 9));
    let out = lm.beat(5, &reading(40, Vec::new()));
    assert!(out.minutes.is_empty());
}

/// Asserts that a lease whose files cannot be read keeps its clock and gets no minute, not a
/// zero one.
#[test]
fn an_unreadable_lease_has_a_clock_and_no_minutes() {
    let mut lm = ten_seconds();
    for (n, at_s) in (0..=70u64).step_by(10).enumerate() {
        let mut one = lease(1, 1, 0, at_s);
        one.usage = None;
        let out = lm.beat(n as u64 + 1, &reading(at_s, vec![one]));
        assert_eq!(clocks_of(&out), [(1, 1, 0)]);
        assert!(out.minutes.is_empty());
    }
    assert_eq!(lm.pending(), 0);
}

/// Asserts that the window of a new stream takes effect and keeps what waits: a step of
/// twenty seconds is a gap under a five second window and not under a twenty second one, and
/// the minute kept across the change still goes out.
#[test]
fn a_longer_welcome_interval_takes_effect_and_keeps_pending_minutes() {
    let mut control = LeaseMinutes::new();
    control.new_stream(Duration::from_secs(5));
    beats(&mut control, 1, [0, 5, 25]);
    assert_eq!(control.counters().gaps, 1);

    let mut lm = LeaseMinutes::new();
    lm.new_stream(Duration::from_secs(5));
    beats(&mut lm, 1, (0..=60).step_by(5));
    assert_eq!(lm.in_flight(), Some(13));
    lm.new_stream(Duration::from_secs(20));
    let out = beats(&mut lm, 1, [80]);
    assert_eq!(lm.counters().gaps, 0);
    assert_eq!(minutes_of(&out[0]), [(1, 1, 0, 500, 500, 55)]);
}

/// Asserts that the binner's name for a lease and its inverse agree at the extremes, and that
/// the inverse refuses every string `key` never makes.
#[test]
fn the_binner_key_round_trips() {
    for (term, seq) in [(0, 0), (1, 2), (u64::MAX, u64::MAX), (0, u64::MAX)] {
        let lease = LeaseId::new(term, seq);
        assert_eq!(lease_of(&key(lease)), Some(lease));
    }
    for bad in [
        "",
        "1-",
        "-1",
        "1-1-1",
        "a-1",
        "+1-2",
        "01-2",
        "1-02",
        "18446744073709551616-1",
        "1",
    ] {
        assert_eq!(lease_of(bad), None, "{bad:?}");
    }
}

/// Asserts that a minute the binner holds under a name `key` never made is left out of the
/// batch (the batch is still in flight, so the minute is dropped on its ack).
#[test]
fn a_minute_with_a_foreign_name_is_not_sent() {
    let mut lm = ten_seconds();
    lm.binner.requeue(vec![ClosedMinute {
        lease_id: "not-a-lease".to_owned(),
        minute: 0,
        cpu_mean_millis: 1,
        cpu_peak_window_millis: 1,
        ws_max_bytes: 1,
    }]);
    let out = lm.beat(1, &steady(0));
    assert!(out.minutes.is_empty());
    assert_eq!(lm.in_flight(), Some(1));
}

/// Asserts what the server's vetting sees of what the collector builds: over scenarios that
/// fold a 200 minute lease into the tail minute, run more leases than the cap, and restart a
/// lease's clock, every heartbeat's load passes `vet` with nothing dropped and within the caps.
#[test]
fn what_the_daemon_builds_the_server_accepts() {
    let check = |lm: &mut LeaseMinutes, seq: u64, seen: &NodeReading| -> Attached {
        let out = lm.beat(seq, seen);
        let load = NodeLoad {
            mono_usec: seen.load.mono_usec,
            covered: out.covered.clone(),
            lease_clocks: out.clocks.clone(),
            minutes: out.minutes.clone(),
            ..NodeLoad::default()
        };
        let vetted = load.vet();
        assert_eq!(vetted.drops, LoadDrops::default(), "beat {seq}");
        assert_eq!(vetted.minutes.len(), out.minutes.len());
        assert_eq!(vetted.clocks.len(), out.clocks.len());
        assert!(out.minutes.len() <= MAX_LOAD_MINUTES);
        assert!(out.clocks.len() <= MAX_LOAD_LEASES);
        assert!(out.covered.len() <= MAX_LOAD_LEASES);
        lm.acked(seq);
        out
    };

    // One lease of 200 minutes: every minute after the 63rd folds into 63.
    let mut lm = ten_seconds();
    let mut tail = 0;
    for (n, at_s) in (0..=12_100u64).step_by(10).enumerate() {
        let out = check(&mut lm, n as u64 + 1, &steady(at_s));
        for minute in out.minutes {
            assert!(minute.minute <= 63);
            tail += usize::from(minute.minute == 63);
        }
    }
    assert!(tail >= 2, "the tail minute is sent again as it grows");

    // More leases than the cap, started three seconds apart.
    let mut lm = ten_seconds();
    for (n, at_s) in (300..=600u64).step_by(10).enumerate() {
        let leases = (0..100u64).map(|i| lease(1, i, i * 3, at_s)).collect();
        check(&mut lm, n as u64 + 1, &reading(at_s, leases));
    }

    // A lease whose start is made again, as the driver's retry does.
    let mut lm = ten_seconds();
    for (n, at_s) in (0..=90u64).step_by(10).enumerate() {
        check(&mut lm, n as u64 + 1, &steady(at_s));
    }
    for (n, at_s) in (100..=300u64).step_by(10).enumerate() {
        let restarted = reading(at_s, vec![lease(1, 1, 95, at_s)]);
        check(&mut lm, n as u64 + 11, &restarted);
    }
}

/// Asserts that 88 leases that end inside one window lose no final minute with a cap of 64
/// a heartbeat: the first heartbeat after carries 64, the acknowledged one carries the other
/// 24, every lease's minute 0 arrives once and nothing is sent again.
#[test]
fn leases_that_end_together_lose_no_final_minute() {
    let mut lm = ten_seconds();
    let ids: Vec<_> = (0..88u64).collect();
    for (n, at_s) in [0u64, 10, 20, 30, 40, 45].into_iter().enumerate() {
        let leases = ids.iter().map(|&i| lease(1, i, 0, at_s)).collect();
        let out = lm.beat(n as u64 + 1, &reading(at_s, leases));
        assert!(out.minutes.is_empty());
    }
    for &i in &ids {
        lm.lease_ended(LeaseId::new(1, i));
    }
    let first = lm.beat(7, &reading(55, Vec::new()));
    assert_eq!(first.minutes.len(), 64);
    lm.acked(7);
    let second = lm.beat(8, &reading(65, Vec::new()));
    assert_eq!(second.minutes.len(), 24);
    lm.acked(8);
    let third = lm.beat(9, &reading(75, Vec::new()));
    assert!(third.minutes.is_empty());

    let mut got: Vec<_> = minutes_of(&first)
        .into_iter()
        .chain(minutes_of(&second))
        .map(|m| (m.1, m.2))
        .collect();
    got.sort_unstable();
    let want: Vec<_> = ids.iter().map(|&i| (i, 0)).collect();
    assert_eq!(got, want, "each lease's final minute, once");
}

/// Asserts that a server that never acknowledges costs bounded memory: after more than ten
/// minutes of heartbeats the binner has dropped what is older than ten minutes and holds at
/// most ten minutes of closings.
#[test]
fn without_acks_pending_minutes_age_out() {
    let mut lm = ten_seconds();
    beats(&mut lm, 1, (0..=800).step_by(10));
    assert_eq!(
        lm.in_flight(),
        Some(7),
        "the first batch is never acknowledged"
    );
    assert!(lm.counters().aged_out > 0);
    assert!(lm.pending() <= 10, "{}", lm.pending());
}

/// Asserts that a window that is too long is logged once and the next one is not, and that
/// another kind is logged again.
#[test]
fn a_counter_is_logged_once_per_kind() {
    let mut lm = LeaseMinutes::new();
    // The floor window is a second: ten seconds between readings is a gap.
    lm.beat(1, &steady(0));
    lm.beat(2, &steady(10));
    assert_eq!(lm.counters().gaps, 1);
    assert!(lm.logged.contains("gaps"));
    assert!(lm.fresh_kinds().is_empty(), "already logged");

    lm.beat(3, &steady(20));
    assert_eq!(lm.counters().gaps, 2);
    assert!(lm.fresh_kinds().is_empty(), "the same kind again");

    lm.beat(4, &steady(5));
    assert_eq!(lm.counters().clock_back, 1);
    assert!(lm.logged.contains("clock_back"));
    assert_eq!(lm.logged.len(), 2);
}

/// Asserts that a kind not logged yet is reported once and the same kind never again.
#[test]
fn a_kind_is_reported_once() {
    let mut lm = LeaseMinutes::new();
    lm.binner.observe(&sample(&steady(0)));
    lm.binner.observe(&sample(&steady(10)));
    assert_eq!(lm.fresh_kinds(), ["gaps"]);
    assert!(lm.fresh_kinds().is_empty());
}

/// Asserts the sampling window: the Welcome's interval, never under a second, and the test
/// override over both.
#[test]
fn the_window_is_the_interval_but_never_under_the_floor() {
    let s = Duration::from_secs;
    assert_eq!(usage_window(s(5), None), s(5));
    assert_eq!(usage_window(Duration::from_millis(1), None), s(1));
    assert_eq!(usage_window(Duration::from_millis(100), Some(s(10))), s(10));
    assert_eq!(usage_window(s(5), Some(s(3))), s(3));
}

/// Asserts that the daemon's tail minute is the scheduler's, so a rev raise that moves it is
/// seen here.
#[test]
fn the_tail_minute_is_the_binners() {
    assert_eq!(
        kbf_proto::TAIL_MINUTE,
        u32::from(js_agent_usage::TAIL_MINUTE)
    );
}
