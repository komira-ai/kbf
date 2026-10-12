use std::cell::RefCell;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;

use super::*;
use kbf_host::testkit::{FakeExec, reply};

fn argv(args: &[&str]) -> Vec<OsString> {
    std::iter::once("kbf-deploy")
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect()
}

const BUCKETS: &[&str] = &[
    "minio",
    "buckets",
    "--endpoint",
    "http://127.0.0.1:9000",
    "--bucket",
    "kbf-cas",
    "--policy",
    "kbf-linux-rw",
    "--account",
    "kbf-linux-server",
    "--mc",
    "/opt/kbf/bin/mc",
];

const NODEPORT: &[&str] = &[
    "minio",
    "nodeport",
    "--kubectl",
    "/usr/local/bin/kubectl",
    "--kubeconfig",
    "/srv/admin/kubeconfig",
    "--namespace",
    "minio",
    "--service",
    "minio-hdd",
    "--node-port",
    "30900",
    "--remove",
];

fn admin_env(key: &str) -> Option<OsString> {
    Some(OsString::from(if key == "MINIO_ROOT_USER" {
        "kbf-admin"
    } else {
        "pw"
    }))
}

/// Exit status, stdout and stderr of `run` over `exec`.
fn cli(
    exec: &mut FakeExec,
    env: &dyn Fn(&str) -> Option<OsString>,
    args: &[&str],
) -> (ExitCode, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(argv(args), exec, env, &mut out, &mut err);
    let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
    (code, text(out), text(err))
}

/// Catches a parser that takes a bad flag value (a name that is not kbf's, a path that
/// is not absolute, a node port outside the NodePort range, an owner without a key
/// file) instead of exiting 2 before any query, and help that goes to stderr.
#[test]
fn bad_flags_exit_2_before_anything_runs() {
    let mut exec = FakeExec::new(501);
    let (code, out, _) = cli(&mut exec, &admin_env, &["--help"]);
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(out.contains("minio"), "{out}");
    let swap = |from: &str, to: &str, base: &[&str]| -> Vec<String> {
        base.iter()
            .map(|a| if *a == from { to } else { a }.to_owned())
            .collect()
    };
    let mut bad: Vec<Vec<String>> = vec![
        swap("kbf-cas", "cas", BUCKETS),
        swap("/opt/kbf/bin/mc", "mc", BUCKETS),
        swap("http://127.0.0.1:9000", "http://127.0.0.1:9000/x", BUCKETS),
        swap("30900", "8080", NODEPORT),
        swap("minio-hdd", "Minio", NODEPORT),
        vec!["frobnicate".to_owned()],
    ];
    bad.push(
        BUCKETS
            .iter()
            .map(|a| (*a).to_owned())
            .chain(["--key-owner".to_owned(), "kbf".to_owned()])
            .collect(),
    );
    for args in bad {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, out, err) = cli(&mut exec, &admin_env, &args);
        assert_eq!(code, ExitCode::from(2), "{args:?}");
        assert_eq!(out, "", "{args:?}");
        assert_ne!(err, "", "{args:?}");
    }
    assert!(
        exec.queries.borrow().is_empty(),
        "a refused command line ran a query"
    );
}

