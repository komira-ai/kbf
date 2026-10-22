//! The frontend's health: has kbf-proxy swapped to the target and is it serving?
//!
//! The answer is kbf-proxy's `GET /v1/proxy/swap` (kbf#1525, `docs/design/frontend-proxy.md`
//! section 4b), which the proxy serves with `--kbf-binary` on its public port, behind the
//! operator token ([`crate::https`]). `/readyz` answers for the old child during a swap and
//! `kbf --version` reads the file on disk, so neither says the swap is over.
//!
//! The frontend is **healthy** when all of these hold:
//!
//! - `pending_binary` is `null` (no changed kbf waits for its swap);
//! - the newest `update` began at or after this roll's install call for the host, is to the
//!   target, and its `state` is `done`;
//! - `active_commit` and `installed_commit` are both the target's, and the same.
//!
//! A change of `proxy_boot` between two reads (the proxy restarted) changes nothing: the
//! status is read afresh each time, a `done` swap survives the restart in `update.json`, and
//! the rule above is all there is. `seq` restarts with the proxy and is not compared.
//!
//! The roll **waits** while `state` is `running` or `pending_binary` is set, and while no
//! update has begun since the install call (a kbf that cannot run shows none, and the swap
//! may not have started yet); the roll's frontend bound ends that wait with the reason. It
//! **fails** at once when the update since the install call, to the target, is `abandoned`
//! or `refused`, with the reason the proxy gave.
//!
//! The proxy's design doc reads a 404 (a proxy without `--kbf-binary`) as "not healthy yet".
//! This adapter fails at once instead: without `--kbf-binary` the proxy will never answer
//! the question, and waiting out the bound only delays the same failure.
//!
//! `started_at` has whole seconds, so it is compared with the install call in whole seconds.

use crate::health::{InstallTimes, WallClock, commit_of, names, system_wall_clock};
use crate::https::{CHECK, ProxyApi, WHAT};
use crate::inventory::Host;
use crate::ports::{Health, HealthCheck, PortError};
use crate::version::Version;
use serde::Deserialize;

/// The path of the swap status on the proxy.
pub const PATH: &str = "/v1/proxy/swap";

/// The state of an update, as the proxy words it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Running,
    Done,
    Abandoned,
    Refused,
    /// A state this updater does not know: not healthy, and not a failure.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct Update {
    to: String,
    phase: String,
    state: State,
    reason: Option<String>,
    started_at: String,
}

/// The fields of the proxy's answer the rule reads; the rest is ignored.
#[derive(Debug, Deserialize)]
struct Status {
    installed_commit: Option<String>,
    active_commit: Option<String>,
    update: Option<Update>,
    pending_binary: Option<String>,
}

/// Health of the frontend host, over kbf-proxy's swap status.
pub struct FrontendHealth {
    api: ProxyApi,
    installs: InstallTimes,
}

impl FrontendHealth {
    pub fn new(api: ProxyApi) -> FrontendHealth {
        FrontendHealth::with_clock(api, system_wall_clock())
    }

    /// [`FrontendHealth::new`] with the wall clock given (a test sets the install time).
    pub fn with_clock(api: ProxyApi, now: WallClock) -> FrontendHealth {
        FrontendHealth {
            api,
            installs: InstallTimes::new(now),
        }
    }
}

impl HealthCheck for FrontendHealth {
    fn installing(&self, host: &Host, target: &Version) {
        self.installs.note(&host.alias, target);
    }

    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        let commit = match commit_of(target) {
            Ok(commit) => commit,
            Err(health) => return Ok(health),
        };
        let reply = self.api.get(PATH)?;
        match reply.status {
            200 => {}
            404 => {
                return Ok(Health::Abandoned(format!(
                    "{WHAT} has no swap status: it runs without --kbf-binary, so it cannot \
                     say whether the swap is done"
                )));
            }
            _ => return Err(reply.failure(WHAT, CHECK)),
        }
        let status: Status = serde_json::from_slice(&reply.body)
            .map_err(|_| PortError::other(format!("{WHAT}'s answer is not a swap status")))?;
        judge(
            &status,
            commit,
            self.installs.at(&host.alias, target).map(|ms| ms / 1000),
        )
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` as seconds since the epoch.
pub(crate) fn parse_utc(text: &str) -> Option<u64> {
    const SHAPE: &[u8; 20] = b"dddd-dd-ddTdd:dd:ddZ";
    let bytes = text.as_bytes();
    let fits = bytes.len() == SHAPE.len()
        && bytes.iter().zip(SHAPE).all(|(c, s)| {
            if *s == b'd' {
                c.is_ascii_digit()
            } else {
                c == s
            }
        });
    if !fits {
        return None;
    }
    let n = |from: usize, to: usize| text[from..to].parse::<i64>().unwrap_or_default();
    let (year, month, day) = (n(0, 4), n(5, 7), n(8, 10));
    let (hour, minute, second) = (n(11, 13), n(14, 16), n(17, 19));
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    let valid = (1..=12).contains(&month)
        && (1..=month_days).contains(&day)
        && hour < 24
        && minute < 60
        && second < 60;
    if !valid {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm); March is month 0 of its year.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let m = (month + 9) % 12;
    let day_of_year = (153 * m + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

/// The rule, over one read of the status. `installed` is the install call in seconds, if this
/// process made one.
fn judge(status: &Status, commit: &str, installed: Option<u64>) -> Result<Health, PortError> {
    let waiting = |why: String| Ok(Health::Waiting(why));
    let Some(update) = &status.update else {
        return waiting(match &status.pending_binary {
            Some(_) => "a new kbf waits for its swap to start".to_owned(),
            None => "no update has started since the install".to_owned(),
        });
    };
    let began = parse_utc(&update.started_at)
        .ok_or_else(|| PortError::other(format!("{WHAT}'s update has no usable started_at")))?;
    if installed.is_some_and(|t| began < t) {
        return waiting(
            "the newest update began before the install: no swap has started since".to_owned(),
        );
    }
    if !names(&update.to, commit) {
        return waiting(format!(
            "the newest update is to {}, not to the target",
            update.to
        ));
    }
    let why = update.reason.as_deref().unwrap_or("no reason given");
    match update.state {
        State::Refused => return Ok(Health::Abandoned(format!("the swap was refused: {why}"))),
        State::Abandoned => return Ok(Health::Abandoned(format!("the swap was abandoned: {why}"))),
        State::Running => return waiting(format!("the swap is running ({})", update.phase)),
        State::Unknown => {
            return waiting("the swap is in a state this updater does not know".to_owned());
        }
        State::Done => {}
    }
    if status.pending_binary.is_some() {
        return waiting("the swap is done, and a newer kbf waits for its swap".to_owned());
    }
    match (&status.active_commit, &status.installed_commit) {
        (Some(active), Some(recorded)) if active == recorded && names(active, commit) => {
            Ok(Health::Healthy)
        }
        (active, recorded) => waiting(format!(
            "the swap is done, but the proxy serves {} and records {} as installed",
            active.as_deref().unwrap_or("nothing"),
            recorded.as_deref().unwrap_or("nothing")
        )),
    }
}
