//! The enroll listener's own certificate: issued by the CA for the frontend's names, kept
//! fresh under one key, and never replaced by a failed attempt.

use std::time::Duration;

use kbf_ca::SystemEntropy;

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

/// Waits (in real time, with the paused clock untouched) until `done` holds: the renewal
/// runs on the blocking pool, which paused time does not drive.
async fn settle(what: &str, done: impl Fn() -> bool) {
    for _ in 0..2_000 {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("never happened: {what}");
}

/// Lets the loop that just finished an attempt reach its next sleep.
async fn let_it_sleep() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The listener's certificate is replaced at half its life and not before; a renewal that
/// fails (the CA refuses, or the task dies) is tried again after one minute and not
/// sooner; and after a success the next one is half a life away, not a minute.
///
/// Catches: a renewal at the full lifetime (nodes would meet an expired frontend for
/// the second half of every certificate), at once or too often (a CA hammered), a retry
/// that waits the half life (a day without a certificate after one refusal), a loop that
/// ends at its first failure, and a success that keeps the one-minute rhythm.
#[tokio::test(start_paused = true)]
async fn the_certificate_is_renewed_at_half_life_and_a_failure_is_retried_in_a_minute() {
    use std::sync::atomic::{AtomicU32, Ordering};

    let farm = farm("tls-schedule");
    let tls = std::sync::Arc::new(
        FrontendTls::start(&farm.ca, &names(), now(), &mut SystemEntropy).expect("tls"),
    );
    let half = 86_400 / 2;
    let attempts = std::sync::Arc::new(AtomicU32::new(0));
    let counted = std::sync::Arc::clone(&attempts);
    // Attempt 1 kills its task, attempt 2 is refused by the CA (a clock before the
    // build's floor), later ones succeed.
    let clock: std::sync::Arc<dyn Fn() -> i64 + Send + Sync> =
        std::sync::Arc::new(move || match counted.fetch_add(1, Ordering::SeqCst) {
            0 => panic!("the renewal task dies"),
            1 => 1_000,
            _ => now(),
        });
    let looping = tokio::spawn(std::sync::Arc::clone(&tls).keep_fresh_by(
        std::sync::Arc::clone(&farm.ca),
        clock,
        None,
    ));
    let tried = || attempts.load(Ordering::SeqCst);
    let applied = || tls.source().reloads().applied;
    let_it_sleep().await;

    tokio::time::advance(Duration::from_secs(half - 1)).await;
    let_it_sleep().await;
    assert_eq!(tried(), 0, "nothing before half the life");
    tokio::time::advance(Duration::from_secs(1)).await;
    settle("the first attempt", || tried() == 1).await;
    let_it_sleep().await;
    assert_eq!(applied(), 0);

    tokio::time::advance(Duration::from_secs(59)).await;
    let_it_sleep().await;
    assert_eq!(tried(), 1, "a retry waits a minute after a task that died");
    tokio::time::advance(Duration::from_secs(1)).await;
    settle("the retry", || tried() == 2).await;
    let_it_sleep().await;
    assert_eq!(
        applied(),
        0,
        "the CA refused: the served certificate is unchanged"
    );

    tokio::time::advance(Duration::from_secs(59)).await;
    let_it_sleep().await;
    assert_eq!(tried(), 2, "a retry waits a minute after a refusal");
    tokio::time::advance(Duration::from_secs(1)).await;
    settle("the renewal", || applied() == 1).await;
    let_it_sleep().await;
    assert_eq!(tried(), 3);

    tokio::time::advance(Duration::from_secs(60)).await;
    let_it_sleep().await;
    assert_eq!(
        tried(),
        3,
        "after a success the next one is half a life away"
    );
    tokio::time::advance(Duration::from_secs(half - 60)).await;
    settle("the next renewal", || applied() == 2).await;
    assert!(!looping.is_finished(), "the loop never ends");
    looping.abort();
}
