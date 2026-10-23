//! The version of a `kbf-installer` package (`xtask::pkg_version`): its format, the builds it
//! is not made for, and that dpkg and rpm put every pair of builds in the same order.
//!
//! The two orders below are the algorithms of dpkg (`verrevcmp` and the epoch and revision
//! split of `parseversion`, lib/dpkg/version.c) and of rpm (`rpmvercmp`, lib/rpmvercmp.c,
//! without the caret rule, which no version here contains), written again with no call to
//! either tool. They are held to known answers first, so that the agreement the last tests
//! show is not two copies of one mistake.

use std::cmp::Ordering;

use xtask::Error;
use xtask::pkg_version::PackageVersion;

/// A commit that sorts before `HIGH` as text, and one that sorts after it.
const LOW: &str = "0123456789ab";
const HIGH: &str = "fedcba987654";
const MAIN: &str = "refs/heads/main";

// ---- dpkg ----

/// dpkg's weight of a character: a digit 0 (digits are compared as numbers apart), a letter
/// its own value, `~` below everything (the end of the string too), any other above the letters.
fn dpkg_order(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(c) if c.is_ascii_digit() => 0,
        Some(c) if c.is_ascii_alphabetic() => i32::from(c),
        Some(b'~') => -1,
        Some(c) => i32::from(c) + 256,
    }
}

/// dpkg's `verrevcmp`: an upstream version, or a revision, against another.
fn dpkg_part(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let at = |s: &[u8], k: usize| s.get(k).copied();
    let digit = |c: Option<u8>| c.is_some_and(|c| c.is_ascii_digit());
    let sign = |c: Option<u8>| c.is_some_and(|c| !c.is_ascii_digit());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        while sign(at(a, i)) || sign(at(b, j)) {
            let (x, y) = (dpkg_order(at(a, i)), dpkg_order(at(b, j)));
            if x != y {
                return x.cmp(&y);
            }
            i += 1;
            j += 1;
        }
        while at(a, i) == Some(b'0') {
            i += 1;
        }
        while at(b, j) == Some(b'0') {
            j += 1;
        }
        let mut first = 0;
        while digit(at(a, i)) && digit(at(b, j)) {
            if first == 0 {
                first = i32::from(a[i]) - i32::from(b[j]);
            }
            i += 1;
            j += 1;
        }
        if digit(at(a, i)) {
            return Ordering::Greater;
        }
        if digit(at(b, j)) {
            return Ordering::Less;
        }
        if first != 0 {
            return first.cmp(&0);
        }
    }
    Ordering::Equal
}

/// A dpkg version as its epoch, upstream version and revision (what follows the last `-`).
fn dpkg_split(v: &str) -> (u64, &str, &str) {
    let (epoch, rest) = match v.split_once(':') {
        Some((epoch, rest)) => (epoch.parse().unwrap_or(0), rest),
        None => (0, v),
    };
    let (upstream, revision) = rest.rsplit_once('-').unwrap_or((rest, ""));
    (epoch, upstream, revision)
}

/// dpkg's order of two versions.
fn dpkg_cmp(a: &str, b: &str) -> Ordering {
    let (ea, ua, ra) = dpkg_split(a);
    let (eb, ub, rb) = dpkg_split(b);
    ea.cmp(&eb)
        .then_with(|| dpkg_part(ua, ub))
        .then_with(|| dpkg_part(ra, rb))
}

// ---- rpm ----

