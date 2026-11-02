//! The tests of [`SwapKill`] and [`SwapWatch`]: the threshold, and the watch's split
//! of a lease's swap into its own and the host's over series of samples.

use super::*;

/// Catches a threshold that drops the floor (a small booking killed for a few cold
/// pages) or the share of the booking (a large one killed at 512 MiB of swap), and
/// a share that overflows for a huge booking.
#[test]
fn the_swap_threshold_is_the_floor_or_a_share_of_the_booking() {
    let policy = SwapKill::DEFAULT;
    assert_eq!(policy.threshold(0), 512 << 20);
    assert_eq!(policy.threshold(1 << 30), 512 << 20);
    assert_eq!(policy.threshold(2 << 30), 512 << 20);
    assert_eq!(policy.threshold(8 << 30), 2 << 30);
    assert_eq!(policy.threshold(u64::MAX), u64::MAX / 4);
    let flat = SwapKill {
        floor_bytes: 7,
        percent: 0,
        every: Duration::ZERO,
    };
    assert_eq!(flat.threshold(1 << 40), 7);
}

const MIB: u64 = 1 << 20;
/// The cap of the leases the samples describe. The samples of [`killed_at`] hold
/// no RAM (`memory.current` 0), well below it.
const CAP: u64 = 536 * MIB;

/// The index of the sample, fed in order to the watch of a new lease, at which it
/// kills the lease: each sample is (`memory.events` `max`, `memory.swap.current`).
fn killed_at(samples: &[(u64, u64)], threshold: u64) -> Option<usize> {
    let mut watch = SwapWatch::new(threshold, CAP);
    samples.iter().position(|&(max, swap)| {
        watch.kills(Pressure {
            max,
            current: 0,
            swap,
        })
    })
}

/// Catches the false positive seen on the real-crun arm job (a lease killed as out of memory
/// for swap the host put there): the absolute-swap rule. Host reclaim moved
/// 143265792 bytes of the lease to swap below its cap (`max` 0), a few pages were
/// read back (143233024), then the lease pressed its cap (`max` 156 to 7205) and
/// pushed 32 KiB back out. The lease pushed 32 KiB into swap itself, far below the
/// 8 MiB threshold; the old rule counted all 143 MB and killed it.
#[test]
fn a_lease_the_host_swapped_is_not_killed_for_a_few_pages_at_its_cap() {
    let mut samples = vec![(0, 143_265_792); 17];
    samples.push((0, 143_233_024));
    for max in [
        156, 565, 1184, 1986, 2838, 3650, 4268, 4829, 5411, 6043, 6628, 7205,
    ] {
        samples.push((max, 143_233_024));
    }
    samples.push((7800, 143_265_792));
    assert_eq!(killed_at(&samples, 8 * MIB), None);
    let mut watch = SwapWatch::new(8 * MIB, CAP);
    for &(max, swap) in &samples {
        watch.kills(Pressure {
            max,
            current: 0,
            swap,
        });
    }
    assert_eq!(watch.own(), 32 << 10, "what the lease pushed itself");
}

/// Catches a rule that never kills (or kills too late, or at the threshold rather
/// than past it): a lease at 0 swap pressing its cap pushes 3 MiB more into swap at
/// each sample. At exactly the 8 MiB threshold it lives; one byte past, it dies.
#[test]
fn a_lease_pressing_its_cap_into_swap_is_killed_past_the_threshold() {
    let samples = [(10, 3 * MIB), (20, 6 * MIB), (30, 9 * MIB)];
    assert_eq!(killed_at(&samples, 8 * MIB), Some(2));
    let samples = [(10, 4 * MIB), (20, 8 * MIB), (30, 8 * MIB + 1)];
    assert_eq!(killed_at(&samples, 8 * MIB), Some(2));
    assert_eq!(killed_at(&samples[..2], 8 * MIB), None);
}

/// Catches host swap ignored for good (a lease the host swapped never killed, even
/// when it then pushes past the threshold itself at its cap) and the host's share
/// taken wrong: from the first sample (143265792) rather than after the pages read
/// back (143233024), the lease's own share at the last sample would be 32 KiB
/// short of the threshold and it would live.
#[test]
fn a_lease_the_host_swapped_is_killed_for_what_it_pushes_itself_past_the_threshold() {
    let host = 143_233_024;
    let samples = [
        (0, 143_265_792),
        (0, host),
        (100, host + 4 * MIB),
        (200, host + 8 * MIB),
        (300, host + 8 * MIB + 1),
    ];
    assert_eq!(killed_at(&samples, 8 * MIB), Some(4));
    assert_eq!(killed_at(&samples[..4], 8 * MIB), None);
}

