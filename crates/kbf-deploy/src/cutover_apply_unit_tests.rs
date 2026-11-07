//! The small parts of `job-service apply` and `back-out`, each alone: the names the restart
//! takes, the canary made from the file, kbf-host-install's answer, the asks of a deadline,
//! and the real host's seam.

use kbf_host::testkit::{FakeExec, reply};

use super::tests::restart;
use super::*;

/// Behaviour: the restart's names are checked as `kbf-host-install` and the updater check
/// them, and an address ssh would read as an option is refused.
#[test]
fn the_restart_names_are_checked_as_the_host_checks_them() {
    for ok in ["-", "a", "fe-a1", &"a".repeat(32)] {
        assert!(instance_ok(ok), "{ok}");
    }
    for bad in ["", "Bad", "1a", "a_b", "a.b", &"a".repeat(33)] {
        assert!(!instance_ok(bad), "{bad}");
    }
    for ok in ["kbf-updater", "_u", "u9_x-y", &"u".repeat(32)] {
        assert!(user_ok(ok), "{ok}");
    }
    for bad in ["", "9x", "-u", "U", "u.x", &"u".repeat(33)] {
        assert!(!user_ok(bad), "{bad}");
    }
    for ok in ["fe.farm.example", "192.0.2.10", "[2001:db8::1]"] {
        assert!(address_ok(ok), "{ok}");
    }
    for bad in ["", "-oX", "a b", "a;b"] {
        assert!(!address_ok(bad), "{bad}");
    }
}

/// Behaviour: a deadline is a number of asks, at least one.
#[test]
fn a_deadline_is_a_number_of_asks() {
    assert_eq!(asks(300, 5), 60);
    assert_eq!(asks(301, 5), 61);
    assert_eq!(asks(1, 5), 1);
}

/// Behaviour: the canary is the file's kbf image without `docker://`, on its `executor-arch`
/// (`x86_64` when the file names none), named by the instant.
#[test]
fn the_canary_is_made_from_the_file() {
    let table = |extra: &str| {
        Executor::parse(
            Path::new("k"),
            &format!("[executor]\nexecutor-frontend-id = \"fe-b\"\n{extra}"),
        )
        .unwrap()
    };
    let c = canary_of(
        &table("executor-arch = \"arm64\"\nexecutor-kbf-image = \"docker://r/k@sha256:aa\"\n"),
        7,
    );
    assert_eq!(
        c,
        Canary {
            id: "kbf-canary-7".into(),
            image: "r/k@sha256:aa".into(),
            arch: "arm64".into(),
            frontend: "fe-b".into()
        }
    );
    let c = canary_of(&table("executor-kbf-image = \"r/k@sha256:aa\"\n"), 8);
    assert_eq!(
        (c.image.as_str(), c.arch.as_str()),
        ("r/k@sha256:aa", "x86_64")
    );
}

/// Behaviour: kbf-host-install's answer must be both exit 0 and `restarted`: a refusal on exit
/// 0 is still a refusal.
#[test]
fn a_restart_answer_must_say_restarted() {
    let e = restarted(&reply(
        0,
        "{\"result\":\"refused\",\"reason\":\"busy\"}\n",
        "",
    ))
    .unwrap_err();
    assert_eq!(
        e.to_string(),
        "the restart through kbf-host-install: exit 0: refused: busy"
    );
    restarted(&reply(
        0,
        "{\"result\":\"restarted\",\"unit\":\"kbf-frontend.service\"}",
        "",
    ))
    .unwrap();
}

/// Behaviour: a config path that is not UTF-8 cannot be given to `kbf --check`, and is
/// refused naming the flag.
#[test]
fn a_config_path_that_is_not_utf8_is_refused() {
    use std::os::unix::ffi::OsStrExt as _;
    let path = Path::new(std::ffi::OsStr::from_bytes(b"/etc/\xff.toml"));
    let e = check_config(&restart(), path).unwrap_err();
    assert_eq!(e.to_string(), "--config: /etc/\u{fffd}.toml is not UTF-8");
}

/// Behaviour: an operator API on port 0 cannot be asked, and is refused.
#[test]
fn an_api_on_port_0_is_refused() {
    let e = restart_steps(&Restart {
        api: "127.0.0.1:0".parse().unwrap(),
        ..restart()
    })
    .unwrap_err();
    assert!(e.to_string().starts_with("--api: "), "{e}");
}

/// Behaviour: the real host runs tools and asks URLs through the host's `Exec`.
#[test]
fn the_real_host_goes_through_exec() {
    let mut exec = FakeExec::new(501);
    exec.http = Err(io::ErrorKind::ConnectionRefused);
    exec.answer = Box::new(|_| Ok(reply(7, "", "")));
    let mut host = OnHost { exec: &exec };
    let cmd = Cmd::new(HostPath::new("/bin/x").unwrap());
    assert_eq!(host.run(&cmd).unwrap().status.code(), Some(7));
    let url = api_url("127.0.0.1:1".parse().unwrap(), "/").unwrap();
    assert_eq!(
        host.status(&url).unwrap_err().kind(),
        io::ErrorKind::ConnectionRefused
    );
}

/// Behaviour: the real host's pause returns (a pause of no seconds; nothing is timed).
#[test]
fn the_real_host_pauses() {
    let exec = FakeExec::new(501);
    OnHost { exec: &exec }.pause(0);
}