/// Catches an admin key pair taken from anywhere but the environment, or taken when it
/// is empty (mc would run with no credentials and every check would read "absent").
#[test]
fn the_admin_pair_must_be_set_and_not_empty() {
    let no_user = |key: &str| (key == "MINIO_ROOT_PASSWORD").then_some(OsString::from("pw"));
    let empty_password = |key: &str| {
        Some(OsString::from(if key == "MINIO_ROOT_USER" {
            "u"
        } else {
            ""
        }))
    };
    type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;
    let cases: [(Env<'_>, &str); 2] = [
        (&no_user, "MINIO_ROOT_USER"),
        (&empty_password, "MINIO_ROOT_PASSWORD"),
    ];
    for (env, key) in cases {
        let mut exec = FakeExec::new(501);
        let (code, _, err) = cli(&mut exec, env, BUCKETS);
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(
            err,
            format!(
                "kbf-deploy: {key} is not set in the environment (the admin key pair is \
                 never read from a flag or a file)\n"
            )
        );
    }
}

/// Catches `mc` given a config directory others can read (it would hold the admin
/// URL if mc ever saved an alias) or one left behind after the run, and an apply flag
/// that does not reach the plan.
#[test]
fn buckets_runs_mc_in_a_private_scratch_directory_removed_after() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut exec = FakeExec::new(501);
    let record = Rc::clone(&seen);
    exec.answer = Box::new(move |cmd| {
        if let Some((_, dir)) = cmd.env.iter().find(|(k, _)| k == "MC_CONFIG_DIR") {
            let mode = fs::metadata(dir).map(|m| m.permissions().mode() & 0o777);
            record.borrow_mut().push((dir.clone(), mode.ok()));
        }
        Ok(reply(0, "mc version RELEASE.2025-08-13T08-35-41Z", ""))
    });
    let (code, out, err) = cli(&mut exec, &admin_env, BUCKETS);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    assert!(
        out.ends_with("dry run: nothing changed. Re-run with --apply.\n"),
        "{out}"
    );
    assert!(exec.log.is_empty());

    let apply: Vec<&str> = BUCKETS.iter().copied().chain(["--apply"]).collect();
    let (code, out, _) = cli(&mut exec, &admin_env, &apply);
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(
        out.contains("+ run /opt/kbf/bin/mc admin policy create"),
        "{out}"
    );
    assert_eq!(exec.log.len(), 1);

    let seen = seen.borrow();
    assert!(seen.len() >= 4, "{seen:?}");
    for (dir, mode) in seen.iter() {
        assert_eq!(*mode, Some(0o700), "{dir}");
        assert!(!std::path::Path::new(dir).exists(), "{dir} was left behind");
    }
}

