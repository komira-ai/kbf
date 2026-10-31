//! `preflight`: every check on a node that is right, and each check on a node that is wrong in
//! that one way.

mod ops_support;

use std::fs;

use tierstore_bench::error::BenchError;
use tierstore_bench::preflight::{
    self, PreflightReport, Snapshot, gather, gather_at, heal_state, instance_leftovers, run_with,
    why,
};
use tierstore_bench::result::Check;

use ops_support::*;

fn find<'a>(report: &'a PreflightReport, name: &str) -> &'a Check {
    report
        .checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no check {name}: {:?}", report.checks))
}

fn failing(report: &PreflightReport) -> Vec<&str> {
    report
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.name.as_str())
        .collect()
}

/// Catches a check that is missing from the run, one that fails on a right node, and a report
/// that leaves out the digest, the versions, the heal state or the hardware of a role: every
/// check by name, the facts a result's provenance is made of.
#[test]
fn a_right_node_passes_every_check_and_the_report_carries_the_facts() {
    let (_dir, host, config, ops) = base("tb-pre-healthy");
    let report = gather(&host, &config, &ops);
    let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "pinned-binary-digest",
            "plan-flags-in-help",
            "cold-only-mode",
            "live-binary-read",
            "live-unit-read",
            "store-version:cold",
            "store-version:hot",
            "cold-heal-state",
            "node-facts",
            "load-gate",
            "test-instance-isolated",
            "test-instance-absent",
            "paper-scan",
        ]
    );
    assert!(report.passed(), "{:?}", failing(&report));

    assert_eq!(report.binary_digest, hex_of(b"pinned binary"));
    assert_eq!(report.tool_versions["tierstore"], "0.0.37");
    assert_eq!(report.tool_versions["cold-store"], "RELEASE.mark");
    assert_eq!(report.tool_versions["hot-store"], "RELEASE.mark");
    assert_eq!(report.tool_versions["cold-heal"], "healing 6% done");
    let roles: Vec<String> = report.hardware.keys().map(ToString::to_string).collect();
    assert_eq!(roles, ["instance", "generator"]);
    let hw = &report.hardware[&tierstore_bench::role::Role::Instance];
    assert_eq!(hw.cpu_cores, 2);
    assert_eq!(hw.memory_bytes, 16_384_000 * 1024);
    assert_eq!(hw.link_mbps, Some(10_000));

    let facts = &report.snapshot.facts;
    assert_eq!(facts["pinned_binary_sha256"], hex_of(b"pinned binary"));
    assert_eq!(facts["live_binary_sha256"], hex_of(b"live binary"));
    assert_eq!(facts.len(), 4);

    let p = report.provenance("run-1", "abc123");
    assert_eq!(p.run_id, "run-1");
    assert_eq!(p.git_sha, "abc123");
    assert_eq!(p.binary_digest, report.binary_digest);
    assert_eq!(p.tool_versions, report.tool_versions);
    assert_eq!(p.hardware, report.hardware);
}

/// Catches an admin secret that reaches a command line: the admin key pair is in the environment
/// of `mc` only, and `mc` keeps its own files in the result directory.
#[test]
fn the_admin_secret_is_in_mc_s_environment_and_never_on_a_command_line() {
    let (dir, host, config, ops) = base("tb-pre-env");
    let _ = gather(&host, &config, &ops);
    let mc: Vec<_> = host
        .ran()
        .into_iter()
        .filter(|c| c.program.ends_with("mc"))
        .collect();
    let mut envs: Vec<String> = mc.iter().map(|c| c.env[0].1.clone()).collect();
    envs.sort();
    assert_eq!(
        envs,
        [
            "http://ADMIN-KEYID-MARK:ADMIN-SECRET-MARK@host-mark:9000",
            "http://ADMIN-KEYID-MARK:ADMIN-SECRET-MARK@host-mark:9001"
        ]
    );
    let config_dir = dir.path().join("mc").display().to_string();
    for cmd in &mc {
        assert_eq!(cmd.env[0].0, "MC_HOST_tbops");
        assert!(cmd.args.iter().all(|a| !a.contains("SECRET")), "{cmd:?}");
        assert!(cmd.args.contains(&config_dir));
        assert!(!format!("{cmd:?}").contains("SECRET"));
    }
}

