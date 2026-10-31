//! The definition fields of the rollback: `rollback`, `rollback_after_failed` and
//! `min_version`. Each one is read, defaulted and refused with its name.

use kbf_updater::version::Version;

use crate::reconcile::{FailThreshold, Rollback};
use crate::services::{ServiceDef, Services};

fn service(extra: &str) -> String {
    format!("[[service]]\nname = \"a\"\n{extra}\n[service.roles.daemon]\n")
}

fn parsed(extra: &str) -> ServiceDef {
    Services::parse(&service(extra))
        .expect("services")
        .get("a")
        .expect("a")
        .clone()
}

fn refusal(extra: &str) -> String {
    Services::parse(&service(extra))
        .expect_err(extra)
        .to_string()
}

/// A service that says nothing about it rolls back by hand, at the default threshold: a quarter
/// of the hosts and at least two, and kbf as shipped does the same.
#[test]
fn rollback_defaults_to_manual_with_a_quarter_of_the_hosts_and_at_least_two() {
    for def in [parsed(""), ServiceDef::kbf()] {
        assert_eq!(def.rollback, Rollback::Manual);
        assert_eq!(def.rollback_after_failed.percent, 25);
        assert_eq!(def.rollback_after_failed.min_hosts, 2);
        assert_eq!(def.min_version, None);
    }
    assert_eq!(
        FailThreshold::default(),
        FailThreshold {
            percent: 25,
            min_hosts: 2
        }
    );
}

/// Each field is read as written, and a threshold that names only one of its numbers keeps
/// the default of the other.
#[test]
fn the_rollback_fields_are_read_as_written() {
    assert_eq!(parsed("rollback = \"manual\"").rollback, Rollback::Manual);
    assert_eq!(
        parsed("rollback = \"automatic\"").rollback,
        Rollback::Automatic
    );
    let both = parsed("rollback_after_failed = { percent = 50, min_hosts = 3 }");
    assert_eq!(
        both.rollback_after_failed,
        FailThreshold {
            percent: 50,
            min_hosts: 3
        }
    );
    let percent = parsed("rollback_after_failed = { percent = 100 }");
    assert_eq!(
        percent.rollback_after_failed,
        FailThreshold {
            percent: 100,
            min_hosts: 2
        }
    );
    let hosts = parsed("rollback_after_failed = { min_hosts = 1 }");
    assert_eq!(
        hosts.rollback_after_failed,
        FailThreshold {
            percent: 25,
            min_hosts: 1
        }
    );
    assert_eq!(
        parsed("min_version = \"0.0.18\"").min_version,
        Some(Version::parse("0.0.18").expect("a version"))
    );
}

/// A value that cannot work is refused, and the refusal names the field: a mode that is not
/// one of the two, a percentage that is 0 or over 100, no minimum of hosts, a key the
/// threshold does not have, and a minimum version that is no version.
#[test]
fn a_rollback_field_that_cannot_work_is_refused_with_its_name() {
    for (extra, field) in [
        ("rollback = \"sometimes\"", "rollback"),
        ("rollback = true", "rollback"),
        ("rollback_after_failed = { percent = 0 }", "percent"),
        ("rollback_after_failed = { percent = 101 }", "percent"),
        ("rollback_after_failed = { min_hosts = 0 }", "min_hosts"),
        ("rollback_after_failed = { hosts = 2 }", "hosts"),
        ("min_version = \"not a version\"", "min_version"),
    ] {
        let why = refusal(extra);
        assert!(why.contains(field), "{extra}: {why}");
    }
}
