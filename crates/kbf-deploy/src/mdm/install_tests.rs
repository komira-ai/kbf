use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use super::*;
use crate::exec::Entry;
use crate::mdm::archive::tests::{tar_gz_of, zip_of};
use crate::mdm::pins::{Packing, Tool};
use crate::mdm::tests::{config, lan};
use crate::step::Step;
use crate::testkit::{FakeExec, reply};
use crate::user::{GETENT, ID};

const BIN_DIR: &str = "/srv/archives";
const UID: u32 = 990;
const STAMP: &str = "1700000000";

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

/// What the fake host's tools see.
struct World {
    user: bool,
    enabled: BTreeSet<String>,
    active: BTreeSet<String>,
    /// `ip -o -4 addr show`.
    addrs: String,
    /// `ss -Hltn`.
    listening: String,
}

/// The bytes of `tool`'s binary `name` in the test archives.
fn binary_of(name: &str) -> Vec<u8> {
    format!("{name} v1").into_bytes()
}

/// Each tool's test archive: its file name, packing and bytes, holding its members
/// and a README; `skip` leaves one member out.
fn archive(tool: Tool, skip: Option<&str>) -> (String, Packing, Vec<u8>) {
    let members = tool.members("1.0");
    let mut files: Vec<(&str, Vec<u8>)> = vec![("README.md", b"docs".to_vec())];
    for (member, name) in &members {
        if Some(*name) != skip {
            files.push((member.as_str(), binary_of(name)));
        }
    }
    let files: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (*n, b.as_slice())).collect();
    match tool {
        Tool::Caddy | Tool::Lego => (format!("{tool}.tar.gz"), Packing::TarGz, tar_gz_of(&files)),
        _ => (format!("{tool}.zip"), Packing::Zip, zip_of(&files)),
    }
}

/// Pins for the test archives.
fn pins() -> Pins {
    let text: String = Tool::ALL
        .into_iter()
        .map(|t| {
            let (file, _, bytes) = archive(t, None);
            let sum = hex::encode(Sha256::digest(&bytes));
            format!("{t} 1.0 https://example.com/r/v1.0/{file} {sum}\n")
        })
        .collect();
    Pins::parse(&text).expect("valid pins")
}

fn file(bytes: &[u8], perms: u32, uid: u32) -> Entry {
    Entry::File {
        perms: Perms::new(perms),
        owner: Owner { uid, gid: uid },
        bytes: bytes.to_vec(),
    }
}

/// A fresh host that passes every check: root, the user exists, an interface carries
/// the bind address, nothing listens on the kit's ports, and the archives are in
/// `BIN_DIR`. Nothing of the kit is installed.
fn host() -> (FakeExec, Rc<RefCell<World>>) {
    let mut exec = FakeExec::new(0);
    let world = Rc::new(RefCell::new(World {
        user: true,
        enabled: BTreeSet::new(),
        active: BTreeSet::new(),
        addrs: format!(
            "1: lo    inet 127.0.0.1/8 scope host lo\n3: wg0    link/none\n2: eth0    inet {}/24 brd {} scope global eth0\n",
            lan(10),
            lan(255)
        ),
        listening: "LISTEN 0 4096 0.0.0.0:22 0.0.0.0:*\n".to_owned(),
    }));
    let w = Rc::clone(&world);
    exec.answer = Box::new(move |cmd| {
        let mut w = w.borrow_mut();
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        Ok(match (cmd.program.as_str(), &args[..]) {
            (GETENT, ["passwd", "--", name]) if w.user => reply(
                0,
                &format!("{name}:x:{UID}:{UID}::/var/lib/kbf-mdm:/usr/sbin/nologin\n"),
                "",
            ),
            (GETENT, _) => reply(2, "", ""),
            (ID, _) => reply(0, &format!("{UID}\n"), ""),
            (IP, _) => reply(0, &w.addrs, ""),
            (SS, _) => reply(0, &w.listening, ""),
            (SYSTEMCTL, ["is-active", "--quiet", u]) => {
                reply(i32::from(!w.active.contains(*u)), "", "")
            }
            (SYSTEMCTL, ["is-enabled", "--quiet", u]) => {
                reply(i32::from(!w.enabled.contains(*u)), "", "")
            }
            (SYSTEMCTL, ["enable", u]) => {
                w.enabled.insert((*u).to_owned());
                reply(0, "", "")
            }
            (SYSTEMCTL, ["restart" | "start", u]) => {
                w.active.insert((*u).to_owned());
                reply(0, "", "")
            }
            // daemon-reload, disable, scepserver.
            _ => reply(0, "", ""),
        })
    });
    for tool in Tool::ALL {
        let (name, _, bytes) = archive(tool, None);
        exec.entries
            .insert(p(&format!("{BIN_DIR}/{name}")), file(&bytes, 0o644, 501));
    }
    (exec, world)
}

