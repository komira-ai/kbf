//! The account part of `postinst` that the whole-verb tests do not reach.

use std::fs;

use kbf_host::testkit::{ScratchHost, reply};

use super::super::testkit::*;
use super::*;

fn plan_of(f: &Fixture, systemd: bool) -> (Vec<String>, String) {
    let mut out = Vec::new();
    let plan = plan(&f.exec, systemd, me(), &mut out).expect("plans");
    (
        plan.steps().iter().map(ToString::to_string).collect(),
        String::from_utf8(out).expect("utf-8"),
    )
}

/// Catches a second group made for a user that has none (`groupadd` of a name that
/// exists exits 9), and a user made when one exists.
#[test]
fn a_group_without_a_user_gets_only_the_user() {
    let f = fixture("pkg-account-group-only");
    f.world.borrow_mut().groups.insert("kbf".to_owned());
    let (steps, _) = plan_of(&f, true);
    assert!(steps.iter().all(|s| !s.contains("groupadd")), "{steps:?}");
    assert!(steps[0].starts_with("run /usr/sbin/useradd "), "{steps:?}");
    assert_eq!(steps.len(), 3, "{steps:?}");
}

/// Catches `/etc/subuid` being a directory or a link read as empty, and so a block chosen
/// without reading what is there; and a `getent group` that fails being read as "no".
#[test]
fn a_subid_file_that_is_not_a_file_and_a_failing_getent_are_errors() {
    let f = fixture("pkg-account-odd");
    fs::create_dir(f.exec.at("/etc/subuid")).expect("dir");
    let e = plan(&f.exec, true, me(), &mut Vec::new()).expect_err("a directory");
    assert!(
        e.to_string()
            .contains("cannot read /etc/subuid: not a regular file"),
        "{e}"
    );

    let mut exec = ScratchHost::new("pkg-account-getent");
    exec.answer = Box::new(|_| Ok(reply(1, "", "getent: boom")));
    let e = group_exists(&exec, "kbf").expect_err("exit 1");
    assert!(e.to_string().contains("printed \"getent: boom\""), "{e}");
}

/// Catches lingering planned where it is already on, and `loginctl` used where systemd
/// does not run.
#[test]
fn lingering_is_planned_only_when_missing() {
    let f = fixture("pkg-account-linger");
    f.with_kbf();
    f.subids("kbf:100000:65536\n");
    assert_eq!(
        plan_of(&f, true).0,
        ["run /usr/bin/loginctl enable-linger kbf"]
    );
    let (steps, _) = plan_of(&f, false);
    assert!(steps.iter().all(|s| !s.contains("loginctl")), "{steps:?}");
    assert!(
        steps
            .iter()
            .any(|s| s.starts_with("write /var/lib/systemd/linger/kbf")),
        "{steps:?}"
    );
    write(&f.exec, "/var/lib/systemd/linger/kbf", "", 0o644);
    assert_eq!(plan_of(&f, true).0, Vec::<String>::new());
    assert_eq!(plan_of(&f, false).0, Vec::<String>::new());
}
