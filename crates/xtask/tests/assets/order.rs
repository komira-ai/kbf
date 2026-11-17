//! `verify-assets attestations` checks the files in parallel: what it prints and the error it
//! returns follow SHA256SUMS, whatever order the checks finish in, and a file refused among
//! the parallel checks is checked once more, alone, after them.

use super::fixture::*;

#[test]
fn several_failing_files_are_reported_as_the_first_of_the_sums() {
    // Catches: the run reporting the last failure, or one after the first, instead of the
    // first failing line of SHA256SUMS (the order of results whatever order the checks
    // finish in is `sums::in_order`'s own test).
    let r = Release::build("first-failure");
    r.attest(None);
    let names: Vec<String> = r.sums().into_iter().map(|(_, n)| n).collect();
    assert!(names.len() > 8, "more lines than workers: {names:?}");
    let failing = [&names[2], &names[5], &names[names.len() - 1]];
    r.unattest(&failing.map(String::as_str));
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(err, refused(s(&r.dist.join(&names[2])), "build provenance"));
}

#[test]
fn a_failure_on_the_first_or_the_last_line_of_the_sums_fails_the_run() {
    // Catches: the first line, which is checked alone before the others, passed over when
    // it fails, and a pool that stops before the last line or drops its result.
    for (case, pick) in [("first-line-failure", 0), ("last-line-failure", usize::MAX)] {
        let r = Release::build(case);
        r.attest(None);
        let names: Vec<String> = r.sums().into_iter().map(|(_, n)| n).collect();
        let name = &names[pick.min(names.len() - 1)];
        r.unattest(&[name.as_str()]);
        let (code, _, err) = r.verify(WORKFLOW, None);
        assert_eq!(code, Some(1), "{case}");
        assert_eq!(err, refused(s(&r.dist.join(name)), "build provenance"));
    }
}

#[test]
fn verified_lines_are_printed_in_sums_order_with_more_lines_than_workers() {
    // Catches: output in the order the checks finish rather than the order of SHA256SUMS,
    // or a line lost or doubled by the pool.
    let r = Release::build("print-order");
    r.attest(None);
    r.bundles("refs/heads/main", COMMIT);
    let names: Vec<String> = r.sums().into_iter().map(|(_, n)| n).collect();
    assert!(names.len() > 8, "more lines than workers: {names:?}");
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(0), "{err}");
    let printed: Vec<&str> = out
        .lines()
        .filter_map(|l| l.strip_prefix("verified: "))
        .map(|l| l.split_once(' ').expect("name and kinds").0)
        .collect();
    assert_eq!(printed, names);
}

#[test]
fn an_attested_file_refused_once_verifies_when_checked_again_alone_after_the_pool() {
    // Catches: one refused `gh attestation verify` among the parallel checks failing the run
    // for a file whose attestation is there (main's pipeline went red on one such refusal);
    // the second check made inside the pool rather than after it; and the first line,
    // checked alone before the pool, not checked again.
    let r = Release::build("refused-once");
    r.attest(None);
    r.bundles("refs/heads/main", COMMIT);
    let names: Vec<String> = r.sums().into_iter().map(|(_, n)| n).collect();
    assert!(names.len() > 8, "more lines than workers: {names:?}");
    let refused_once = [&names[0], &names[4], &names[names.len() - 1]];
    for name in refused_once {
        r.refuse_once(name);
    }
    let (code, out, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(0), "{err}");
    let printed: Vec<&str> = out
        .lines()
        .filter_map(|l| l.strip_prefix("verified: "))
        .map(|l| l.split_once(' ').expect("name and kinds").0)
        .collect();
    assert_eq!(printed, names);
    let calls = r.gh_calls();
    let provenance = |name: &str| {
        format!(
            "attestation verify {} {}",
            s(&r.dist.join(name)),
            flags(WORKFLOW)
        )
    };
    // Every other file's provenance is asked for once, and each of these twice.
    for name in &names {
        let made = calls.iter().filter(|c| **c == provenance(name)).count();
        let want = if refused_once.contains(&name) { 2 } else { 1 };
        assert_eq!(made, want, "{name}: {calls:#?}");
    }
    // The second checks come after every file's first, in the order of SHA256SUMS.
    let last_first = names
        .iter()
        .map(|n| {
            calls
                .iter()
                .position(|c| *c == provenance(n))
                .expect("asked")
        })
        .max()
        .expect("lines");
    let again: Vec<usize> = refused_once
        .iter()
        .map(|n| {
            calls
                .iter()
                .rposition(|c| *c == provenance(n))
                .expect("asked")
        })
        .collect();
    assert!(again[0] > last_first, "{calls:#?}");
    assert!(again.windows(2).all(|w| w[0] < w[1]), "{calls:#?}");
}

#[test]
fn a_file_refused_again_when_checked_alone_fails_the_run_after_two_checks() {
    // Catches: a refusal retried until it passes, or more than once (a file nobody attested
    // must cost two checks, not a loop), and a refusal the second check is not asked for.
    let r = Release::build("refused-twice");
    r.attest(None);
    let names: Vec<String> = r.sums().into_iter().map(|(_, n)| n).collect();
    let name = &names[3];
    r.unattest(&[name.as_str()]);
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1));
    let path = s(&r.dist.join(name)).to_owned();
    assert_eq!(err, refused(&path, "build provenance"));
    let asked = r
        .gh_calls()
        .into_iter()
        .filter(|c| *c == format!("attestation verify {path} {}", flags(WORKFLOW)))
        .count();
    assert_eq!(asked, 2);
}