/// Catches a share reset when the lease's `max` events pause (a lease pressing its
/// cap into swap in bursts never killed), and swap the host moves during the pause
/// counted as the lease's. The lease pushes 5 MiB at its cap, pauses (`max` flat)
/// while the host moves 100 MiB more of it to swap, then pushes 4 MiB more at its
/// cap: 9 MiB is its own, past 8 MiB. With only 1 MiB more, 6 MiB is its own.
#[test]
fn pushes_at_the_cap_add_up_across_a_pause_and_host_swap_in_the_pause_does_not() {
    let samples = [
        (10, 5 * MIB),
        (10, 5 * MIB),
        (10, 105 * MIB),
        (20, 109 * MIB),
    ];
    assert_eq!(killed_at(&samples, 8 * MIB), Some(3));
    let samples = [(10, 5 * MIB), (10, 105 * MIB), (20, 106 * MIB)];
    assert_eq!(killed_at(&samples, 8 * MIB), None);
}

/// Catches a fall in swap taken from the host's share first: pages read back at the
/// cap and pushed out again would then add up as the lease's own until it was
/// killed. The host moved 100 MiB of the lease to swap; at its cap the lease pushes
/// 6 MiB out, reads 4 MiB back, pushes it out again, and so on. Its own share never
/// passes 6 MiB.
#[test]
fn pages_read_back_and_pushed_out_again_at_the_cap_do_not_add_up() {
    let mut samples = vec![(0, 100 * MIB)];
    for i in 1..=20 {
        let swap = if i % 2 == 1 { 106 * MIB } else { 102 * MIB };
        samples.push((i * 10, swap));
    }
    assert_eq!(killed_at(&samples, 8 * MIB), None);
}

/// Catches a kill on swap alone (the "ignores max events" catches: host pressure
/// moves a lease below its cap, or one that hit its cap once before, into swap) and
/// on `max` events alone (a lease at its cap that reclaim keeps in RAM, or whose
/// swap is flat at the threshold).
#[test]
fn swap_without_max_events_or_max_events_without_swap_kill_nothing() {
    assert_eq!(killed_at(&[(0, 1 << 40)], 8 * MIB), None);
    assert_eq!(
        killed_at(&[(3, 0), (3, 1 << 40), (3, 1 << 41)], 8 * MIB),
        None
    );
    assert_eq!(killed_at(&[(9, 0), (99, 0), (999, 0)], 8 * MIB), None);
    assert_eq!(
        killed_at(&[(10, 8 * MIB), (20, 8 * MIB), (30, 8 * MIB)], 8 * MIB),
        None
    );
}

