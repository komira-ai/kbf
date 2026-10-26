//! The health of a frontend host whose frontend runs as a container: what the helper says runs.

use std::sync::{Arc, Mutex};

use super::container_health::ContainerHealth;
use super::testing::{COMMIT_A, COMMIT_B, VA, VB, host, version};
use crate::inventory::{Host, Role};
use crate::ports::{Health, HealthCheck, InstallAnswer, Installed, Installer, PortError, Report};
use crate::version::Version;

/// A helper that reports what it is told.
struct Says(Mutex<Result<Report, PortError>>);

impl Installer for Says {
    fn install(&self, _host: &Host, _version: &Version) -> Result<InstallAnswer, PortError> {
        unreachable!("a health check installs nothing")
    }

    fn report(&self, _host: &Host) -> Result<Report, PortError> {
        self.0.lock().unwrap().clone()
    }
}

fn report(installed: Installed, running: Option<&str>) -> Result<Report, PortError> {
    Ok(Report {
        installed,
        running_commit: running.map(str::to_owned),
        node_id: None,
        apply: None,
    })
}

fn check(says: Result<Report, PortError>, target: &str) -> Result<Health, PortError> {
    let health = ContainerHealth::new(Arc::new(Says(Mutex::new(says))));
    let h = host(Role::Frontend);
    let held = health.check_held(&h, &version(target));
    assert_eq!(held, health.check(&h, &version(target)));
    held
}

/// The host is healthy when the helper reports the target's commit, waiting while it reports
/// another, none, or is at work, and the roll never refuses a frontend host.
#[test]
fn the_host_is_healthy_when_the_helper_reports_the_targets_commit() {
    let v = |c| Installed::Version(c);
    assert_eq!(
        check(report(v(VB.to_owned()), Some(COMMIT_B)), VB),
        Ok(Health::Healthy)
    );
    // 12 digits name the commit as 40 do.
    assert_eq!(
        check(report(v(VB.to_owned()), Some(&COMMIT_B[..12])), VB),
        Ok(Health::Healthy)
    );
    let waiting = |health: Result<Health, PortError>| match health {
        Ok(Health::Waiting(why)) => why,
        other => panic!("{other:?}"),
    };
    assert!(waiting(check(report(v(VA.to_owned()), Some(COMMIT_A)), VB)).contains("runs build"));
    assert!(waiting(check(report(Installed::NotInstalled, None), VB)).contains("does not say"));
    assert!(waiting(check(report(Installed::InProgress, Some(COMMIT_B)), VB)).contains("working"));
    // A target that names no commit cannot be shown to run.
    assert!(matches!(
        check(report(v("0.1".to_owned()), Some(COMMIT_B)), "0.1.7"),
        Ok(Health::Abandoned(_))
    ));
    // A helper that cannot be reached is an error, not an answer.
    assert!(check(Err(PortError::unreachable("no socket")), VB).is_err());

    let health = ContainerHealth::new(Arc::new(Says(Mutex::new(report(
        Installed::NotInstalled,
        None,
    )))));
    assert_eq!(health.refusal(&host(Role::Frontend)), None);
}