fn install() -> Install {
    Install {
        config: config(),
        bin_dir: p(BIN_DIR),
    }
}

fn plan_of(exec: &FakeExec) -> (Result<Plan, MdmError>, String) {
    let mut out = Vec::new();
    let got = plan(exec, &install(), &pins(), &mut out);
    (got, String::from_utf8_lossy(&out).into_owned())
}

/// The steps' lines.
fn lines(plan: &Plan) -> Vec<String> {
    plan.steps().iter().map(ToString::to_string).collect()
}

fn index(lines: &[String], start: &str) -> usize {
    lines
        .iter()
        .position(|l| l.starts_with(start))
        .unwrap_or_else(|| panic!("no step {start:?} in {lines:#?}"))
}

/// Apply `exec`'s plan, as the operator's `--apply` would; then the SCEP CA and its
/// file exist, as `scepserver ca -init` leaves them.
fn apply(exec: &mut FakeExec) {
    let (got, _) = plan_of(exec);
    let planned = got.expect("plans");
    execute(&planned, RunAs::Root, Mode::Apply, exec, &mut Vec::new()).expect("applies");
    exec.entries.insert(
        p("/var/lib/kbf-mdm/scep/depot/ca.pem"),
        file(b"CA", 0o644, UID),
    );
}

/// Catches a fresh install that misses a part, puts it in the wrong order (the CA made
/// before scepserver is installed, a service started before its unit and secret), or
/// starts the proxy without a certificate; and secrets printed or on an argv.
#[test]
fn a_fresh_host_gets_every_part_in_order() {
    let (exec, _) = host();
    let (got, text) = plan_of(&exec);
    let plan = got.expect("plans");
    let l = lines(&plan);
    let mine = format!("owner {UID}:{UID}");
    for want in [
        format!("mkdir /var/lib/kbf-mdm (0700, {mine})"),
        format!("mkdir /var/lib/kbf-mdm/scep/depot (0700, {mine})"),
        format!("mkdir /etc/kbf-mdm (0750, owner 0:{UID})"),
        "mkdir /opt/kbf-mdm (0755)".to_owned(),
        "mkdir /opt/kbf-mdm/bin (0755, owner 0:0)".to_owned(),
        "write /opt/kbf-mdm/bin/depsyncer (0755, owner 0:0, 12 bytes)".to_owned(),
        "write /etc/kbf-mdm/scep.env (0600, owner 0:0, 89 bytes)".to_owned(),
        "write /etc/systemd/system/kbf-mdm.slice (0644, owner 0:0".to_owned(),
        format!("write /etc/kbf-mdm/Caddyfile (0640, owner 0:{UID}"),
        "run /usr/bin/systemctl daemon-reload".to_owned(),
        "run /usr/bin/systemctl enable kbf-mdm-nanohub.service".to_owned(),
        "run /usr/bin/systemctl restart kbf-mdm-depserver.service".to_owned(),
        "wait up to 20s for http://127.0.0.1:9004/version".to_owned(),
        "wait up to 20s for http://127.0.0.1:9005/scep?operation=GetCACert".to_owned(),
    ] {
        index(&l, &want);
    }
    let ca = index(&l, "run (as kbf-mdm) /opt/kbf-mdm/bin/scepserver ca -init");
    assert!(index(&l, "write /opt/kbf-mdm/bin/scepserver ") < ca);
    assert!(
        index(&l, "write /etc/kbf-mdm/nanohub.env")
            < index(&l, "write /etc/systemd/system/kbf-mdm-nanohub")
    );
    assert!(
        index(&l, "run /usr/bin/systemctl daemon-reload")
            < index(&l, "run /usr/bin/systemctl enable")
    );
    assert!(
        l[ca].contains("-organization Example -organizational_unit kbf-mdm"),
        "{}",
        l[ca]
    );
    let proxy = |s: &&String| s.contains("systemctl") && s.contains("proxy");
    assert!(!l.iter().any(|s| proxy(&s)), "{l:#?}");
    assert!(text.contains("proxy: not started; it waits for the TLS certificate /var/lib/kbf-mdm/lego/certificates/mdm.example.com.crt"));
    assert!(
        text.contains("    | ExecStart=/opt/kbf-mdm/bin/nanohub -listen 127.0.0.1:9004"),
        "{text}"
    );
    for step in plan.steps() {
        if let Step::WriteFile { path, bytes, .. } = step
            && path.as_str().ends_with(".env")
        {
            let line = String::from_utf8_lossy(bytes).into_owned();
            let (key, value) = line.trim_end().split_once('=').expect("KEY=VALUE");
            assert_eq!(value.len(), 64, "{key}");
            assert!(value.chars().all(|c| c.is_ascii_hexdigit()), "{key}");
            assert!(
                !text.contains(value) && !l.iter().any(|s| s.contains(value)),
                "{key} shown"
            );
        }
    }
}

