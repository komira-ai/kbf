//! `mdm dep`: link NanoDEP to Apple Business Manager and give it the Automated Device
//! Enrollment profile, as root. Each verb is one of the operator's ABM steps, in this
//! order:
//!
//! - `get-cert --out FILE`: NanoDEP makes its token key pair and answers the public
//!   certificate, written to `FILE` (0644, outside every git work tree), to upload in
//!   ABM. When `FILE` already holds a certificate it is kept and nothing is asked:
//!   NanoDEP makes a new pair on every call, and the certificate uploaded must be the
//!   last one made.
//! - `upload-token --token FILE`: NanoDEP decrypts and stores the server token (the
//!   `.p7m` ABM gives for that certificate), then asks Apple for the account, whose
//!   organisation and MDM server name it prints as the proof. Every run uploads.
//! - `define-profile`: the ADE profile ([`AdeProfile`]) is defined at Apple and made the
//!   assigner's, which depsyncer gives to new devices; `MDM_STATE/dep-profile-uuid`
//!   and `MDM_STATE/dep-profile.sha256` record it, and a profile with the recorded
//!   sha256 is not defined again. depsyncer is enabled and started.
//! - `assign --serial SERIAL...`: the recorded profile is assigned to devices already
//!   in ABM. Every run assigns; the verb fails unless Apple answers `SUCCESS` for each.
//!
//! Every call goes to depserver on loopback ([`api`]) with the key from
//! `MDM_ETC/nanodep.env`, and is made only with `--apply`.
//!
//! [`api`]: super::api

use std::io::Write;

use serde_json::json;
use sha2::{Digest, Sha256};

use super::MdmError;
use super::api::{self, Api, Call};
use super::config::MdmConfig;
use super::install;
use super::profile::AdeProfile;
use super::push_cert::outside_git;
use super::render::DEPSYNCER;
use crate::linux::host::{self, note, under};
use kbf_host::exec::{Entry, Exec};
use kbf_host::path::HostPath;
use kbf_host::plan::{DRY_RUN_NOTE, Mode, Planner, RunAs, execute};
use kbf_host::step::{Owner, Perms};

const ROOT: Owner = Owner { uid: 0, gid: 0 };

/// One `mdm dep` verb.
pub enum Dep {
    GetCert { out: HostPath },
    UploadToken { token: HostPath },
    DefineProfile,
    Assign { serials: Vec<Serial> },
}

/// A device serial number as ABM lists it: 8 to 14 upper-case letters and digits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Serial(String);

impl Serial {
    pub fn new(text: &str) -> Result<Serial, String> {
        let ok = (8..=14).contains(&text.len())
            && text
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if !ok {
            return Err(format!(
                "{text:?} is not a serial number (8 to 14 upper-case letters and digits)"
            ));
        }
        Ok(Serial(text.to_owned()))
    }
}

