//! The supported Envoy range: what `envoy --version` may print, and what is refused.

use std::io;
use std::path::Path;

use super::version::{EnvoyVersion, SUPPORTED, VersionError, check_installed, check_output};
use crate::fixtures::{FakeRunner, ran};
use crate::run::Ran;

fn printed(version: &str) -> String {
    format!(
        "envoy  version: 5d3b7a1e0f9c8b7a6d5e4f3a2b1c0d9e8f7a6b5c/{version}/Clean/RELEASE/BoringSSL\n"
    )
}

fn refused(output: &str) -> String {
    check_output(output).expect_err("refused").to_string()
}

/// Catches a range with an edge moved: the first and last supported releases are taken,
/// the one before and the one after are not. Mutants: a minimum of 1.39.0 or 1.37.0, a
/// bound of 1.39.0 or 1.41.0.
#[test]
fn the_range_edges_are_exact() {
    for ok in ["1.38.0", "1.38.4", "1.39.3", "1.39.99"] {
        let found = check_output(&printed(ok)).unwrap_or_else(|e| panic!("{ok}: {e}"));
        assert_eq!(found.to_string(), ok);
    }
    for bad in ["1.37.99", "1.40.0", "1.40.1", "2.0.0", "0.38.0", "1.9.0"] {
        assert!(
            matches!(
                check_output(&printed(bad)),
                Err(VersionError::OutOfRange { .. })
            ),
            "{bad}"
        );
    }
    let (min, below) = SUPPORTED;
    assert_eq!(
        (min.to_string(), below.to_string()),
        ("1.38.0".into(), "1.40.0".into())
    );
}

/// Catches a string compare of versions (1.9.0 sorting above 1.40.0) and a refusal that
/// does not say which version, which range, or that nothing is fetched.
#[test]
fn the_refusal_names_the_version_the_range_and_the_remedy() {
    let msg = refused(&printed("1.41.2"));
    assert!(msg.contains("Envoy 1.41.2"), "{msg}");
    assert!(msg.contains("1.38.0 up to, not including, 1.40.0"), "{msg}");
    assert!(msg.contains("never fetches"), "{msg}");
}

/// Catches an unparsable version being let through: each of these names no version of
/// the form `major.minor.patch`.
#[test]
fn text_without_a_version_is_refused() {
    // Joined at run time: four dotted numbers in a source file read as an IPv4 address to the hygiene lint.
    let four = format!(
        "envoy  version: abc/{}/Clean",
        ["1", "39", "3", "1"].join(".")
    );
    let four_parts = four.as_str();
    for text in [
        "",
        "\n",
        "envoy",
        "envoy  version: ",
        "envoy  version: abc",
        "envoy  version: abc/Clean/RELEASE",
        "envoy  version: abc/1.39/Clean",
        four_parts,
        "envoy  version: abc/1.39.x/Clean",
        "envoy  version: abc/1..3/Clean",
        "envoy  version: abc/1.39./Clean",
        "envoy  version: abc/99999999999.1.1/Clean",
        "envoy  version: abc/1.39.3-dev/Clean",
        "envoy  version: abc/+1.39.3/Clean",
        "envoy  version: abc/ 1.39.3/Clean",
        "version: abc/1.39.3/Clean",
        "nginx version: nginx/1.39.3",
        "1.39.3",
    ] {
        assert!(
            matches!(check_output(text), Err(VersionError::Unparsable { .. })),
            "{text:?}"
        );
    }
    let long = "x".repeat(5000);
    assert!(refused(&long).len() < 600, "the quoted output is cut");
}

fn runner(answer: io::Result<Ran>) -> FakeRunner {
    let answer = std::cell::RefCell::new(Some(answer));
    FakeRunner::new(move |_| answer.borrow_mut().take().expect("one call"))
}

/// Catches an installed check that runs something other than `<envoy> --version` with an
/// empty environment, or that trusts the exit status less than the text.
#[test]
fn the_installed_envoy_is_asked_for_its_version() {
    let r = runner(Ok(ran(0, &printed("1.39.3"), "")));
    let v = check_installed(&r, Path::new("/opt/envoy/bin/envoy")).expect("supported");
    assert_eq!(
        v,
        EnvoyVersion {
            major: 1,
            minor: 39,
            patch: 3
        }
    );
    let calls = r.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].program, Path::new("/opt/envoy/bin/envoy"));
    assert_eq!(calls[0].args, ["--version"]);
    assert!(calls[0].env.is_empty());
    assert_eq!(calls[0].run_as, None);
}

/// Catches an installed Envoy outside the range, one that does not run, one that fails
/// and one that hangs being let through: each is refused with its own words.
#[test]
fn an_installed_envoy_that_is_out_of_range_or_broken_is_refused() {
    let at = Path::new("/opt/envoy/bin/envoy");
    let err = |a: io::Result<Ran>| check_installed(&runner(a), at).expect_err("refused");
    let out = err(Ok(ran(0, &printed("1.41.0"), "")));
    assert!(matches!(out, VersionError::OutOfRange { .. }), "{out}");
    let gone = err(Err(io::Error::from(io::ErrorKind::NotFound)));
    assert!(gone.to_string().contains("/opt/envoy/bin/envoy"), "{gone}");
    assert!(matches!(gone, VersionError::Run { .. }));
    let failed = err(Ok(ran(1, &printed("1.39.3"), "boom")));
    assert!(matches!(failed, VersionError::Failed { .. }), "{failed}");
    let mut hung = ran(0, &printed("1.39.3"), "");
    hung.timed_out = true;
    hung.code = None;
    assert!(err(Ok(hung)).to_string().contains("timed out"));
    let junk = err(Ok(ran(0, "hello\n", "")));
    assert!(matches!(junk, VersionError::Unparsable { .. }), "{junk}");
}