/// Catches `run_with` that does not write what `verify-teardown --before` reads, that lets a
/// private name into a file, or that ends a right preflight in an error: the two files, parsed
/// back equal, and grepped for every marker of a private name.
#[test]
fn the_report_and_the_snapshot_are_written_and_name_nothing_private() {
    let (dir, host, config, ops) = base("tb-pre-files");
    let report = run_with(&host, &config, &ops).unwrap();
    let text = fs::read_to_string(dir.path().join("preflight.json")).unwrap();
    let back: PreflightReport = serde_json::from_str(&text).unwrap();
    assert_eq!(back, report);
    let snap = fs::read_to_string(dir.path().join("before.json")).unwrap();
    let back: Snapshot = serde_json::from_str(&snap).unwrap();
    assert_eq!(back, report.snapshot);
    for word in PRIVATE {
        assert!(!text.contains(word), "{word} in the report");
        assert!(!snap.contains(word), "{word} in the snapshot");
    }
}

/// Catches a verb that exits 0 when a check failed: the error names the verb and the check, and
/// the files are written first so the operator can read what failed.
#[test]
fn a_failed_check_ends_the_verb_in_an_error_after_the_files_are_written() {
    let (dir, host, config, ops) = base("tb-pre-failed");
    host.file("/private-mark/pinned/tierstore", "another build");
    let err = run_with(&host, &config, &ops).unwrap_err();
    let BenchError::Ops(text) = err else {
        panic!("{err:?}");
    };
    assert!(
        text.starts_with("preflight: 1 of 13 checks failed: pinned-binary-digest"),
        "{text}"
    );
    assert!(dir.path().join("preflight.json").exists());
    assert!(dir.path().join("before.json").exists());
}

/// Catches a binary that is not the pinned one passing, and a digest check that stops the other
/// checks: the digest fails with both digests' meaning and the flags are still read.
#[test]
fn a_binary_that_is_not_the_pinned_one_fails_the_digest_and_nothing_else() {
    let (_dir, host, config, ops) = base("tb-pre-digest");
    host.file("/private-mark/pinned/tierstore", "another build");
    let report = gather(&host, &config, &ops);
    assert_eq!(failing(&report), ["pinned-binary-digest"]);
    assert!(
        find(&report, "pinned-binary-digest")
            .detail
            .contains("is not the pinned digest")
    );
    assert!(!report.snapshot.facts.is_empty());

    // The pinned digest in the file may be upper case.
    let (_dir, host, config, mut ops) = base("tb-pre-digest-case");
    ops.tierstore.sha256 = ops.tierstore.sha256.to_uppercase();
    assert!(gather(&host, &config, &ops).passed());

    // A binary that cannot be read fails, and records no digest.
    let (_dir, host, config, ops) = base("tb-pre-digest-missing");
    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/pinned/tierstore"));
    let report = gather(&host, &config, &ops);
    assert!(failing(&report).contains(&"pinned-binary-digest"));
    assert_eq!(report.binary_digest, "");
    assert!(!report.snapshot.facts.contains_key("pinned_binary_sha256"));
}

/// Catches a plan flag the binary does not have passing: the check names the flag that is
/// missing, and only that one.
#[test]
fn a_plan_flag_the_help_does_not_list_fails_by_name() {
    let (_dir, host, config, ops) = base("tb-pre-flags");
    host.answer(|cmd| {
        if cmd.program.ends_with("tierstore") && cmd.program.starts_with("/private-mark/pinned") {
            ok(&HELP_HOT_REQUIRED.replace("--cold-endpoint", "--cold-endpoin"))
        } else {
            ok("not-found\n")
        }
    });
    let report = gather(&host, &config, &ops);
    let flags = find(&report, "plan-flags-in-help");
    assert!(!flags.passed);
    assert_eq!(flags.detail, "not listed: --cold-endpoint");
}

