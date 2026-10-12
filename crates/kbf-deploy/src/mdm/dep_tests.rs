use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use super::*;
use crate::linux::host::SYSTEMCTL;
use crate::mdm::push_cert::tests::{config_at, host, pair};
use crate::testkit::{HttpServer, Request, http, http_server};
use kbf_host::step::Cmd;
use kbf_host::testkit::{FakeExec, reply};

const UUID: &str = "0123456789ABCDEF0123456789ABCDEF";
const OUT: &str = "/srv/abm/token-cert.pem";
const TOKEN: &str = "/srv/abm/token.p7m";

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

/// depserver as the kit calls it, with Apple behind its `/proxy/` routes.
fn depserver(uuid: &'static str) -> HttpServer {
    let (cert, _) = pair("token", false);
    http_server(move |r: &Request| {
        match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/v1/tokenpki/example?cn=kbf-mdm&validity_days=7") => {
            http("200 OK", cert.as_bytes())
        }
        ("GET", "/proxy/example/account") => http(
            "200 OK",
            br#"{"org_name":"Example Org","server_name":"kbf farm"}"#,
        ),
        ("POST", "/proxy/example/profile") => http(
            "200 OK",
            format!(r#"{{"profile_uuid":"{uuid}","devices":{{}}}}"#).as_bytes(),
        ),
        ("POST", "/proxy/example/profile/devices") => http(
            "200 OK",
            br#"{"profile_uuid":"x","devices":{"C02AB12CD3EF":"SUCCESS","C02ZZ99ZZ9ZZ":"NOT_ACCESSIBLE"}}"#,
        ),
        ("PUT", _) => http("200 OK", b""),
        _ => http("404 Not Found", b"no route"),
    }
    })
}

/// [`host`], with systemctl keeping which units are enabled and active.
fn host_with_units() -> (FakeExec, Rc<RefCell<BTreeSet<String>>>) {
    let mut exec = host();
    let units = Rc::new(RefCell::new(BTreeSet::new()));
    let u = Rc::clone(&units);
    let users = std::mem::replace(&mut exec.answer, Box::new(|_| Ok(reply(0, "", ""))));
    exec.answer = Box::new(move |cmd: &Cmd| {
        if cmd.program.as_str() != SYSTEMCTL {
            return users(cmd);
        }
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        let mut u = u.borrow_mut();
        Ok(match &args[..] {
            [verb @ ("is-enabled" | "is-active"), "--quiet", unit] => {
                reply(i32::from(!u.contains(&format!("{verb} {unit}"))), "", "")
            }
            ["enable", unit] => {
                u.insert(format!("is-enabled {unit}"));
                reply(0, "", "")
            }
            ["start" | "restart", unit] => {
                u.insert(format!("is-active {unit}"));
                reply(0, "", "")
            }
            _ => reply(0, "", ""),
        })
    });
    (exec, units)
}

fn run_it(
    exec: &mut FakeExec,
    server: &HttpServer,
    d: &Dep,
    mode: Mode,
) -> (Result<bool, MdmError>, String) {
    let mut out = Vec::new();
    let got = run(exec, &config_at(server), d, mode, &mut out);
    (got, String::from_utf8_lossy(&out).into_owned())
}

fn err_of(got: Result<bool, MdmError>) -> (u8, String) {
    got.err()
        .map(|e| (e.exit_code(), e.to_string()))
        .unwrap_or_default()
}

/// Catches a dry run that has NanoDEP make a key pair, an apply that writes anything but
/// the certificate it answered (or writes it where others can change it), a re-run
/// that makes a new pair (the one uploaded in ABM would no longer match), and a file
/// at the path that is not a certificate overwritten.
#[test]
fn get_cert_writes_the_answered_certificate_once() {
    let server = depserver(UUID);
    let d = Dep::GetCert { out: p(OUT) };
    let mut exec = host();
    let (got, out) = run_it(&mut exec, &server, &d, Mode::DryRun);
    assert_eq!(got.ok(), Some(true));
    assert!(server.requests().is_empty() && exec.log.is_empty());
    assert!(out.contains("DRY-RUN: GET http://127.0.0.1:"), "{out}");
    assert!(
        out.contains(&format!("DRY-RUN: write {OUT} (0644")),
        "{out}"
    );

    let (got, out) = run_it(&mut exec, &server, &d, Mode::Apply);
    assert_eq!(got.ok(), Some(true), "{out}");
    let r = server.requests();
    assert_eq!(r.len(), 1);
    assert_eq!(
        r[0].header("authorization"),
        Some("Basic ZGVwc2VydmVyOmFiYzEyMw==")
    );
    let Some(Entry::File {
        perms,
        owner,
        bytes,
    }) = exec.entries.get(&p(OUT)).cloned()
    else {
        panic!("no certificate written: {out}");
    };
    assert_eq!((perms, owner.uid), (Perms::new(0o644), 0));
    assert!(is_certificate(&bytes));
    assert!(out.contains("Upload Public Key"), "{out}");

    let (got, out) = run_it(&mut exec, &server, &d, Mode::Apply);
    assert_eq!(got.ok(), Some(true));
    assert_eq!(server.requests().len(), 1, "{out}");
    assert!(out.contains("holds a certificate, kept"), "{out}");

    exec.entries.insert(p(OUT), file(b"notes", 0o644, 501));
    let (code, err) = err_of(run_it(&mut exec, &server, &d, Mode::Apply).0);
    assert!(
        code == 2 && err.contains("is there and is not a PEM certificate"),
        "{err}"
    );
    assert_eq!(server.requests().len(), 1);
}

