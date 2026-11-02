//! The arguments the host layer passes to the tools, as pure functions of the settings.

use crate::config::MdmConfig;

/// The arguments of `scepserver ca -init` that create the SCEP CA in
/// `<MDM_STATE>/scep/depot`, after the binary's own path. The subject is the issuer of
/// every Mac's MDM identity certificate; changing the CA later re-enrolls every Mac.
pub fn scep_ca_init_args(c: &MdmConfig) -> Vec<String> {
    let args: [&str; 12] = [
        "ca",
        "-init",
        "-depot",
        &format!("{}/scep/depot", c.state),
        "-years",
        &c.scep_ca.years.to_string(),
        "-common_name",
        &c.scep_ca.common_name,
        "-organization",
        &c.org,
        "-organizational_unit",
        &c.scep_ca.organizational_unit,
    ];
    args.into_iter().map(str::to_owned).collect()
}

/// The path (with its query) NanoDEP's API answers with the DEP token certificate:
/// `GET /v1/tokenpki/<MDM_DEP_NAME>?cn=<MDM_DEP_CERT_CN>&validity_days=<MDM_DEP_CERT_DAYS>`.
pub fn dep_cert_request_path(c: &MdmConfig) -> String {
    format!(
        "/v1/tokenpki/{}?cn={}&validity_days={}",
        c.dep_name, c.dep_cert.common_name, c.dep_cert.days
    )
}
