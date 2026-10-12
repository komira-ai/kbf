//! What the whole-machine runtime keeps on disk across the daemon's restarts: the
//! lease it works on and how far it got ([`Record`], the file `whole-machine-lease`),
//! and a quarantine (the file `quarantine`, its reason).
//!
//! The lease record is a few `key value` lines, written whole to a temporary name,
//! synced and renamed over the old one, so a restart finds either the old record or the
//! new one. Free text (an error) and the action result are stored as hexadecimal, so no
//! value holds a newline.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use kbf_daemon::RuntimeError;
use kbf_proto::reapi::ActionResult;
use kbf_types::{LeaseId, LostWhy};
use prost::Message as _;

/// The lease record's file name in the runtime's directory.
pub const LEASE_FILE: &str = "whole-machine-lease";

/// The quarantine's file name in the runtime's directory: an operator removes it once
/// the node is repaired, then restarts the daemon.
pub const QUARANTINE_FILE: &str = "quarantine";

/// Where the lease is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Its user is made and logged in; its action has not been handed to the helper.
    Preparing,
    /// Its action was handed to the helper's `run` and has not ended: a daemon that
    /// restarts here finds the action's result lost, kills the lease user's processes
    /// and reports the lease lost, never running the action again itself.
    Started,
    /// Its action ended (the outcome is kept); the node is being cleaned.
    Restoring,
}

/// The lease the runtime works on, and what it has done for it.
#[derive(Debug)]
pub struct Record {
    pub lease: LeaseId,
    pub step: Step,
    /// The lease user was made (and not deleted by a boot reset since).
    pub user: bool,
    /// `session-login` was asked for.
    pub login: bool,
    /// `kill-uid` succeeded after the action.
    pub killed: bool,
    /// `session-idle` was asked for.
    pub idle: bool,
    /// When the lease was first taken, in milliseconds since the Unix epoch: the session
    /// wait counts from here, across restarts.
    pub since_ms: u64,
    /// How the action ended, once it has.
    pub outcome: Option<Result<ActionResult, RuntimeError>>,
}

impl Record {
    /// A lease taken at `since_ms`, nothing done yet.
    #[must_use]
    pub const fn new(lease: LeaseId, since_ms: u64) -> Self {
        Self {
            lease,
            step: Step::Preparing,
            user: false,
            login: false,
            killed: false,
            idle: false,
            since_ms,
            outcome: None,
        }
    }

    /// The record as its file holds it.
    #[must_use]
    pub fn encode(&self) -> String {
        let flag = |b: bool| if b { "yes" } else { "no" };
        let step = match self.step {
            Step::Preparing => "preparing",
            Step::Started => "started",
            Step::Restoring => "restoring",
        };
        let mut text = format!(
            "lease {}\nstep {step}\nuser {}\nlogin {}\nkilled {}\nidle {}\nsince {}\n",
            self.lease,
            flag(self.user),
            flag(self.login),
            flag(self.killed),
            flag(self.idle),
            self.since_ms
        );
        if let Some(outcome) = &self.outcome {
            let (tag, bytes) = match outcome {
                Ok(result) => ("ok", result.encode_to_vec()),
                Err(RuntimeError::TimedOut { stdout, stderr }) => {
                    let logs = ActionResult {
                        stdout_digest: stdout.clone(),
                        stderr_digest: stderr.clone(),
                        ..ActionResult::default()
                    };
                    ("timed-out", logs.encode_to_vec())
                }
                Err(RuntimeError::Killed) => ("killed", Vec::new()),
                Err(RuntimeError::Invalid(why)) => ("invalid", why.as_bytes().to_vec()),
                Err(RuntimeError::MissingBlob(blob)) => ("missing", blob.as_bytes().to_vec()),
                Err(RuntimeError::Lost(LostWhy::DaemonRestarted, why)) => {
                    ("lost-restarted", why.as_bytes().to_vec())
                }
                Err(RuntimeError::Lost(LostWhy::Fenced, why)) => {
                    ("lost-fenced", why.as_bytes().to_vec())
                }
                Err(other) => ("failed", other.to_string().into_bytes()),
            };
            text.push_str(&format!("outcome {tag} {}\n", hex(&bytes)));
        }
        text
    }