/// Catches a certificate NanoDEP did not answer written as if it were one, and an
/// output path inside a git work tree accepted.
#[test]
fn get_cert_refuses_a_bad_answer_and_a_path_in_a_repository() {
    let server = http_server(|_| http("200 OK", b"<html>login</html>"));
    let d = Dep::GetCert { out: p(OUT) };
    let mut exec = host();
    let (code, err) = err_of(run_it(&mut exec, &server, &d, Mode::Apply).0);
    assert!(code == 1 && err.contains("not a PEM certificate"), "{err}");
    assert!(exec.log.is_empty());
    exec.entries
        .insert(p("/srv/abm/.git"), file(b"", 0o644, 501));
    let (code, err) = err_of(run_it(&mut exec, &server, &d, Mode::DryRun).0);
    assert!(code == 2 && err.contains("inside a git work tree"), "{err}");
}

/// Catches a token upload that sends anything but the token's bytes, skips the account
/// check that proves NanoDEP holds a working token, or runs without `--apply`.
#[test]
fn upload_token_sends_the_token_then_shows_the_account() {
    let server = depserver(UUID);
    let d = Dep::UploadToken { token: p(TOKEN) };
    let mut exec = host();
    let (code, err) = err_of(run_it(&mut exec, &server, &d, Mode::DryRun).0);
    assert!(
        code == 2 && err.contains("/srv/abm/token.p7m is not a file"),
        "{err}"
    );
    exec.entries
        .insert(p(TOKEN), file(b"SMIME token", 0o600, 501));
    let (got, out) = run_it(&mut exec, &server, &d, Mode::DryRun);
    assert_eq!(got.ok(), Some(true));
    assert!(server.requests().is_empty(), "{out}");
    assert!(
        out.contains("DRY-RUN: PUT ") && out.contains("DRY-RUN: GET "),
        "{out}"
    );

    let (got, out) = run_it(&mut exec, &server, &d, Mode::Apply);
    assert_eq!(got.ok(), Some(true), "{out}");
    let r = server.requests();
    let calls: Vec<(&str, &str)> = r
        .iter()
        .map(|r| (r.method.as_str(), r.path.as_str()))
        .collect();
    assert_eq!(
        calls,
        [
            ("PUT", "/v1/tokenpki/example"),
            ("GET", "/proxy/example/account")
        ]
    );
    assert_eq!(r[0].body, b"SMIME token");
    assert!(
        out.contains("ABM: org Example Org, MDM server kbf farm"),
        "{out}"
    );
}

