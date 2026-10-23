use super::*;

/// Dropping a guard gives back every byte it took, including those added after the hold.
#[test]
fn a_dropped_guard_gives_back_everything_it_took() {
    let gauge = Gauge::new();
    let mut held = gauge.hold(10);
    held.add(5);
    assert_eq!((gauge.current(), held.bytes()), (15, 15));
    drop(held);
    assert_eq!(gauge.current(), 0);
}

/// The peak is the most bytes held at once; it rises with the count and does not fall
/// when the count does.
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

/// Concurrent holds are counted exactly: the peak reaches the sum of holds alive together
/// and the count returns to 0.
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

/// Counted bytes stay counted while any slice or clone of them is alive and are given
/// back once the last is dropped; `counted_with` takes over a guard taken for the
/// allocation, so the buffer is not counted twice.
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

/// `counted` counts the bytes' length in the process gauge. Bounds from below only, since
/// the process gauge is shared by every test of the crate that runs a write.
#[test]
fn counted_counts_the_length_in_the_process_gauge() {
    let bytes = counted(vec![1u8; 4096]);
    assert!(process().current() >= 4096);
    assert!(process().peak() >= 4096);
    drop(bytes);
}

/// The export names both gauge families, each with its own value.
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
