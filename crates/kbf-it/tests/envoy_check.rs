//! `kbf-deploy envoy check` against the server it fronts. kbf-deploy cannot depend on
//! kbf-server (a deploy tool links no server), so its copies of the server's names
//! are compared here.

use kbf_deploy::envoy::check::{HEALTH_SERVICES, METHODS};
use kbf_server::grpc_health::SERVICES;

/// Catches a health-check service list that drifts from the names kbf-server's
/// grpc.health.v1 answers: a name added on one side only makes `envoy check` pass a
/// health check every server answers NOT_FOUND (no server is ever healthy, so the
/// front routes nothing), or fail one that works.
#[test]
fn the_health_services_are_the_servers() {
    assert_eq!(HEALTH_SERVICES.as_slice(), SERVICES.as_slice());
}

/// Catches a routed method whose service kbf-server does not serve on its REAPI
/// listener (a typo in a package or service name), which `envoy check` would require
/// a route for while the server answers it UNIMPLEMENTED.
#[test]
fn every_routed_method_is_a_service_the_server_serves() {
    for method in METHODS {
        let (service, name) = method.rsplit_once('/').expect("<service>/<method>");
        assert!(!name.is_empty(), "{method}");
        assert!(
            SERVICES[1..].contains(&service),
            "{method}: {service} is not served"
        );
    }
}

/// Catches the installer's copy of the health-check service names drifting from the
/// names kbf-server's grpc.health.v1 answers (kbf-installer cannot depend on kbf-server):
/// the box check would then pass a health check every instance answers NOT_FOUND, so
/// Envoy routes nothing, or fail one that works.
#[test]
fn the_installers_health_services_are_the_servers() {
    assert_eq!(
        kbf_installer::envoy::HEALTH_SERVICES.as_slice(),
        SERVICES.as_slice()
    );
}

/// Catches a method the installer's box check requires a route for that kbf-server does
/// not serve on its REAPI listener (a typo in a package or service name).
#[test]
fn every_method_the_installer_routes_is_a_service_the_server_serves() {
    for method in kbf_installer::envoy::METHODS {
        let (service, name) = method.rsplit_once('/').expect("<service>/<method>");
        assert!(!name.is_empty(), "{method}");
        assert!(
            SERVICES[1..].contains(&service),
            "{method}: {service} is not served"
        );
    }
}

/// Catches the installer's copy of the routed methods and health names drifting from
/// kbf-deploy's while both exist: a farm checked by one and run by the other would not
/// be held to the same rules.
#[test]
fn the_installer_and_kbf_deploy_check_the_same_calls() {
    assert_eq!(kbf_installer::envoy::METHODS, METHODS);
    assert_eq!(kbf_installer::envoy::HEALTH_SERVICES, HEALTH_SERVICES);
}
