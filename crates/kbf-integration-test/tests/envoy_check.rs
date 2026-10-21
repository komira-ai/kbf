//! The kbf-installer's envoy check against the server it fronts. kbf-installer cannot
//! depend on kbf-server (an installer links no server), so its copies of the server's
//! names are compared here.

use kbf_server::grpc_health::SERVICES;

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