/// Catches a reclaim at the cap whose swap-out spans two samples credited to the
/// host (the lease never killed), and the fix for it reaching too far: host swap
/// of a lease well below its cap, swap with no `max` event, or swap two samples
/// after the event counted as the lease's own. The kernel counts one `max` event
/// when a charge at the cap first fails, then swaps pages out for a while with no
/// further event, so a one-second sample can land inside that window: `max` already
/// counted, swap still rising. The real-crun swap-kill test's lease (cap 536 MiB,
/// [`CAP`]) sampled every 50 ms read `max` 1 while swap went 24403968 to 241504256
/// bytes over 153 ms, RAM at 537464832 bytes or more (95.6 % of the cap). On a run
/// that timed out, its one-second samples read `max` 0 and no swap, then `max` 1 with
/// 439328768 bytes in swap and RAM at 99.6 % of the cap, then `max` 1241 with swap
/// flat. The driver's own samples are not those, so the first row splits that rise
/// across two of them. Each sample is (`memory.events` `max`, `memory.current`,
/// `memory.swap.current`); a row gives the sample the lease is killed at, if any,
/// and its own share of swap then (or after the last sample).
#[test]
fn a_reclaim_at_the_cap_that_outlasts_its_max_event_is_the_leases_own() {
    /// RAM held at the cap, at its edge (7/8 of it), and well below it.
    const AT: u64 = CAP;
    const EDGE: u64 = CAP - CAP / 8;
    const LOW: u64 = CAP / 4;
    type Sample = (u64, u64, u64);
    let rows: [(&str, &[Sample], Option<usize>, u64); 10] = [
        (
            "timed-out run: max 1 while swap climbs across two samples, at the cap",
            &[
                (0, AT, 0),
                (1, AT, 100 * MIB),
                (1, 559_939_584, 439_328_768),
                (1241, 561_995_776, 440_868_864),
                (2211, 561_942_528, 440_864_768),
            ],
            Some(2),
            439_328_768,
        ),
        (
            "50 ms trace: max 1 while swap goes 24 MB to 241 MB, at 95.6 % of the cap",
            &[
                (0, 1_044_480, 0),
                (1, 562_036_736, 24_403_968),
                (1, 537_464_832, 241_504_256),
            ],
            Some(2),
            241_504_256,
        ),
        (
            "host swap held before: an event, a carried rise, a swap-in, a reclaim",
            &[
                (0, LOW, 200 * MIB),
                (1, AT, 210 * MIB),
                (1, AT, 300 * MIB),
                (1, AT, 260 * MIB),
                (2, AT, 330 * MIB),
            ],
            Some(4),
            130 * MIB,
        ),
        (
            "host swap held before, and the last reclaim 10 MiB short of the threshold",
            &[
                (0, LOW, 200 * MIB),
                (1, AT, 210 * MIB),
                (1, AT, 300 * MIB),
                (1, AT, 260 * MIB),
                (2, AT, 320 * MIB),
            ],
            None,
            120 * MIB,
        ),
        (
            "failed run's series with the RAM well below the cap: host reclaim",
            &[
                (0, LOW, 0),
                (1, LOW, 100 * MIB),
                (1, LOW, 439 * MIB),
                (1241, LOW, 441 * MIB),
                (2211, LOW, 441 * MIB),
            ],
            None,
            102 * MIB,
        ),
        (
            "at the cap after the event, then its RAM taken down with the swap rise",
            &[(0, AT, 0), (1, AT, MIB), (1, LOW, 439 * MIB)],
            None,
            MIB,
        ),
        (
            "max never grows: all the swap is the host's, at the cap or not",
            &[
                (0, AT, 0),
                (0, AT, 100 * MIB),
                (0, AT, 439 * MIB),
                (0, LOW, 441 * MIB),
            ],
            None,
            0,
        ),
        (
            "the rise two samples after the event is the host's",
            &[
                (0, AT, 0),
                (1, AT, MIB),
                (1, AT, 2 * MIB),
                (1, AT, 439 * MIB),
            ],
            None,
            2 * MIB,
        ),
        (
            "RAM at 7/8 of the cap still holds about its cap",
            &[(0, AT, 0), (1, AT, MIB), (1, EDGE, 439 * MIB)],
            Some(2),
            439 * MIB,
        ),
        (
            "RAM a byte below 7/8 of the cap is well below it",
            &[(0, AT, 0), (1, AT, MIB), (1, EDGE - 1, 439 * MIB)],
            None,
            MIB,
        ),
    ];
    for (row, samples, killed, own) in rows {
        let mut watch = SwapWatch::new(128 * MIB, CAP);
        let at = samples
            .iter()
            .position(|&(max, current, swap)| watch.kills(Pressure { max, current, swap }));
        assert_eq!((at, watch.own()), (killed, own), "{row}");
    }
}

/// Catches a read that fails between a `max` event and the rise after it dropping the
/// event's credit, or taken as a sample. The watch takes the event at the cap, a failed
/// read, then the swap-out's rise: the rise is compared with the event's sample, so it
/// is the lease's own if the lease still holds about its cap, and the host's if not.
#[test]
fn a_failed_read_between_the_max_event_and_the_rise_keeps_the_event_credit() {
    for (current, kill) in [(CAP, true), (CAP / 4, false)] {
        let mut watch = SwapWatch::new(128 * MIB, CAP);
        let event = Pressure {
            max: 1,
            current: CAP,
            swap: 24 * MIB,
        };
        assert_eq!(watch.take(Ok(event)), None);
        assert_eq!(watch.take(Err(io::Error::other("gone"))), None);
        let rise = Pressure {
            max: 1,
            current,
            swap: 241 * MIB,
        };
        let own = if kill { 241 * MIB } else { 24 * MIB };
        assert_eq!(
            watch.take(Ok(rise)),
            kill.then_some((rise, own)),
            "RAM {current}"
        );
        assert_eq!(watch.own(), own, "RAM {current}");
    }
}
