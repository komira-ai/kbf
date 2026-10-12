//! `mdm push-cert`: give NanoHUB the APNs push certificate and its key, and write the
//! enrollment profile the Macs fetch, as root.
//!
//! With `--cert PEM --key PEM` it checks the pair first and refuses, before anything
//! changes: a file inside a git work tree (keys are kept outside every repository); a
//! key file that is not mode 0600; a certificate that is not an MDM push certificate
//! (its subject UID is not `com.apple.mgmt.External.<uuid>`) or has expired; a key
//! that is encrypted (NanoHUB needs it plain), is not a private key, or is not the
//! certificate's. Then, unless NanoHUB already has this certificate (its sha256 is in
//! `MDM_STATE/push-cert.sha256` and its topic in `MDM_STATE/push-topic`), it `PUT`s the
//! certificate and key to NanoHUB's `/api/v1/nanomdm/pushcert` on loopback ([`api`]),
//! checks NanoHUB answers the same topic, and records the topic and the sha256.
//!
//! With `--profile-only` it skips the certificate and takes the topic from `--topic`,
//! else from `MDM_STATE/push-topic`.
//!
//! Either way it writes the enrollment profile ([`profile::enrollment`]: the topic,
//! `MDM_DNS_NAME`, `MDM_ORG` and the SCEP challenge from `MDM_ETC/scep.env`) to
//! `MDM_STATE/www/enroll.mobileconfig`, 0600 and `MDM_USER`'s, where the proxy serves
//! it at `https://<MDM_DNS_NAME>/enroll`. The challenge is in the file and never
//! printed. A run with nothing new changes nothing: the payload UUIDs are fixed.
//!
//! [`api`]: super::api

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::pem::PemObject;
use sha2::{Digest, Sha256};

use super::MdmError;
use super::api::{self, Api, Call};
use super::config::MdmConfig;
use super::profile::{self, Topic};
use super::secret;
use crate::linux::host::{self, note, under};
use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;
use kbf_host::plan::{Mode, Planner, RunAs, execute};
use kbf_host::step::{Owner, Perms};
use kbf_host::user;

/// The subject attribute an APNs certificate names its topic in: `UID`.
const UID_OID: &str = "0.9.2342.19200300.100.1.1";
const ROOT: Owner = Owner { uid: 0, gid: 0 };

/// What `mdm push-cert` was asked for.
pub struct PushCert {
    pub config: MdmConfig,
    pub with: With,
}

/// Where the topic comes from.
pub enum With {
    /// A push certificate and its key, given to NanoHUB.
    Pair { cert: HostPath, key: HostPath },
    /// `--profile-only`: the profile alone, for `--topic` or the recorded topic.
    ProfileOnly(Option<Topic>),
}

/// Why a push certificate or its key was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PairError {
    #[error("{0} is not a PEM certificate")]
    NotCertificate(HostPath),
    #[error(
        "{cert} is not an MDM push certificate: its subject UID is {uid:?}, not com.apple.mgmt.External.<uuid>"
    )]
    NotPush { cert: HostPath, uid: String },
    #[error("{cert} expired at {not_after}: renew it at the Apple Push Certificates Portal")]
    Expired { cert: HostPath, not_after: String },
    #[error("{0} is encrypted or not a PEM private key (NanoHUB needs it unencrypted)")]
    Key(HostPath),
    #[error("{key} is not the key of {cert}")]
    NotItsKey { key: HostPath, cert: HostPath },
}

/// A checked push certificate: its topic, expiry and sha256.
#[derive(Debug, PartialEq, Eq)]
pub struct Checked {
    pub topic: Topic,
    pub not_after: String,
    pub sha256: String,
}

/// Check that `cert_pem` is an unexpired MDM push certificate and `key_pem` its plain
/// private key (`cert` and `key` name the files in errors).
pub fn check_pair(
    cert: &HostPath,
    cert_pem: &[u8],
    key: &HostPath,
    key_pem: &[u8],
    now: SystemTime,
) -> Result<Checked, PairError> {
    let not_cert = || PairError::NotCertificate(cert.clone());
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem).map_err(|_| not_cert())?;
    let parsed = pem.parse_x509().map_err(|_| not_cert())?;
    let uid = parsed
        .subject()
        .iter_attributes()
        .find(|a| a.attr_type().to_id_string() == UID_OID)
        .and_then(|a| a.as_str().ok())
        .unwrap_or_default();
    let topic = Topic::new(uid).map_err(|_| PairError::NotPush {
        cert: cert.clone(),
        uid: uid.to_owned(),
    })?;
    let not_after = parsed.validity().not_after;
    let now = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if not_after.timestamp() <= i64::try_from(now).unwrap_or(i64::MAX) {
        return Err(PairError::Expired {
            cert: cert.clone(),
            not_after: not_after.to_string(),
        });
    }
    let bad_key = || PairError::Key(key.clone());
    let der = PrivateKeyDer::from_pem_slice(key_pem).map_err(|_| bad_key())?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&der).map_err(|_| bad_key())?;
    let public = signer.public_key().ok_or(PairError::Key(key.clone()))?;
    if public.as_ref() != parsed.public_key().raw {
        return Err(PairError::NotItsKey {
            key: key.clone(),
            cert: cert.clone(),
        });
    }
    Ok(Checked {
        topic,
        not_after: not_after.to_string(),
        sha256: hex::encode(Sha256::digest(cert_pem)),
    })
}

