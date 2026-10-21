use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;

use super::*;
use crate::config::Config;
use crate::pilot::config::KEYS;
use kbf_host::Step;
use kbf_host::exec::Entry;
use kbf_host::testkit::{FakeExec, RUNNER, ScratchHost, reply};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};

const EXAMPLE: &str = include_str!("../../templates/pilot/config.example");
const UID: u32 = 1000;
const BIN: &str = "/opt/new/kbf-server";
const PKI: &str = "/srv/op/.config/kbf-pilot/pki";
const S3: &str = "/srv/op/.config/kbf-pilot/s3.env";
const STATE: &str = "/srv/op/.local/share/kbf-pilot";
const UNITS: &str = "/srv/op/.config/systemd/user";
const X86: Platform = Platform {
    os: "linux",
    arch: "x86_64",
};

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("a test path")
}

fn settings() -> ServerSettings {
    let c = Config::parse(EXAMPLE, KEYS).expect("valid");
    ServerSettings::from_config(&c, None).expect("valid")
}

fn home() -> Home {
    Home {
        home: p("/srv/op"),
        xdg_config: None,
    }
}

fn install() -> Install {
    Install {
        settings: settings(),
        bin: p(BIN),
        pki: None,
        s3_env: None,
        home: home(),
    }
}

/// An ELF header for `machine` (62 x86-64, 183 arm64), padded to 64 bytes.
fn elf(machine: u16) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[..6].copy_from_slice(b"\x7fELF\x02\x01");
    b[18..20].copy_from_slice(&machine.to_le_bytes());
    b
}

/// A CA, and a server certificate it signs for `name`; PEM texts.
fn pki(name: &str) -> (String, String, String) {
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("ca params");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("ca");
    let key = KeyPair::generate().expect("key");
    let cert = CertificateParams::new(vec![name.to_owned()])
        .expect("params")
        .signed_by(&key, &ca)
        .expect("cert");
    (ca.pem(), cert.pem(), key.serialize_pem())
}

/// What the fake host's tools answer.
struct World {
    active: bool,
    installed: bool,
    carried: bool,
    listening: String,
    help: String,
    version_fails: bool,
    /// systemctl --user calls that changed something, in order.
    calls: Vec<String>,
}

fn help() -> String {
    "--role --cold-store --meta-dir \
     --listen --worker-listen --worker-tls-cert --worker-tls-key --worker-client-ca"
        .to_owned()
}

fn answer(w: &Rc<RefCell<World>>) -> kbf_host::testkit::Answer {
    let w = Rc::clone(w);
    Box::new(move |cmd| {
        let mut w = w.borrow_mut();
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        Ok(match (cmd.program.as_str(), &args[..]) {
            (IP, ["-o", "-4", "addr", "show", "dev", "tailscale0"]) if w.carried => reply(
                0,
                "4: tailscale0    inet 192.0.2.10/32 scope global tailscale0\\       valid_lft forever\n",
                "",
            ),
            (IP, _) => reply(1, "", "Device \"tailscale0\" does not exist.\n"),
            (SS, ["-Hltn"]) => reply(0, &w.listening, ""),
            (ID, ["-un"]) => reply(0, "op\n", ""),
            (SYSTEMCTL, ["--user", "is-active", "--quiet", SERVICE]) => {
                reply(i32::from(!w.active), "", "")
            }
            (SYSTEMCTL, ["--user", "cat", SERVICE]) => reply(i32::from(!w.installed), "", ""),
            (SYSTEMCTL, ["--user", rest @ ..]) => {
                w.calls.push(rest.join(" "));
                match rest {
                    ["restart", _] => w.active = true,
                    ["disable", "--now", _] => (w.active, w.installed) = (false, false),
                    _ => {}
                }
                reply(0, "", "")
            }
            (BIN, ["--version"]) => reply(i32::from(w.version_fails), "kbf-server 0.1.0\n", "boom"),
            (BIN, ["--help"]) => reply(0, &w.help, ""),
            (other, _) => panic!("unexpected tool {other} {args:?}"),
        })
    })
}

