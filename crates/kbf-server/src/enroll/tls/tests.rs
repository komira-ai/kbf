//! The enroll listener's own certificate: issued by the CA for the frontend's names, kept
//! fresh under one key, and never replaced by a failed attempt.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_ca::SystemEntropy;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::*;
use crate::enroll::testkit::{FRONTEND, START, farm};

/// Certificates are judged by the real clock when they are installed, so a test that
/// installs one issues it now.
fn now() -> i64 {
    unix_now()
}

fn names() -> Vec<String> {
    vec![FRONTEND.to_owned()]
}

/// A certificate is issued for the names given, and a renewal serves a new one to new
/// handshakes under the same key.
///
/// Catches: a listener that never replaces its certificate (nodes enrolling after the
/// first week would see an expired frontend), and one whose renewal does not reach the
/// TLS configuration.
#[test]
fn the_certificate_is_renewed_into_the_served_configuration() {
    let farm = farm("tls-renew");
    let tls = FrontendTls::start(&farm.ca, &names(), now(), &mut SystemEntropy).expect("tls");
    assert_eq!(tls.source().reloads().applied, 0);
    tls.renew(&farm.ca, now() + 60, &mut SystemEntropy)
        .expect("renewed");
    assert_eq!(tls.source().reloads().applied, 1);
    tls.renew(&farm.ca, now() + 120, &mut SystemEntropy)
        .expect("renewed");
    assert_eq!(tls.source().reloads().applied, 2);
}

/// The service replaces the certificate at half the CA's lifetime when no interval is
/// given.
///
/// Catches: an interval that is the whole lifetime (the certificate would expire before it
/// is replaced) or a fixed one that ignores the policy.
#[test]
fn the_default_interval_is_half_the_certificate_lifetime() {
    // The test CA issues for 86 400 seconds.
    let farm = farm("tls-half-life");
    assert_eq!(half_life(&farm.ca), Duration::from_secs(43_200));
}

/// A renewal the CA refuses (its clock is before the build's floor) changes nothing and is
/// an error the loop logs and retries.
///
/// Catches: a failed renewal replacing the served certificate with nothing, or being
/// dropped silently.
#[test]
fn a_renewal_the_ca_refuses_leaves_the_served_certificate_alone() {
    let farm = farm("tls-refused");
    let tls = FrontendTls::start(&farm.ca, &names(), now(), &mut SystemEntropy).expect("tls");
    let refused = tls.renew(&farm.ca, 1_000, &mut SystemEntropy);
    assert!(
        matches!(refused, Err(TlsIdentityError::Ca(_))),
        "{refused:?}"
    );
    assert_eq!(tls.source().reloads().applied, 0);
}

/// A certificate for no name, or for a name no certificate can carry, is refused at
/// start.
///
/// Catches: a listener whose certificate no node could verify, found only when the first
/// node fails.
#[test]
fn a_certificate_needs_names_it_can_carry() {
    let farm = farm("tls-names");
    assert!(FrontendTls::start(&farm.ca, &[], START, &mut SystemEntropy).is_err());
    let bad = vec!["not a name".to_owned()];
    assert!(FrontendTls::start(&farm.ca, &bad, START, &mut SystemEntropy).is_err());
}

/// Half the life of the certificates the test CA issues (86 400 seconds): when the renewal
/// loop makes its first attempt.
const HALF: u64 = 86_400 / 2;

/// A renewal loop over a fresh certificate, watched from the test.
///
/// Every attempt reports itself from its blocking thread before it renews, and the test
/// reads those reports and nothing else. The paused clock moves only when the runtime has
/// nothing to run and a timer is waiting (tokio also holds it still while a blocking task
/// runs), so the loop's one pending sleep is all that ever moves it, and it jumps there
/// when the loop arms it. The test has no `advance`, no sleep and no timeout while an
/// attempt is out: how long the blocking pool takes changes how long the test runs, never
/// what it sees.
struct Watched {
    /// How many certificates the earlier attempts had installed, as each attempt began.
    seen: UnboundedReceiver<u64>,
    looping: JoinHandle<()>,
    /// When the loop was started, on the paused clock.
    start: Instant,
}

