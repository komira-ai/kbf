//! A few seeds of every scenario kind, small enough for the pull-request gate. The full
//! sweeps (`cell.rs`, `write_path.rs`, `write_tier.rs`) run their 16 to 64 seeds on the
//! push; these keep the code each kind reaches under test on every pull request. Every
//! run checks all its properties after every step (`check.rs`); each test adds what its
//! kind must show beyond them, and that its fault was met.

use kbf_sim_cell::{
    Down, Kind, Met, RunReport, Scenario, ServerFault, StorePoint, Sweep, TierPoint, sweep,
};

fn swept(kind: Kind, seeds: std::ops::Range<u64>) -> Sweep {
    let n = seeds.end - seeds.start;
    let swept = sweep(seeds, |seed| Scenario::new(kind, seed));
    swept.assert_clean();
    assert_eq!(swept.reports.len() as u64, n);
    swept
}

fn count(swept: &Sweep, met: impl Fn(&RunReport) -> bool) -> usize {
    swept.each().filter(|r| met(r)).count()
}

/// Catches a calm network, a restart or a whole-process kill that breaks a property, a
/// run that answers nothing, and a restart kind that never restarts.
#[test]
fn calm_restart_and_whole_process_seeds_answer_and_keep_every_property() {
    for kind in [Kind::Calm, Kind::Restart, Kind::WholeProcess] {
        let swept = swept(kind, 0..3);
        for (seed, r) in &swept.reports {
            assert!(
                r.present > 0 && r.accepted > 0,
                "{kind:?} seed {seed}: {r:?}"
            );
            if kind.restarts() {
                assert!(r.starts > 1, "{kind:?} seed {seed} never restarted");
            }
        }
        if kind == Kind::WholeProcess {
            assert!(
                count(&swept, |r| r.lost > 0) > 0,
                "no kill cut the daemon half"
            );
        }
    }
}

/// Catches a store outage (hanging on even seeds, refusing on odd ones) that meets no
/// PUT, a slow store that slows nothing, and faults that lose, keep or fail PUTs
/// without the server answering for them.
#[test]
fn a_few_seeds_of_each_store_fault_meet_it_and_keep_every_property() {
    for (seed, r) in &swept(Kind::StoreOutage, 0..2).reports {
        let met = if seed % 2 == 0 {
            r.store.held
        } else {
            r.store.failed
        };
        assert!(met > 0, "outage seed {seed} met no PUT: {r:?}");
    }
    for (seed, r) in &swept(Kind::StoreSlow, 0..3).reports {
        assert!(r.store.slowed > 0, "slow seed {seed}: {r:?}");
    }
    let errors = swept(Kind::StoreErrors, 0..6);
    let failed = |m: Met| {
        errors
            .each()
            .flat_map(|r| &r.met)
            .filter(|x| x.2 == m)
            .count()
    };
    assert!(failed(Met::Failed) > 0, "no PUT was refused");
    let lost = swept(Kind::StoreLostAck, 0..4);
    assert!(
        count(&lost, |r| r.store.lost_acks > 0) > 0,
        "no ack was lost"
    );
}

/// Catches a crash point that never fires, or a restart after it that breaks a
/// property: the server goes down once at a random point of the write path, and at each
/// point under a kill and a power cut keeping nothing.
#[test]
fn a_crash_on_the_write_path_fires_once_and_the_server_comes_back() {
    let swept = swept(Kind::StoreCrash, 0..6);
    assert!(count(&swept, |r| r.crashes > 0) > 0, "no seed crashed");
    for (seed, r) in &swept.reports {
        assert_eq!(r.starts, 1 + r.crashes as u64, "seed {seed}");
    }
    let points = [
        StorePoint::PutSent,
        StorePoint::BeforeCommit,
        StorePoint::InCommit { after: 1 },
        StorePoint::BeforeReply,
    ];
    for point in points {
        for down in [Down::Kill, Down::PowerCut { keep: 0, names: 0 }] {
            let swept = sweep(0..1, |seed| Scenario::crash_at(seed, point, down));
            swept.assert_clean();
            for (seed, r) in &swept.reports {
                assert_eq!((r.crashes, r.starts), (1, 2), "{point} {down} seed {seed}");
            }
        }
    }
}