/// Check, then print or carry out `d`. `Ok(false)` when Apple did not assign every
/// device.
pub fn run<E: Exec>(
    exec: &mut E,
    c: &MdmConfig,
    d: &Dep,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<bool, MdmError> {
    let euid = exec.euid();
    if euid != 0 {
        return Err(MdmError::NeedsRoot {
            verb: "mdm dep",
            euid,
        });
    }
    let api = Api::nanodep(&*exec, c)?;
    let name = &c.dep_name;
    match d {
        Dep::GetCert { out: file } => get_cert(exec, &api, name, file, mode, out)?,
        Dep::UploadToken { token } => upload_token(&*exec, &api, name, token, mode, out)?,
        Dep::DefineProfile => define_profile(exec, &api, c, mode, out)?,
        Dep::Assign { serials } => return assign(&*exec, &api, c, serials, mode, out),
    }
    Ok(true)
}

fn get_cert<E: Exec>(
    exec: &mut E,
    api: &Api,
    name: &str,
    file: &HostPath,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), MdmError> {
    outside_git(&*exec, file)?;
    let next = "upload it in Apple Business Manager (your MDM server, Upload Public Key), then download the server token (.p7m)";
    match host::inspect(&*exec, file)? {
        Some(Entry::File { bytes, .. }) if is_certificate(&bytes) => {
            note(
                out,
                format_args!("{file}: holds a certificate, kept; {next}"),
            )?;
            return Ok(());
        }
        Some(_) => return Err(MdmError::NotCertificate(file.clone())),
        None => {}
    }
    let get = Call::get(format!("/v1/tokenpki/{name}?cn=kbf-mdm&validity_days=7"));
    let Some(pem) = api::call(&*exec, api, &get, mode, out)? else {
        note(
            out,
            format_args!("DRY-RUN: write {file} (0644, the certificate NanoDEP answers)"),
        )?;
        note(out, format_args!("{DRY_RUN_NOTE}"))?;
        return Ok(());
    };
    if !is_certificate(&pem) {
        return Err(MdmError::Answer {
            what: "the token certificate",
            why: "not a PEM certificate".to_owned(),
        });
    }
    let mut planner = Planner::new(&*exec);
    planner.file(file.clone(), Perms::new(0o644), Some(ROOT), pem)?;
    let plan = planner.finish();
    execute(&plan, RunAs::Root, mode, exec, &mut out)?;
    note(out, format_args!("{file}: {next}"))?;
    Ok(())
}

fn is_certificate(bytes: &[u8]) -> bool {
    x509_parser::pem::parse_x509_pem(bytes).is_ok_and(|(_, pem)| pem.parse_x509().is_ok())
}

fn upload_token<E: Exec>(
    exec: &E,
    api: &Api,
    name: &str,
    token: &HostPath,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), MdmError> {
    outside_git(exec, token)?;
    let Some(Entry::File { bytes, .. }) = host::inspect(exec, token)? else {
        return Err(MdmError::NoFile(token.clone()));
    };
    let put = Call::put(format!("/v1/tokenpki/{name}"), bytes);
    api::call(exec, api, &put, mode, out)?;
    let get = Call::get(format!("/proxy/{name}/account"));
    let Some(account) = api::call(exec, api, &get, mode, out)? else {
        note(out, format_args!("{DRY_RUN_NOTE}"))?;
        return Ok(());
    };
    let account: serde_json::Value = serde_json::from_slice(&account).unwrap_or_default();
    let field = |key: &str| account.get(key).and_then(|v| v.as_str()).unwrap_or("?");
    note(
        out,
        format_args!(
            "ABM: org {}, MDM server {}",
            field("org_name"),
            field("server_name")
        ),
    )?;
    note(
        out,
        format_args!("the token is stored; you may delete {token} (NanoDEP keeps it)"),
    )?;
    Ok(())
}

fn define_profile<E: Exec>(
    exec: &mut E,
    api: &Api,
    c: &MdmConfig,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), MdmError> {
    let json = AdeProfile::render(c)?.to_json();
    let sum = format!("{}\n", hex::encode(Sha256::digest(&json)));
    let uuid_file = under(&c.state, "dep-profile-uuid")?;
    let sum_file = under(&c.state, "dep-profile.sha256")?;
    let recorded = |path: &HostPath| -> Result<Option<Vec<u8>>, MdmError> {
        Ok(match host::inspect(&*exec, path)? {
            Some(Entry::File { bytes, .. }) => Some(bytes),
            _ => None,
        })
    };
    let mut planner = Planner::new(&*exec);
    if recorded(&sum_file)?.as_deref() == Some(sum.as_bytes()) {
        let uuid = recorded(&uuid_file)?.unwrap_or_default();
        let uuid = String::from_utf8_lossy(&uuid).trim().to_owned();
        note(out, format_args!("ADE profile: unchanged (uuid {uuid})"))?;
    } else {
        let name = &c.dep_name;
        let define = Call::post_json(format!("/proxy/{name}/profile"), json.clone());
        match api::call(&*exec, api, &define, mode, out)? {
            Some(answer) => {
                let uuid = profile_uuid(&answer)?;
                let assigner = Call::put(
                    format!("/v1/assigner/{name}?profile_uuid={uuid}"),
                    Vec::new(),
                );
                api::call(&*exec, api, &assigner, mode, out)?;
                note(
                    out,
                    format_args!(
                        "ADE profile: defined, uuid {uuid}; the assigner gives it to new devices"
                    ),
                )?;
                let line = format!("{uuid}\n").into_bytes();
                planner.file(uuid_file, Perms::new(0o644), Some(ROOT), line)?;
            }
            None => {
                note(
                    out,
                    format_args!(
                        "DRY-RUN: PUT /v1/assigner/{name}?profile_uuid=<the profile_uuid Apple answers>"
                    ),
                )?;
                note(out, format_args!("DRY-RUN: write {uuid_file} (that uuid)"))?;
                for line in String::from_utf8_lossy(&json).lines() {
                    note(out, format_args!("    | {line}"))?;
                }
            }
        }
        planner.file(sum_file, Perms::new(0o644), Some(ROOT), sum.into_bytes())?;
    }
    install::service(&*exec, &mut planner, DEPSYNCER, false)?;
    let plan = planner.finish();
    Ok(execute(&plan, RunAs::Root, mode, exec, &mut out)?)
}

/// The `profile_uuid` of Apple's answer to a profile definition: 16 to 64 hex digits.
fn profile_uuid(answer: &[u8]) -> Result<String, MdmError> {
    let value: serde_json::Value = serde_json::from_slice(answer).unwrap_or_default();
    let uuid = value
        .get("profile_uuid")
        .and_then(|u| u.as_str())
        .unwrap_or_default();
    if !(16..=64).contains(&uuid.len()) || !uuid.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(MdmError::Answer {
            what: "the profile definition",
            why: format!(
                "no profile_uuid of 16 to 64 hex digits in {}",
                String::from_utf8_lossy(answer)
            ),
        });
    }
    Ok(uuid.to_owned())
}