impl Watched {
    /// Starts the loop. `reading` is the clock each attempt issues by (the attempts are
    /// counted from 0), asked on the attempt's blocking thread after it has reported.
    fn new(
        farm: &crate::enroll::testkit::Farm,
        reading: impl Fn(u32) -> i64 + Send + Sync + 'static,
    ) -> Self {
        let tls = FrontendTls::start(&farm.ca, &names(), now(), &mut SystemEntropy);
        let tls = Arc::new(tls.expect("tls"));
        let (told, seen) = tokio::sync::mpsc::unbounded_channel();
        let served = tls.source();
        let calls = AtomicU32::new(0);
        let clock: Arc<dyn Fn() -> i64 + Send + Sync> = Arc::new(move || {
            // A test that has gone away does not stop the attempt.
            let _ = told.send(served.reloads().applied);
            reading(calls.fetch_add(1, Ordering::SeqCst))
        });
        let start = Instant::now();
        let looping = tokio::spawn(tls.keep_fresh_by(Arc::clone(&farm.ca), clock, None));
        Self {
            seen,
            looping,
            start,
        }
    }

    /// The next attempt's report: when it began, in whole seconds of the paused clock since
    /// the loop was started, and how many certificates the attempts before it had
    /// installed.
    ///
    /// # Panics
    /// If the loop ended: its clock, and with it the sending end, was dropped.
    async fn next(&mut self) -> (u64, u64) {
        let applied = self.seen.recv().await.expect("the loop ended");
        (self.start.elapsed().as_secs(), applied)
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.looping.abort();
    }
}

/// The listener's certificate is replaced at half its life and not before; a renewal that
/// fails (the CA refuses, or the task dies) is tried again after one minute and not
/// sooner; and after a success the next one is half a life away, not a minute.
///
/// The test reads the schedule from the attempts themselves: each reports when it was
/// made (on the paused clock) and how many certificates the attempts before it had
/// installed. Five are expected:
///
/// 1. at half the life, which kills its task;
/// 2. a minute later, which the CA refuses (a clock before the build's floor);
/// 3. a minute later, which succeeds;
/// 4. half a life later, which succeeds;
/// 5. half a life later again.
///
/// Catches: a renewal at the full lifetime (nodes would meet an expired frontend for
/// the second half of every certificate), at once or too often (a CA hammered), a retry
/// that waits the half life (a day without a certificate after one refusal), a loop that
/// ends at its first failure (the report of the next attempt never comes), and a success
/// that keeps the one-minute rhythm.
#[tokio::test(start_paused = true)]
async fn the_certificate_is_renewed_at_half_life_and_a_failure_is_retried_in_a_minute() {
    let farm = farm("tls-schedule");
    let mut run = Watched::new(&farm, |attempt| match attempt {
        0 => panic!("the renewal task dies"),
        1 => 1_000,
        _ => now(),
    });

    assert_eq!(
        run.next().await,
        (HALF, 0),
        "the first attempt is at half the life; it dies"
    );
    assert_eq!(
        run.next().await,
        (HALF + 60, 0),
        "a dead task is retried a minute later, not sooner"
    );
    assert_eq!(
        run.next().await,
        (HALF + 120, 0),
        "a refusal is retried a minute later and installed nothing"
    );
    assert_eq!(
        run.next().await,
        (2 * HALF + 120, 1),
        "after a success the next is half a life away"
    );
    assert_eq!(
        run.next().await,
        (3 * HALF + 120, 2),
        "and again: the loop never ends"
    );
}

/// A retry is due a minute after the failed attempt ended, not a minute after it began.
///
/// The first attempt is held at a gate while the clock moves 59 seconds, and is then
/// released. This is the interleaving that the first version of the test above lost to a
/// slow runner (it moved the clock by hand and waited, in real time, for the loop to
/// notice that an attempt had ended): here the test decides it, so it is the same every
/// run, and the retry is 59 + 60 seconds after the attempt began.
///
/// Catches: a retry on a fixed rhythm from the start of the attempt (a slow CA would be
/// asked again while it is still working), and a retry timed before the attempt has ended.
#[tokio::test(start_paused = true)]
async fn a_retry_is_a_minute_after_the_attempt_ended_not_after_it_began() {
    let farm = farm("tls-late");
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let mut run = Watched::new(&farm, move |attempt| match attempt {
        0 => {
            let held = gate.lock().expect("the gate").recv();
            held.expect("the test releases the attempt");
            panic!("the renewal task dies")
        }
        _ => now(),
    });

    assert_eq!(
        run.next().await,
        (HALF, 0),
        "the first attempt is at half the life"
    );
    // The attempt is out and the clock is the test's to move: 59 seconds pass before it ends.
    tokio::time::advance(Duration::from_secs(59)).await;
    release.send(()).expect("the attempt is waiting");
    assert_eq!(
        run.next().await,
        (HALF + 59 + 60, 0),
        "the retry is a minute after the attempt ended"
    );
}