/// Catches nodeport running without its scratch `--cache-dir` (kubectl would write a
/// cache under the working directory, since its environment has no HOME), and a
/// failed apply guard read as success: a nodeport plan never applies as root.
#[test]
fn nodeport_runs_kubectl_with_a_scratch_cache_and_never_as_root() {
    let mut exec = FakeExec::new(501);
    exec.answer = Box::new(|_| Ok(reply(0, r#"{"items":[]}"#, "")));
    let (code, out, _) = cli(&mut exec, &admin_env, NODEPORT);
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(out, "no Service minio/minio-hdd-kbf-np\nnothing to do\n");
    let cache = exec.queries.borrow()[0].args[3].clone();
    assert!(
        cache.starts_with(&std::env::temp_dir().to_string_lossy().into_owned()),
        "{cache}"
    );

    let mut root = FakeExec::new(0);
    root.answer = Box::new(|_| Ok(reply(0, r#"{"items":[]}"#, "")));
    let apply: Vec<&str> = NODEPORT.iter().copied().chain(["--apply"]).collect();
    let (code, _, err) = cli(&mut root, &admin_env, &apply);
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(
        err,
        "kbf-deploy: this step runs as a user, not as root: run it without sudo\n"
    );
}

/// Catches `main` not reaching the parser and the host: a refused run must exit 1. The
/// endpoint is a closed port, so the run stops at the environment or at the health
/// check, whichever the test process has.
#[test]
fn main_runs_on_the_host() {
    let args: Vec<&str> = BUCKETS
        .iter()
        .map(|a| {
            if *a == "http://127.0.0.1:9000" {
                "http://127.0.0.1:1"
            } else {
                a
            }
        })
        .collect();
    assert_eq!(main(argv(&args)), ExitCode::FAILURE);
}

const LINUX_CONFIG: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/templates/linux/config.example"
);

fn no_env(_: &str) -> Option<OsString> {
    None
}

/// Catches a host-changing `linux` verb whose refusal does not exit 2 with a
/// `REFUSED:` line, whose failed step does not exit 1, or whose dry run does not exit
/// 0; and flags that conflict (a rollback with an install's flags) being taken.
#[test]
fn the_linux_host_verbs_exit_0_done_1_failed_2_refused() {
    fn base(verb: &str) -> Vec<&str> {
        vec!["linux", verb, "--config", LINUX_CONFIG]
    }
    let busybox = format!("busybox@sha256:{}", "a".repeat(64));
    let refused = [
        // The fake's default answer, exit 0, is every kubelet active.
        (base("swap"), "kubelet.service is active"),
        // ... and a cgroup.controllers that is not there.
        (base("install-units"), "no cgroup v2 memory controller"),
        (
            [base("image-pull"), vec!["--image", &busybox]].concat(),
            "image-pull reads and fills kbf's Podman store as kbf",
        ),
        (
            vec!["linux", "swap", "--config", "/srv/none/config"],
            "/srv/none/config",
        ),
    ];
    for (args, want) in refused {
        let mut exec = FakeExec::new(501);
        let (code, _, err) = cli(&mut exec, &no_env, &args);
        assert_eq!(code, ExitCode::from(2), "{args:?}");
        assert!(
            err.starts_with("REFUSED: ") && err.contains(want),
            "{args:?}: {err}"
        );
    }

    let conflicts = [
        [
            base("install-units"),
            vec!["--uninstall", "--restart-minio"],
        ]
        .concat(),
        [base("swap"), vec!["--remove", "--kubelet-allows-swap"]].concat(),
        [base("image-pull"), vec!["--image", "busybox:latest"]].concat(),
    ];
    for args in conflicts {
        let mut exec = FakeExec::new(0);
        let (code, _, _) = cli(&mut exec, &no_env, &args);
        assert_eq!(code, ExitCode::from(2), "{args:?}");
        assert!(exec.queries.borrow().is_empty(), "{args:?} asked the host");
    }

    // A rollback of a host whose services are on: a dry run plans, exits 0.
    let mut exec = FakeExec::new(501);
    let (code, out, _) = cli(
        &mut exec,
        &no_env,
        &[base("install-units"), vec!["--uninstall"]].concat(),
    );
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(
        out.contains("\nDRY-RUN: run /usr/bin/systemctl disable --now"),
        "{out}"
    );

    // Removing a swapfile whose removal fails: exit 1.
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|_| Ok(reply(0, "", "")));
    exec.fail_at = Some(0);
    exec.entries.insert(
        HostPath::new("/swapfile").expect("path"),
        kbf_host::exec::Entry::Other,
    );
    let args = [base("swap"), vec!["--remove", "--apply"]].concat();
    let (code, _, err) = cli(&mut exec, &no_env, &args);
    assert_eq!(code, ExitCode::FAILURE, "{err}");
    assert!(
        err.starts_with("kbf-deploy: step 0 (remove /swapfile) failed"),
        "{err}"
    );
}

/// An image-pull whose pull ran but whose image is still missing: the host changed, so
/// it is a failed verb (exit 1, a `FAIL` line), not a refusal (exit 2, `REFUSED:`,
/// which says nothing was done). Catches the check after an apply reported as a
/// refusal.
#[test]
fn a_failed_check_after_an_apply_exits_1_with_a_fail_line() {
    let busybox = format!("busybox@sha256:{}", "a".repeat(64));
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd| {
        Ok(match cmd.program.as_str() {
            kbf_host::user::GETENT => reply(0, "kbf:x:990:990::/h:/s\n", ""),
            // `podman image exists`: never, even after the pull.
            "/usr/bin/podman" if cmd.args[0] == "image" => reply(1, "", ""),
            _ => reply(0, "990\n", ""),
        })
    });
    let args = [
        "linux",
        "image-pull",
        "--config",
        LINUX_CONFIG,
        "--image",
        &busybox,
        "--apply",
    ];
    let (code, _, err) = cli(&mut exec, &no_env, &args);
    assert_eq!(exec.log.len(), 1, "the pull did not run");
    assert_eq!(code, ExitCode::FAILURE, "{err}");
    assert_eq!(
        err,
        format!(
            "FAIL the check after the apply: {busybox} is still not in the store after its pull\n"
        )
    );
}

/// Catches a rollback directory name that is not the seconds since the epoch (two
/// rollbacks in one run of the clock would share it, or sort out of order).
#[test]
fn the_rollback_stamp_is_unix_seconds() {
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let stamp: u64 = stamp().parse().expect("a number");
    assert!(stamp >= before && stamp <= before + 5, "{stamp}");
}

/// Catches the `linux` kit dropped from the merged command line (clap would exit 2 on
/// an unknown subcommand, but on stderr with no `REFUSED:`), a linux refusal written to
/// stdout or exiting 1 (1 means "the units are wrong"). (The binary's tests in
/// `tests/linux_cli.rs` run the verbs end to end; this one also runs `run`'s linux arm in
/// the lib tests' instantiation of `run`, the one the coverage ratchet counts.)
#[test]
fn a_linux_verb_runs_through_the_same_command_line() {
    let mut exec = FakeExec::new(501);
    let (code, out, err) = cli(
        &mut exec,
        &admin_env,
        &[
            "linux",
            "render-units",
            "--config",
            "/nonexistent/kbf-deploy/config.local",
            "--out",
            "/nonexistent/kbf-deploy/units",
        ],
    );
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""), "{err}");
    assert!(
        err.starts_with("REFUSED: cannot read /nonexistent/kbf-deploy/config.local"),
        "{err}"
    );
}

/// Catches `render-units` or `check-units` output lost through the merged command line
/// (a check's FAIL lines not printed, or a broken unit exiting 0). These run the lib
/// tests' instantiation of the `linux` arm, the one the coverage ratchet counts.
#[test]
fn render_units_then_check_units_exit_0_and_a_broken_unit_exits_1() {
    let dir = kbf_host::testkit::scratch("cli-render");
    let units = dir.join("units");
    let units = units.to_str().expect("utf-8");
    let mut exec = FakeExec::new(501);
    let render = [
        "linux",
        "render-units",
        "--config",
        LINUX_CONFIG,
        "--out",
        units,
        "--kbf-uid",
        "1500",
    ];
    let (code, out, err) = cli(&mut exec, &no_env, &render);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(out.starts_with("rendered kbf.slice "), "{out}");
    let check = [
        "linux",
        "check-units",
        "--config",
        LINUX_CONFIG,
        "--dir",
        units,
        "--installable",
    ];
    let (code, out, err) = cli(&mut exec, &no_env, &check);
    assert_eq!(code, ExitCode::SUCCESS, "{out}{err}");
    assert!(out.starts_with("UNITS OK"), "{out}");

    let server = format!("{units}/kbf-server.service");
    let text = std::fs::read_to_string(&server).expect("rendered");
    std::fs::write(&server, text.replace("MemorySwapMax=0", "MemorySwapMax=1G")).expect("plant");
    let (code, out, _) = cli(&mut exec, &no_env, &check);
    assert_eq!(code, ExitCode::FAILURE, "{out}");
    assert!(out.starts_with("FAIL "), "{out}");
    assert!(out.contains("UNITS BROKEN"), "{out}");
}

/// The kit's settings in a file, with a LAN address filled in.
fn mdm_config(name: &str) -> String {
    let dir = kbf_host::testkit::scratch(name);
    let file = dir.join("config.local");
    std::fs::write(&file, crate::mdm::tests::config_text()).expect("write");
    file.to_string_lossy().into_owned()
}

/// Catches an `mdm` verb whose refusal does not exit 2 with a `REFUSED:` line (an
/// install without `--bin-dir`, as a user, with a bad config; a fetch into a relative
/// directory or as root), whose failed step does not exit 1, or whose dry run or
/// rollback with nothing to do does not exit 0.
#[test]
fn the_mdm_verbs_exit_0_done_1_failed_2_refused() {
    let config = mdm_config("cli_mdm");
    let install = |extra: &[&str]| -> Vec<String> {
        ["mdm", "install", "--config", config.as_str()]
            .iter()
            .chain(extra)
            .map(|s| (*s).to_owned())
            .collect()
    };
    let refused: [(u32, Vec<String>, &str); 6] = [
        (0, install(&[]), "mdm install needs --bin-dir DIR"),
        (
            501,
            install(&["--bin-dir", "/srv/archives"]),
            "run it with sudo",
        ),
        (
            0,
            vec![
                "mdm".into(),
                "install".into(),
                "--config".into(),
                "/srv/none/config".into(),
                "--uninstall".into(),
            ],
            "/srv/none/config",
        ),
        (
            501,
            vec![
                "mdm".into(),
                "fetch".into(),
                "--dir".into(),
                "archives".into(),
            ],
            "is not absolute",
        ),
        (
            0,
            vec![
                "mdm".into(),
                "fetch".into(),
                "--dir".into(),
                "/srv/archives".into(),
            ],
            "not root",
        ),
        (
            0,
            vec![
                "mdm".into(),
                "verify".into(),
                "--config".into(),
                "/srv/none/config".into(),
            ],
            "/srv/none/config",
        ),
    ];
    for (euid, args, want) in refused {
        let mut exec = FakeExec::new(euid);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, _, err) = cli(&mut exec, &no_env, &args);
        assert_eq!(code, ExitCode::from(2), "{args:?}: {err}");
        assert!(
            err.starts_with("REFUSED: ") && err.contains(want),
            "{args:?}: {err}"
        );
    }

    // A rollback with nothing installed: exit 0.
    let mut exec = FakeExec::new(0);
    let args = install(&["--uninstall"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, out, err) = cli(&mut exec, &no_env, &args);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(out.contains("no kbf-mdm unit is installed"), "{out}");
    // ... and one whose first step fails: exit 1.
    exec.entries.insert(
        HostPath::new("/etc/systemd/system/kbf-mdm.slice").expect("path"),
        kbf_host::exec::Entry::Other,
    );
    exec.fail_at = Some(0);
    let args = install(&["--uninstall", "--apply"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, _, err) = cli(&mut exec, &no_env, &args);
    assert_eq!(code, ExitCode::FAILURE, "{err}");
    assert!(err.starts_with("kbf-deploy: step 0 ("), "{err}");

    // A dry run of a fetch into an empty directory: every pinned archive, exit 0.
    let dir = std::env::temp_dir().join(format!("kbf-deploy-cli-fetch.{}", std::process::id()));
    let mut exec = FakeExec::new(501);
    let dir_arg = dir.to_string_lossy().into_owned();
    let (code, out, err) = cli(
        &mut exec,
        &no_env,
        &["mdm", "fetch", "--dir", &dir_arg, "--only", "caddy"],
    );
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(
        out.contains("DRY-RUN: download https://github.com/caddyserver/caddy/"),
        "{out}"
    );
    assert!(!dir.exists());
    let (code, _, _) = cli(
        &mut exec,
        &no_env,
        &["mdm", "fetch", "--dir", &dir_arg, "--only", "cady"],
    );
    assert_eq!(code, ExitCode::from(2));
}

/// Catches `mdm verify` exiting 0 when checks fail, or dialling MDM_BIND when told to
/// dial elsewhere (`--via`, here a closed port).
#[test]
fn mdm_verify_exits_1_when_a_check_fails() {
    let config = mdm_config("cli_mdm_verify");
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let via = closed.local_addr().expect("addr").to_string();
    drop(closed);
    let mut exec = FakeExec::new(0);
    let (code, out, err) = cli(
        &mut exec,
        &no_env,
        &["mdm", "verify", "--config", &config, "--via", &via],
    );
    assert_eq!(code, ExitCode::FAILURE, "{err}");
    assert!(out.contains("FAIL  https://mdm.example.com/version answers: https://mdm.example.com/version: Connection refused"), "{out}");
    assert!(out.ends_with(" check(s) failed\n"), "{out}");
}

/// Catches `envoy render` not wired to the kit, its line lost, or a refusal that
/// exits other than 2 or prints elsewhere than stderr. In-process, so the lib tests'
/// instantiation of `run` covers the `envoy` arm.
#[test]
fn envoy_render_exits_0_and_a_bad_setting_exits_2() {
    let dir = kbf_host::testkit::scratch("cli-envoy");
    let config = dir.join("config.local");
    let example = include_str!("../templates/envoy/config.example");
    std::fs::write(&config, example).expect("config");
    let config = config.to_str().expect("utf-8").to_owned();
    let rendered = dir.join("rendered");
    let rendered = rendered.to_str().expect("utf-8").to_owned();
    let mut exec = FakeExec::new(501);
    let args = ["envoy", "render", "--config", &config, "--out", &rendered];
    let (code, out, err) = cli(&mut exec, &no_env, &args);
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""), "{out}");
    assert_eq!(
        out,
        format!("rendered manifests.json envoy.json into {rendered}\n")
    );

    std::fs::write(&config, example.replace("KBF_SERVERS=", "KBF_SERVER=")).expect("plant");
    let (code, out, err) = cli(&mut exec, &no_env, &args);
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""), "{err}");
    assert!(
        err.starts_with("REFUSED: line ") && err.ends_with(": unknown key KBF_SERVER\n"),
        "{err}"
    );
}