fn assign<E: Exec>(
    exec: &E,
    api: &Api,
    c: &MdmConfig,
    serials: &[Serial],
    mode: Mode,
    out: &mut dyn Write,
) -> Result<bool, MdmError> {
    let uuid_file = under(&c.state, "dep-profile-uuid")?;
    let Some(Entry::File { bytes, .. }) = host::inspect(exec, &uuid_file)? else {
        return Err(MdmError::NoProfile(uuid_file));
    };
    let uuid = String::from_utf8_lossy(&bytes).trim().to_owned();
    let devices: Vec<&str> = serials.iter().map(|s| s.0.as_str()).collect();
    let body = json!({ "profile_uuid": uuid, "devices": devices });
    let post = Call::post_json(
        format!("/proxy/{}/profile/devices", c.dep_name),
        body.to_string().into_bytes(),
    );
    let Some(answer) = api::call(exec, api, &post, mode, out)? else {
        note(out, format_args!("{DRY_RUN_NOTE}"))?;
        return Ok(true);
    };
    // `{"profile_uuid": ..., "devices": {"<serial>": "SUCCESS" | "NOT_ACCESSIBLE" | ...}}`
    let answer: serde_json::Value = serde_json::from_slice(&answer).unwrap_or_default();
    let mut all = true;
    for serial in devices {
        let status = answer
            .get("devices")
            .and_then(|d| d.get(serial))
            .and_then(|s| s.as_str())
            .unwrap_or("no answer");
        all &= status == "SUCCESS";
        note(out, format_args!("{serial}: {status}"))?;
    }
    Ok(all)
}

#[cfg(test)]
#[path = "dep_tests.rs"]
mod tests;