/// Catches an install that plans again after it was applied (each re-run would restart
/// the MDM), and two secrets made from the same random bytes.
#[test]
fn a_rerun_after_an_apply_plans_nothing() {
    let (mut exec, _) = host();
    apply(&mut exec);
    let keys: BTreeSet<Vec<u8>> = SECRETS
        .iter()
        .map(
            |(f, _)| match &exec.entries[&p(&format!("/etc/kbf-mdm/{f}"))] {
                Entry::File { bytes, .. } => bytes[bytes.len() - 65..].to_vec(),
                other => panic!("{f}: {other:?}"),
            },
        )
        .collect();
    assert_eq!(keys.len(), 3);
    let (got, text) = plan_of(&exec);
    assert_eq!(lines(&got.expect("plans")), Vec::<String>::new());
    assert!(
        text.contains("secret SCEP_CHALLENGE_PASSWORD: kept (/etc/kbf-mdm/scep.env)"),
        "{text}"
    );
    assert!(text.contains("SCEP CA: kept"), "{text}");
    assert!(text.contains("/opt/kbf-mdm/bin/caddy: unchanged"), "{text}");
}

/// Catches a new binary installed without its service restarted, the old one not kept
/// for a rollback, or an unrelated service restarted with it.
#[test]
fn a_new_binary_keeps_the_old_one_and_restarts_only_its_service() {
    let (mut exec, _) = host();
    apply(&mut exec);
    exec.entries.insert(
        p("/opt/kbf-mdm/bin/depserver"),
        file(b"depserver v0", 0o755, 0),
    );
    let (got, _) = plan_of(&exec);
    let l = lines(&got.expect("plans"));
    assert_eq!(
        l,
        [
            "write /opt/kbf-mdm/bin/depserver.previous (0755, owner 0:0, 12 bytes)",
            "write /opt/kbf-mdm/bin/depserver (0755, owner 0:0, 12 bytes)",
            "run /usr/bin/systemctl restart kbf-mdm-depserver.service",
            "wait up to 20s for http://127.0.0.1:9004/version",
            "wait up to 20s for http://127.0.0.1:9005/scep?operation=GetCACert",
        ]
    );
}