/// Catches a cold-only finding that is wrong in either direction: `--hot` required says "not
/// possible" and passes; `--hot` listed but not required says "possible" and passes; and a help
/// that cannot be read fails the flags and the finding both.
#[test]
fn the_cold_only_finding_follows_whether_hot_is_required() {
    let (_dir, host, config, ops) = base("tb-pre-cold-only");
    let report = gather(&host, &config, &ops);
    let found = find(&report, "cold-only-mode");
    assert!(found.passed);
    assert_eq!(found.detail, "not possible: --hot is required");

    let optional = HELP_HOT_REQUIRED.replace(
        "Usage: tierstore [OPTIONS] --proxy-id <PROXY_ID> --hot <HOT> --cold <COLD> --hot-bytes <HOT_BYTES>",
        "Usage: tierstore [OPTIONS] --proxy-id <PROXY_ID> --cold <COLD> --hot-bytes <HOT_BYTES>",
    );
    host.answer(move |cmd| {
        if cmd.program.starts_with("/private-mark/pinned") {
            ok(&optional)
        } else {
            ok("not-found\n")
        }
    });
    let report = gather(&host, &config, &ops);
    assert_eq!(
        find(&report, "cold-only-mode").detail,
        "possible: --hot is optional"
    );

    host.answer(|_| failed(2, "no such thing"));
    let report = gather(&host, &config, &ops);
    for name in ["plan-flags-in-help", "cold-only-mode"] {
        let c = find(&report, name);
        assert!(!c.passed);
        assert_eq!(c.detail, "tierstore --help: exited Some(2)");
    }
}

/// Catches a live unit that is not read (no facts for the snapshot) and flag parity that does not
/// say which plan flags the live command line leaves unset: names only, no value.
#[test]
fn the_live_unit_is_read_for_its_flag_names_and_hashed_for_the_snapshot() {
    let (_dir, host, config, ops) = base("tb-pre-live");
    let report = gather(&host, &config, &ops);
    let unit = find(&report, "live-unit-read");
    assert_eq!(
        unit.detail,
        "3 flags set (--cold-endpoint --hot-bytes --proxy-id); plan flags not set live: "
    );
    assert!(!unit.detail.contains("host-mark"));
    assert_eq!(
        find(&report, "live-binary-read").detail,
        format!("sha256 {}", hex_of(b"live binary"))
    );

    host.answer(|cmd| {
        if cmd.program.starts_with("/private-mark/pinned") {
            ok(HELP_HOT_REQUIRED)
        } else if cmd.program.to_str() == Some("systemctl") {
            failed(1, "Failed to connect to bus")
        } else {
            ok(&info_json("v", false))
        }
    });
    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/live/tierstore"));
    let report = gather(&host, &config, &ops);
    assert!(!find(&report, "live-unit-read").passed);
    assert!(!find(&report, "live-binary-read").passed);
    assert_eq!(
        find(&report, "live-binary-read").detail,
        "cannot read the binary: NotFound"
    );
    assert_eq!(report.snapshot.facts.len(), 1, "only the pinned digest");
}

