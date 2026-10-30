use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::mdm::tests::config_text;
use crate::testkit::{HttpServer, http, http_server};
use kbf_host::step::Step;
use kbf_host::testkit::{FakeExec, reply};
use kbf_host::user::{GETENT, ID};

pub(crate) const TOPIC: &str = "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d";
const CERT: &str = "/srv/keys/push.pem";
const KEY: &str = "/srv/keys/push.key";
const UID: u32 = 990;

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

fn file(bytes: &[u8], perms: u32, uid: u32) -> Entry {
    Entry::File {
        perms: Perms::new(perms),
        owner: Owner { uid, gid: uid },
        bytes: bytes.to_vec(),
    }
}

/// A push certificate for `uid` (its subject UID), and its key, as PEM.
pub(crate) fn pair(uid: &str, expired: bool) -> (String, String) {
    let key = rcgen::KeyPair::generate().expect("key");
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.distinguished_name.push(
        rcgen::DnType::CustomDnType(vec![0, 9, 2342, 19_200_300, 100, 1, 1]),
        uid,
    );
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "APSP:test");
    if expired {
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
    }
    let cert = params.self_signed(&key).expect("certificate");
    (cert.pem(), key.serialize_pem())
}

/// A host `mdm install` has set up, with the settings pointing NanoHUB at `server`.
pub(crate) fn host() -> FakeExec {
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd| {
        Ok(match cmd.program.as_str() {
            GETENT => reply(
                0,
                &format!("kbf-mdm:x:{UID}:{UID}::/var/lib/kbf-mdm:/usr/sbin/nologin\n"),
                "",
            ),
            ID => reply(0, &format!("{UID}\n"), ""),
            _ => reply(0, "", ""),
        })
    });
    let dir = Entry::Dir {
        perms: Perms::new(0o700),
        owner: Owner { uid: UID, gid: UID },
    };
    exec.entries.insert(p("/var/lib/kbf-mdm/www"), dir);
    let secrets = [
        ("scep.env", "SCEP_CHALLENGE_PASSWORD"),
        ("nanohub.env", "NANOHUB_API_KEY"),
        ("nanodep.env", "NANODEP_API"),
    ];
    for (name, key) in secrets {
        let value = match key {
            "SCEP_CHALLENGE_PASSWORD" => test_challenge().expose().to_owned(),
            _ => "abc123".to_owned(),
        };
        exec.entries.insert(
            p(&format!("/etc/kbf-mdm/{name}")),
            file(format!("{key}={value}\n").as_bytes(), 0o600, 0),
        );
    }
    exec
}

/// The kit's settings, with the API ports at `server`'s.
pub(crate) fn config_at(server: &HttpServer) -> MdmConfig {
    let port = server.addr.port();
    let text = config_text()
        .replace("MDM_NANOHUB_PORT=9004", &format!("MDM_NANOHUB_PORT={port}"))
        .replace("MDM_DEP_PORT=9001", &format!("MDM_DEP_PORT={port}"));
    MdmConfig::parse(&text).expect("valid settings")
}

fn nanohub(topic: &'static str) -> HttpServer {
    http_server(move |_| http("200 OK", format!("{{\"topic\":\"{topic}\"}}").as_bytes()))
}

/// A PEM block labelled `label` whose bytes are not one.
fn garbage(label: &str) -> String {
    format!("-----BEGIN {label}-----\nMIIBAAAA\n-----END {label}-----\n")
}

fn with_pair(exec: &mut FakeExec, cert: &str, key: &str) {
    exec.entries
        .insert(p(CERT), file(cert.as_bytes(), 0o644, 501));
    exec.entries
        .insert(p(KEY), file(key.as_bytes(), 0o600, 501));
}

fn pair_args(server: &HttpServer) -> PushCert {
    PushCert {
        config: config_at(server),
        with: With::Pair {
            cert: p(CERT),
            key: p(KEY),
        },
    }
}

/// The run's fixed instant: 2026-01-01T00:00:00Z, after the expired cert's 2001 and before
/// the good cert's default expiry.
fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_767_225_600)
}

fn run_it(exec: &mut FakeExec, a: &PushCert, mode: Mode) -> (Result<(), MdmError>, String) {
    let mut out = Vec::new();
    let got = run(exec, a, now(), mode, &mut out);
    (got, String::from_utf8_lossy(&out).into_owned())
}