fn world() -> Rc<RefCell<World>> {
    Rc::new(RefCell::new(World {
        active: false,
        installed: false,
        carried: true,
        listening: "LISTEN 0 4096 0.0.0.0:22 0.0.0.0:*\n".to_owned(),
        help: help(),
        version_fails: false,
        calls: Vec::new(),
    }))
}

fn put(exec: &mut FakeExec, path: &str, mode: u32, bytes: &[u8]) {
    let entry = Entry::File {
        perms: Perms::new(mode),
        owner: RUNNER,
        bytes: bytes.to_vec(),
    };
    exec.entries.insert(p(path), entry);
}

/// A fake host that passes every check, with its world.
fn host() -> (FakeExec, Rc<RefCell<World>>) {
    let w = world();
    let mut exec = FakeExec::new(UID);
    exec.answer = answer(&w);
    let (ca, server, key) = pki("kbf-pilot-server");
    put(&mut exec, &format!("{PKI}/ca.pem"), 0o644, ca.as_bytes());
    put(
        &mut exec,
        &format!("{PKI}/server.pem"),
        0o644,
        server.as_bytes(),
    );
    put(
        &mut exec,
        &format!("{PKI}/server.key"),
        0o600,
        key.as_bytes(),
    );
    put(
        &mut exec,
        S3,
        0o600,
        b"AWS_ACCESS_KEY_ID=kbf\nAWS_SECRET_ACCESS_KEY=s\n",
    );
    put(&mut exec, BIN, 0o755, &elf(62));
    for dir in ["/srv", "/srv/op"] {
        exec.entries.insert(
            p(dir),
            Entry::Dir {
                perms: Perms::new(0o755),
                owner: RUNNER,
            },
        );
    }
    put(&mut exec, "/var/lib/systemd/linger/op", 0o644, b"");
    (exec, w)
}

fn planned(exec: &FakeExec, a: &Install, mode: Mode) -> (Result<Planned, PilotError>, String) {
    let mut out = Vec::new();
    let got = plan(exec, a, X86, mode, &mut out);
    (got, String::from_utf8(out).expect("utf-8"))
}