/// Catches a tier whose outage never fills it, a full disk that is not told from a
/// failing one, a bad sector no read or flush finds, and eviction that never happens.
#[test]
fn a_few_seeds_of_each_tier_fault_meet_it_and_keep_every_property() {
    for (seed, r) in &swept(Kind::TierStoreOutage, 0..2).reports {
        assert!(
            r.tier.full > 0,
            "outage seed {seed}: the budget never pushed back"
        );
    }
    let disk = swept(Kind::TierDiskFull, 0..6);
    assert!(
        count(&disk, |r| r.tier.disk_errors > 0) > 0,
        "no disk fault met a write"
    );
    for (seed, r) in &disk.reports {
        let want = if seed % 2 == 0 { r.tier.disk_errors } else { 0 };
        assert_eq!(r.tier.no_space, want, "seed {seed}: ENOSPC told apart");
    }
    let bad = swept(Kind::TierBadBlock, 0..6);
    let damaged: usize = bad.each().map(|r| r.tier.damaged.len()).sum();
    assert!(damaged > 0, "no seed damaged a block");
    for (seed, r) in &bad.reports {
        for object in &r.tier.damaged {
            let marked = r.tier.corrupt.contains(object) || r.tier.marked_on_read.contains(object);
            assert!(
                marked && !r.tier.flushed.contains(object),
                "seed {seed}: {object}"
            );
        }
    }
    let evict = swept(Kind::TierEviction, 0..4);
    assert!(
        count(&evict, |r| r.tier.evicted > 0) > 0,
        "nothing was evicted"
    );
}

/// Catches a tier crash point that never fires, or a restart after it that breaks a
/// property: random points over a few seeds, and every pinned point under a kill.
#[test]
fn a_crash_in_the_tier_fires_and_its_next_start_keeps_every_property() {
    let swept = swept(Kind::TierCrash, 0..6);
    assert!(
        count(&swept, |r| !r.tier.cuts.is_empty()) > 0,
        "no seed crashed"
    );
    let points = TierPoint::ALL.into_iter().chain([
        TierPoint::InPutStaged { after: 1 },
        TierPoint::InFlushCommit { after: 1 },
    ]);
    for point in points {
        let mut fired = 0;
        for down in [Down::Kill, Down::PowerCut { keep: 0, names: 0 }] {
            let swept = sweep(0..2, |seed| Scenario::tier_crash_at(seed, point, down));
            swept.assert_clean();
            fired += count(&swept, |r| {
                r.tier.cuts.first().is_some_and(|c| c.point == point)
            });
        }
        assert!(fired > 0, "{point}: no run crashed there");
    }
}

/// Catches: a crash kind whose seeds never draw one of its points (a sweep that cannot
/// meet it), a power cut kept in the wrong range, and an outage window of the wrong
/// length: minutes for the plain kinds (two to ten), an hour to an hour and a half for
/// the `Hours` kinds. Only drawn, not run: running an hour-long outage is the push
/// sweep's job.
#[test]
fn seeds_draw_every_crash_point_and_the_outage_windows_their_lengths() {
    let mut store_points = Vec::new();
    for seed in 0..64 {
        let s = Scenario::new(Kind::StoreCrash, seed);
        let [(_, ServerFault::CrashAt { point, nth, .. })] = s.server.faults[..] else {
            panic!("seed {seed}: {:?}", s.server.faults);
        };
        assert!(nth <= 1, "seed {seed}: nth {nth}");
        if !store_points.contains(&point) {
            store_points.push(point);
        }
    }
    let want = [
        StorePoint::PutSent,
        StorePoint::BeforeCommit,
        StorePoint::InCommit { after: 0 },
        StorePoint::InCommit { after: 1 },
        StorePoint::BeforeReply,
    ];
    assert!(
        want.iter().all(|p| store_points.contains(p)),
        "{store_points:?}"
    );
    assert_eq!(store_points.len(), want.len(), "{store_points:?}");
    let mut tier_points = Vec::new();
    for seed in 0..64 {
        let s = Scenario::new(Kind::TierCrash, seed);
        let [(_, ServerFault::TierCrashAt { point, .. })] = s.server.faults[..] else {
            panic!("seed {seed}: {:?}", s.server.faults);
        };
        if !tier_points.contains(&point) {
            tier_points.push(point);
        }
    }
    let every = TierPoint::ALL.into_iter().flat_map(|p| match p {
        TierPoint::InPutStaged { .. } => (0..=1)
            .map(|after| TierPoint::InPutStaged { after })
            .collect(),
        TierPoint::InFlushCommit { .. } => (0..=1)
            .map(|after| TierPoint::InFlushCommit { after })
            .collect(),
        other => vec![other],
    });
    let every: Vec<_> = every.collect();
    assert!(
        every.iter().all(|p| tier_points.contains(p)),
        "{tier_points:?}"
    );
    assert_eq!(tier_points.len(), every.len(), "{tier_points:?}");
    let minutes = |kind: Kind, seed: u64| {
        let (from, fault) = Scenario::new(kind, seed).store.faults[0];
        let until = fault.until().expect("an outage ends");
        (until.as_millis() - from.as_millis()) / 60_000
    };
    for seed in 0..16 {
        for kind in [Kind::StoreOutage, Kind::TierStoreOutage] {
            assert!(
                (2..=10).contains(&minutes(kind, seed)),
                "{kind:?} seed {seed}"
            );
        }
        for kind in [Kind::StoreOutageHours, Kind::TierStoreOutageHours] {
            assert!(
                (60..=90).contains(&minutes(kind, seed)),
                "{kind:?} seed {seed}"
            );
        }
    }
}