/// Catches a store check that hides why it failed: no admin key file, an unreadable one, an
/// admin tool that fails, and an answer without a version each fail their own check, and a
/// store that is not configured has no check.
#[test]
fn a_store_that_cannot_be_asked_fails_its_own_check_with_the_reason() {
    let (dir, host, _config, ops) = base("tb-pre-store");
    let only_cold = run_config(dir.path(), "http://host-mark:9000", None);
    let report = gather(&host, &only_cold, &ops);
    assert!(report.checks.iter().all(|c| c.name != "store-version:hot"));

    let mut no_key = ops.clone();
    no_key.admin.remove(&tierstore_bench::role::Role::Cold);
    no_key.cold_heal_status_file = None;
    let report = gather(&host, &only_cold, &no_key);
    assert_eq!(
        find(&report, "store-version:cold").detail,
        "the ops file has no admin key file for cold"
    );
    assert!(!find(&report, "cold-heal-state").passed);

    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/admin-cold.json"));
    let report = gather(&host, &only_cold, &ops);
    assert_eq!(
        find(&report, "store-version:cold").detail,
        "a file cannot be read: NotFound"
    );

    let (dir, host, _c, ops) = base("tb-pre-store-bad");
    let only_cold = run_config(dir.path(), "ftp://host-mark", None);
    let report = gather(&host, &only_cold, &ops);
    assert_eq!(
        find(&report, "store-version:cold").detail,
        "a store endpoint must be http://host[:port]"
    );

    let (dir, host, _c, ops) = base("tb-pre-store-fail");
    let only_cold = run_config(dir.path(), "http://host-mark:9000", None);
    host.answer(|cmd| {
        if cmd.program.ends_with("mc") {
            failed(1, "Unable to connect to host-mark")
        } else {
            ok("not-found\n")
        }
    });
    let report = gather(&host, &only_cold, &ops);
    assert_eq!(
        find(&report, "store-version:cold").detail,
        "mc admin info: exited Some(1)"
    );
    assert!(
        !find(&report, "store-version:cold")
            .detail
            .contains("host-mark")
    );

    host.answer(|cmd| {
        if cmd.program.ends_with("mc") {
            ok("not json")
        } else {
            ok("not-found\n")
        }
    });
    let report = gather(&host, &only_cold, &ops);
    assert!(
        find(&report, "store-version:cold")
            .detail
            .starts_with("mc admin info: not JSON")
    );

    host.answer(|cmd| {
        if cmd.program.ends_with("mc") {
            ok(r#"{"info": {"servers": []}}"#)
        } else {
            ok("not-found\n")
        }
    });
    let report = gather(&host, &only_cold, &ops);
    assert_eq!(
        find(&report, "store-version:cold").detail,
        "the admin info has no version"
    );
}

/// Catches a heal state that is lost: a healing drive with no status file says progress unknown,
/// a status file says percent, a store that cannot be asked is still answered by the status file,
/// and with neither the check fails; a finished heal and quiet drives say none.
#[test]
fn the_cold_heal_state_comes_from_the_drives_and_the_status_file() {
    let (dir, host, _config, ops) = base("tb-pre-heal");
    let only_cold = run_config(dir.path(), "http://host-mark:9000", None);

    let mut no_file = ops.clone();
    no_file.cold_heal_status_file = None;
    let report = gather(&host, &only_cold, &no_file);
    assert_eq!(
        find(&report, "cold-heal-state").detail,
        "healing, progress unknown"
    );

    // Admin info fails; the status file alone answers.
    host.answer(|cmd| {
        if cmd.program.ends_with("mc") {
            failed(1, "")
        } else {
            ok("not-found\n")
        }
    });
    let report = gather(&host, &only_cold, &ops);
    assert_eq!(find(&report, "cold-heal-state").detail, "healing 6% done");
    assert!(find(&report, "cold-heal-state").passed);

    // Neither answers.
    let report = gather(&host, &only_cold, &no_file);
    assert_eq!(
        find(&report, "cold-heal-state").detail,
        "neither the admin info nor a status file gives the heal state"
    );

    // A status file that is not one fails the check whatever the drives say.
    host.file("/private-mark/heal.json", "[]");
    let report = gather(&host, &only_cold, &ops);
    assert!(!find(&report, "cold-heal-state").passed);
    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/heal.json"));
    let report = gather(&host, &only_cold, &ops);
    assert_eq!(
        find(&report, "cold-heal-state").detail,
        "a file cannot be read: NotFound"
    );

    // Quiet drives and a finished heal.
    host.answer(|cmd| {
        if cmd.program.ends_with("mc") {
            ok(&info_json("v", false))
        } else {
            ok("not-found\n")
        }
    });
    host.file("/private-mark/heal.json", r#"{"percent_done": 100.0}"#);
    let report = gather(&host, &only_cold, &ops);
    assert_eq!(find(&report, "cold-heal-state").detail, "none");
}

/// Catches `heal_state` mixing up its inputs: each combination, as words.
#[test]
fn heal_state_reads_drives_and_percent_together() {
    let healing: serde_json::Value = serde_json::from_str(&info_json("v", true)).unwrap();
    let quiet: serde_json::Value = serde_json::from_str(&info_json("v", false)).unwrap();
    assert_eq!(heal_state(Some(&healing), Some(6.4)), "healing 6% done");
    assert_eq!(
        heal_state(Some(&healing), None),
        "healing, progress unknown"
    );
    assert_eq!(heal_state(Some(&quiet), Some(50.0)), "healing 50% done");
    assert_eq!(heal_state(Some(&quiet), Some(100.0)), "none");
    assert_eq!(heal_state(Some(&quiet), None), "none");
    assert_eq!(heal_state(None, None), "none");
}

/// Catches node facts that attach to no role, or that hide what could not be read.
#[test]
fn node_facts_need_a_role_and_readable_files() {
    let (_dir, host, config, ops) = base("tb-pre-node");
    let mut roleless = ops.clone();
    roleless.node.roles.clear();
    let report = gather(&host, &config, &roleless);
    assert_eq!(
        find(&report, "node-facts").detail,
        "the ops file names no role for this machine"
    );
    assert!(report.hardware.is_empty());

    host.file("/proc/meminfo", "MemFree: 1 kB\n");
    let report = gather(&host, &config, &ops);
    assert_eq!(
        find(&report, "node-facts").detail,
        "/proc/meminfo has no MemTotal"
    );

    let ok_report = gather(&healthy_host(), &config, &ops);
    assert_eq!(
        find(&ok_report, "node-facts").detail,
        "2 cores, 16777216000 bytes of memory, Debian GNU/Linux 12 (bookworm), kernel 6.1.0-mark, link Some(10000) Mbit/s"
    );
}

/// Catches a load gate that passes a machine over its limits, or fails on one that is calm, and
/// a load reading that cannot be made passing for a reading.
#[test]
fn the_load_gate_fails_on_a_busy_machine_and_on_no_reading() {
    let (_dir, host, config, ops) = base("tb-pre-load");
    let report = gather(&host, &config, &ops);
    assert_eq!(
        find(&report, "load-gate").detail,
        "load 0.50, cpu pressure 0.0, io pressure 0.0, disk 1.0%"
    );
    let mut busy = calm();
    busy.psi_io_avg10 = 55.0;
    *host.load.lock().unwrap() = Some(busy);
    let report = gather(&host, &config, &ops);
    assert!(!find(&report, "load-gate").passed);
    assert!(find(&report, "load-gate").detail.contains("psi_io_avg10"));
    *host.load.lock().unwrap() = None;
    let report = gather(&host, &config, &ops);
    assert_eq!(find(&report, "load-gate").detail, "no load sample");
}

/// Catches a paper that drifted from its map unnoticed, in either direction, and a paper that
/// cannot be read passing: lines are named, and without `paper` in the ops file there is no check.
#[test]
fn the_paper_scan_names_the_lines_that_disagree() {
    let (_dir, host, config, ops) = base("tb-pre-paper");
    let report = gather(&host, &config, &ops);
    assert_eq!(
        find(&report, "paper-scan").detail,
        "2 open places, every one in the map"
    );

    host.file(
        "/private-mark/paper.html",
        "a\nclass=\"tbd\"\nb\nc\nNOT MEASURED\n",
    );
    let report = gather(&host, &config, &ops);
    let scan = find(&report, "paper-scan");
    assert!(!scan.passed);
    assert!(
        scan.detail.contains("not in the map: [5]"),
        "{}",
        scan.detail
    );
    assert!(
        scan.detail.contains("not open in the paper: [4]"),
        "{}",
        scan.detail
    );

    host.file("/private-mark/paper-map.json", "{}");
    let report = gather(&host, &config, &ops);
    assert!(
        find(&report, "paper-scan")
            .detail
            .contains("not a JSON array")
    );

    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/paper.html"));
    let report = gather(&host, &config, &ops);
    assert_eq!(
        find(&report, "paper-scan").detail,
        "a file cannot be read: NotFound"
    );
    host.file("/private-mark/paper.html", PAPER);
    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/paper-map.json"));
    let report = gather(&host, &config, &ops);
    assert_eq!(
        find(&report, "paper-scan").detail,
        "a file cannot be read: NotFound"
    );

    let mut none = ops.clone();
    none.paper = None;
    let report = gather(&host, &config, &none);
    assert!(report.checks.iter().all(|c| c.name != "paper-scan"));
}

/// Catches a test instance that could collide with the live one: the live unit's name, the live
/// port (the one in the ops file and 9300 itself) and a directory that holds, is, or lies inside
/// a live one each fail, and an instance of its own passes.
#[test]
fn a_test_instance_that_shares_anything_with_the_live_one_fails_isolation() {
    let (_dir, host, config, ops) = base("tb-pre-isolation");
    let report = gather(&host, &config, &ops);
    assert_eq!(
        find(&report, "test-instance-isolated").detail,
        "own unit, own port, own directories"
    );
    assert!(ops.isolation_problems().is_empty());

    let mut same_unit = ops.clone();
    same_unit.test_instance.unit = same_unit.live.unit.clone();
    assert_eq!(
        same_unit.isolation_problems(),
        ["the test unit has the live unit's name"]
    );

    let mut live_port = ops.clone();
    live_port.test_instance.port = 9300;
    assert_eq!(
        live_port.isolation_problems(),
        ["the test port is a live port"]
    );
    let mut moved_live = ops.clone();
    moved_live.live.port = 9411;
    assert_eq!(
        moved_live.isolation_problems(),
        ["the test port is a live port"]
    );

    let mut live_elsewhere = ops.clone();
    live_elsewhere.live.port = 9000;
    live_elsewhere.test_instance.port = 9300;
    assert_eq!(
        live_elsewhere.isolation_problems(),
        ["the test port is a live port"]
    );

    let mut inside = ops.clone();
    inside.test_instance.dirs = vec!["/private-mark/live-state/sub".into()];
    assert_eq!(
        inside.isolation_problems(),
        ["a test directory overlaps a live directory"]
    );
    let mut holds = ops.clone();
    holds.test_instance.dirs = vec!["/private-mark".into()];
    assert_eq!(
        holds.isolation_problems(),
        ["a test directory overlaps a live directory"]
    );

    let report = gather(&host, &config, &holds);
    assert!(!find(&report, "test-instance-isolated").passed);
}

/// Catches a test instance that already exists passing as absent: the loaded unit, the open port
/// and the directory are each reported, and a unit state that cannot be read fails.
#[test]
fn a_test_instance_that_already_exists_is_reported_in_each_part() {
    let (_dir, host, config, ops) = base("tb-pre-absent");
    assert_eq!(
        find(&gather(&host, &config, &ops), "test-instance-absent").detail,
        "no unit, nothing on the port, no directory"
    );
    assert_eq!(
        instance_leftovers(&host, &ops.test_instance).unwrap(),
        Vec::<String>::new()
    );

    host.open_ports.lock().unwrap().insert(9411);
    host.dir("/private-mark/bench-data");
    host.answer(|cmd| {
        if cmd.args.last().is_some_and(|a| a == "--value") {
            ok("loaded\n")
        } else {
            ok("")
        }
    });
    let left = instance_leftovers(&host, &ops.test_instance).unwrap();
    assert_eq!(
        left,
        [
            "the unit is loaded",
            "the port accepts connections",
            "a directory exists"
        ]
    );
    let report = gather(&host, &config, &ops);
    assert!(!find(&report, "test-instance-absent").passed);

    host.answer(|_| failed(1, ""));
    assert!(instance_leftovers(&host, &ops.test_instance).is_err());
    let report = gather(&host, &config, &ops);
    assert_eq!(
        find(&report, "test-instance-absent").detail,
        "systemctl show: exited Some(1)"
    );
}

/// Catches `why` leaking: a file error names its class only, a refused store configuration says
/// so without the endpoint or the bucket, and anything else is its own text.
#[test]
fn why_names_no_path_endpoint_or_bucket() {
    let io = BenchError::Io {
        path: "/private-mark/x".into(),
        source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    };
    assert_eq!(why(&io), "a file cannot be read: PermissionDenied");
    let spec = tierstore_bench::config::StoreSpec {
        endpoint: "ftp://host-mark".to_owned(),
        bucket: "bucket-mark".to_owned(),
        region: "r".to_owned(),
        access_key_id: "k".to_owned(),
        secret_access_key: tierstore_bench::config::Secret::new("s"),
    };
    let store = tierstore_bench::s3::BenchS3::new(
        tierstore_bench::role::Role::Cold,
        &spec,
        tierstore_bench::s3::Timeouts::default(),
        std::sync::Arc::new(tierstore_bench::requestlog::RequestLog::to_writer(
            std::io::sink(),
        )),
    )
    .err()
    .unwrap();
    assert_eq!(
        why(&store),
        "a store's endpoint or bucket name is not accepted"
    );
    assert_eq!(why(&BenchError::Ops("x".into())), "x");
}

/// Catches `check` that loses the text of what was found.
#[test]
fn check_keeps_the_text_and_the_verdict() {
    let pass = preflight::check("a", Ok("fine".to_owned()));
    assert!(pass.passed);
    assert_eq!((pass.name.as_str(), pass.detail.as_str()), ("a", "fine"));
    let fail = preflight::check("b", Err("bad".to_owned()));
    assert!(!fail.passed);
    assert_eq!(fail.detail, "bad");
    assert!(preflight::all_passed("v", std::slice::from_ref(&pass)).is_ok());
    let err = preflight::all_passed("v", &[pass, fail]).unwrap_err();
    assert_eq!(err.to_string(), "v: 1 of 2 checks failed: b");
}

/// Catches an `mc` that is looked up in a place the operator did not name: with no `mc` in the
/// ops file the command is `mc`, found on `PATH`.
#[test]
fn without_an_mc_path_the_command_is_mc() {
    let (_dir, host, config, mut ops) = base("tb-pre-mc-path");
    ops.mc = None;
    let _ = gather(&host, &config, &ops);
    let ran = host.ran();
    let mc: Vec<_> = ran
        .iter()
        .filter(|c| c.args.contains(&"info".to_owned()))
        .collect();
    assert_eq!(mc.len(), 2);
    assert!(mc.iter().all(|c| c.program.to_str() == Some("mc")));
}

/// Catches a heal check on a run that has no cold store: with the hot store only there is a
/// version check for it and no heal state.
#[test]
fn a_run_without_a_cold_store_has_no_heal_check() {
    let (dir, host, _config, ops) = base("tb-pre-hot-only");
    let config = hot_only(dir.path(), "http://host-mark:9001");
    let report = gather(&host, &config, &ops);
    assert!(find(&report, "store-version:hot").passed);
    assert!(report.checks.iter().all(|c| c.name != "cold-heal-state"));
    assert!(!report.tool_versions.contains_key("cold-heal"));
}

/// Catches a report that cannot be written going unreported: a result directory under a file, and
/// a report path that is a directory, each end the verb in a file error.
#[test]
fn a_report_that_cannot_be_written_is_an_error() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-pre-write");
    let file = dir.path().join("a-file");
    fs::write(&file, "x").unwrap();
    let err = preflight::write_json(&file.join("out").join("r.json"), &"x").unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    fs::create_dir_all(dir.path().join("r.json")).unwrap();
    let err = preflight::write_json(&dir.path().join("r.json"), &"x").unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    // A path whose directory exists is written.
    let ok = preflight::write_json(&dir.path().join("fine.json"), &"x");
    assert!(ok.is_ok());
}

/// Catches `run` that does not read the ops file of the result directory or that goes on without
/// one: on the real machine with the fixture's private paths, which do not exist, every check
/// that needs them fails, the files are written, and the verb ends in an error.
#[tokio::test]
async fn run_reads_the_ops_file_and_on_a_machine_without_the_fixtures_it_fails_its_checks() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-pre-run");
    let config = run_config(dir.path(), "http://host-mark:9000", None);
    let err = preflight::run(&config).await.unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");

    fs::write(dir.path().join("ops.json"), ops_json().to_string()).unwrap();
    let err = preflight::run(&config).await.unwrap_err();
    let BenchError::Ops(text) = err else {
        panic!("{err:?}");
    };
    assert!(text.starts_with("preflight: "), "{text}");
    assert!(text.contains("pinned-binary-digest"), "{text}");
    assert!(dir.path().join("preflight.json").exists());
    assert!(dir.path().join("before.json").exists());
}