/// rpm's `rpmvercmp`: a Version, or a Release, against another.
fn rpm_part(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // The byte at `k`, 0 past the end as a C string has it.
    let at = |s: &[u8], k: usize| s.get(k).copied().unwrap_or(0);
    let separator = |c: u8| c != 0 && !c.is_ascii_alphanumeric() && c != b'~';
    let (mut one, mut two) = (0, 0);
    while at(a, one) != 0 || at(b, two) != 0 {
        while separator(at(a, one)) {
            one += 1;
        }
        while separator(at(b, two)) {
            two += 1;
        }
        if at(a, one) == b'~' || at(b, two) == b'~' {
            if at(a, one) != b'~' {
                return Ordering::Greater;
            }
            if at(b, two) != b'~' {
                return Ordering::Less;
            }
            one += 1;
            two += 1;
            continue;
        }
        if at(a, one) == 0 || at(b, two) == 0 {
            break;
        }
        let numeric = at(a, one).is_ascii_digit();
        let kind = |c: u8| {
            if numeric {
                c.is_ascii_digit()
            } else {
                c.is_ascii_alphabetic()
            }
        };
        let (mut end_a, mut end_b) = (one, two);
        while kind(at(a, end_a)) {
            end_a += 1;
        }
        while kind(at(b, end_b)) {
            end_b += 1;
        }
        // A numeric segment is newer than an alphabetic one, and the other way round.
        if end_b == two {
            return if numeric {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let (mut x, mut y) = (&a[one..end_a], &b[two..end_b]);
        if numeric {
            while x.first() == Some(&b'0') {
                x = &x[1..];
            }
            while y.first() == Some(&b'0') {
                y = &y[1..];
            }
            if x.len() != y.len() {
                return x.len().cmp(&y.len());
            }
        }
        if x != y {
            return x.cmp(y);
        }
        one = end_a;
        two = end_b;
    }
    match (at(a, one) == 0, at(b, two) == 0) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        _ => Ordering::Greater,
    }
}

/// rpm's order of two packages by (Version, Release).
fn rpm_cmp(a: (&str, &str), b: (&str, &str)) -> Ordering {
    rpm_part(a.0, b.0).then_with(|| rpm_part(a.1, b.1))
}

// ---- the oracles, against known answers ----

/// Catches a dpkg comparison that is not dpkg's: the answers are the ones `deb-version(7)`
/// and dpkg's own tests give. A `~` below the end of the string, a letter below any other
/// sign, digits compared as numbers with leading zeros ignored, the revision after the last
/// hyphen, the epoch first.
#[test]
fn the_dpkg_comparison_gives_dpkgs_answers() {
    use Ordering::{Equal, Greater, Less};
    let known = [
        ("1.0", "1.0", Equal),
        ("1.0", "1.1", Less),
        ("1.9", "1.10", Less),
        ("1.01", "1.1", Equal),
        ("1.0~rc1", "1.0", Less),
        ("1.0~~", "1.0~", Less),
        ("1.0~", "1.0", Less),
        ("1.0", "1.0+b1", Less),
        ("1.0a", "1.0", Greater),
        ("1.0a", "1.0+", Less),
        ("1.0-1", "1.0-2", Less),
        ("1.0-9", "1.0-10", Less),
        ("1.0-2", "1.0-1-1", Less),
        ("1:0.1", "9.9", Greater),
        ("2.0", "1:0.1", Less),
    ];
    for (a, b, want) in known {
        assert_eq!(dpkg_cmp(a, b), want, "{a} against {b}");
        assert_eq!(dpkg_cmp(b, a), want.reverse(), "{b} against {a}");
    }
}

/// Catches an rpm comparison that is not rpm's: the answers are those of rpm's
/// `rpmvercmp.at`. Numbers by value with leading zeros ignored, a number newer than letters,
/// separators of any kind equal, `~` before the end of the string, a version with segments
/// left over newer.
#[test]
fn the_rpm_comparison_gives_rpms_answers() {
    use Ordering::{Equal, Greater, Less};
    let known = [
        ("1.0", "1.0", Equal),
        ("1.0", "2.0", Less),
        ("2.0", "2.0.1", Less),
        ("2.0.1a", "2.0.1", Greater),
        ("5.5p10", "5.5p2", Greater),
        ("10xyz", "10.1xyz", Less),
        ("xyz.4", "8", Less),
        ("6.0.rc1", "6.0", Greater),
        ("6.0.rc1", "6.0.1", Less),
        ("10b2", "10a1", Greater),
        ("10.0001", "10.1", Equal),
        ("4.999.9", "5.0", Less),
        ("2.0", "2_0", Equal),
        ("1.0~rc1", "1.0", Less),
        ("1.0", "1.0~rc1", Greater),
        ("1.0~rc1~git123", "1.0~rc1", Less),
        ("1.0~rc1", "1.0~rc2", Less),
        ("1.9", "1.10", Less),
    ];
    for (a, b, want) in known {
        assert_eq!(rpm_part(a, b), want, "{a} against {b}");
        assert_eq!(rpm_part(b, a), want.reverse(), "{b} against {a}");
    }
    // The Release decides only when the Versions are the same.
    assert_eq!(rpm_cmp(("1.0", "9"), ("1.0", "10")), Less);
    assert_eq!(rpm_cmp(("1.1", "1"), ("1.0", "10")), Greater);
}

// ---- the version ----

/// The version of a build, which must be one.
fn named(seq: u64, commit: &str, run: u64, git_ref: &str) -> PackageVersion {
    PackageVersion::new(seq, commit, run, git_ref).expect("a build that can be named")
}

/// The reasons `PackageVersion::new` refuses a build.
fn refused(seq: u64, commit: &str, run: u64, git_ref: &str) -> Vec<String> {
    match PackageVersion::new(seq, commit, run, git_ref) {
        Err(Error::Refused(problems)) => problems,
        Err(other) => panic!("expected a refusal, got: {other}"),
        Ok(v) => panic!("expected a refusal, got {}", v.deb()),
    }
}

/// Catches the format drifting: the deb `0.1.<seq>+g<sha12>-<run>` and the rpm Version
/// `0.1.<seq>`, Release `<run>.g<sha12>` of a build of `main`, from a commit given in full.
#[test]
fn a_build_of_main_is_named_by_its_seq_commit_and_run() {
    let full = "0123456789abcdef0123456789abcdef01234567";
    let v = named(123, full, 45, MAIN);
    assert_eq!(v.deb(), "0.1.123+g0123456789ab-45");
    assert_eq!(v.rpm_version(), "0.1.123");
    assert_eq!(v.rpm_release(), "45.g0123456789ab");
}

/// Catches a build of another ref named like a build of `main` (it would then replace one in
/// a mirror), a ref that reaches the version as it is (a hyphen ends a deb's upstream version,
/// an rpm takes none, a slash is neither's), and a ref of any length.
#[test]
fn a_build_of_another_ref_carries_the_word_of_its_name() {
    let v = named(123, LOW, 45, "refs/heads/Feature/Air-Gap_2");
    assert_eq!(v.deb(), "0.1.123~featureairgap2+g0123456789ab-45");
    assert_eq!(v.rpm_version(), "0.1.123~featureairgap2");
    assert_eq!(v.rpm_release(), "45.g0123456789ab");
    let pull = named(7, LOW, 3, "refs/pull/17/merge");
    assert_eq!(pull.deb(), "0.1.7~refspull17merge+g0123456789ab-3");
    let long = named(7, LOW, 3, &format!("refs/heads/{}", "b".repeat(40)));
    assert_eq!(long.rpm_version(), format!("0.1.7~{}", "b".repeat(24)));
}

/// Catches a build that cannot be put in order getting a version anyway: a `seq` of 0 (a
/// shallow checkout, which the message tells how to fix), a commit that is not one, a run
/// that is not numbered, a ref that is not a full ref or names nothing. Each is refused on its
/// own, and a build with all of them is refused with all of them.
#[test]
fn a_build_that_cannot_be_put_in_order_is_refused_with_every_reason() {
    for (seq, commit, run, git_ref, word) in [
        (0, LOW, 1, MAIN, "seq is 0"),
        (1, "0123", 1, MAIN, "not 12 to 40 hex digits"),
        (1, "0123456789AB", 1, MAIN, "not 12 to 40 hex digits"),
        (1, LOW, 0, MAIN, "run number is 0"),
        (1, LOW, 1, "main", "not a full ref"),
        (1, LOW, 1, "refs/heads/---", "no letter or digit"),
    ] {
        let problems = refused(seq, commit, run, git_ref);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains(word), "{word}: {problems:?}");
    }
    let shallow = refused(0, LOW, 1, MAIN);
    assert!(shallow[0].contains("fetch-depth: 0"), "{shallow:?}");
    let all = refused(0, "x", 0, "main");
    assert_eq!(all.len(), 4, "{all:?}");
}

