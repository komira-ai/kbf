//! The arguments the host layer passes to the tools.

mod common;

use common::{config, with};
use mdm_core::commands::{dep_cert_request_path, scep_ca_init_args};
use mdm_core::config::MdmConfig;

/// Catches a SCEP CA created with a subject other than the configured one (the CA is
/// the issuer of every Mac's identity certificate, so a wrong subject means every Mac
/// re-enrolls), a depot outside the state directory, or arguments in an order the tool
/// reads differently.
#[test]
fn the_scep_ca_is_created_with_the_configured_subject() {
    assert_eq!(
        scep_ca_init_args(&config()),
        [
            "ca",
            "-init",
            "-depot",
            "/var/lib/js-mdm/scep/depot",
            "-years",
            "10",
            "-common_name",
            "Example MDM SCEP CA",
            "-organization",
            "Example",
            "-organizational_unit",
            "js-mdm",
        ]
    );
    let text = with("MDM_SCEP_CA_CN", "Site CA")
        .replace("MDM_SCEP_CA_OU=js-mdm", "MDM_SCEP_CA_OU=Site Unit")
        .replace("MDM_SCEP_CA_YEARS=10", "MDM_SCEP_CA_YEARS=3")
        .replace("MDM_ORG=Example", "MDM_ORG=Site Org");
    let args = scep_ca_init_args(&MdmConfig::parse(&text).expect("valid"));
    assert_eq!(args[5], "3");
    assert_eq!(args[7], "Site CA");
    assert_eq!(args[9], "Site Org");
    assert_eq!(args[11], "Site Unit");
}

/// Catches the DEP token certificate requested under another name, common name or
/// validity than configured.
#[test]
fn the_dep_certificate_request_carries_the_configured_name() {
    assert_eq!(
        dep_cert_request_path(&config()),
        "/v1/tokenpki/example?cn=js-mdm&validity_days=7"
    );
    let text = with("MDM_DEP_CERT_CN", "site.cert")
        .replace("MDM_DEP_CERT_DAYS=7", "MDM_DEP_CERT_DAYS=30")
        .replace("MDM_DEP_NAME=example", "MDM_DEP_NAME=site");
    assert_eq!(
        dep_cert_request_path(&MdmConfig::parse(&text).expect("valid")),
        "/v1/tokenpki/site?cn=site.cert&validity_days=30"
    );
}
