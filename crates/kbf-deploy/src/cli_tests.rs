use std::cell::RefCell;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;

use super::*;
use kbf_host::testkit::{FakeExec, reply};
use kbf_testkit::dir::TestDir;

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

/// The instant the tests that read the clock run at: 2026-01-01T00:00:00Z.
const NOW_SECS: u64 = 1_767_225_600;

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW_SECS)
}

/// Exit status, stdout and stderr of `run` over `exec`, at [`now`].
fn cli(
    exec: &mut FakeExec,
    env: &dyn Fn(&str) -> Option<OsString>,
    args: &[&str],
) -> (ExitCode, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_at(argv(args), exec, env, now(), &mut out, &mut err);
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

const LINUX_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/templates/linux/swap.example");

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
    let refused = [
        // The fake's default answer, exit 0, is every kubelet active.
        (base("swap"), "kubelet.service is active"),
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

    let conflicts = [[base("swap"), vec!["--remove", "--kubelet-allows-swap"]].concat()];
    for args in conflicts {
        let mut exec = FakeExec::new(0);
        let (code, _, _) = cli(&mut exec, &no_env, &args);
        assert_eq!(code, ExitCode::from(2), "{args:?}");
        assert!(exec.queries.borrow().is_empty(), "{args:?} asked the host");
    }

    // verify-cert reaches its kit: a CA that is not there is refused; a daemon's row
    // needs its node id.
    let mut exec = FakeExec::new(0);
    let args = [
        "linux",
        "verify-cert",
        "--row",
        "storage",
        "--cert",
        "/srv/none/storage.pem",
        "--ca-cert",
        "/srv/none/ca.pem",
    ];
    let (code, _, err) = cli(&mut exec, &no_env, &args);
    assert_eq!(code, ExitCode::from(2));
    assert!(
        err.starts_with("REFUSED: cannot read /srv/none/ca.pem"),
        "{err}"
    );
    let args = [
        "linux",
        "verify-cert",
        "--row",
        "daemon",
        "--cert",
        "/srv/none/node.pem",
        "--ca-cert",
        "/srv/none/ca.pem",
    ];
    let (code, _, err) = cli(&mut exec, &no_env, &args);
    assert_eq!(code, ExitCode::from(2));
    assert!(err.contains("--node-id"), "{err}");
    // The frontend's REAPI certificate is not the farm CA's: no row checks it.
    let args = [
        "linux",
        "verify-cert",
        "--row",
        "frontend-reapi",
        "--cert",
        "/srv/none/reapi.pem",
        "--ca-cert",
        "/srv/none/ca.pem",
    ];
    let (code, out, err) = cli(&mut exec, &no_env, &args);
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""));
    assert!(
        err.contains("invalid value 'frontend-reapi' for '--row <ROW>'"),
        "{err}"
    );
    assert!(!err.contains("REFUSED: cannot read"), "{err}");

    // A swapfile to remove, without --apply: a dry run plans, exits 0.
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|_| Ok(reply(0, "", "")));
    exec.entries.insert(
        HostPath::new("/swapfile").expect("path"),
        kbf_host::exec::Entry::Other,
    );
    let (code, out, err) = cli(
        &mut exec,
        &no_env,
        &[base("swap"), vec!["--remove"]].concat(),
    );
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(out.contains("DRY-RUN: "), "{out}");

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

/// Catches a rollback directory name that is not the seconds since the epoch of the
/// instant it is given (a stamp in milliseconds, or one read from anywhere but the
/// clock the run was handed).
#[test]
fn the_rollback_stamp_is_unix_seconds() {
    assert_eq!(stamp(now()), NOW_SECS.to_string());
    assert_eq!(stamp(UNIX_EPOCH), "0");
}

/// Catches a retired units verb still taken by the command line (or answered by
/// anything but clap's usage error, exit 2 with nothing asked of the host or written):
/// an operator or script that still calls `render-units`, `check-units` or
/// `install-units` must fail loudly, not find a verb that quietly does something else.
#[test]
fn the_retired_units_verbs_are_usage_errors() {
    // Every argument the base's verb required, so that on the base each one parses.
    let verbs: [(&str, &[&str]); 3] = [
        ("render-units", &["--out", "/nonexistent/kbf-deploy/units"]),
        ("check-units", &["--dir", "/nonexistent/kbf-deploy/units"]),
        ("install-units", &["--apply"]),
    ];
    for (verb, rest) in verbs {
        let mut exec = FakeExec::new(0);
        let mut args = vec!["linux", verb, "--mode", "daemon", "--config", LINUX_CONFIG];
        args.extend(rest);
        let (code, out, err) = cli(&mut exec, &no_env, &args);
        assert_eq!(
            (code, out.as_str()),
            (ExitCode::from(2), ""),
            "{verb}: {err}"
        );
        assert!(err.contains(verb), "{verb}: {err}");
        assert!(exec.queries.borrow().is_empty(), "{verb} asked the host");
        assert!(exec.log.is_empty(), "{verb} changed the host");
    }
}