/// Catches the proxy started without a certificate, not restarted when its
/// Caddyfile changes, or not started when it is down.
#[test]
fn the_proxy_starts_once_the_certificate_is_there() {
    let (mut exec, world) = host();
    apply(&mut exec);
    let cert = p("/var/lib/kbf-mdm/lego/certificates/mdm.example.com.crt");
    exec.entries.insert(cert, file(b"PEM", 0o600, UID));
    let (got, _) = plan_of(&exec);
    let l = lines(&got.expect("plans"));
    assert_eq!(
        l,
        [
            "run /usr/bin/systemctl enable kbf-mdm-proxy.service",
            "run /usr/bin/systemctl start kbf-mdm-proxy.service",
        ]
    );
    world.borrow_mut().enabled.insert(PROXY.to_owned());
    world.borrow_mut().active.insert(PROXY.to_owned());
    exec.entries
        .insert(p("/etc/kbf-mdm/Caddyfile"), file(b"old", 0o640, 0));
    let (got, _) = plan_of(&exec);
    let l = lines(&got.expect("plans"));
    assert_eq!(l.len(), 2, "{l:#?}");
    assert!(l[0].starts_with("write /etc/kbf-mdm/Caddyfile"), "{l:#?}");
    assert_eq!(l[1], "run /usr/bin/systemctl restart kbf-mdm-proxy.service");
}

/// Catches a secret file kept although others can read it or it is not root's, and one
/// without its key kept (the service would start without it).
#[test]
fn a_secret_must_be_root_s_and_0600_and_hold_its_key() {
    let (mut exec, _) = host();
    let path = p("/etc/kbf-mdm/nanohub.env");
    exec.entries
        .insert(path.clone(), file(b"NANOHUB_API_KEY=k\n", 0o644, 0));
    let (got, _) = plan_of(&exec);
    let err = got.err().map(|e| e.to_string()).unwrap_or_default();
    assert_eq!(
        err,
        "/etc/kbf-mdm/nanohub.env has mode 0644, not 0600: chmod 600 /etc/kbf-mdm/nanohub.env"
    );
    exec.entries
        .insert(path.clone(), file(b"NANOHUB_API_KEY=k\n", 0o600, 501));
    let (got, _) = plan_of(&exec);
    assert!(
        matches!(got, Err(MdmError::SecretOwner { uid: 501, .. })),
        "{got:?}"
    );
    exec.entries
        .insert(path, file(b"NANOHUB_API_KEY=\nOTHER=x\n", 0o600, 0));
    let (got, _) = plan_of(&exec);
    let l = lines(&got.expect("plans"));
    index(
        &l,
        "write /etc/kbf-mdm/nanohub.env (0600, owner 0:0, 81 bytes)",
    );
}

/// Each row is one host the old kit refused before changing anything: the plan must
/// refuse it, naming the fault.
#[test]
fn a_host_that_fails_a_check_is_refused_before_any_step() {
    type Plant = fn(&mut FakeExec, &mut World);
    let cases: [(Plant, &str); 9] = [
        (
            |e, _| e.euid = 501,
            "run it with sudo, even for a dry run (effective uid is 501)",
        ),
        (
            |_, w| w.addrs = "1: lo    inet 127.0.0.1/8 scope host lo\n".to_owned(),
            "no interface carries MDM_BIND=",
        ),
        (
            |_, w| {
                w.listening
                    .push_str("LISTEN 0 4096 127.0.0.1:9005 0.0.0.0:*\n")
            },
            "port 9005 is held by another process",
        ),
        (
            |_, w| w.user = false,
            "no user kbf-mdm: useradd --system --user-group --home-dir /var/lib/kbf-mdm",
        ),
        (
            |e, _| drop(e.entries.remove(&p("/srv/archives/lego.tar.gz"))),
            "/srv/archives/lego.tar.gz is missing (kbf-deploy mdm fetch --apply downloads it)",
        ),
        (
            |e, _| {
                drop(
                    e.entries
                        .insert(p("/srv/archives/lego.tar.gz"), file(b"x", 0o644, 0)),
                )
            },
            "/srv/archives/lego.tar.gz: sha256 2d711642",
        ),
        (
            |e, _| {
                drop(e.entries.insert(
                    p("/srv/archives/lego.tar.gz"),
                    Entry::Dir {
                        perms: Perms::new(0o755),
                        owner: Owner { uid: 0, gid: 0 },
                    },
                ))
            },
            "/srv/archives/lego.tar.gz is missing",
        ),
        (
            |e, _| {
                drop(e.entries.insert(
                    p("/etc/kbf-mdm/scep.env"),
                    file(b"SCEP_CHALLENGE_PASSWORD=x\n", 0o640, 0),
                ))
            },
            "/etc/kbf-mdm/scep.env has mode 0640",
        ),
        (
            |_, w| {
                w.listening
                    .push_str(&format!("LISTEN 0 4096 {}:443 0.0.0.0:*\n", lan(10)))
            },
            "port 443 is held by another process",
        ),
    ];
    for (plant, want) in cases {
        let (mut exec, world) = host();
        plant(&mut exec, &mut world.borrow_mut());
        let (got, _) = plan_of(&exec);
        let err = got.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains(want), "{err:?} lacks {want:?}");
    }
}