/// Catches `envoy check` not wired to the kit, a check that reads another file than
/// `--dir`'s manifests.json (or no render without `--dir`), a failure printed
/// elsewhere than stdout or with an exit other than 1, and an unreadable or non-JSON
/// render or a bad setting exiting other than 2 with a `REFUSED:` line.
#[test]
fn envoy_check_exits_0_passed_1_failed_2_refused() {
    let dir = kbf_host::testkit::scratch("cli-envoy-check");
    let config = dir.join("config.local");
    let example = include_str!("../templates/envoy/config.example");
    std::fs::write(&config, example).expect("config");
    let config = config.to_str().expect("utf-8").to_owned();
    let rendered = dir.join("rendered");
    let rendered = rendered.to_str().expect("utf-8").to_owned();
    let mut exec = FakeExec::new(501);
    let passed = "12 methods routed, every check passed\n";

    let (code, out, err) = cli(&mut exec, &no_env, &["envoy", "check", "--config", &config]);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (ExitCode::SUCCESS, passed, "")
    );

    let check = ["envoy", "check", "--config", &config, "--dir", &rendered];
    let (code, out, err) = cli(&mut exec, &no_env, &check);
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""), "{err}");
    assert!(
        err.starts_with(&format!("REFUSED: cannot read {rendered}/manifests.json: ")),
        "{err}"
    );

    let render = ["envoy", "render", "--config", &config, "--out", &rendered];
    assert_eq!(cli(&mut exec, &no_env, &render).0, ExitCode::SUCCESS);
    let (code, out, err) = cli(&mut exec, &no_env, &check);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (ExitCode::SUCCESS, passed, "")
    );

    let manifests = format!("{rendered}/manifests.json");
    let text = std::fs::read_to_string(&manifests).expect("rendered");
    let class = "\"ingressClassName\": \"tailscale\"";
    assert_eq!(text.matches(class).count(), 1);
    std::fs::write(
        &manifests,
        text.replace(class, "\"ingressClassName\": \"nginx\""),
    )
    .expect("plant");
    let (code, out, err) = cli(&mut exec, &no_env, &check);
    assert_eq!((code, err.as_str()), (ExitCode::FAILURE, ""), "{out}");
    assert_eq!(
        out,
        "FAIL ingress-class: ingressClassName nginx is not the tailscale IngressClass\n"
    );

    std::fs::write(&manifests, "{").expect("plant");
    let (code, out, err) = cli(&mut exec, &no_env, &check);
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""), "{err}");
    assert!(
        err.starts_with(&format!("REFUSED: {manifests} is not JSON: ")),
        "{err}"
    );

    std::fs::write(&config, example.replace("KBF_SERVERS=", "KBF_SERVER=")).expect("plant");
    let (code, out, err) = cli(&mut exec, &no_env, &check);
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""), "{err}");
    assert!(err.ends_with(": unknown key KBF_SERVER\n"), "{err}");
}

