//! Goldens of the helper's timeouts and its confirm window, as they are today
//! (`docs/design/host-updates.md`, section 8, step 1): the constants, the defaults the flags
//! give, the bounds the window flag accepts, and what the shipped unit passes. A later change
//! that makes these per service moves a literal here in the same change.

use std::time::Duration;

use super::serve::{DEFAULT_WINDOW_SECS, REVERT_RETRY, ServeConfig};
use crate::plan::{
    ACTIVE_POLL_MILLIS, ACTIVE_WAIT, ACTIVE_WAIT_SECS, IS_ACTIVE_TIMEOUT_SECS, LOCK_TIMEOUT_SECS,
    RESTART_TIMEOUT_SECS, RUNNING_BUILD_TIMEOUT_SECS, RUNTIME_MAX_SECS, Wait, restart_unit,
    unit_active,
};
use crate::request::Unit;

const UNIT_FILE: &str =
    include_str!("../../../../packaging/host-agent/linux/kbf-host-local.service");

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// The constants of `plan.rs` that bound a command or a wait, written out. `restart` kills
/// `systemctl restart` after 120 s, the unit has a further 120 s after it to be active, and
/// it is looked at every 500 ms.
#[test]
fn the_timeout_constants_are_those_of_today() {
    let seen = format!(
        "running_build_timeout_secs={RUNNING_BUILD_TIMEOUT_SECS}\n\
         restart_timeout_secs={RESTART_TIMEOUT_SECS}\n\
         is_active_timeout_secs={IS_ACTIVE_TIMEOUT_SECS}\n\
         active_wait_secs={ACTIVE_WAIT_SECS}\n\
         active_poll_millis={ACTIVE_POLL_MILLIS}\n\
         lock_timeout_secs={LOCK_TIMEOUT_SECS}\n\
         runtime_max_secs={RUNTIME_MAX_SECS}\n\
         default_window_secs={DEFAULT_WINDOW_SECS}\n"
    );
    assert_eq!(
        seen,
        "running_build_timeout_secs=10\n\
         restart_timeout_secs=120\n\
         is_active_timeout_secs=10\n\
         active_wait_secs=120\n\
         active_poll_millis=500\n\
         lock_timeout_secs=300\n\
         runtime_max_secs=1800\n\
         default_window_secs=600\n"
    );
}

/// After a revert that failed, the serve loop leaves reverting alone for 30 s before it tries
/// again.
#[test]
fn a_failed_revert_is_left_alone_for_30_seconds() {
    assert_eq!(REVERT_RETRY, Duration::from_secs(30));
}

/// The commands and the wait a restart is made of carry those limits: the restart is killed
/// after 120 s, an `is-active` after 10 s, and the wait after a restart is 120 s at 500 ms.
#[test]
fn a_restart_is_killed_after_120_seconds_and_waited_for_another_120() {
    let unit = Unit::service("kbf-daemon.service").unwrap();
    assert_eq!(restart_unit(&unit).timeout, Some(Duration::from_secs(120)));
    assert_eq!(unit_active(&unit).timeout, Some(Duration::from_secs(10)));
    assert_eq!(
        ACTIVE_WAIT,
        Wait {
            timeout: Duration::from_secs(120),
            interval: Duration::from_millis(500),
        }
    );
}

/// `--window-secs` is how long a release on trial has to be confirmed: 600 s when absent, and
/// a number from 120 to 1800 when given; 119, 1801, a word and an empty value are refused with
/// the one message.
#[test]
fn the_confirm_window_defaults_to_600_and_is_accepted_from_120_to_1800() {
    let window = |value: Option<&str>| {
        let mut words = vec!["--socket", "/s", "--socket-user", "u"];
        if let Some(value) = value {
            words.extend(["--window-secs", value]);
        }
        ServeConfig::parse(&args(&words)).map(|c| c.window_secs)
    };
    assert_eq!(window(None), Ok(600));
    let refused = Err("--window-secs must be a number from 120 to 1800".to_owned());
    assert_eq!(window(Some("119")), refused);
    assert_eq!(window(Some("120")), Ok(120));
    assert_eq!(window(Some("600")), Ok(600));
    assert_eq!(window(Some("1800")), Ok(1800));
    assert_eq!(window(Some("1801")), refused);
    assert_eq!(window(Some("ten")), refused);
    assert_eq!(window(Some("")), refused);
    assert_eq!(window(Some("-1")), refused);
}

/// The shipped unit's command line, and the settings the helper reads from it: the account,
/// a 600 s window and the one restart unit `kbf-daemon.service`.
#[test]
fn the_shipped_unit_gives_a_600_second_window_and_the_daemon_unit() {
    let exec: Vec<&str> = UNIT_FILE
        .lines()
        .filter(|l| l.starts_with("ExecStart="))
        .collect();
    assert_eq!(
        exec,
        [
            "ExecStart=/usr/libexec/kbf/kbf-host-install --serve --socket /run/kbf-host-install/local.sock --socket-user kbf-updater --window-secs 600 --restart-unit kbf-daemon.service"
        ]
    );
    let words: Vec<&str> = exec[0].split_whitespace().collect();
    assert_eq!(words[1], "--serve");
    let config = ServeConfig::parse(&args(&words[2..])).unwrap();
    assert_eq!(config.user, "kbf-updater");
    assert_eq!(config.window_secs, DEFAULT_WINDOW_SECS);
    let units: Vec<_> = config.units.iter().map(Unit::as_str).collect();
    assert_eq!(units, ["kbf-daemon.service"]);
}
