//! Where the container driver makes the spool its missing outputs are copied into:
//! the lease's scratch directory, which only the daemon writes, never the overlay's
//! upper directory, whose names the action chose. Alone in its test binary: the spool
//! names count up from 0 in each process, so no other test may make a spool beside it.

mod support;

use support::Spec;
use support::fake::{Fake, image};

/// Catches the spool made in the upper directory (mutant seen red: `&spec.upper` in
/// place of `&lease.dir` in `runtime.rs`): the action names what is there, so a file
/// it leaves under the spool's name (the daemon's pid and a count from 0) makes the
/// spool's exclusive create fail and the lease fail with it. In the lease's scratch
/// directory the action names nothing and the run is stored.
#[tokio::test]
async fn the_spool_is_made_where_the_action_names_nothing() {
    let fake = Fake::new("spool-dir");
    let mut spec = Spec::new(&image(), "unused by the fake");
    spec.outputs = vec!["out.txt".to_owned()];
    let mut steps = vec![
        "write-lines".to_owned(),
        "$UPPER/out.txt".to_owned(),
        "an output".to_owned(),
    ];
    for n in 0..16 {
        let name = format!("$UPPER/.kbf-spool-{}-{n}", std::process::id());
        steps.extend([
            ";".to_owned(),
            "write-lines".to_owned(),
            name,
            "x".to_owned(),
        ]);
    }
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(result.output_files.len(), 1, "the output stored");
    assert_eq!(
        support::blob(&fake.cas, result.output_files[0].digest.as_ref()),
        b"an output\n"
    );
    fake.assert_clean(1);
}
