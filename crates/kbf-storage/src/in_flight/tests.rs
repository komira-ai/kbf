use super::*;

/// Catches: a guard that never gives its bytes back (a leak the gauge would report as
/// load forever), and a drop that gives back the wrong amount after `add`.
#[test]
fn a_dropped_guard_gives_back_everything_it_took() {
    let gauge = Gauge::new();
    let mut held = gauge.hold(10);
    held.add(5);
    assert_eq!((gauge.current(), held.bytes()), (15, 15));
    drop(held);
    assert_eq!(gauge.current(), 0);
}

/// Catches: a peak that is never raised, one that follows the current count down, and
/// one that is the sum of every hold instead of the most at once.
#[test]
fn the_peak_is_the_most_held_at_once_and_stays() {
    let gauge = Gauge::new();
    let a = gauge.hold(10);
    let b = gauge.hold(5);
    drop(a);
    let c = gauge.hold(3);
    assert_eq!((gauge.current(), gauge.peak()), (8, 15));
    drop((b, c));
    assert_eq!((gauge.current(), gauge.peak()), (0, 15));
}

/// Catches: concurrent holds losing an update (a peak below the sum of holds that were
/// all alive together) or a count that does not return to 0.
#[test]
fn concurrent_holds_count_exactly() {
    let gauge = Gauge::new();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                let held = gauge.hold(100);
                barrier.wait();
                drop(held);
                for _ in 0..1000 {
                    drop(gauge.hold(1));
                }
            });
        }
    });
    assert_eq!(gauge.current(), 0);
    assert_eq!(gauge.peak(), 800);
}

/// Catches: counted bytes given back while a slice or clone of them is alive (a buffer
/// kept past its count), or never given back once the last one is dropped; and a buffer
/// counted twice when its allocation was counted first (`counted_with` takes the guard
/// over).
#[test]
fn counted_bytes_are_held_until_the_last_slice_is_dropped() {
    static GAUGE: Gauge = Gauge::new();
    let bytes = counted_with(vec![7u8; 1000], GAUGE.hold(1000));
    assert_eq!(GAUGE.current(), 1000);
    let slice = bytes.slice(10..20);
    let clone = bytes.clone();
    drop(bytes);
    assert_eq!(GAUGE.current(), 1000);
    drop(clone);
    assert_eq!(GAUGE.current(), 1000);
    assert_eq!(&slice[..], &[7u8; 10]);
    drop(slice);
    assert_eq!((GAUGE.current(), GAUGE.peak()), (0, 1000));
}

/// Catches: `counted` counting in a gauge other than the process's, or at other than the
/// bytes' length. Bounds from below only, since the process gauge is shared by every test
/// of the crate that runs a write.
#[test]
fn counted_counts_the_length_in_the_process_gauge() {
    let bytes = counted(vec![1u8; 4096]);
    assert!(process().current() >= 4096);
    assert!(process().peak() >= 4096);
    drop(bytes);
}

/// Catches: an export that drops a family, renames it, or swaps the two values.
#[test]
fn render_names_both_families_with_their_values() {
    let gauge = Gauge::new();
    drop(gauge.hold(9));
    let _live = gauge.hold(4);
    let mut out = String::new();
    render_gauge(&gauge, &mut out);
    assert!(out.contains("\nkbf_bytes_in_flight 4\n"), "{out}");
    assert!(out.contains("\nkbf_bytes_in_flight_peak 9\n"), "{out}");
    assert!(out.contains("# TYPE kbf_bytes_in_flight gauge\n"), "{out}");
    assert!(
        out.contains("# TYPE kbf_bytes_in_flight_peak gauge\n"),
        "{out}"
    );
}