/// Catches `linux swap` refusing an existing node.conf, or taking it without a word:
/// the retired keys are ignored with one warning on stderr naming them.
#[test]
fn swap_ignores_the_retired_keys_of_an_old_node_conf_and_says_so() {
    let dir = kbf_host::testkit::scratch("cli-swap-old-conf");
    let file = dir.join("node.conf");
    std::fs::write(&file, include_str!("linux/testdata/node.conf")).expect("write");
    let mut exec = FakeExec::new(501);
    let args = ["linux", "swap", "--config", file.to_str().expect("utf-8")];
    let (_, _, err) = cli(&mut exec, &no_env, &args);
    assert!(!err.contains("unknown key"), "{err}");
    assert_eq!(err.matches("no longer read").count(), 1, "{err}");
    assert!(
        err.contains("KBF_HOST_NAME") && err.contains("docs/deploy/kbf-units.md"),
        "{err}"
    );
    assert!(!err.contains("KBF_SWAP_FILE,"), "{err}");
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
    let own = TestDir::under(std::env::temp_dir(), "kbf-deploy-cli-fetch");
    let dir = own.join("archives");
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
    let via = kbf_testkit::port::dead_addr().to_string();
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

/// Catches the `pilot` kit dropped from the merged command line (clap would exit 2 on
/// an unknown subcommand, with no `REFUSED:`), and its refusals not reaching stderr
/// as `REFUSED:` through `run`; this also runs `run`'s pilot arm in the lib tests'
/// instantiation, the one the coverage ratchet counts.
#[test]
fn a_pilot_verb_runs_through_the_same_command_line() {
    let mut exec = FakeExec::new(0);
    let config = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/templates/pilot/config.example"
    );
    let args = [
        "pilot",
        "server-install",
        "--config",
        config,
        "--bin",
        "/opt/kbf-server",
    ];
    let home = |k: &str| (k == "HOME").then(|| OsString::from("/srv/op"));
    let (code, out, err) = cli(&mut exec, &home, &args);
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""));
    assert_eq!(
        err,
        "REFUSED: this verb installs a systemd user service: run it as yourself, not root (no sudo)\n"
    );
    assert!(exec.log.is_empty() && exec.queries.borrow().is_empty());
}