// ---- the orders agree ----

/// One build of the sample, with the place it has in the order of builds.
struct Build {
    seq: u64,
    run: u64,
    main: bool,
    version: PackageVersion,
}

impl Build {
    /// What puts builds in order: the `seq`, then a build of another ref below the build of
    /// `main` at its `seq`, then the run.
    fn rank(&self) -> (u64, bool, u64) {
        (self.seq, self.main, self.run)
    }
}

/// Builds that exercise the order: numbers that sort differently as text (9 and 10, 15 and 16,
/// 99 and 100), runs of one commit (a re-run), a build of another ref at the seq of a build
/// of `main` whose commit sorts above that build's, and branch names with signs in them.
fn builds() -> Vec<Build> {
    let sample = [
        (1, LOW, 1, MAIN),
        (2, LOW, 1, MAIN),
        (9, LOW, 1, MAIN),
        (9, HIGH, 5, "refs/heads/feature"),
        (10, LOW, 1, MAIN),
        (10, LOW, 2, MAIN),
        (10, LOW, 9, MAIN),
        (10, LOW, 10, MAIN),
        (10, HIGH, 20, "refs/heads/feature"),
        (12, LOW, 1, "refs/pull/17/merge"),
        (15, HIGH, 3, MAIN),
        (16, HIGH, 3, MAIN),
        (99, LOW, 3, MAIN),
        (100, HIGH, 3, MAIN),
        (100, HIGH, 40, "refs/heads/Air-Gap"),
        (1000, LOW, 3, MAIN),
        (1000, LOW, 12, MAIN),
    ];
    let mut builds = Vec::new();
    for (seq, sha, run, git_ref) in sample {
        let version = named(seq, sha, run, git_ref);
        builds.push(Build {
            seq,
            run,
            main: git_ref == MAIN,
            version,
        });
    }
    builds
}