fn written(exec: &FakeExec, path: &str) -> Vec<u8> {
    match exec.entries.get(&p(path)) {
        Some(Entry::File { bytes, .. }) => bytes.clone(),
        other => panic!("{path}: {other:?}"),
    }
}

/// Catches a dry run that uploads the certificate or writes a file, and one that does
/// not show what an apply would do (the upload, the two records, the profile).
#[test]
fn a_dry_run_checks_the_pair_and_shows_the_upload_and_the_profile() {
    let server = nanohub(TOPIC);
    let mut exec = host();
    let (cert, key) = pair(TOPIC, false);
    with_pair(&mut exec, &cert, &key);
    let (got, out) = run_it(&mut exec, &pair_args(&server), Mode::DryRun);
    got.expect("planned");
    assert!(server.requests().is_empty() && exec.log.is_empty(), "{out}");
    for line in [
        format!("push certificate: topic {TOPIC}, valid until"),
        "DRY-RUN: PUT http://127.0.0.1:".to_owned(),
        "/api/v1/nanomdm/pushcert (NanoHUB API, ".to_owned(),
        "DRY-RUN: write /var/lib/kbf-mdm/push-topic (0644, owner 0:0".to_owned(),
        "DRY-RUN: write /var/lib/kbf-mdm/push-cert.sha256 (0644, owner 0:0".to_owned(),
        format!("DRY-RUN: write /var/lib/kbf-mdm/www/enroll.mobileconfig (0600, owner {UID}:{UID}"),
    ] {
        assert!(out.contains(&line), "{line:?} not in {out}");
    }
    assert!(!out.contains(test_challenge().expose()) && !out.contains("PRIVATE"));
}

/// Catches an apply that does not send NanoHUB the certificate followed by its key,
/// records the wrong topic, writes a profile other than the one for this topic and
/// challenge (or one others can read: the challenge is in it), and a re-run that
/// uploads or writes again.
#[test]
fn an_apply_uploads_records_and_writes_the_profile_and_a_rerun_does_nothing() {
    let server = nanohub(TOPIC);
    let mut exec = host();
    let (cert, key) = pair(TOPIC, false);
    with_pair(&mut exec, &cert, &key);
    let a = pair_args(&server);
    let (got, out) = run_it(&mut exec, &a, Mode::Apply);
    got.expect("applied");
    let r = server.requests();
    assert_eq!(r.len(), 1, "{out}");
    assert_eq!(r[0].body, format!("{cert}{key}").into_bytes());
    assert!(out.contains(&format!("NanoHUB: push certificate stored for {TOPIC}")));
    assert_eq!(
        written(&exec, "/var/lib/kbf-mdm/push-topic"),
        format!("{TOPIC}\n").into_bytes()
    );
    let sum = format!("{}\n", hex::encode(Sha256::digest(cert.as_bytes())));
    assert_eq!(
        written(&exec, "/var/lib/kbf-mdm/push-cert.sha256"),
        sum.into_bytes()
    );
    let c = &a.config;
    let want = profile::enrollment(c, &Topic::new(TOPIC).expect("topic"), &test_challenge());
    let profile = exec
        .entries
        .get(&p("/var/lib/kbf-mdm/www/enroll.mobileconfig"));
    assert_eq!(profile, Some(&file(&want, 0o600, UID)));

    let steps = exec.log.len();
    let (got, out) = run_it(&mut exec, &a, Mode::Apply);
    got.expect("re-run");
    assert_eq!(server.requests().len(), 1, "{out}");
    assert_eq!(exec.log.len(), steps);
    assert!(
        out.contains("NanoHUB: has this certificate already"),
        "{out}"
    );
    assert!(out.ends_with("nothing to do\n"), "{out}");
}