/// Catches `mdm erase-request` not wired to the kit, a pair that does not check
/// exiting other than 1, and a refusal other than 2. Also catches the verb not wired to
/// the clock: the signed request's not-after must equal the run's instant plus 15 min
/// (a stuck or zero clock, or a wrong offset, fails). And catches a fixed or counted
/// nonce: two signed requests must carry nonces that differ in at least 8 of their 16
/// bytes (equal nonces make the gate's replay check refuse every request after the
/// first; a counter differs in one byte).
/// In-process, so the lib tests' instantiation of `run` covers the arm.
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
    let signed = Rc::new(RefCell::new(Vec::<Vec<u8>>::new()));
    let log = Rc::clone(&signed);
    exec.answer = Box::new(move |cmd| {
        log.borrow_mut().push(cmd.stdin.clone());
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

    let (code, _, err) = call(&mut exec, &with("/srv/op/a2.req", &["--apply"]));
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    let requests: Vec<kbf_mdm::request::EraseRequest> = signed
        .borrow()
        .iter()
        .map(|b| kbf_mdm::request::parse(std::str::from_utf8(b).expect("utf-8")).expect("parse"))
        .collect();
    assert_eq!(requests.len(), 2);
    let not_after = requests[0].not_after;
    assert_eq!(
        not_after,
        i64::try_from(NOW_SECS).expect("secs") + 15 * 60,
        "not-after is the run's instant plus 900 s"
    );
    let nonce = |r: &kbf_mdm::request::EraseRequest| hex::decode(&r.nonce).expect("hex");
    let (n1, n2) = (nonce(&requests[0]), nonce(&requests[1]));
    assert_eq!((n1.len(), n2.len()), (16, 16));
    let differ = n1.iter().zip(&n2).filter(|(a, b)| a != b).count();
    assert!(
        differ >= 8,
        "nonces {} and {} differ in {differ} bytes",
        requests[0].nonce,
        requests[1].nonce
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

/// Catches a `tls`, `push-cert` or `dep` verb not wired to its kit (each reaches the
/// kit's root check, so each refuses as a user with a `REFUSED:` line and exit 2), a
/// `--check-godaddy` that skips that check, and flags clap must refuse: a topic
/// without `--profile-only` or malformed, a push certificate without its key, a
/// malformed serial, `--check-godaddy` with `--apply`.
#[test]
fn the_tls_push_cert_and_dep_verbs_reach_their_kit() {
    let dir = kbf_host::testkit::scratch("cli_mdm_more");
    let file = dir.join("config.local");
    let text = crate::mdm::tests::config_text()
        .replace("MDM_ACME_EMAIL=\n", "MDM_ACME_EMAIL=ops@example.com\n")
        .replace(
            "MDM_ACME_DNS_PROVIDER=\n",
            "MDM_ACME_DNS_PROVIDER=godaddy\n",
        );
    std::fs::write(&file, text).expect("write");
    let config = file.to_string_lossy().into_owned();
    let topic = "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d";
    let c = config.as_str();
    let refused: [&[&str]; 8] = [
        &["mdm", "tls", "--config", c],
        &["mdm", "tls", "--config", c, "--check-godaddy"],
        &[
            "mdm",
            "push-cert",
            "--config",
            c,
            "--profile-only",
            "--topic",
            topic,
        ],
        &[
            "mdm",
            "push-cert",
            "--config",
            c,
            "--cert",
            "/srv/k/p.pem",
            "--key",
            "/srv/k/p.key",
        ],
        &[
            "mdm",
            "dep",
            "get-cert",
            "--config",
            c,
            "--out",
            "/srv/abm/c.pem",
        ],
        &[
            "mdm",
            "dep",
            "upload-token",
            "--config",
            c,
            "--token",
            "/srv/abm/t.p7m",
        ],
        &["mdm", "dep", "define-profile", "--config", c, "--apply"],
        &[
            "mdm",
            "dep",
            "assign",
            "--config",
            c,
            "--serial",
            "C02AB12CD3EF",
        ],
    ];
    for args in refused {
        let mut exec = FakeExec::new(501);
        let (code, _, err) = cli(&mut exec, &no_env, args);
        assert_eq!(code, ExitCode::from(2), "{args:?}: {err}");
        assert!(
            err.starts_with("REFUSED: mdm ") && err.contains("run it with sudo"),
            "{args:?}: {err}"
        );
    }
    let example = mdm_config("cli_mdm_acme");
    let mut exec = FakeExec::new(0);
    let (code, _, err) = cli(&mut exec, &no_env, &["mdm", "tls", "--config", &example]);
    assert_eq!(code, ExitCode::from(2));
    assert!(err.contains("MDM_ACME_EMAIL is not set"), "{err}");
    let bad_flags: [&[&str]; 5] = [
        &["mdm", "push-cert", "--config", c, "--topic", topic],
        &[
            "mdm",
            "push-cert",
            "--config",
            c,
            "--profile-only",
            "--topic",
            "com.apple.x",
        ],
        &["mdm", "push-cert", "--config", c, "--cert", "/srv/k/p.pem"],
        &[
            "mdm",
            "dep",
            "assign",
            "--config",
            c,
            "--serial",
            "c02ab12cd3ef",
        ],
        &["mdm", "tls", "--config", c, "--check-godaddy", "--apply"],
    ];
    for args in bad_flags {
        let mut exec = FakeExec::new(0);
        let (code, _, err) = cli(&mut exec, &no_env, args);
        assert_eq!(code, ExitCode::from(2), "{args:?}: {err}");
        assert!(exec.queries.borrow().is_empty(), "{args:?}");
    }
}

/// Catches the retired `envoy` kit coming back or being accepted under another
/// spelling: kbf fronts its frontend with kbf-proxy and runs no Envoy, so
/// `kbf-deploy envoy render|check|apply` must be refused by the parser (exit 2, clap's
/// "unrecognized subcommand" on stderr, nothing on stdout) and run nothing.
#[test]
fn the_retired_envoy_kit_is_an_unknown_subcommand() {
    for args in [
        &["envoy", "render", "--config", "/c", "--out", "/o"][..],
        &["envoy", "check", "--config", "/c"][..],
        &["envoy", "apply", "--config", "/c"][..],
        &["envoy"][..],
    ] {
        let mut exec = FakeExec::new(0);
        let (code, out, err) = cli(&mut exec, &no_env, args);
        assert_eq!(code, ExitCode::from(2), "{args:?}: {err}");
        assert_eq!(out, "", "{args:?}");
        assert!(
            err.starts_with("error: unrecognized subcommand 'envoy'"),
            "{args:?}: {err}"
        );
        assert!(exec.queries.borrow().is_empty(), "{args:?}");
    }
}