/// Catches `mdm erase-request` not wired to the kit (or to the clock and the random
/// nonce), a pair that does not check exiting other than 1, and a refusal other than
/// 2. In-process, so the lib tests' instantiation of `run` covers the arm.
#[test]
fn mdm_erase_request_signs_and_its_refusals_exit_2() {
    use kbf_host::exec::Entry;
    use kbf_mdm::signers::{FLAG_USER_PRESENT, NAMESPACE};
    use kbf_mdm::testkit::SkKey;

    let key = SkKey::new(9);
    let mut exec = FakeExec::new(501);
    let public = HostPath::new("/srv/op/id_erase_sk.pub").expect("path");
    let bytes = format!("{}\n", key.openssh()).into_bytes();
    exec.entries.insert(
        public,
        Entry::File {
            perms: kbf_host::step::Perms::new(0o644),
            owner: kbf_host::testkit::RUNNER,
            bytes,
        },
    );
    let flags = Rc::new(RefCell::new(FLAG_USER_PRESENT));
    let seen = Rc::clone(&flags);
    exec.answer = Box::new(move |cmd| {
        let sig = key.sign_with(NAMESPACE, &cmd.stdin, *seen.borrow());
        Ok(reply(0, &sig, ""))
    });
    let base = [
        "mdm",
        "erase-request",
        "--serial",
        "C02ABC123XYZ",
        "--reason",
        "leak after reboot",
        "--key",
        "/srv/op/id_erase_sk",
    ];
    let with = |out: &str, extra: &[&'static str]| -> Vec<String> {
        let mut v: Vec<String> = base.iter().map(|s| (*s).to_owned()).collect();
        v.extend(["--out".to_owned(), out.to_owned()]);
        v.extend(extra.iter().map(|s| (*s).to_owned()));
        v
    };
    let call = |exec: &mut FakeExec, v: &[String]| {
        let v: Vec<&str> = v.iter().map(String::as_str).collect();
        cli(exec, &no_env, &v)
    };

    let (code, out, err) = call(&mut exec, &with("/srv/op/a.req", &["--apply"]));
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(
        out.starts_with("erase request for C02ABC123XYZ, acceptable until "),
        "{out}"
    );
    assert!(out.contains("    purpose erase-now\n"), "{out}");
    assert!(
        out.contains("+ run /usr/bin/ssh-keygen -Y sign -n kbf-mdm-erase"),
        "{out}"
    );
    assert!(
        out.contains("send /srv/op/a.req and /srv/op/a.req.sig"),
        "{out}"
    );

    *flags.borrow_mut() = 0;
    let (code, _, err) = call(&mut exec, &with("/srv/op/b.req", &["--apply"]));
    assert_eq!(code, ExitCode::FAILURE, "{err}");
    assert!(
        err.starts_with(
            "kbf-deploy: /srv/op/b.req and /srv/op/b.req.sig would be refused by the gate"
        ),
        "{err}"
    );

    let (code, out, err) = call(&mut exec, &with("/srv/op/c.req", &["--valid-for", "61"]));
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""), "{err}");
    assert!(err.starts_with("REFUSED: --valid-for 61"), "{err}");

    let (code, _, err) = call(&mut exec, &with("c.req", &[]));
    assert_eq!(code, ExitCode::from(2), "{err}");
    assert!(err.contains("is not absolute"), "{err}");
}
