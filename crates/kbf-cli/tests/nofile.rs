//! `kbf_cli::nofile::raise`: systemd starts a unit at a soft open-file limit of 1024, and
//! a storage node under a build's burst of uploads needs more. This file holds one test
//! because it lowers the process's soft limit.

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// With the soft limit at 256, `raise` takes it to the hard limit (at most the ceiling),
/// and 600 files can then be open at once. Mutants: `raise` a no-op (the soft limit
/// stays 256 and the 257th `open` fails with EMFILE), a raise to a fixed 1024 (the
/// soft-limit assertion fails).
#[test]
fn raise_takes_the_soft_limit_to_the_hard_limit() {
    let before = getrlimit(Resource::Nofile);
    let hard = before.maximum;
    assert!(
        hard.is_none_or(|h| h > 1024),
        "the test needs a hard limit above 1024, got {hard:?}"
    );
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(256),
            maximum: hard,
        },
    )
    .expect("lower the soft limit");

    let now = kbf_cli::nofile::raise().expect("raise");

    let want = hard.map_or(kbf_cli::nofile::CEILING, |h| {
        h.min(kbf_cli::nofile::CEILING)
    });
    assert_eq!(now, want);
    assert_eq!(getrlimit(Resource::Nofile).current, Some(want));
    let open: Vec<_> = (0..600)
        .map(|_| std::fs::File::open("/dev/null").expect("open under the raised limit"))
        .collect();
    assert_eq!(open.len(), 600);
}
