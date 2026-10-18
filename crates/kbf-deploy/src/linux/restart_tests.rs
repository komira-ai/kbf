//! Rule `restart` read on its own: which restart delay and start limit systemd uses.

use super::*;

fn unit(text: &str) -> UnitFile {
    UnitFile::parse(text).expect("a unit")
}

const OK: &str = "[Unit]\nStartLimitIntervalSec=0\n\n[Service]\nRestartSec=5\n";

const NO_DELAY: &str = "gives no RestartSec=5: systemd restarts after 100 ms, and a failure that repeats quickly ends in start-limit-hit with the unit down";
const NO_LIMIT: &str = "gives no StartLimitIntervalSec=0 in [Unit]: systemd's default of 5 starts in 10 s ends a repeating failure in start-limit-hit with the unit down";

/// Catches a unit missing either setting, a wrong value of either (`100ms`, `10`),
/// `StartLimitIntervalSec=` taken from `[Service]` (systemd reads it in `[Unit]`),
/// `5s` refused, and the last of two `RestartSec=` not the one read.
#[test]
fn the_unit_restarts_after_5_s_and_has_no_start_limit() {
    assert_eq!(check(&unit(OK)), Vec::<String>::new());
    let rows: [(&str, Vec<String>); 8] = [
        ("[Service]\nRestart=on-failure\n", vec![NO_DELAY.into(), NO_LIMIT.into()]),
        ("[Unit]\nStartLimitIntervalSec=0\n[Service]\n", vec![NO_DELAY.into()]),
        ("[Unit]\n[Service]\nRestartSec=5\n", vec![NO_LIMIT.into()]),
        (
            "[Unit]\nStartLimitIntervalSec=0\n[Service]\nRestartSec=100ms\n",
            vec!["has RestartSec=100ms, not RestartSec=5: a failure that repeats quickly must not use up the start limit".into()],
        ),
        (
            "[Unit]\nStartLimitIntervalSec=10\n[Service]\nRestartSec=5\n",
            vec!["has StartLimitIntervalSec=10, not StartLimitIntervalSec=0: five starts in that interval end in start-limit-hit with the unit down".into()],
        ),
        ("[Unit]\n[Service]\nRestartSec=5\nStartLimitIntervalSec=0\n", vec![NO_LIMIT.into()]),
        (
            "[Unit]\nStartLimitIntervalSec=0\n[Service]\nRestartSec=5\nRestartSec=1\n",
            vec!["has RestartSec=1, not RestartSec=5: a failure that repeats quickly must not use up the start limit".into()],
        ),
        ("[Unit]\nStartLimitIntervalSec=0\n[Service]\nRestartSec=1\nRestartSec=5s\n", vec![]),
    ];
    for (text, want) in rows {
        assert_eq!(check(&unit(text)), want, "{text}");
    }
}