/// Catches each pair NanoHUB must not get, accepted: a certificate that is not an MDM
/// push certificate, an expired one, an encrypted key, a PEM key that is not one, another
/// certificate's key, a file that is not a certificate (not PEM, or PEM that is not one), a key others can read, a key in a git work tree,
/// and a missing file. Nothing is sent in any case.
#[test]
fn a_pair_that_is_not_a_plain_unexpired_mdm_push_pair_is_refused() {
    let server = nanohub(TOPIC);
    let (cert, key) = pair(TOPIC, false);
    let (_, other_key) = pair(TOPIC, false);
    let (web_cert, web_key) = pair("www.example.com", false);
    let (old_cert, old_key) = pair(TOPIC, true);
    let encrypted =
        "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIB\n-----END ENCRYPTED PRIVATE KEY-----\n";
    type Edit = Box<dyn Fn(&mut FakeExec)>;
    let set = |c: String, k: String| -> Edit { Box::new(move |e| with_pair(e, &c, &k)) };
    let cases: Vec<(Edit, &str)> = vec![
        (
            set(web_cert, web_key),
            "not an MDM push certificate: its subject UID is \"www.example.com\"",
        ),
        (set(old_cert, old_key), "expired at"),
        (
            set(cert.clone(), encrypted.to_owned()),
            "is encrypted or not a PEM private key",
        ),
        (set(cert.clone(), other_key), "is not the key of"),
        (
            set(cert.clone(), garbage("PRIVATE KEY")),
            "is encrypted or not a PEM private key",
        ),
        (
            set("not pem".to_owned(), key.clone()),
            "is not a PEM certificate",
        ),
        (
            set(garbage("CERTIFICATE"), key.clone()),
            "is not a PEM certificate",
        ),
        (
            Box::new({
                let (c, k) = (cert.clone(), key.clone());
                move |e: &mut FakeExec| {
                    with_pair(e, &c, &k);
                    e.entries.insert(p(KEY), file(k.as_bytes(), 0o644, 501));
                }
            }),
            "has mode 0644, not 0600",
        ),
        (
            Box::new({
                let (c, k) = (cert.clone(), key.clone());
                move |e: &mut FakeExec| {
                    with_pair(e, &c, &k);
                    e.entries.insert(p("/srv/.git"), file(b"", 0o644, 501));
                }
            }),
            "is inside a git work tree (/srv/.git exists)",
        ),
        (
            Box::new({
                let k = key.clone();
                move |e: &mut FakeExec| {
                    e.entries.insert(p(KEY), file(k.as_bytes(), 0o600, 501));
                }
            }),
            "/srv/keys/push.pem is not a file",
        ),
        (
            Box::new({
                let c = cert.clone();
                move |e: &mut FakeExec| {
                    e.entries.insert(p(CERT), file(c.as_bytes(), 0o644, 501));
                }
            }),
            "/srv/keys/push.key is not a file",
        ),
    ];
    for (edit, want) in cases {
        let mut exec = host();
        edit(&mut exec);
        let (got, _) = run_it(&mut exec, &pair_args(&server), Mode::Apply);
        let err = got.err().map(|e| (e.exit_code(), e.to_string()));
        assert!(
            err.as_ref()
                .is_some_and(|(code, e)| *code == 2 && e.contains(want)),
            "{want}: {err:?}"
        );
        assert!(
            server.requests().is_empty() && exec.log.is_empty(),
            "{want}"
        );
    }
}

/// Catches either file's git check dropped: the certificate alone inside a git work
/// tree, then the key alone, each refused naming that file; nothing is sent. (The row
/// above plants `.git` above both, so either check alone passes it.)
#[test]
fn a_certificate_or_a_key_alone_inside_a_git_work_tree_is_refused() {
    let server = nanohub(TOPIC);
    let (cert, key) = pair(TOPIC, false);
    for (in_git, want) in [
        (
            true,
            "/repo/push.pem is inside a git work tree (/repo/.git exists)",
        ),
        (
            false,
            "/repo/push.key is inside a git work tree (/repo/.git exists)",
        ),
    ] {
        let (c, k) = if in_git {
            ("/repo/push.pem", KEY)
        } else {
            (CERT, "/repo/push.key")
        };
        let mut exec = host();
        exec.entries.insert(p(c), file(cert.as_bytes(), 0o644, 501));
        exec.entries.insert(p(k), file(key.as_bytes(), 0o600, 501));
        exec.entries.insert(p("/repo/.git"), file(b"", 0o644, 501));
        let a = PushCert {
            config: config_at(&server),
            with: With::Pair {
                cert: p(c),
                key: p(k),
            },
        };
        let (got, _) = run_it(&mut exec, &a, Mode::Apply);
        let err = got.err().map(|e| e.to_string());
        assert!(
            err.as_ref().is_some_and(|e| e.starts_with(want)),
            "{want}: {err:?}"
        );
        assert!(server.requests().is_empty(), "{want}");
        assert!(exec.log.is_empty(), "{want}");
    }
}