/// Catches the property the format exists for: dpkg and rpm each put every pair of builds in
/// the order of their `seq` (as numbers: 9 before 10, 15 before 16), a build of another ref
/// below the build of `main` at its `seq` whatever its commit and run, and a re-run after
/// the run it repeats (9 before 10), and so agree with each other.
///
/// The defects it catches: the `~` word put after the commit in the deb, or left out
/// (the commit then decides, and `HIGH` is above `LOW`); the `seq` written in hex (15 and
/// 16 swap).
#[test]
fn dpkg_and_rpm_put_every_pair_of_builds_in_the_same_order() {
    let all = builds();
    let mut wrong = Vec::new();
    for x in &all {
        for y in &all {
            let want = x.rank().cmp(&y.rank());
            let deb = dpkg_cmp(&x.version.deb(), &y.version.deb());
            let rpm = rpm_cmp(
                (&x.version.rpm_version(), &x.version.rpm_release()),
                (&y.version.rpm_version(), &y.version.rpm_release()),
            );
            if deb != want || rpm != want {
                wrong.push(format!(
                    "{} against {}: dpkg {deb:?}, rpm {rpm:?}, the builds {want:?}",
                    x.version.deb(),
                    y.version.deb()
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Catches a build of the new form that is not above every package of the old form,
/// `0.1.0+<12 hex digits>`, which apt and dnf must replace on upgrade (a `seq` of 0 would not
/// be, which is why none is named).
#[test]
fn every_build_is_newer_than_a_package_of_the_old_form() {
    let old = "0.1.0+0123456789ab";
    for b in builds() {
        let deb = b.version.deb();
        assert_eq!(dpkg_cmp(old, &deb), Ordering::Less, "{old} against {deb}");
        let (version, release) = (b.version.rpm_version(), b.version.rpm_release());
        let rpm = rpm_cmp((old, "1"), (&version, &release));
        assert_eq!(rpm, Ordering::Less, "{old} against {version}-{release}");
    }
}

/// Catches a part holding a character its format does not take: a deb's version has exactly
/// one hyphen (before the revision), an rpm's Version none, and its Release neither a hyphen
/// nor a `~`. The sample has a branch named with a hyphen and an underscore.
#[test]
fn each_part_holds_only_what_its_format_takes() {
    for b in builds() {
        let deb = b.version.deb();
        assert!(deb.starts_with(|c: char| c.is_ascii_digit()), "{deb}");
        assert!(
            deb.chars()
                .all(|c| c.is_ascii_alphanumeric() || ".+~-".contains(c)),
            "{deb}"
        );
        assert_eq!(deb.matches('-').count(), 1, "{deb}");
        let version = b.version.rpm_version();
        assert!(
            version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".+~".contains(c)),
            "{version}"
        );
        let release = b.version.rpm_release();
        assert!(
            release
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.'),
            "{release}"
        );
    }
}
