use std::time::Duration;

use kbf_lease_client::{HostLeaseClock, HostLeaseMinute, HostLoad};
use tokio::time::Instant;

use super::Seen;

const POLL: Duration = Duration::from_secs(5);

fn reading(seq: u64, mono_usec: u64) -> HostLoad {
    HostLoad {
        seq,
        mono_usec,
        cpu_usage_usec: seq * 1_000_000,
        memory_working_set_bytes: 3 << 30,
        memory_active_file_bytes: 7,
        actions_cpu_limited: true,
        covered: vec!["4-9".to_owned()],
        lease_clocks: vec![HostLeaseClock {
            lease: "4-9".to_owned(),
            started_mono_usec: 100,
        }],
        minutes: vec![HostLeaseMinute {
            lease: "4-9".to_owned(),
            minute: 2,
            cpu_mean_millis: 1500,
            cpu_peak_window_millis: 2000,
            ws_max_bytes: 1 << 30,
        }],
    }
}

/// Catches a conversion that drops or reshapes a figure on its way to the farm: every figure
/// arrives as listed, a lease text becomes the farm's term and seq, and the two telemetry
/// figures the lease server does not send are absent, not zero.
#[tokio::test(start_paused = true)]
async fn a_reading_reaches_the_farm_as_listed() {
    let now = Instant::now();
    let mut seen = Seen::after_poll(None, Some(&reading(5, 61_000_000)), now).expect("kept");
    let load = seen.take(now, POLL).expect("a fresh reading is handed");
    assert_eq!(
        (load.seq, load.mono_usec, load.cpu_usage_usec),
        (5, 61_000_000, 5_000_000)
    );
    assert_eq!(
        (load.memory_working_set_bytes, load.memory_active_file_bytes),
        (3 << 30, 7)
    );
    assert!(load.actions_cpu_limited);
    assert_eq!(load.covered.len(), 1);
    assert_eq!((load.covered[0].term, load.covered[0].seq), (4, 9));
    assert_eq!(load.lease_clocks[0].started_mono_usec, 100);
    assert_eq!(load.minutes[0].cpu_peak_window_millis, 2000);
    assert_eq!(load.minutes[0].ws_max_bytes, 1 << 30);
    assert_eq!(load.cpu_throttled_usec, None);
    assert_eq!(load.cpu_pressure_some_total_usec, None);
}

/// Catches a lease text the farm cannot name failing the whole reading, or being turned into
/// lease 0-0: only the record that carries it is left out.
#[tokio::test(start_paused = true)]
async fn a_lease_that_is_not_term_dash_seq_leaves_out_its_record_alone() {
    let now = Instant::now();
    let mut listed = reading(5, 61_000_000);
    listed.covered.push("4.9".to_owned());
    listed.lease_clocks[0].lease = "01-2".to_owned();
    listed.minutes.push(HostLeaseMinute {
        lease: "5-1".to_owned(),
        ..listed.minutes[0].clone()
    });
    let load = Seen::after_poll(None, Some(&listed), now)
        .expect("kept")
        .take(now, POLL)
        .expect("handed");
    assert_eq!(load.covered.len(), 1);
    assert!(load.lease_clocks.is_empty());
    assert_eq!(load.minutes.len(), 2);
}

/// Catches a reading handed twice (the farm drops a repeat and counts its minutes as
/// refused), and a poll of the same reading starting its age again: the same listing
/// is one reading, a higher `seq` is a new one, and a host listed with no load has none.
#[tokio::test(start_paused = true)]
async fn a_reading_is_handed_once_and_a_poll_of_the_same_one_changes_nothing() {
    let start = Instant::now();
    let first = reading(5, 60_000_000);
    let mut seen = Seen::after_poll(None, Some(&first), start).expect("kept");
    assert!(seen.take(start, POLL).is_some());
    assert!(seen.take(start, POLL).is_none(), "handed twice");
    tokio::time::advance(POLL).await;
    let mut seen = Seen::after_poll(Some(seen), Some(&first), Instant::now()).expect("kept");
    assert!(
        seen.take(Instant::now(), POLL).is_none(),
        "a repeat poll made it new"
    );
    let second = reading(6, 65_000_000);
    let mut seen = Seen::after_poll(Some(seen), Some(&second), Instant::now()).expect("kept");
    assert_eq!(seen.take(Instant::now(), POLL).expect("new").seq, 6);
    assert!(Seen::after_poll(Some(seen), None, Instant::now()).is_none());
}

/// Catches the staleness rule being off by a poll or ignoring the window: with a window of
/// 10 s between two readings the limit is three windows and one poll (35 s), measured from
/// when the reading was first seen; at the limit it is still handed, one tick past it is not.
/// A first reading has no window, so the poll interval stands in for it.
#[tokio::test(start_paused = true)]
async fn a_reading_older_than_three_windows_and_a_poll_is_not_handed() {
    let start = Instant::now();
    let older = reading(5, 50_000_000);
    let newer = reading(6, 60_000_000);
    let seen = |at| {
        let before = Seen::after_poll(None, Some(&older), start);
        Seen::after_poll(before, Some(&newer), at).expect("kept")
    };
    let limit = Duration::from_secs(35);
    assert!(seen(start).take(start + limit, POLL).is_some());
    assert!(
        seen(start)
            .take(start + limit + Duration::from_millis(1), POLL)
            .is_none()
    );
    // No earlier reading: 3 * 5 s + 5 s.
    let mut at_limit = Seen::after_poll(None, Some(&newer), start).expect("kept");
    assert!(
        at_limit
            .take(start + Duration::from_secs(20), POLL)
            .is_some()
    );
    let mut past = Seen::after_poll(None, Some(&newer), start).expect("kept");
    assert!(past.take(start + Duration::from_secs(21), POLL).is_none());
}

/// Catches a window taken across agent processes: a reading whose `seq` is not above the one
/// before it starts a new process, whose window is unknown, so the poll interval stands in.
#[tokio::test(start_paused = true)]
async fn a_new_agent_process_has_no_window() {
    let start = Instant::now();
    let before = Seen::after_poll(None, Some(&reading(50, 50_000_000)), start);
    let restarted = reading(1, 100);
    let mut seen = Seen::after_poll(before, Some(&restarted), start).expect("kept");
    assert!(seen.take(start + Duration::from_secs(21), POLL).is_none());
}