/// Catches a profile definition that sends Apple anything but the checked profile, a
/// uuid not handed to the assigner, a record that does not match what was sent (so a
/// re-run defines a second profile), depsyncer left off, and a dry run that calls.
#[test]
fn define_profile_defines_assigns_records_and_a_rerun_does_nothing() {
    let server = depserver(UUID);
    let (mut exec, units) = host_with_units();
    let (got, out) = run_it(&mut exec, &server, &Dep::DefineProfile, Mode::DryRun);
    assert_eq!(got.ok(), Some(true), "{out}");
    assert!(server.requests().is_empty() && exec.log.is_empty());
    assert!(
        out.contains("DRY-RUN: POST ") && out.contains("\"is_mdm_removable\": false"),
        "{out}"
    );
    assert!(
        out.contains("DRY-RUN: run /usr/bin/systemctl enable kbf-mdm-depsyncer.service"),
        "{out}"
    );

    let (got, out) = run_it(&mut exec, &server, &Dep::DefineProfile, Mode::Apply);
    assert_eq!(got.ok(), Some(true), "{out}");
    let r = server.requests();
    let sent = AdeProfile::render(&config_at(&server))
        .expect("profile")
        .to_json();
    assert_eq!(
        (r[0].method.as_str(), r[0].path.as_str()),
        ("POST", "/proxy/example/profile")
    );
    assert_eq!(r[0].body, sent);
    assert_eq!(
        r[0].header("content-type"),
        Some("application/json;charset=UTF8")
    );
    assert_eq!(
        r[1].path,
        format!("/v1/assigner/example?profile_uuid={UUID}")
    );
    let uuid = exec.entries.get(&p("/var/lib/kbf-mdm/dep-profile-uuid"));
    assert_eq!(uuid, Some(&file(format!("{UUID}\n").as_bytes(), 0o644, 0)));
    let sum = format!("{}\n", hex::encode(Sha256::digest(&sent)));
    let recorded = exec.entries.get(&p("/var/lib/kbf-mdm/dep-profile.sha256"));
    assert_eq!(recorded, Some(&file(sum.as_bytes(), 0o644, 0)));
    assert!(
        units
            .borrow()
            .contains("is-active kbf-mdm-depsyncer.service")
    );

    let steps = exec.log.len();
    let (got, out) = run_it(&mut exec, &server, &Dep::DefineProfile, Mode::Apply);
    assert_eq!(got.ok(), Some(true));
    assert_eq!(server.requests().len(), 2, "{out}");
    assert_eq!(exec.log.len(), steps);
    assert!(
        out.contains(&format!("ADE profile: unchanged (uuid {UUID})")),
        "{out}"
    );
    assert!(out.ends_with("nothing to do\n"), "{out}");
}

/// Catches an answer without a usable profile uuid taken as a defined profile: the
/// assigner would be given garbage and the record would stop a retry.
#[test]
fn define_profile_refuses_an_answer_without_a_profile_uuid() {
    let server = depserver("not-hex");
    let (mut exec, _) = host_with_units();
    let (code, err) = err_of(run_it(&mut exec, &server, &Dep::DefineProfile, Mode::Apply).0);
    assert!(
        code == 1 && err.contains("no profile_uuid of 16 to 64 hex digits"),
        "{err}"
    );
    assert_eq!(server.requests().len(), 1);
    assert!(exec.log.is_empty());
}

/// Catches an assignment of no recorded profile, a request that names other devices or
/// another profile, and a device Apple did not assign reported as done.
#[test]
fn assign_sends_the_recorded_profile_and_fails_unless_every_device_succeeds() {
    let server = depserver(UUID);
    let serials = ["C02AB12CD3EF", "C02ZZ99ZZ9ZZ"].map(|s| Serial::new(s).expect("serial"));
    let d = Dep::Assign {
        serials: serials.to_vec(),
    };
    let mut exec = host();
    let (code, err) = err_of(run_it(&mut exec, &server, &d, Mode::Apply).0);
    assert!(code == 2 && err.contains("no ADE profile yet"), "{err}");
    exec.entries.insert(
        p("/var/lib/kbf-mdm/dep-profile-uuid"),
        file(format!("{UUID}\n").as_bytes(), 0o644, 0),
    );
    let (got, out) = run_it(&mut exec, &server, &d, Mode::DryRun);
    assert_eq!(got.ok(), Some(true));
    assert!(server.requests().is_empty(), "{out}");

    let (got, out) = run_it(&mut exec, &server, &d, Mode::Apply);
    assert_eq!(got.ok(), Some(false), "{out}");
    let body: serde_json::Value = serde_json::from_slice(&server.requests()[0].body).expect("json");
    assert_eq!(
        body,
        json!({"profile_uuid": UUID, "devices": ["C02AB12CD3EF", "C02ZZ99ZZ9ZZ"]})
    );
    assert!(
        out.contains("C02AB12CD3EF: SUCCESS\nC02ZZ99ZZ9ZZ: NOT_ACCESSIBLE\n"),
        "{out}"
    );

    let one = Dep::Assign {
        serials: serials[..1].to_vec(),
    };
    assert_eq!(
        run_it(&mut exec, &server, &one, Mode::Apply).0.ok(),
        Some(true)
    );
}

/// Catches a serial check that lets through what ABM never lists (lower case, too short
/// or long, punctuation that would change the request), and a verb run as a user.
#[test]
fn serials_are_checked_and_dep_runs_only_as_root() {
    assert!(Serial::new("C02AB12CD3EF").is_ok());
    for bad in [
        "c02ab12cd3ef",
        "C02AB12",
        "C02AB12CD3EF123",
        "C02AB12-D3EF",
        "",
    ] {
        assert!(Serial::new(bad).is_err(), "{bad}");
    }
    let server = depserver(UUID);
    let mut exec = host();
    exec.euid = 501;
    let got = run_it(&mut exec, &server, &Dep::DefineProfile, Mode::DryRun).0;
    assert!(matches!(got, Err(MdmError::NeedsRoot { euid: 501, .. })));
}