/// Catches an upload taken as done when NanoHUB stored the certificate under another
/// topic (the Macs would enroll for pushes that never come), and a refusal by NanoHUB
/// taken as done; neither records anything.
#[test]
fn nanohub_answering_another_topic_or_refusing_fails_and_records_nothing() {
    let other = "com.apple.mgmt.External.ffffffff-4e5f-6a7b-8c9d-0e1f2a3b4c5d";
    let refusing = http_server(|_| http("400 Bad Request", b"bad certificate"));
    for server in [nanohub(other), refusing] {
        let mut exec = host();
        let (cert, key) = pair(TOPIC, false);
        with_pair(&mut exec, &cert, &key);
        let (got, _) = run_it(&mut exec, &pair_args(&server), Mode::Apply);
        let err = got.err().map(|e| (e.exit_code(), e.to_string()));
        assert!(err.as_ref().is_some_and(|(code, _)| *code == 1), "{err:?}");
        assert_eq!(server.requests().len(), 1);
        assert!(exec.log.is_empty(), "{:?}", exec.log);
    }
}

/// Catches `--profile-only` that ignores `--topic`, cannot find the recorded topic, or
/// writes a profile for no topic; and a verb that runs as a user or before the MDM is
/// installed.
#[test]
fn profile_only_takes_the_given_or_recorded_topic() {
    let server = nanohub(TOPIC);
    let given = "com.apple.mgmt.External.11111111-4e5f-6a7b-8c9d-0e1f2a3b4c5d";
    let profile_with = |topic: &str| {
        let c = config_at(&server);
        let t = Topic::new(topic).expect("topic");
        profile::enrollment(&c, &t, &test_challenge())
    };
    let only = |topic: Option<&str>| PushCert {
        config: config_at(&server),
        with: With::ProfileOnly(topic.map(|t| Topic::new(t).expect("topic"))),
    };
    let mut exec = host();
    let (got, out) = run_it(&mut exec, &only(None), Mode::Apply);
    let err = got.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(err.contains("no push topic"), "{err} {out}");
    run_it(&mut exec, &only(Some(given)), Mode::Apply)
        .0
        .expect("given");
    let at = "/var/lib/kbf-mdm/www/enroll.mobileconfig";
    assert_eq!(written(&exec, at), profile_with(given));
    exec.entries.insert(
        p("/var/lib/kbf-mdm/push-topic"),
        file(format!("{TOPIC}\n").as_bytes(), 0o644, 0),
    );
    run_it(&mut exec, &only(None), Mode::Apply)
        .0
        .expect("recorded");
    assert_eq!(written(&exec, at), profile_with(TOPIC));
    assert!(server.requests().is_empty());
    assert!(exec.log.iter().all(|s| matches!(s, Step::WriteFile { .. })));

    let mut user = host();
    user.euid = 501;
    let err = run_it(&mut user, &only(Some(given)), Mode::DryRun).0.err();
    assert!(matches!(err, Some(MdmError::NeedsRoot { euid: 501, .. })));
    let mut bare = host();
    bare.entries.remove(&p("/var/lib/kbf-mdm/www"));
    let err = run_it(&mut bare, &only(Some(given)), Mode::DryRun).0.err();
    assert!(matches!(err, Some(MdmError::NotInstalled(_))), "{err:?}");
    let mut nobody = host();
    nobody.answer = Box::new(|_| Ok(reply(2, "", "")));
    let err = run_it(&mut nobody, &only(Some(given)), Mode::DryRun)
        .0
        .err();
    assert!(matches!(err, Some(MdmError::NoUser { .. })), "{err:?}");
}

/// Catches a renewal that is never sent: the renewed certificate keeps the topic, so
/// only its recorded sha256 tells NanoHUB's copy is the old one, which would expire.
#[test]
fn a_renewed_certificate_for_the_same_topic_is_uploaded_again() {
    let server = nanohub(TOPIC);
    let mut exec = host();
    let (cert, key) = pair(TOPIC, false);
    with_pair(&mut exec, &cert, &key);
    let a = pair_args(&server);
    run_it(&mut exec, &a, Mode::Apply).0.expect("first");
    let (renewed, renewed_key) = pair(TOPIC, false);
    with_pair(&mut exec, &renewed, &renewed_key);
    let (got, out) = run_it(&mut exec, &a, Mode::Apply);
    got.expect("renewal");
    let r = server.requests();
    assert_eq!(r.len(), 2, "{out}");
    assert_eq!(r[1].body, format!("{renewed}{renewed_key}").into_bytes());
    let sum = format!("{}\n", hex::encode(Sha256::digest(renewed.as_bytes())));
    assert_eq!(
        written(&exec, "/var/lib/kbf-mdm/push-cert.sha256"),
        sum.into_bytes()
    );
}