/// Catches a port held by the kit's own running service refused (no re-run could pass),
/// and an archive that lacks a member installed as an empty binary.
#[test]
fn a_port_the_kit_holds_is_a_rerun_and_a_member_must_be_there() {
    let (mut exec, world) = host();
    {
        let mut w = world.borrow_mut();
        w.listening
            .push_str("LISTEN 0 4096 127.0.0.1:9004 0.0.0.0:*\n");
        w.active.insert(NANOHUB.to_owned());
    }
    let (got, text) = plan_of(&exec);
    assert!(got.is_ok(), "{got:?}");
    assert!(
        text.contains("port 9004: held by kbf-mdm-nanohub.service (a re-run)"),
        "{text}"
    );
    let (name, _, bytes) = archive(Tool::Nanodep, Some("depsyncer"));
    exec.entries
        .insert(p(&format!("{BIN_DIR}/{name}")), file(&bytes, 0o644, 501));
    let sum = hex::encode(Sha256::digest(&bytes));
    let text: String = pins()
        .all()
        .iter()
        .map(|pin| {
            let hash = if pin.tool == Tool::Nanodep {
                sum.clone()
            } else {
                pin.sha256_hex()
            };
            format!("{} 1.0 {} {hash}\n", pin.tool, pin.url)
        })
        .collect();
    let pins = Pins::parse(&text).expect("valid");
    let got = plan(&exec, &install(), &pins, &mut Vec::new());
    let err = got.err().map(|e| e.to_string()).unwrap_or_default();
    assert_eq!(
        err,
        "nanodep.zip has no nanodep-linux-amd64-v1.0/depsyncer-linux-amd64, or it is empty"
    );
}

/// Catches a rollback that deletes the unit files instead of moving them aside, leaves
/// a unit running, stops the slice or a job by name (systemd refuses that), or plans
/// again once done; and one run as anyone but root.
#[test]
fn uninstall_stops_the_services_and_moves_the_units_aside_once() {
    let (mut exec, _) = host();
    apply(&mut exec);
    exec.entries.insert(
        p("/etc/systemd/system/kbf-mdm-cert.timer"),
        file(b"[Timer]\n", 0o644, 0),
    );
    exec.entries.insert(
        p("/etc/systemd/system/kbf-mdm-cert.service"),
        file(b"[Service]\n", 0o644, 0),
    );
    let c = config();
    let mut out = Vec::new();
    let plan = plan_uninstall(&exec, &c, STAMP, &mut out).expect("plans");
    let l = lines(&plan);
    assert_eq!(
        l[0],
        "run /usr/bin/systemctl disable --now kbf-mdm-scep.service kbf-mdm-nanohub.service \
         kbf-mdm-depserver.service kbf-mdm-depsyncer.service kbf-mdm-proxy.service kbf-mdm-cert.timer"
    );
    index(&l, "mkdir /var/lib/kbf-mdm/removed (0755)");
    index(
        &l,
        "write /var/lib/kbf-mdm/removed/1700000000/kbf-mdm.slice (0644, owner 0:0",
    );
    index(&l, "remove /etc/systemd/system/kbf-mdm-cert.service");
    assert_eq!(
        l.last().map(String::as_str),
        Some("run /usr/bin/systemctl daemon-reload")
    );
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(text.contains("kept: /var/lib/kbf-mdm (database"), "{text}");
    execute(&plan, RunAs::Root, Mode::Apply, &mut exec, &mut Vec::new()).expect("applies");
    let kept = &exec.entries[&p("/var/lib/kbf-mdm/removed/1700000000/kbf-mdm-proxy.service")];
    assert!(matches!(kept, Entry::File { bytes, .. } if bytes.starts_with(b"# kbf-mdm-proxy")));
    let mut out = Vec::new();
    let again = plan_uninstall(&exec, &c, STAMP, &mut out).expect("plans");
    assert!(again.is_empty());
    assert!(String::from_utf8_lossy(&out).contains("no kbf-mdm unit is installed"));
    exec.euid = 501;
    let err = plan_uninstall(&exec, &c, STAMP, &mut Vec::new()).err();
    assert!(
        matches!(err, Some(MdmError::InstallNeedsRoot { euid: 501 })),
        "{err:?}"
    );
}