fn refusal(exec: &FakeExec, a: &Install) -> String {
    planned(exec, a, Mode::DryRun)
        .0
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

fn steps(plan: &Plan) -> Vec<String> {
    plan.steps().iter().map(ToString::to_string).collect()
}

fn exec_start() -> String {
    format!(
        "{STATE}/bin/kbf-server --role all \
         --cold-store s3://kbf-pilot-cas/kbf/?endpoint=http://192.0.2.20:30900&region=us-east-1&conditional-put=true \
         --meta-dir {STATE}/meta --listen 192.0.2.10:8980 --worker-listen 192.0.2.10:8981 \
         --worker-tls-cert {PKI}/server.pem --worker-tls-key {PKI}/server.key \
         --worker-client-ca {PKI}/ca.pem"
    )
}

/// Catches a first install that misses a step (a directory, the binary, a unit, the
/// reload, enable or restart), runs them out of order (a restart before the units
/// are written starts the old ones), makes the state directories readable by
/// others, or renders a unit wrongly: the caps, the S3 key file, the logs and the
/// whole `ExecStart=` line are compared.
#[test]
fn a_first_install_plans_every_step_in_order() {
    let (exec, _) = host();
    let (got, out) = planned(&exec, &install(), Mode::DryRun);
    let planned = got.expect("plans");
    let env = "(env XDG_RUNTIME_DIR)";
    let mut want = vec![
        "mkdir /srv/op/.local (0755)".to_owned(),
        "mkdir /srv/op/.local/share (0755)".to_owned(),
        format!("mkdir {STATE} (0700)"),
        format!("mkdir {STATE}/bin (0700)"),
        format!("mkdir {STATE}/log (0700)"),
        "mkdir /srv/op/.config (0755)".to_owned(),
        "mkdir /srv/op/.config/systemd (0755)".to_owned(),
        format!("mkdir {UNITS} (0755)"),
        format!("write {STATE}/bin/kbf-server (0755, 64 bytes)"),
    ];
    let slice = planned.plan.steps().iter().find_map(|s| match s {
        Step::WriteFile { path, bytes, .. } if path.as_str().ends_with(SLICE) => {
            Some(bytes.clone())
        }
        _ => None,
    });
    let service = planned.plan.steps().iter().find_map(|s| match s {
        Step::WriteFile { path, bytes, .. } if path.as_str().ends_with(SERVICE) => {
            Some(bytes.clone())
        }
        _ => None,
    });
    let (slice, service) = (slice.expect("slice"), service.expect("service"));
    want.push(format!(
        "write {UNITS}/{SLICE} (0644, {} bytes)",
        slice.len()
    ));
    want.push(format!(
        "write {UNITS}/{SERVICE} (0644, {} bytes)",
        service.len()
    ));
    for verb in [
        "daemon-reload",
        "enable kbf-pilot-server.service",
        "restart kbf-pilot-server.service",
    ] {
        want.push(format!("run {SYSTEMCTL} --user {verb} {env}"));
    }
    assert_eq!(steps(&planned.plan), want);
    let slice = String::from_utf8(slice).expect("text");
    for line in [
        "MemoryMax=4G",
        "MemorySwapMax=0",
        "CPUQuota=200%",
        "TasksMax=512",
    ] {
        assert!(slice.lines().any(|l| l == line), "{line} in\n{slice}");
    }
    let service = String::from_utf8(service).expect("text");
    let lines = [
        format!("EnvironmentFile={S3}"),
        format!("ExecStart={}", exec_start()),
        "Slice=kbf-pilot.slice".to_owned(),
        format!("StandardOutput=append:{STATE}/log/kbf-server.out.log"),
        format!("StandardError=append:{STATE}/log/kbf-server.err.log"),
    ];
    for line in lines {
        assert!(service.lines().any(|l| l == line), "{line} in\n{service}");
    }
    assert_eq!(
        planned.ready,
        Some(Ready {
            log: p(&format!("{STATE}/log/kbf-server.out.log")),
            err_log: p(&format!("{STATE}/log/kbf-server.err.log")),
            offset: 0,
        })
    );
    for line in [
        "bind: 192.0.2.10 on tailscale0",
        "certificates: server.pem chains to ca.pem (serverAuth, kbf-pilot-server)",
        "store: http://192.0.2.20:30900/minio/health/live answers",
        "binary: kbf-server 0.1.0",
    ] {
        assert!(out.lines().any(|l| l == line), "{line} in\n{out}");
    }
    assert!(
        out.contains(&format!("{UNITS}/{SERVICE}: writes\n")),
        "{out}"
    );
    assert!(
        out.contains(&format!("    | ExecStart={}\n", exec_start())),
        "{out}"
    );
    assert!(!out.contains("WARN"), "{out}");
}

/// Catches systemctl run without the user manager's runtime directory (with an empty
/// environment, `systemctl --user` cannot reach the manager at all) or as a system
/// call (no `--user`).
#[test]
fn systemctl_reaches_the_users_own_manager() {
    let (exec, _) = host();
    let planned = planned(&exec, &install(), Mode::DryRun).0.expect("plans");
    let runs: Vec<&Cmd> = planned
        .plan
        .steps()
        .iter()
        .filter_map(|s| match s {
            Step::Run(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 3);
    for cmd in runs.iter().copied().chain(
        exec.queries
            .borrow()
            .iter()
            .filter(|c| c.program.as_str() == SYSTEMCTL),
    ) {
        assert_eq!(cmd.args[0], "--user", "{cmd}");
        assert_eq!(
            cmd.env,
            [("XDG_RUNTIME_DIR".to_owned(), format!("/run/user/{UID}"))]
        );
    }
}

/// Catches an install that is not idempotent: after an apply (the service then
/// active), a re-run must plan nothing and restart nothing.
#[test]
fn a_rerun_after_an_apply_plans_nothing() {
    let (mut exec, w) = host();
    let a = install();
    let first = planned(&exec, &a, Mode::Apply).0.expect("plans");
    execute(
        &first.plan,
        RunAs::User,
        Mode::Apply,
        &mut exec,
        &mut Vec::new(),
    )
    .expect("applies");
    assert_eq!(
        w.borrow().calls,
        [
            "daemon-reload",
            "enable kbf-pilot-server.service",
            "restart kbf-pilot-server.service"
        ]
    );
    w.borrow_mut()
        .listening
        .push_str("LISTEN 0 4096 192.0.2.10:8980 0.0.0.0:*\n");
    let (again, out) = planned(&exec, &a, Mode::DryRun);
    let again = again.expect("plans");
    assert!(again.plan.is_empty(), "{:?}", steps(&again.plan));
    assert_eq!(again.ready, None);
    assert!(
        out.contains("port 8980: held by kbf-pilot-server.service (a re-run)"),
        "{out}"
    );
    assert!(
        out.contains(&format!("{UNITS}/{SLICE}: unchanged")),
        "{out}"
    );
    assert!(
        out.contains("kbf-pilot-server.service is active and nothing changed"),
        "{out}"
    );
}

/// Catches a stopped service with unchanged files left stopped (a crash would never
/// be undone by a re-run), or reloaded for nothing.
#[test]
fn an_unchanged_but_stopped_service_is_enabled_and_restarted_only() {
    let (mut exec, w) = host();
    let a = install();
    let first = planned(&exec, &a, Mode::Apply).0.expect("plans");
    execute(
        &first.plan,
        RunAs::User,
        Mode::Apply,
        &mut exec,
        &mut Vec::new(),
    )
    .expect("applies");
    w.borrow_mut().active = false;
    let again = planned(&exec, &a, Mode::DryRun).0.expect("plans");
    assert_eq!(
        steps(&again.plan),
        [
            format!("run {SYSTEMCTL} --user enable {SERVICE} (env XDG_RUNTIME_DIR)"),
            format!("run {SYSTEMCTL} --user restart {SERVICE} (env XDG_RUNTIME_DIR)"),
        ]
    );
    assert!(again.ready.is_some());
}

/// Catches a new binary installed without keeping the one it replaces (no way back),
/// a `.previous` made when the binary is the same, and the start line looked for
/// from the start of a log that already holds an old one.
#[test]
fn a_new_binary_keeps_the_old_one_as_previous() {
    let (mut exec, _) = host();
    let old = {
        let mut b = elf(62);
        b.push(1);
        b
    };
    put(&mut exec, &format!("{STATE}/bin/kbf-server"), 0o750, &old);
    put(
        &mut exec,
        &format!("{STATE}/log/kbf-server.out.log"),
        0o600,
        b"kbf-server 0.0.9 reapi=x\n",
    );
    let got = planned(&exec, &install(), Mode::DryRun).0.expect("plans");
    let previous = got.plan.steps().iter().find_map(|s| match s {
        Step::WriteFile {
            path,
            perms,
            bytes,
            owner,
        } if path.as_str().ends_with(".previous") => {
            Some((path.to_string(), *perms, bytes.clone(), *owner))
        }
        _ => None,
    });
    assert_eq!(
        previous,
        Some((
            format!("{STATE}/bin/kbf-server.previous"),
            Perms::new(0o750),
            old,
            None
        ))
    );
    assert_eq!(got.ready.map(|r| r.offset), Some(25));
    put(
        &mut exec,
        &format!("{STATE}/bin/kbf-server"),
        0o755,
        &elf(62),
    );
    let same = planned(&exec, &install(), Mode::DryRun).0.expect("plans");
    let bin = format!("write {STATE}/bin/");
    assert!(
        steps(&same.plan).iter().all(|s| !s.starts_with(&bin)),
        "{:?}",
        steps(&same.plan)
    );
}

/// Catches each refusal of the host checks missing (the unit would start against a
/// port another process holds, an address no interface carries, keys others can
/// read, a certificate the daemons refuse, or a binary that is not kbf-server for
/// this host), each named with its fix.
#[test]
fn each_host_defect_is_refused_before_any_step() {
    type Plant = fn(&mut FakeExec, &RefCell<World>, &mut Install);
    let cases: Vec<(Plant, &str)> = vec![
        (
            |_, w, _| w.borrow_mut().carried = false,
            "tailscale0 does not carry 192.0.2.10 (ip -4 addr show dev tailscale0)",
        ),
        (
            |_, w, _| {
                w.borrow_mut()
                    .listening
                    .push_str("LISTEN 0 4096 0.0.0.0:8981 0.0.0.0:*\n")
            },
            "port 8981 is held by another process (ss -ltnp 'sport = :8981')",
        ),
        (
            |e, _, _| drop(e.entries.remove(&p(&format!("{PKI}/ca.pem")))),
            "/srv/op/.config/kbf-pilot/pki/ca.pem is missing",
        ),
        (
            |e, _, _| drop(e.entries.remove(&p(&format!("{PKI}/server.pem")))),
            "/srv/op/.config/kbf-pilot/pki/server.pem is missing",
        ),
        (
            |e, _, _| drop(e.entries.remove(&p(&format!("{PKI}/server.key")))),
            "/srv/op/.config/kbf-pilot/pki/server.key is missing",
        ),
        (
            |e, _, _| {
                let key = e.entries.remove(&p(&format!("{PKI}/server.key")));
                if let Some(Entry::File { bytes, .. }) = key {
                    put(e, &format!("{PKI}/server.key"), 0o640, &bytes);
                }
            },
            "/srv/op/.config/kbf-pilot/pki/server.key has mode 0640, not 0600: chmod 600 /srv/op/.config/kbf-pilot/pki/server.key",
        ),
        (
            |e, _, _| {
                put(
                    e,
                    S3,
                    0o644,
                    b"AWS_ACCESS_KEY_ID=a\nAWS_SECRET_ACCESS_KEY=b\n",
                )
            },
            "/srv/op/.config/kbf-pilot/s3.env has mode 0644, not 0600: chmod 600 /srv/op/.config/kbf-pilot/s3.env",
        ),
        (
            |e, _, _| {
                put(
                    e,
                    S3,
                    0o600,
                    b"AWS_ACCESS_KEY_ID=a\nAWS_SECRET_ACCESS_KEY=\n",
                )
            },
            "/srv/op/.config/kbf-pilot/s3.env needs AWS_ACCESS_KEY_ID= and AWS_SECRET_ACCESS_KEY= lines, each with a value",
        ),
        (
            |e, _, _| {
                put(
                    e,
                    S3,
                    0o600,
                    b"AWS_ACCESS_KEY_ID=a\nAWS_SECRET_ACCESS_KEY=\"\"\n",
                )
            },
            "/srv/op/.config/kbf-pilot/s3.env needs AWS_ACCESS_KEY_ID= and AWS_SECRET_ACCESS_KEY= lines, each with a value",
        ),
        (
            |e, _, _| {
                put(
                    e,
                    S3,
                    0o600,
                    b"AWS_ACCESS_KEY_ID=''\nAWS_SECRET_ACCESS_KEY=b\n",
                )
            },
            "/srv/op/.config/kbf-pilot/s3.env needs AWS_ACCESS_KEY_ID= and AWS_SECRET_ACCESS_KEY= lines, each with a value",
        ),
        (
            |e, _, _| put(e, S3, 0o600, b"AWS_SECRET_ACCESS_KEY=b\n"),
            "/srv/op/.config/kbf-pilot/s3.env needs AWS_ACCESS_KEY_ID= and AWS_SECRET_ACCESS_KEY= lines, each with a value",
        ),
        (
            |e, _, _| drop(e.entries.remove(&p(S3))),
            "/srv/op/.config/kbf-pilot/s3.env is missing",
        ),
        (
            |e, _, _| put(e, BIN, 0o644, &elf(62)),
            "/opt/new/kbf-server is not an executable file",
        ),
        (
            |e, _, _| drop(e.entries.remove(&p(BIN))),
            "/opt/new/kbf-server is not an executable file",
        ),
        (
            |e, _, _| put(e, BIN, 0o755, &elf(183)),
            "/opt/new/kbf-server is not an ELF executable for x86_64",
        ),
        (
            |e, _, _| put(e, BIN, 0o755, b"#!/bin/sh\n"),
            "/opt/new/kbf-server is not an ELF executable for x86_64",
        ),
        (
            |e, _, _| put(e, BIN, 0o755, &elf(62)[..19]),
            "/opt/new/kbf-server is not an ELF executable for x86_64",
        ),
        (
            |e, _, _| {
                let mut b = elf(62);
                b[4] = 1;
                put(e, BIN, 0o755, &b);
            },
            "/opt/new/kbf-server is not an ELF executable for x86_64",
        ),
        (
            |e, _, _| {
                let mut b = elf(62);
                b[5] = 2;
                put(e, BIN, 0o755, &b);
            },
            "/opt/new/kbf-server is not an ELF executable for x86_64",
        ),
        (
            |e, _, _| {
                let mut b = elf(62);
                b[0] = b'E';
                put(e, BIN, 0o755, &b);
            },
            "/opt/new/kbf-server is not an ELF executable for x86_64",
        ),
        (
            |_, w, _| w.borrow_mut().version_fails = true,
            "/opt/new/kbf-server --version: /opt/new/kbf-server exited with exit status: 1: boom",
        ),
        (
            |_, w, _| {
                let h = help().replace("--worker-client-ca", "");
                w.borrow_mut().help = h;
            },
            "/opt/new/kbf-server has no --worker-client-ca (its --help does not list it), which its unit passes",
        ),
        (
            |_, w, _| {
                let h = help().replace(" --meta-dir", "");
                w.borrow_mut().help = h;
            },
            "/opt/new/kbf-server has no --meta-dir (its --help does not list it), which its unit passes",
        ),
        (
            |e, _, _| {
                let (_, other, _) = pki("kbf-pilot-server");
                put(e, &format!("{PKI}/server.pem"), 0o644, other.as_bytes());
            },
            "/srv/op/.config/kbf-pilot/pki/server.pem: server.pem does not chain to ca.pem",
        ),
        (
            |_, _, a| a.settings.tls_server_name = "other".into(),
            "/srv/op/.config/kbf-pilot/pki/server.pem: server.pem does not chain to ca.pem for serverAuth with the name other",
        ),
        (
            |_, _, a| a.pki = Some(p("/srv/op/my pki")),
            "--pki holds a character the unit cannot carry: \"/srv/op/my pki\"",
        ),
        (
            |_, _, a| a.s3_env = Some(p("/srv/op/s3%.env")),
            "--s3-env-file holds a character the unit cannot carry: \"/srv/op/s3%.env\"",
        ),
        (
            |_, _, a| a.settings.state = Some(p("/srv/a b")),
            "the state directory holds a character the unit cannot carry: \"/srv/a b\"",
        ),
        (
            |_, _, a| a.settings.s3_prefix = "kbf %n/".into(),
            "a kbf-server argument holds a character the unit cannot carry: \"kbf %n/\"",
        ),
    ];
    for (plant, want) in cases {
        let (mut exec, w) = host();
        let mut a = install();
        plant(&mut exec, &w, &mut a);
        let got = refusal(&exec, &a);
        assert!(got.starts_with(want), "want {want}\n got {got}");
        assert!(exec.log.is_empty());
    }
}

/// Catches keys accepted inside a git work tree (one `git add` from a commit): a
/// `.git` in the PKI directory itself, above it, or above the S3 file alone, each
/// planted alone so a check of only one of the two paths goes red.
#[test]
fn keys_inside_a_git_work_tree_are_refused() {
    let cases = [
        (format!("{PKI}/.git"), None, PKI.to_owned()),
        ("/srv/op/.config/.git".to_owned(), None, PKI.to_owned()),
        ("/.git".to_owned(), None, PKI.to_owned()),
        (
            "/srv/keys/.git".to_owned(),
            Some("/srv/keys/s3.env"),
            "/srv/keys/s3.env".to_owned(),
        ),
    ];
    for (git, s3, named) in cases {
        let (mut exec, _) = host();
        let mut a = install();
        if let Some(s3) = s3 {
            put(
                &mut exec,
                s3,
                0o600,
                b"AWS_ACCESS_KEY_ID=a\nAWS_SECRET_ACCESS_KEY=b\n",
            );
            a.s3_env = Some(p(s3));
        }
        exec.entries.insert(
            p(&git),
            Entry::Dir {
                perms: Perms::new(0o755),
                owner: RUNNER,
            },
        );
        assert_eq!(
            refusal(&exec, &a),
            format!(
                "{named} is inside a git work tree ({git} exists): keep keys and credentials outside every repository"
            )
        );
    }
}

/// Catches the platform and user guards missing: on a Mac the verb would write
/// systemd units nothing reads, and as root it would install the service for root.
#[test]
fn another_os_root_or_an_unknown_cpu_is_refused() {
    let (exec, _) = host();
    let mut out = Vec::new();
    let mac = Platform {
        os: "macos",
        arch: "aarch64",
    };
    let e = plan(&exec, &install(), mac, Mode::DryRun, &mut out)
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        e.as_deref(),
        Some("server-install runs on Linux (systemd user services), not macos")
    );
    let riscv = Platform {
        os: "linux",
        arch: "riscv64",
    };
    let e = plan(&exec, &install(), riscv, Mode::DryRun, &mut out)
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        e.as_deref(),
        Some("no binary of this host's CPU (riscv64) is known to server-install")
    );
    let (mut exec, _) = host();
    put(&mut exec, BIN, 0o755, &elf(183));
    let arm = Platform {
        os: "linux",
        arch: "aarch64",
    };
    assert!(plan(&exec, &install(), arm, Mode::DryRun, &mut out).is_ok());
    exec.euid = 0;
    assert_eq!(refusal(&exec, &install()), PilotError::IsRoot.to_string());
}

/// Catches a dry run that refuses a store that is down (the operator could not see
/// the plan before fixing it), an apply that goes ahead regardless, and a store URL
/// the check cannot speak counted as down.
#[test]
fn a_store_that_is_down_is_a_warning_in_a_dry_run_and_a_refusal_in_an_apply() {
    let url = "http://192.0.2.20:30900/minio/health/live";
    for http in [Ok(503), Err(std::io::ErrorKind::ConnectionRefused)] {
        let (mut exec, _) = host();
        exec.http = http;
        let got = if http.is_ok() {
            "503"
        } else {
            "connection refused"
        };
        let (dry, out) = planned(&exec, &install(), Mode::DryRun);
        assert!(dry.is_ok());
        assert!(
            out.contains(&format!(
                "WARN: {url} answered {got}; --apply refuses until it answers 200"
            )),
            "{out}"
        );
        let apply = planned(&exec, &install(), Mode::Apply)
            .0
            .err()
            .map(|e| e.to_string());
        assert_eq!(
            apply,
            Some(format!(
                "{url} answered {got}; --apply refuses until it answers 200"
            ))
        );
    }
    let (exec, _) = host();
    let mut a = install();
    a.settings.minio_endpoint = "https://s3.example".into();
    let (got, out) = planned(&exec, &a, Mode::Apply);
    assert!(got.is_ok());
    assert!(
        out.contains(
            "store: https://s3.example/minio/health/live not checked (not an http:// URL)"
        ),
        "{out}"
    );
}

/// Catches a missing linger warning (the service would stop at logout with no word)
/// and one printed for a user who lingers.
#[test]
fn a_user_who_does_not_linger_is_warned() {
    let (mut exec, _) = host();
    exec.entries.remove(&p("/var/lib/systemd/linger/op"));
    let (got, out) = planned(&exec, &install(), Mode::DryRun);
    assert!(got.is_ok());
    assert!(out.contains("WARN: op does not linger: the service stops when the user's last session ends (loginctl enable-linger op)"), "{out}");
}

/// Catches the flags and XDG_CONFIG_HOME ignored (the units would go where systemd
/// does not read them, or the unit would name the default key files), and the
/// conditional-put flag passed when the setting is 0.
#[test]
fn flags_and_the_config_home_are_used() {
    let (mut exec, _) = host();
    let (ca, server, key) = pki("kbf-pilot-server");
    put(&mut exec, "/srv/pki/ca.pem", 0o644, ca.as_bytes());
    put(&mut exec, "/srv/pki/server.pem", 0o644, server.as_bytes());
    put(&mut exec, "/srv/pki/server.key", 0o600, key.as_bytes());
    put(
        &mut exec,
        "/srv/s3.env",
        0o600,
        b"AWS_ACCESS_KEY_ID=a\nAWS_SECRET_ACCESS_KEY=b\n",
    );
    let mut a = install();
    a.pki = Some(p("/srv/pki"));
    a.s3_env = Some(p("/srv/s3.env"));
    a.home.xdg_config = Some(p("/srv/xdg"));
    a.settings.conditional_put = false;
    a.settings.state = Some(p("/srv/state"));
    let got = planned(&exec, &a, Mode::DryRun).0.expect("plans");
    let service = got.plan.steps().iter().find_map(|s| match s {
        Step::WriteFile { path, bytes, .. }
            if path.as_str() == "/srv/xdg/systemd/user/kbf-pilot-server.service" =>
        {
            Some(String::from_utf8(bytes.clone()).expect("text"))
        }
        _ => None,
    });
    let service = service.expect("the unit goes under XDG_CONFIG_HOME");
    assert!(
        service.contains("EnvironmentFile=/srv/s3.env\n"),
        "{service}"
    );
    assert!(
        service.contains("--worker-tls-cert /srv/pki/server.pem"),
        "{service}"
    );
    assert!(
        service.contains("ExecStart=/srv/state/bin/kbf-server "),
        "{service}"
    );
    assert!(!service.contains("conditional-put"), "{service}");
}

/// Catches the interface check passing on `ip`'s exit status alone (an interface
/// that is up but carries another address), and an address that only starts like
/// the bind address (192.0.2.1 inside 192.0.2.10/32).
#[test]
fn an_interface_with_another_address_does_not_carry_the_bind() {
    for line in [
        "4: tailscale0    inet 192.0.2.11/32 scope global tailscale0\n",
        "4: tailscale0    inet 192.0.2.10/32 scope global tailscale0\n",
    ] {
        let (mut exec, w) = host();
        let tools = answer(&w);
        let shown = line.to_owned();
        exec.answer = Box::new(move |cmd| {
            if cmd.program.as_str() == IP {
                return Ok(reply(0, &shown, ""));
            }
            tools(cmd)
        });
        let mut a = install();
        if line.contains("192.0.2.10/") {
            a.settings.bind = std::net::Ipv4Addr::new(192, 0, 2, 1);
        }
        let want = format!(
            "tailscale0 does not carry {} (ip -4 addr show dev tailscale0)",
            a.settings.bind
        );
        assert_eq!(refusal(&exec, &a), want);
    }
}

/// Catches a directory both the state and the unit directory sit under being
/// planned twice (the second Mkdir of the same path would be a step a re-run never
/// clears from the plan).
#[test]
fn a_shared_parent_directory_is_planned_once() {
    let (exec, _) = host();
    let mut a = install();
    a.settings.state = Some(p("/srv/op/.config/kbf-state"));
    let got = planned(&exec, &a, Mode::DryRun).0.expect("plans");
    let config = steps(&got.plan)
        .iter()
        .filter(|s| s.starts_with("mkdir /srv/op/.config "))
        .count();
    assert_eq!(config, 1, "{:?}", steps(&got.plan));
}

#[path = "server_run_tests.rs"]
mod run;