/// The cold store only, file mode: no cold admin entry, the status file `text`, any command
/// refused.
fn file_mode_report(name: &str, text: &str, now_ms: u64) -> (FakeHost, PreflightReport) {
    let (dir, host, _config, ops) = base(name);
    let only_cold = run_config(dir.path(), "http://host-mark:9000", None);
    host.file("/private-mark/heal.json", text);
    host.answer(|cmd| {
        if cmd.program.ends_with("mc") {
            failed(1, "mc must not run in file mode")
        } else {
            ok("not-found\n")
        }
    });
    let report = gather_at(&host, &only_cold, &file_mode(&ops), now_ms);
    (host, report)
}

/// Catches a file mode that spends an `mc` call, a version that is not the file's, a heal state
/// that is not the file's and a fresh file that fails a check: both store checks pass on a fresh
/// file, the version and the heal state in the report are the file's, and no `mc` ran.
#[test]
fn file_mode_takes_the_version_and_the_heal_state_from_a_fresh_file() {
    let (host, report) = file_mode_report("tb-pre-file", &status_file(NOW_MS - 1_000), NOW_MS);
    let version = find(&report, "store-version:cold");
    assert!(version.passed, "{}", version.detail);
    assert!(
        version.detail.contains("RELEASE.file-mark"),
        "{}",
        version.detail
    );
    assert_eq!(
        report.tool_versions.get("cold-store").map(String::as_str),
        Some("RELEASE.file-mark")
    );
    let heal = find(&report, "cold-heal-state");
    assert!(heal.passed);
    assert_eq!(heal.detail, "healing 12% done");
    assert!(
        host.ran().iter().all(|c| !c.program.ends_with("mc")),
        "{:?}",
        host.ran()
    );
}

