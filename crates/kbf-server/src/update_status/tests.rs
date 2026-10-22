use kbf_proto::update::update_status::Available;

use super::*;

fn status(state: State) -> UpdateStatus {
    UpdateStatus {
        running_commit: "0123456789ab".to_owned(),
        state: state as i32,
        ..UpdateStatus::default()
    }
}

/// The wire state is kept as the controller reads it; zero and a number this build does not
/// know read as unspecified, never as a state.
#[test]
fn the_wire_state_is_kept_and_an_unknown_one_is_unspecified() {
    for state in [
        State::Idle,
        State::Fetching,
        State::Staged,
        State::ActivatedUnconfirmed,
        State::Confirmed,
        State::Reverted,
        State::Refused,
        State::Manual,
    ] {
        assert_eq!(UpdateReport::new(status(state)).kind, state);
    }
    let odd = UpdateStatus {
        state: 99,
        ..UpdateStatus::default()
    };
    assert_eq!(UpdateReport::new(odd).kind, State::Unspecified);
    assert_eq!(
        UpdateReport::new(UpdateStatus::default()).kind,
        State::Unspecified
    );
}

/// The commits and the acked order id a host sends are kept; the commits are cut to the
/// bound on a character boundary and have control characters replaced.
#[test]
fn text_from_a_host_is_cut_on_a_character_boundary_and_has_no_control_characters() {
    let report = UpdateReport::new(UpdateStatus {
        running_commit: "r".repeat(1000),
        installed_commit: "i".repeat(1000),
        acked_order_id: 42,
        ..UpdateStatus::default()
    });
    assert_eq!(report.running_commit.len(), COMMIT_MAX_BYTES);
    assert_eq!(report.installed_commit.len(), COMMIT_MAX_BYTES);
    assert_eq!(report.acked_order_id, 42);

    // 3-byte characters: 21 fit in 64 bytes (63), the 22nd would make 66.
    let wide = UpdateReport::new(UpdateStatus {
        installed_commit: "\u{20ac}".repeat(1000),
        ..UpdateStatus::default()
    });
    assert_eq!(wide.installed_commit.len(), 63);
    assert!(wide.installed_commit.chars().all(|c| c == '\u{20ac}'));

    let odd = UpdateReport::new(UpdateStatus {
        running_commit: "a\nb\u{1b}[31mc\0d\u{7f}".to_owned(),
        ..UpdateStatus::default()
    });
    assert_eq!(
        odd.running_commit, "a\u{fffd}b\u{fffd}[31mc\u{fffd}d\u{fffd}",
        "control characters are replaced, the rest kept"
    );
}

/// A commit of exactly the bound is kept whole, and one a byte over loses its last
/// character only.
#[test]
fn a_commit_at_the_bound_is_kept_whole() {
    let exact = "e".repeat(COMMIT_MAX_BYTES);
    let kept = |installed_commit: String| {
        UpdateReport::new(UpdateStatus {
            installed_commit,
            ..UpdateStatus::default()
        })
        .installed_commit
    };
    assert_eq!(kept(exact.clone()), exact);
    assert_eq!(kept(format!("{exact}e")), exact);
}

/// The report of an idle host that sent `available` and `release_source` as given.
fn package(available: Option<(&str, u64)>, release_source: i32) -> UpdateReport {
    UpdateReport::new(UpdateStatus {
        available: available.map(|(commit, seq)| Available {
            commit: commit.to_owned(),
            seq,
        }),
        release_source,
        ..status(State::Idle)
    })
}

/// A report that names no package has `None`, never an empty commit; one that names a package
/// keeps its commit and its `seq`, and the source it came with. Each wire source has its own
/// variant. Zero and a number this build does not know (above the last, or negative) read as
/// unreported, never as one of the others.
#[test]
fn the_package_and_the_source_a_host_reports_are_kept() {
    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    let kept = package(Some((COMMIT, 812)), WireSource::Bundled as i32);
    let have = kept.available.as_ref().expect("a package");
    assert_eq!((have.commit.as_str(), have.seq), (COMMIT, 812));
    assert_eq!(kept.release_source, ReleaseSource::Bundled);

    let none = package(None, WireSource::Url as i32);
    assert!(none.available.is_none());
    assert_eq!(none.release_source, ReleaseSource::Url);

    for (number, want) in [
        (WireSource::Unspecified as i32, ReleaseSource::Unreported),
        (WireSource::Url as i32, ReleaseSource::Url),
        (WireSource::Frontend as i32, ReleaseSource::Frontend),
        (WireSource::Bundled as i32, ReleaseSource::Bundled),
        (4, ReleaseSource::Unreported),
        (99, ReleaseSource::Unreported),
        (-1, ReleaseSource::Unreported),
    ] {
        let got = package(None, number).release_source;
        assert_eq!(got, want, "number {number}");
    }
}

/// A package commit is a string from a host: cut to the bound of every commit and with
/// control characters replaced. The report keeps a commit that is no commit; the facts
/// refuse it.
#[test]
fn the_commit_of_a_package_is_kept_as_data() {
    let text = "a".repeat(1000);
    let long = package(Some((text.as_str(), 1)), 0);
    let have = long.available.as_ref().expect("a package");
    assert_eq!(have.commit.len(), COMMIT_MAX_BYTES);
    let odd = package(Some(("ab\ncd", 1)), 0);
    let have = odd.available.as_ref().expect("a package");
    assert_eq!(have.commit, "ab\u{fffd}cd");
}