/// Catches `run` printing a plan it did not apply, or applying without the root guard.
#[test]
fn run_prints_and_applies_through_the_guards() {
    let (mut exec, _) = host();
    let mut out = Vec::new();
    run(&mut exec, &install(), &pins(), Mode::DryRun, &mut out).expect("dry run");
    assert!(exec.log.is_empty());
    assert!(String::from_utf8_lossy(&out).contains("DRY-RUN: write /opt/kbf-mdm/bin/nanohub"));
    run(&mut exec, &install(), &pins(), Mode::Apply, &mut Vec::new()).expect("applies");
    assert!(!exec.log.is_empty());
    let n = exec.log.len();
    run_uninstall(&mut exec, &config(), STAMP, Mode::Apply, &mut Vec::new()).expect("rolls back");
    assert!(exec.log.len() > n);
    assert!(has_value(b"A=\nB=1\n", "B") && !has_value(b"A=\nB=1\n", "A"));
}

/// Each row changes one input of an installed MDM; the plan must restart exactly the
/// services that read it. Catches a change that restarts nothing (the old settings run
/// on) and one that restarts every service (an outage for nothing).
#[test]
fn each_change_restarts_exactly_the_services_it_touches() {
    type Change = fn(&mut FakeExec);
    fn put(e: &mut FakeExec, path: &str, bytes: &[u8], perms: u32) {
        e.entries.insert(p(path), file(bytes, perms, 0));
    }
    let cases: [(Change, &[&str]); 6] = [
        (
            |e| put(e, "/etc/systemd/system/kbf-mdm.slice", b"old", 0o644),
            &[SCEP, NANOHUB, DEPSERVER, PROXY],
        ),
        (
            |e| {
                put(
                    e,
                    "/etc/systemd/system/kbf-mdm-nanohub.service",
                    b"old",
                    0o644,
                )
            },
            &[NANOHUB],
        ),
        (
            |e| drop(e.entries.remove(&p("/etc/kbf-mdm/scep.env"))),
            &[SCEP],
        ),
        (
            |e| put(e, "/opt/kbf-mdm/bin/scepserver", b"old", 0o755),
            &[SCEP],
        ),
        (
            |e| {
                put(
                    e,
                    "/etc/systemd/system/kbf-mdm-proxy.service",
                    b"old",
                    0o644,
                )
            },
            &[PROXY],
        ),
        (
            |e| put(e, "/opt/kbf-mdm/bin/caddy", b"old", 0o755),
            &[PROXY],
        ),
    ];
    for (change, want) in cases {
        let (mut exec, world) = host();
        let cert = p("/var/lib/kbf-mdm/lego/certificates/mdm.example.com.crt");
        exec.entries.insert(cert, file(b"PEM", 0o600, UID));
        apply(&mut exec);
        assert!(world.borrow().active.contains(PROXY));
        change(&mut exec);
        let (got, _) = plan_of(&exec);
        let restarted: Vec<String> = lines(&got.expect("plans"))
            .iter()
            .filter_map(|l| {
                l.strip_prefix("run /usr/bin/systemctl restart ")
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(restarted, want, "{want:?}");
    }
}