/// Catches a file with no version failing the check, or passing without saying the version is
/// unknown, and the version being invented.
#[test]
fn file_mode_without_a_version_says_so_and_passes() {
    let text = format!(r#"{{"read_unix_ms": {NOW_MS}, "healing": false, "drives": []}}"#);
    let (_host, report) = file_mode_report("tb-pre-file-nover", &text, NOW_MS);
    let version = find(&report, "store-version:cold");
    assert!(version.passed);
    assert_eq!(version.detail, "version unknown (file mode)");
    assert!(!report.tool_versions.contains_key("cold-store"));
    assert_eq!(find(&report, "cold-heal-state").detail, "none");
}

/// Catches a stale or future file passing the preflight, and a missing or malformed one: the
/// version check and the heal-state check both fail, with the freshness reason, at 30 s old
/// (the limit of the configuration) but not at 30 s exactly.
#[test]
fn file_mode_fails_on_a_stale_future_or_bad_file() {
    let (_h, report) = file_mode_report("tb-pre-file-edge", &status_file(NOW_MS - 30_000), NOW_MS);
    assert!(find(&report, "store-version:cold").passed);
    for (what, text) in [
        ("stale", status_file(NOW_MS - 30_001)),
        ("future", status_file(NOW_MS + 5_001)),
        ("no stamp", r#"{"healing": false, "drives": []}"#.to_owned()),
        ("malformed", "nope".to_owned()),
    ] {
        let (_h, report) = file_mode_report("tb-pre-file-bad", &text, NOW_MS);
        for name in ["store-version:cold", "cold-heal-state"] {
            let c = find(&report, name);
            assert!(!c.passed, "{what}: {name}: {}", c.detail);
        }
    }
    let (_h, report) = file_mode_report("tb-pre-file-stale", &status_file(NOW_MS - 31_000), NOW_MS);
    assert!(find(&report, "store-version:cold").detail.contains("old"));
    let (_h, report) = file_mode_report("tb-pre-file-fut", &status_file(NOW_MS + 9_000), NOW_MS);
    assert!(find(&report, "cold-heal-state").detail.contains("future"));
}

/// Catches a role with an admin entry changing: the same status file, stale, does not matter to
/// the checks when the admin entry answers (mc first, as before).
#[test]
fn with_an_admin_entry_the_preflight_is_unchanged() {
    let (dir, host, _config, ops) = base("tb-pre-file-admin");
    let only_cold = run_config(dir.path(), "http://host-mark:9000", None);
    host.file("/private-mark/heal.json", &status_file(1));
    let report = gather_at(&host, &only_cold, &ops, NOW_MS);
    assert!(find(&report, "store-version:cold").passed);
    assert_eq!(find(&report, "store-version:cold").detail, "RELEASE.mark");
}