    /// The record in `text`.
    ///
    /// # Errors
    /// A line is missing or malformed.
    pub fn decode(text: &str) -> Result<Self, String> {
        let value = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
                .ok_or_else(|| format!("no {key} line"))
        };
        let flag = |key: &str| match value(key)? {
            "yes" => Ok(true),
            "no" => Ok(false),
            other => Err(format!("{key} {other:?} is not yes or no")),
        };
        let lease = value("lease")?;
        let lease = lease
            .split_once('.')
            .and_then(|(t, s)| Some(LeaseId::new(t.parse().ok()?, s.parse().ok()?)))
            .ok_or_else(|| format!("lease {lease:?} is not <term>.<seq>"))?;
        let step = match value("step")? {
            "preparing" => Step::Preparing,
            "started" => Step::Started,
            "restoring" => Step::Restoring,
            other => return Err(format!("step {other:?} is unknown")),
        };
        let since_ms = value("since")?
            .parse()
            .map_err(|_| "since is not a number".to_owned())?;
        let outcome = match value("outcome") {
            Err(_) => None,
            Ok(outcome) => Some(decode_outcome(outcome)?),
        };
        Ok(Self {
            lease,
            step,
            user: flag("user")?,
            login: flag("login")?,
            killed: flag("killed")?,
            idle: flag("idle")?,
            since_ms,
            outcome,
        })
    }
}

