use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;

use super::*;
use crate::linux::host::SWAPON;
use kbf_host::testkit::{ScratchHost, reply};
use kbf_host::user::{GETENT, ID};

const EXAMPLE: &str = include_str!("../../templates/linux/config.example");
const STAMP: &str = "1700000000";

fn me() -> (u32, u32) {
    (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    )
}

/// What the fake host's tools see.
struct World {
    users: BTreeMap<String, (u32, u32)>,
    enabled: BTreeSet<String>,
    active: BTreeSet<String>,
    minio_unit: bool,
    swap: bool,
    listening: String,
    /// A flag the binaries' `--help` leaves out.
    help_lacks: Option<&'static str>,
    help_fails: bool,
}

fn config() -> LinuxConfig {
    LinuxConfig::parse(EXAMPLE).expect("valid settings")
}

fn install() -> Install {
    Install {
        config: config(),
        bin_dir: Some(HostPath::new("/opt/new").expect("path")),
        restart_minio: false,
    }
}

fn write(exec: &ScratchHost, path: &str, text: &str, mode: u32) {
    let full = exec.at(path);
    fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
    // Over a read-only file too.
    let _ = fs::remove_file(&full);
    fs::write(&full, text).expect("write");
    fs::set_permissions(&full, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A scratch root that passes every check: both users are the test's own uid (so the
/// owner checks hold and the apply can chown), with new binaries in /opt/new. The
/// tools are played from `World`; a binary's `--help` lists every word of both units.
fn host(name: &str) -> (ScratchHost, Rc<RefCell<World>>) {
    let mut exec = ScratchHost::new(name);
    let (uid, gid) = me();
    let world = Rc::new(RefCell::new(World {
        users: BTreeMap::from([
            ("kbf".to_owned(), (uid, gid)),
            ("kbf-server".to_owned(), (uid, gid)),
        ]),
        enabled: BTreeSet::new(),
        active: BTreeSet::new(),
        minio_unit: true,
        swap: true,
        listening: "LISTEN 0 4096 0.0.0.0:22 0.0.0.0:*\n".to_owned(),
        help_lacks: None,
        help_fails: false,
    }));
    let units = render(&config(), Some(uid), &Templates::BUILT_IN).expect("renders");
    let help = format!("{}\n{}", units.server, units.daemon);
    let w = Rc::clone(&world);
    exec.answer = Box::new(move |cmd| {
        let mut w = w.borrow_mut();
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        Ok(match (cmd.program.as_str(), &args[..]) {
            (GETENT, ["passwd", "--", name]) => match w.users.get(*name) {
                Some((u, g)) => reply(
                    0,
                    &format!("{name}:x:{u}:{g}::/srv/{name}:/bin/false\n"),
                    "",
                ),
                None => reply(2, "", ""),
            },
            (ID, ["-G", "--", name]) => reply(0, &format!("{}\n", w.users[*name].1), ""),
            (SYSTEMCTL, ["cat", "--", _]) => reply(i32::from(!w.minio_unit), "", ""),
            (SYSTEMCTL, ["is-active", "--quiet", u]) => {
                reply(i32::from(!w.active.contains(*u)), "", "")
            }
            (SYSTEMCTL, ["is-enabled", "--quiet", u]) => {
                reply(i32::from(!w.enabled.contains(*u)), "", "")
            }
            (SYSTEMCTL, ["enable", units @ ..]) => {
                w.enabled.extend(units.iter().map(|u| (*u).to_owned()));
                reply(0, "", "")
            }
            (SYSTEMCTL, ["restart" | "start", units @ ..]) => {
                w.active.extend(units.iter().map(|u| (*u).to_owned()));
                reply(0, "", "")
            }
            (SYSTEMCTL, ["disable", "--now", units @ ..]) => {
                for u in units {
                    w.enabled.remove(*u);
                    w.active.remove(*u);
                }
                reply(0, "", "")
            }
            (SWAPON, _) => reply(0, if w.swap { "/swapfile\n" } else { "" }, ""),
            (SS, _) => reply(0, &w.listening, ""),
            (_, ["--help"]) => {
                let text = w
                    .help_lacks
                    .map_or(help.clone(), |f| help.replace(f, "--gone"));
                reply(i32::from(w.help_fails), &text, "")
            }
            // daemon-reload.
            _ => reply(0, "", ""),
        })
    });
    write(
        &exec,
        "/sys/fs/cgroup/cgroup.controllers",
        "cpuset cpu io memory pids\n",
        0o444,
    );
    write(&exec, "/etc/subuid", "kbf:100000:65536\n", 0o644);
    write(&exec, "/etc/subgid", "kbf:100000:65536\n", 0o644);
    write(&exec, "/var/lib/systemd/linger/kbf", "", 0o644);
    write(
        &exec,
        "/etc/kbf/s3.env",
        "AWS_ACCESS_KEY_ID=k\nAWS_SECRET_ACCESS_KEY=s\n",
        0o600,
    );
    for key in ["api-token", "reapi.key", "worker.key", "node.key"] {
        write(&exec, &format!("/etc/kbf/{key}"), "secret", 0o600);
    }
    for public in PUBLIC {
        write(&exec, &format!("/etc/kbf/{public}"), "pem", 0o644);
    }
    write(&exec, "/opt/new/kbf-server", "server-2", 0o755);
    write(&exec, "/opt/new/kbf-daemon", "daemon-2", 0o755);
    (exec, world)
}

fn steps(p: &Plan) -> Vec<String> {
    p.steps().iter().map(ToString::to_string).collect()
}

fn go(exec: &mut ScratchHost, a: &Install, mode: Mode) -> String {
    let mut out = Vec::new();
    run(exec, a, mode, &mut out).expect("install-units");
    String::from_utf8(out).expect("utf-8")
}

fn read(exec: &ScratchHost, path: &str) -> String {
    fs::read_to_string(exec.at(path)).expect(path)
}

fn mode_of(exec: &ScratchHost, path: &str) -> u32 {
    fs::metadata(exec.at(path))
        .expect(path)
        .permissions()
        .mode()
        & 0o7777
}

/// The plan's test on a scratch root: a first install plans the binaries, the four
/// units, the owned directories (and every missing parent) and the service steps,
/// and a dry run of it changes nothing; the apply writes each as planned; a re-run
/// plans nothing. Catches a step planned when the host already has it (a restart on
/// every run), a unit written with other bytes than `render-units` makes, and a
/// service step missing from a first install.
#[test]
fn a_first_install_writes_everything_and_a_rerun_plans_nothing() {
    let (mut exec, world) = host("install-first");
    let (uid, gid) = me();
    let p = plan(&exec, &install(), &mut Vec::new()).expect("plan");
    let want = [
        "mkdir /usr (0755)".to_owned(),
        "mkdir /usr/local (0755)".to_owned(),
        "mkdir /usr/local/bin (0755)".to_owned(),
        "write /usr/local/bin/kbf-server (0755, 8 bytes)".to_owned(),
        "write /usr/local/bin/kbf-daemon (0755, 8 bytes)".to_owned(),
        "mkdir /etc/systemd (0755)".to_owned(),
        "mkdir /etc/systemd/system (0755)".to_owned(),
    ];
    let got = steps(&p);
    assert_eq!(got[..7], want);
    let units: Vec<&String> = got
        .iter()
        .filter(|s| s.starts_with("write /etc/systemd"))
        .collect();
    let names = [
        "kbf.slice",
        "minio.service.d/kbf.conf",
        "kbf-server.service",
        "kbf-daemon.service",
    ];
    assert_eq!(units.len(), 4);
    for (line, name) in units.iter().zip(names) {
        assert!(
            line.starts_with(&format!("write /etc/systemd/system/{name} (0644, ")),
            "{line}"
        );
    }
    let tail = [
        "mkdir /var/lib/kbf-server (0755)".to_owned(),
        format!("mkdir /var/lib/kbf-server/raft (0700, owner {uid}:{gid})"),
        "mkdir /var/lib/kbf (0755)".to_owned(),
        format!("mkdir /var/lib/kbf/leases (0700, owner {uid}:{gid})"),
        "run /usr/bin/systemctl daemon-reload".to_owned(),
        "run /usr/bin/systemctl enable kbf-server.service kbf-daemon.service".to_owned(),
        "run /usr/bin/systemctl restart kbf-server.service kbf-daemon.service".to_owned(),
        "wait up to 60s for http://127.0.0.1:8982/readyz".to_owned(),
    ];
    assert_eq!(got[got.len() - tail.len()..], tail);
    assert_eq!(got.len(), 7 + 1 + 4 + tail.len(), "{got:#?}");

    let dry = go(&mut exec, &install(), Mode::DryRun);
    assert!(
        dry.contains("/etc/systemd/system/kbf.slice: writes\n    | # Rendered by"),
        "{dry}"
    );
    assert!(
        dry.contains("NOTE: minio.service keeps its old priorities"),
        "{dry}"
    );
    assert!(!exec.at("/etc/systemd").exists() && exec.log.is_empty());

    go(&mut exec, &install(), Mode::Apply);
    let units = render(&config(), Some(uid), &Templates::BUILT_IN).expect("renders");
    for (name, text) in units.files() {
        let path = format!("/etc/systemd/system/{name}");
        assert_eq!(read(&exec, &path), text, "{name}");
        assert_eq!(mode_of(&exec, &path), 0o644, "{name}");
    }
    assert_eq!(read(&exec, "/usr/local/bin/kbf-daemon"), "daemon-2");
    assert_eq!(mode_of(&exec, "/usr/local/bin/kbf-daemon"), 0o755);
    assert_eq!(mode_of(&exec, "/var/lib/kbf-server/raft"), 0o700);
    assert!(!exec.at("/usr/local/bin/kbf-daemon.previous").exists());
    assert_eq!(world.borrow().active.len(), 2);
    // The fake host answers the readiness URL the wait step names (a fake self-test).
    let ready = HttpUrl::new("http://127.0.0.1:8982/readyz").expect("url");
    assert_eq!(exec.http_status(&ready).expect("answers"), 200);

    let again = go(&mut exec, &install(), Mode::DryRun);
    assert!(
        again.contains("units and binaries unchanged: nothing restarted\n"),
        "{again}"
    );
    assert!(again.ends_with("nothing to do\n"), "{again}");
}

/// Catches a binary replaced without keeping the old one, and a changed binary that
/// does not restart the services (or restarts MinIO unasked).
#[test]
fn a_new_binary_keeps_the_old_one_and_restarts_the_services() {
    let (mut exec, _) = host("install-upgrade");
    go(&mut exec, &install(), Mode::Apply);
    write(&exec, "/opt/new/kbf-daemon", "daemon-3", 0o755);
    let (uid, gid) = me();
    let p = plan(&exec, &install(), &mut Vec::new()).expect("plan");
    let want = [
        format!("write /usr/local/bin/kbf-daemon.previous (0755, owner {uid}:{gid}, 8 bytes)"),
        "write /usr/local/bin/kbf-daemon (0755, 8 bytes)".to_owned(),
        "run /usr/bin/systemctl daemon-reload".to_owned(),
        "run /usr/bin/systemctl enable kbf-server.service kbf-daemon.service".to_owned(),
        "run /usr/bin/systemctl restart kbf-server.service kbf-daemon.service".to_owned(),
        "wait up to 60s for http://127.0.0.1:8982/readyz".to_owned(),
    ];
    assert_eq!(steps(&p), want);
    go(&mut exec, &install(), Mode::Apply);
    assert_eq!(
        read(&exec, "/usr/local/bin/kbf-daemon.previous"),
        "daemon-2"
    );
    assert_eq!(read(&exec, "/usr/local/bin/kbf-daemon"), "daemon-3");

    write(&exec, "/opt/new/kbf-daemon", "daemon-4", 0o755);
    let a = Install {
        restart_minio: true,
        ..install()
    };
    let got = steps(&plan(&exec, &a, &mut Vec::new()).expect("plan"));
    assert_eq!(
        got[3], "run /usr/bin/systemctl restart minio.service",
        "{got:#?}"
    );
}

/// Catches an unchanged host whose stopped or disabled service is left so, and one
/// that restarts both services to start one.
#[test]
fn unchanged_units_start_and_enable_only_what_is_not() {
    let (mut exec, world) = host("install-start");
    go(&mut exec, &install(), Mode::Apply);
    world.borrow_mut().active.remove(DAEMON);
    world.borrow_mut().enabled.remove(SERVER);
    let p = plan(&exec, &install(), &mut Vec::new()).expect("plan");
    let want = [
        "run /usr/bin/systemctl enable kbf-server.service",
        "run /usr/bin/systemctl start kbf-daemon.service",
        "wait up to 60s for http://127.0.0.1:8982/readyz",
    ];
    assert_eq!(steps(&p), want);
}

/// The upgrade path with no new binaries: a re-run with a changed setting (here
/// `KBF_NODE_ID`, which only kbf-daemon's unit names) and no `--bin-dir` rewrites that
/// unit alone and must reload systemd and restart both services, though both are
/// already enabled and running. Catches a unit write that does not count as a change
/// (the services would keep running the old unit until a reboot).
#[test]
fn a_changed_unit_alone_reloads_and_restarts_the_services() {
    let (mut exec, world) = host("install-unit-change");
    go(&mut exec, &install(), Mode::Apply);
    assert_eq!(
        world.borrow().active.len(),
        2,
        "the first install started both"
    );
    let text = EXAMPLE.replace("KBF_NODE_ID=storage-node-1", "KBF_NODE_ID=storage-node-2");
    let a = Install {
        config: LinuxConfig::parse(&text).expect("valid settings"),
        bin_dir: None,
        restart_minio: false,
    };
    let got = steps(&plan(&exec, &a, &mut Vec::new()).expect("plan"));
    assert_eq!(got.len(), 5, "{got:#?}");
    assert!(
        got[0].starts_with("write /etc/systemd/system/kbf-daemon.service (0644, "),
        "{got:#?}"
    );
    let want = [
        "run /usr/bin/systemctl daemon-reload",
        "run /usr/bin/systemctl enable kbf-server.service kbf-daemon.service",
        "run /usr/bin/systemctl restart kbf-server.service kbf-daemon.service",
        "wait up to 60s for http://127.0.0.1:8982/readyz",
    ];
    assert_eq!(got[1..], want);
}

/// A service that runs though it is not enabled (started by hand, or disabled and not
/// stopped) must still be stopped by the rollback; so must one enabled but stopped.
/// Catches a rollback that asks only `is-enabled` (or only `is-active`) and leaves a
/// running kbf-daemon on a host whose units it then moves away.
#[test]
fn uninstall_stops_a_service_that_runs_but_is_not_enabled() {
    let c = config();
    for (why, enabled, active) in [
        ("running, not enabled", false, true),
        ("enabled, not running", true, false),
    ] {
        let (exec, world) = host("install-undo-on");
        {
            let mut w = world.borrow_mut();
            if enabled {
                w.enabled.insert(DAEMON.to_owned());
            }
            if active {
                w.active.insert(DAEMON.to_owned());
            }
        }
        let got = steps(&plan_uninstall(&exec, &c, STAMP, &mut Vec::new()).expect("plan"));
        assert_eq!(
            got,
            ["run /usr/bin/systemctl disable --now kbf-daemon.service kbf-server.service"],
            "{why}"
        );
    }
}

/// Without `--bin-dir` the installed binaries are checked and kept. Catches a plan that
/// copies binaries it was not given.
#[test]
fn without_a_bin_dir_the_installed_binaries_stay() {
    let (exec, _) = host("install-nobin");
    write(&exec, "/usr/local/bin/kbf-server", "server-1", 0o755);
    write(&exec, "/usr/local/bin/kbf-daemon", "daemon-1", 0o755);
    let a = Install {
        bin_dir: None,
        ..install()
    };
    let got = steps(&plan(&exec, &a, &mut Vec::new()).expect("plan"));
    assert!(
        got.iter().all(|s| !s.contains("/usr/local/bin")),
        "{got:#?}"
    );
}

type Plant = fn(&ScratchHost, &RefCell<World>);
type Want = fn(&HostError) -> bool;

/// Each row breaks one thing the host must have, on a host that has all else; each
/// must be refused, before a step, with the refusal meant for it.
#[test]
fn each_missing_prerequisite_is_refused_with_its_fix() {
    let rows: Vec<(&str, Plant, Want)> = vec![
        (
            "no memory controller",
            |e, _| write(e, "/sys/fs/cgroup/cgroup.controllers", "cpu io\n", 0o444),
            |e| matches!(e, HostError::NoMemoryController),
        ),
        (
            "no cgroup v2",
            |e, _| fs::remove_file(e.at("/sys/fs/cgroup/cgroup.controllers")).expect("rm"),
            |e| matches!(e, HostError::NoMemoryController),
        ),
        (
            "no MinIO unit",
            |_, w| w.borrow_mut().minio_unit = false,
            |e| matches!(e, HostError::NoMinioUnit(u) if u == "minio.service"),
        ),
        (
            "no kbf user",
            |_, w| {
                w.borrow_mut().users.remove("kbf");
            },
            |e| matches!(e, HostError::NoUser(u) if u == "kbf"),
        ),
        (
            "no server user",
            |_, w| {
                w.borrow_mut().users.remove("kbf-server");
            },
            |e| matches!(e, HostError::NoUser(u) if u == "kbf-server"),
        ),
        (
            "no subuid range",
            |e, _| write(e, "/etc/subuid", "kbfx:1:2\n", 0o644),
            |e| matches!(e, HostError::NoSubIds { file, .. } if file.as_str() == "/etc/subuid"),
        ),
        (
            "no subgid file",
            |e, _| fs::remove_file(e.at("/etc/subgid")).expect("rm"),
            |e| matches!(e, HostError::NoSubIds { file, .. } if file.as_str() == "/etc/subgid"),
        ),
        (
            "no linger",
            |e, _| fs::remove_file(e.at("/var/lib/systemd/linger/kbf")).expect("rm"),
            |e| matches!(e, HostError::NoLinger { .. }),
        ),
        (
            "no s3.env",
            |e, _| fs::remove_file(e.at("/etc/kbf/s3.env")).expect("rm"),
            |e| matches!(e, HostError::Missing(p) if p.as_str() == "/etc/kbf/s3.env"),
        ),
        (
            "node.key a directory",
            |e, _| {
                fs::remove_file(e.at("/etc/kbf/node.key")).expect("rm");
                fs::create_dir(e.at("/etc/kbf/node.key")).expect("dir");
            },
            |e| matches!(e, HostError::Missing(p) if p.as_str() == "/etc/kbf/node.key"),
        ),
        (
            "a key not the server's",
            |_, w| {
                w.borrow_mut().users.get_mut("kbf-server").expect("user").0 += 1;
            },
            |e| matches!(e, HostError::NotOwned { path, .. } if path.as_str() == "/etc/kbf/s3.env"),
        ),
        (
            "node.key not kbf's",
            |_, w| {
                w.borrow_mut().users.get_mut("kbf").expect("user").0 += 1;
            },
            |e| matches!(e, HostError::NotOwned { path, .. } if path.as_str() == "/etc/kbf/node.key"),
        ),
        (
            "api-token group-readable",
            |e, _| {
                fs::set_permissions(
                    e.at("/etc/kbf/api-token"),
                    fs::Permissions::from_mode(0o640),
                )
                .expect("chmod")
            },
            |e| matches!(e, HostError::Readable { path, .. } if path.as_str() == "/etc/kbf/api-token"),
        ),
        (
            "worker.key other-readable",
            |e, _| {
                fs::set_permissions(
                    e.at("/etc/kbf/worker.key"),
                    fs::Permissions::from_mode(0o604),
                )
                .expect("chmod")
            },
            |e| matches!(e, HostError::Readable { .. }),
        ),
        (
            "no deny-list",
            |e, _| fs::remove_file(e.at("/etc/kbf/deny-list")).expect("rm"),
            |e| matches!(e, HostError::Missing(p) if p.as_str() == "/etc/kbf/deny-list"),
        ),
        (
            "no ca.pem",
            |e, _| fs::remove_file(e.at("/etc/kbf/ca.pem")).expect("rm"),
            |e| matches!(e, HostError::Missing(p) if p.as_str() == "/etc/kbf/ca.pem"),
        ),
        (
            "s3.env without a secret",
            |e, _| write(e, "/etc/kbf/s3.env", "AWS_ACCESS_KEY_ID=k\n", 0o600),
            |e| matches!(e, HostError::S3Env(_)),
        ),
        (
            "s3.env with an empty key id",
            |e, _| {
                write(
                    e,
                    "/etc/kbf/s3.env",
                    "AWS_ACCESS_KEY_ID=\nAWS_SECRET_ACCESS_KEY=s\n",
                    0o600,
                )
            },
            |e| matches!(e, HostError::S3Env(_)),
        ),
        (
            "REAPI port held",
            |_, w| {
                w.borrow_mut()
                    .listening
                    .push_str("LISTEN 0 4096 192.0.2.10:8980 0.0.0.0:*\n")
            },
            |e| matches!(e, HostError::PortInUse(8980)),
        ),
        (
            "worker port held",
            |_, w| {
                w.borrow_mut()
                    .listening
                    .push_str("LISTEN 0 4096 [::]:8981 [::]:*\n")
            },
            |e| matches!(e, HostError::PortInUse(8981)),
        ),
        (
            "API port held",
            |_, w| {
                w.borrow_mut()
                    .listening
                    .push_str("LISTEN 0 4096 127.0.0.1:8982 0.0.0.0:*\n")
            },
            |e| matches!(e, HostError::PortInUse(8982)),
        ),
        (
            "kbf-daemon not executable",
            |e, _| {
                fs::set_permissions(
                    e.at("/opt/new/kbf-daemon"),
                    fs::Permissions::from_mode(0o644),
                )
                .expect("chmod")
            },
            |e| matches!(e, HostError::NotExecutable(p) if p.as_str() == "/opt/new/kbf-daemon"),
        ),
        (
            "no kbf-server",
            |e, _| fs::remove_file(e.at("/opt/new/kbf-server")).expect("rm"),
            |e| matches!(e, HostError::NotExecutable(p) if p.as_str() == "/opt/new/kbf-server"),
        ),
        (
            "a server without --raft-dir",
            |_, w| w.borrow_mut().help_lacks = Some("--raft-dir"),
            |e| matches!(e, HostError::NoFlag { flag, .. } if flag == "--raft-dir"),
        ),
        (
            "a daemon without --actions-memory-max-gib",
            |_, w| w.borrow_mut().help_lacks = Some("--actions-memory-max-gib"),
            |e| matches!(e, HostError::NoFlag { bin, .. } if bin.as_str() == "/opt/new/kbf-daemon"),
        ),
        (
            "--help fails",
            |_, w| w.borrow_mut().help_fails = true,
            |e| matches!(e, HostError::Query { .. }),
        ),
        (
            "cgroup unreadable",
            |e, _| {
                fs::remove_dir_all(e.at("/sys/fs/cgroup")).expect("rm");
                write(e, "/sys/fs/cgroup", "", 0o644);
            },
            |e| {
                matches!(e, HostError::Inspect { path, .. }
                    if path.as_str() == "/sys/fs/cgroup/cgroup.controllers")
            },
        ),
        (
            "linger unreadable",
            |e, _| {
                fs::remove_dir_all(e.at("/var/lib/systemd/linger")).expect("rm");
                write(e, "/var/lib/systemd/linger", "", 0o644);
            },
            |e| {
                matches!(e, HostError::Inspect { path, .. }
                    if path.as_str() == "/var/lib/systemd/linger/kbf")
            },
        ),
    ];
    for (why, plant, want) in rows {
        let (exec, world) = host("install-refused");
        plant(&exec, &world);
        let err = plan(&exec, &install(), &mut Vec::new()).expect_err(why);
        assert!(want(&err), "{why}: {err}");
    }
}

/// subuid(5) keys a range by user name or by uid, and newuidmap takes either. Catches
/// a host whose ranges are keyed by the kbf user's uid being refused, and a line
/// keyed by another uid that starts with kbf's being taken for it.
#[test]
fn a_sub_id_range_keyed_by_uid_is_the_users() {
    let (uid, _) = me();
    let (exec, _) = host("install-subid-uid");
    write(
        &exec,
        "/etc/subuid",
        &format!("{uid}:100000:65536\n"),
        0o644,
    );
    write(
        &exec,
        "/etc/subgid",
        &format!("other:1:2\n{uid}:100000:65536\n"),
        0o644,
    );
    plan(&exec, &install(), &mut Vec::new()).expect("a uid-keyed range");

    write(
        &exec,
        "/etc/subuid",
        &format!("{uid}0:100000:65536\n"),
        0o644,
    );
    let err = plan(&exec, &install(), &mut Vec::new()).expect_err("another uid");
    assert!(
        matches!(&err, HostError::NoSubIds { file, .. } if file.as_str() == "/etc/subuid"),
        "{err}"
    );
}

/// Catches a running kbf-server's own ports read as held (a re-install would always
/// refuse) and a host without swap passing silently.
#[test]
fn a_running_server_keeps_its_ports_and_no_swap_is_a_warning() {
    let (exec, world) = host("install-running");
    world.borrow_mut().active.insert(SERVER.to_owned());
    world.borrow_mut().swap = false;
    world
        .borrow_mut()
        .listening
        .push_str("LISTEN 0 4096 192.0.2.10:8980 0.0.0.0:*\n");
    let mut out = Vec::new();
    plan(&exec, &install(), &mut out).expect("its ports are its own");
    let out = String::from_utf8(out).expect("utf-8");
    assert!(out.starts_with("WARN: no swap is active"), "{out}");
    assert!(
        out.contains("kbf-server is running: its ports are its own\n"),
        "{out}"
    );
    let asked = exec.queries.borrow();
    assert!(
        asked.iter().all(|q| q.program.as_str() != SS),
        "ss was asked"
    );
}

/// Catches units that break the priorities being installed: a planted
/// `MemorySwapMax=1G` in kbf-server's template must refuse the install, naming the rule.
#[test]
fn units_that_break_a_priority_are_refused_before_a_write() {
    let (exec, _) = host("install-broken");
    let t = Templates::BUILT_IN;
    assert_eq!(
        t.server.matches("MemorySwapMax=0").count(),
        1,
        "the defect is not planted"
    );
    let server = t.server.replace("MemorySwapMax=0", "MemorySwapMax=1G");
    let planted = Templates {
        server: &server,
        ..t
    };
    let err = plan_with(&exec, &install(), &planted, &mut Vec::new()).expect_err("broken");
    assert!(
        matches!(&err, HostError::Units(f) if !f.is_empty()),
        "{err}"
    );
    assert!(err.to_string().contains("FAIL "), "{err}");
}

/// The rollback after an install: the services stopped and disabled, the four files
/// moved under the stamp's directory, `daemon-reload`; a re-run plans nothing. Catches
/// a rollback that deletes the files instead of keeping them, leaves one in place, or
/// stops the services again once they are off.
#[test]
fn uninstall_moves_the_units_aside_and_a_rerun_plans_nothing() {
    let (mut exec, world) = host("install-undo");
    go(&mut exec, &install(), Mode::Apply);
    let slice = read(&exec, "/etc/systemd/system/kbf.slice");
    let c = config();
    let p = plan_uninstall(&exec, &c, STAMP, &mut Vec::new()).expect("plan");
    let got = steps(&p);
    assert_eq!(
        got[0],
        "run /usr/bin/systemctl disable --now kbf-daemon.service kbf-server.service"
    );
    assert_eq!(
        got.last().expect("steps"),
        "run /usr/bin/systemctl daemon-reload"
    );
    assert_eq!(
        got.iter()
            .filter(|s| s.starts_with("remove /etc/systemd/system/"))
            .count(),
        4
    );

    let mut out = Vec::new();
    run_uninstall(&mut exec, &c, STAMP, Mode::Apply, &mut out).expect("apply");
    let kept = format!("/var/lib/kbf-linux/removed/{STAMP}");
    assert_eq!(read(&exec, &format!("{kept}/kbf.slice")), slice);
    assert!(
        exec.at(&format!("{kept}/minio.service.d/kbf.conf"))
            .exists()
    );
    assert!(!exec.at("/etc/systemd/system/kbf-server.service").exists());
    assert!(world.borrow().active.is_empty() && world.borrow().enabled.is_empty());

    let mut out = Vec::new();
    run_uninstall(&mut exec, &c, "1700000001", Mode::DryRun, &mut out).expect("re-run");
    let out = String::from_utf8(out).expect("utf-8");
    assert!(out.starts_with("kbf.slice: not installed\n"), "{out}");
    assert!(out.ends_with("nothing to do\n"), "{out}");
    assert!(
        out.contains("kept: the binaries, /var/lib/kbf-server/raft"),
        "{out}"
    );
}

/// Catches a Raft directory made for a server that keeps its index in memory.
#[test]
fn an_in_memory_server_gets_no_raft_directory() {
    let (exec, _) = host("install-memory");
    let text = EXAMPLE.replace("KBF_META_LOG=raft", "KBF_META_LOG=memory");
    let a = Install {
        config: LinuxConfig::parse(&text).expect("valid settings"),
        ..install()
    };
    let got = steps(&plan(&exec, &a, &mut Vec::new()).expect("plan"));
    let raft = got
        .iter()
        .any(|s| s.starts_with("mkdir /var/lib/kbf-server"));
    assert!(!raft, "{got:#?}");
    let scratch = got
        .iter()
        .any(|s| s.starts_with("mkdir /var/lib/kbf/leases (0700"));
    assert!(scratch, "{got:#?}");
}
