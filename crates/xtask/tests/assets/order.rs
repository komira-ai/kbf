//! `verify-assets attestations` checks the files in parallel: what it prints and the error it
//! returns follow SHA256SUMS, whatever order the checks finish in.

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
    assert_eq!(
        err,
        format!(
            "xtask: {}: no build provenance attestation signed by {WORKFLOW} verifies\n",
            s(&r.dist.join(&names[2]))
        )
    );
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
        assert_eq!(
            err,
            format!(
                "xtask: {}: no build provenance attestation signed by {WORKFLOW} verifies\n",
                s(&r.dist.join(name))
            )
        );
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