/// An `outcome` line's value: `<tag> <hex>`.
fn decode_outcome(value: &str) -> Result<Result<ActionResult, RuntimeError>, String> {
    let (tag, data) = value.split_once(' ').unwrap_or((value, ""));
    let bytes = unhex(data).ok_or("the outcome is not hexadecimal")?;
    let text = || String::from_utf8_lossy(&bytes).into_owned();
    let result = || ActionResult::decode(bytes.as_slice()).map_err(|e| e.to_string());
    Ok(match tag {
        "ok" => Ok(result()?),
        "timed-out" => {
            let logs = result()?;
            Err(RuntimeError::TimedOut {
                stdout: logs.stdout_digest,
                stderr: logs.stderr_digest,
            })
        }
        "killed" => Err(RuntimeError::Killed),
        "invalid" => Err(RuntimeError::Invalid(text())),
        "missing" => Err(RuntimeError::MissingBlob(text())),
        "failed" => Err(RuntimeError::Failed(text())),
        "lost-restarted" => Err(RuntimeError::Lost(LostWhy::DaemonRestarted, text())),
        "lost-fenced" => Err(RuntimeError::Lost(LostWhy::Fenced, text())),
        other => return Err(format!("outcome {other:?} is unknown")),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The runtime's files in its directory.
#[derive(Clone, Debug)]
pub struct Files {
    dir: PathBuf,
}

impl Files {
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn lease_path(&self) -> PathBuf {
        self.dir.join(LEASE_FILE)
    }

    /// The lease record, if there is one.
    ///
    /// # Errors
    /// It cannot be read, or does not parse.
    pub fn lease(&self) -> io::Result<Option<Record>> {
        match std::fs::read_to_string(self.lease_path()) {
            Ok(text) => Record::decode(&text)
                .map(Some)
                .map_err(|why| io::Error::new(io::ErrorKind::InvalidData, why)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Writes `record` in place of the old one.
    ///
    /// # Errors
    /// It could not be written.
    pub fn keep(&self, record: &Record) -> io::Result<()> {
        replace(&self.dir, &self.lease_path(), record.encode().as_bytes())
    }

    /// Removes the lease record.
    ///
    /// # Errors
    /// It could not be removed.
    pub fn forget(&self) -> io::Result<()> {
        std::fs::remove_file(self.lease_path())
    }

    /// The quarantine's reason, if the node is quarantined.
    ///
    /// # Errors
    /// The file cannot be read.
    pub fn quarantine(&self) -> io::Result<Option<String>> {
        match std::fs::read_to_string(self.dir.join(QUARANTINE_FILE)) {
            Ok(why) => Ok(Some(why.trim().to_owned())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Quarantines the node for `why`.
    ///
    /// # Errors
    /// The file could not be written.
    pub fn quarantined(&self, why: &str) -> io::Result<()> {
        replace(
            &self.dir,
            &self.dir.join(QUARANTINE_FILE),
            format!("{why}\n").as_bytes(),
        )
    }
}

/// Writes `bytes` to `path` through a synced temporary file renamed over it.
fn replace(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("new");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kbf_proto::reapi::Digest;

    fn digest(n: u8) -> Digest {
        Digest {
            hash: format!("{n:02x}").repeat(32),
            size_bytes: i64::from(n),
        }
    }

    /// Catches a record that does not come back as it was written (a step, a flag, the
    /// start time or an outcome lost across the restart would repeat or skip a step of
    /// the session switch, or answer the lease with another result).
    #[test]
    fn every_record_comes_back_as_written() {
        let mut record = Record::new(LeaseId::new(7, 3), 1_700_000_000_123);
        let mut outcomes = vec![
            None,
            Some(Ok(ActionResult {
                exit_code: 3,
                stdout_digest: Some(digest(1)),
                ..ActionResult::default()
            })),
            Some(Err(RuntimeError::TimedOut {
                stdout: Some(digest(2)),
                stderr: None,
            })),
            Some(Err(RuntimeError::Killed)),
            Some(Err(RuntimeError::Invalid("bad\npath".to_owned()))),
            Some(Err(RuntimeError::MissingBlob("ab/3".to_owned()))),
            Some(Err(RuntimeError::Failed("no session".to_owned()))),
            Some(Err(RuntimeError::Lost(
                LostWhy::DaemonRestarted,
                "its result is lost".to_owned(),
            ))),
            Some(Err(RuntimeError::Lost(
                LostWhy::Fenced,
                "fenced".to_owned(),
            ))),
        ];
        for (i, outcome) in outcomes.drain(..).enumerate() {
            record.step = [Step::Preparing, Step::Started, Step::Restoring][i % 3];
            record.user = i % 2 == 1;
            record.login = i % 3 == 1;
            record.killed = i % 4 == 1;
            record.idle = i % 5 == 1;
            record.outcome = outcome;
            let back = Record::decode(&record.encode()).expect("decodes");
            assert_eq!(format!("{back:?}"), format!("{record:?}"));
        }
        let other = Record {
            outcome: Some(Err(RuntimeError::BusyNode("x".to_owned()))),
            ..record
        };
        let back = Record::decode(&other.encode()).unwrap();
        assert_eq!(
            format!("{:?}", back.outcome),
            r#"Some(Err(Failed("killed on a busy node: x")))"#
        );
    }

    /// Catches a damaged record taken as a valid one (the runtime would resume a lease
    /// it never had, or with steps it never took).
    #[test]
    fn a_damaged_record_is_refused() {
        let good = Record::new(LeaseId::new(1, 2), 5).encode();
        for (from, to) in [
            ("lease 1.2", "lease 1-2"),
            ("lease 1.2", "lease x.2"),
            ("step preparing", "step done"),
            ("step preparing", "step start"),
            ("user no", "user maybe"),
            ("since 5", "since soon"),
            ("idle no\n", ""),
        ] {
            let bad = good.replace(from, to);
            assert!(Record::decode(&bad).is_err(), "{bad}");
        }
        for outcome in [
            "outcome nope 00",
            "outcome ok 0",
            "outcome ok zz",
            "outcome ok ff",
        ] {
            let bad = format!("{good}{outcome}\n");
            assert!(Record::decode(&bad).is_err(), "{bad}");
        }
    }
}
