//! The fetch account's one definition, its plan, its shape
//! check, and the production `Os` reading a login shell. Each test says the defect it catches.

use kbf_host::testkit::{FakeExec, reply};

use super::SetupError;
use super::fake::{FakeOs, user};
use super::os::View;
use super::os::{Os, SystemOs};
use super::plan::ensure_fetch_user;
use crate::exit::Exit;
use crate::layout::Platform;
use kbf_host::Planner;

fn steps(os: &FakeOs) -> Result<Vec<String>, SetupError> {
    let exec = FakeExec::new(0);
    let view = View(&exec);
    let mut p = Planner::new(&view);
    ensure_fetch_user(&mut p, os)?;
    Ok(p.finish().steps().iter().map(ToString::to_string).collect())
}

fn account(name: &str, uid: u32, home: &str) -> kbf_host::RunUser {
    let mut u = user(name, uid);
    u.home = home.to_owned();
    u
}

/// Catches a plan that makes an account that is there, or none when it is missing: absent, the
/// plan is the creation `setup` runs; present with the shape `setup` makes, it is empty.
///
/// Catches: the lookup ignored (always created); the creation skipped.
#[test]
fn the_plan_creates_a_missing_account_and_leaves_a_good_one() {
    let mut os = FakeOs::linux();
    assert_eq!(steps(&os).expect("plan").len(), 1);
    os.users.push(account("kbf-fetch", 991, "/nonexistent"));
    assert!(steps(&os).expect("plan").is_empty());
}

/// Catches an account of the wrong shape being used: root's uid or gid, an id above the system
/// range, another home, a login shell are each refused (exit 5) naming the field.
///
/// Catches: each of the four checks removed in turn.
#[test]
fn a_wrong_shape_is_refused_naming_the_field() {
    let mut os = FakeOs::linux();
    let (name, home) = ("kbf-fetch", "/nonexistent");
    let mut bad = |u: kbf_host::RunUser, sh: Option<&str>, field: &str| {
        os.users = vec![u];
        os.shells = sh
            .map(|s| vec![(name.to_owned(), s.to_owned())])
            .unwrap_or_default();
        let err = steps(&os).expect_err(field);
        assert_eq!(err.exit(), Exit::NeedsOperator);
        assert!(err.to_string().contains(field), "{field}: {err}");
    };
    bad(account(name, 0, home), None, "uid 0");
    bad(account(name, 1000, home), None, "uid 1000");
    bad(account(name, 1500, home), None, "uid 1500");
    let mut g = account(name, 300, home);
    g.gid = 0;
    bad(g, None, "gid 0");
    let mut g = account(name, 300, home);
    g.gid = 2000;
    bad(g, None, "gid 2000");
    bad(account(name, 300, "/var/wrong-home"), None, "home");
    bad(
        account(name, 300, home),
        Some("/usr/local/bin/login-shell"),
        "shell",
    );
    // The system ids end at 1000.
    let mut os = FakeOs::linux();
    os.users.push(account("kbf-fetch", 700, "/nonexistent"));
    assert!(steps(&os).is_ok());
}

/// Catches the production `Os` reading a login shell wrong: it takes the seventh field of
/// `getent passwd`; a failing tool and an output with no such field are errors.
///
/// Catches: the wrong field index.
#[test]
fn the_system_os_reads_a_login_shell() {
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd| {
        let last = cmd.args.last().map(String::as_str);
        Ok(match (cmd.program.as_str(), last) {
            (p, Some("ok")) if p.ends_with("getent") => {
                reply(0, "ok:x:1:1::/nonexistent:/usr/sbin/nologin\n", "")
            }
            (p, Some("short")) if p.ends_with("getent") => reply(0, "short:x\n", ""),
            _ => reply(1, "", "failed"),
        })
    });
    let linux = SystemOs::new(&exec, Platform::Linux);
    assert_eq!(linux.login_shell("ok").expect("shell"), "/usr/sbin/nologin");
    assert!(linux.login_shell("short").is_err());
    assert!(linux.login_shell("gone").is_err());
}