/// Check, then print or carry out the push certificate's upload and the profile.
pub fn run<E: Exec>(
    exec: &mut E,
    a: &PushCert,
    now: SystemTime,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), MdmError> {
    let c = &a.config;
    let euid = exec.euid();
    if euid != 0 {
        return Err(MdmError::NeedsRoot {
            verb: "mdm push-cert",
            euid,
        });
    }
    let www = under(&c.state, "www")?;
    if host::stat(&*exec, &www)?.is_none_or(|m| m.kind != Kind::Dir) {
        return Err(MdmError::NotInstalled(www));
    }
    let topic_file = under(&c.state, "push-topic")?;
    let sum_file = under(&c.state, "push-cert.sha256")?;
    let mut planner = Planner::new(&*exec);
    let mut upload = None;
    let topic = match &a.with {
        With::Pair { cert, key } => {
            let (cert_pem, key_pem) = pair_files(&*exec, cert, key)?;
            let checked = check_pair(cert, &cert_pem, key, &key_pem, now)?;
            note(
                out,
                format_args!(
                    "push certificate: topic {}, valid until {} (renew it before then at the Apple Push Certificates Portal, with the same Apple Account)",
                    checked.topic, checked.not_after
                ),
            )?;
            let topic_line = format!("{}\n", checked.topic).into_bytes();
            let sum_line = format!("{}\n", checked.sha256).into_bytes();
            let topic_changes =
                planner.file(topic_file, Perms::new(0o644), Some(ROOT), topic_line)?;
            let sum_changes = planner.file(sum_file, Perms::new(0o644), Some(ROOT), sum_line)?;
            if topic_changes || sum_changes {
                let mut body = cert_pem;
                body.extend_from_slice(&key_pem);
                upload = Some(body);
            } else {
                note(
                    out,
                    format_args!("NanoHUB: has this certificate already (push-cert.sha256)"),
                )?;
            }
            checked.topic
        }
        With::ProfileOnly(Some(topic)) => topic.clone(),
        With::ProfileOnly(None) => match host::inspect(&*exec, &topic_file)? {
            Some(Entry::File { bytes, .. }) => Topic::new(String::from_utf8_lossy(&bytes).trim())?,
            _ => return Err(MdmError::NoTopic(topic_file)),
        },
    };
    let challenge = secret::read(
        &*exec,
        &under(&c.etc, "scep.env")?,
        "SCEP_CHALLENGE_PASSWORD",
    )?;
    let owner = user::lookup(&*exec, &c.user)?.ok_or(MdmError::NoUser {
        user: c.user.clone(),
        home: c.state.clone(),
    })?;
    let bytes = profile::enrollment(c, &topic, &challenge);
    let dest = under(&www, "enroll.mobileconfig")?;
    let mine = Owner {
        uid: owner.uid,
        gid: owner.gid,
    };
    planner.file(dest, Perms::new(0o600), Some(mine), bytes)?;
    let name = &c.dns_name;
    note(
        out,
        format_args!(
            "profile: ServerURL https://{name}/mdm, SCEP https://{name}/scep, topic {topic}; served at https://{name}/enroll (the challenge is in it, not shown)"
        ),
    )?;
    let plan = planner.finish();
    if let Some(body) = upload {
        let api = Api::nanohub(&*exec, c)?;
        let put = Call::put("/api/v1/nanomdm/pushcert", body);
        if let Some(answer) = api::call(&*exec, &api, &put, mode, out)? {
            let got = serde_json::from_slice::<serde_json::Value>(&answer)
                .ok()
                .and_then(|v| v.get("topic")?.as_str().map(str::to_owned))
                .unwrap_or_default();
            if got != topic.as_str() {
                return Err(MdmError::TopicDiffers {
                    got,
                    want: topic.to_string(),
                });
            }
            note(
                out,
                format_args!("NanoHUB: push certificate stored for {topic}"),
            )?;
        }
    }
    Ok(execute(&plan, RunAs::Root, mode, exec, &mut out)?)
}

/// The certificate and the key, each refused inside a git work tree; the key also
/// unless it is mode 0600.
fn pair_files<E: Exec>(
    exec: &E,
    cert: &HostPath,
    key: &HostPath,
) -> Result<(Vec<u8>, Vec<u8>), MdmError> {
    outside_git(exec, cert)?;
    outside_git(exec, key)?;
    let Some(Entry::File {
        bytes: cert_pem, ..
    }) = host::inspect(exec, cert)?
    else {
        return Err(MdmError::NoFile(cert.clone()));
    };
    let Some(Entry::File { perms, bytes, .. }) = host::inspect(exec, key)? else {
        return Err(MdmError::NoFile(key.clone()));
    };
    if perms != Perms::new(0o600) {
        return Err(MdmError::SecretMode {
            path: key.clone(),
            perms,
        });
    }
    Ok((cert_pem, bytes))
}

/// Refuse `path` when a directory above it holds `.git`: keys, tokens and certificates
/// are kept outside every repository.
pub fn outside_git<E: Exec>(exec: &E, path: &HostPath) -> Result<(), MdmError> {
    let s = path.as_str();
    for (i, _) in s.match_indices('/') {
        let git = HostPath::new(&format!("{}/.git", &s[..i]))?;
        if host::stat(exec, &git)?.is_some() {
            return Err(MdmError::InRepository {
                path: path.clone(),
                git,
            });
        }
    }
    Ok(())
}

/// The secret a test profile is rendered with.
#[cfg(test)]
pub(crate) fn test_challenge() -> secret::Secret {
    secret::Secret::new("0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0")
}

#[cfg(test)]
#[path = "push_cert_tests.rs"]
pub(crate) mod tests;
