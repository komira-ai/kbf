use std::ffi::OsStr;

use super::*;

fn obs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// What a Command that met the namespace layer in order sees.
fn good() -> BTreeMap<String, String> {
    obs(&[
        ("loopback", "ok"),
        ("inet", "errno 101"),
        ("netlink", "errno 1"),
        ("packet", "errno 1"),
        ("io_uring", "errno 1"),
    ])
}

fn by_name<'a>(checks: &'a [Check], name: &str) -> &'a Check {
    checks.iter().find(|c| c.name == name).expect(name)
}

fn lines(checks: &[Check]) -> Vec<String> {
    checks.iter().map(Check::line).collect()
}

/// Catches: a changed spelling of the lines a jobscheduler test parses.
#[test]
fn a_line_is_probe_name_status_detail() {
    let ok = Check::new("loopback", Status::Ok, "bind");
    assert_eq!(ok.line(), "probe loopback: ok bind");
    assert_eq!(
        Check::new("layers", Status::Fail, "seccomp only").line(),
        "probe layers: FAIL seccomp only"
    );
    assert_eq!(
        Check::new("secret_path", Status::Na, "no --probe-secret-path").line(),
        "probe secret_path: n/a no --probe-secret-path"
    );
    assert_eq!(
        Check::new("child", Status::Ok, "").line(),
        "probe child: ok"
    );
}

/// Catches: a full set of required checks that does not pass.
#[test]
fn every_required_check_ok_is_a_pass() {
    let checks = evaluate(&Layers::SeccompAndNetns, Ok(0), &good(), None);
    assert_eq!(
        lines(&checks),
        [
            "probe layers: ok netns (loopback only) + seccomp",
            "probe child: ok exit 0",
            "probe loopback: ok bind, connect and accept on 127.0.0.1",
            "probe inet_unreachable: ok ENETUNREACH (101) (netns)",
            "probe netlink: ok EPERM (1)",
            "probe packet: ok EPERM (1)",
            "probe io_uring: ok EPERM (1)",
            "probe secret_path: n/a no --probe-secret-path",
        ]
    );
    assert!(passes(&checks));
    assert_eq!(result_line(&checks), "probe result: PASS");
}

/// Catches: a fallback to the strict filter, or a missing check, passing.
#[test]
fn a_fallback_or_a_missing_required_check_is_a_fail() {
    let fallback = Layers::SeccompOnly {
        step: "unshare",
        errno: 1,
    };
    let checks = evaluate(
        &fallback,
        Ok(0),
        &obs(&[
            ("inet", "errno 1"),
            ("netlink", "errno 1"),
            ("packet", "errno 1"),
            ("io_uring", "errno 1"),
        ]),
        None,
    );
    assert_eq!(
        by_name(&checks, "layers").line(),
        "probe layers: FAIL seccomp only, no inet (netns: unshare 1)"
    );
    assert_eq!(
        by_name(&checks, "loopback").line(),
        "probe loopback: n/a the namespace path did not run"
    );
    assert_eq!(
        by_name(&checks, "inet_unreachable").line(),
        "probe inet_unreachable: ok EPERM (1) (strict filter)"
    );
    assert!(!passes(&checks));
    assert_eq!(result_line(&checks), "probe result: FAIL");
    // A required check that is n/a does not pass even when nothing says FAIL.
    let only_na = [Check::new("layers", Status::Na, "")];
    assert!(!passes(&only_na));
}

/// Catches: a Fail of any one check being outvoted by the others.
#[test]
fn one_failed_check_fails_the_probe() {
    for (name, bad) in [
        ("loopback", "errno 13"),
        ("inet", "ok"),
        ("netlink", "ok"),
        ("packet", "ok"),
        ("io_uring", "ok"),
    ] {
        let mut seen = good();
        seen.insert(name.to_owned(), bad.to_owned());
        let checks = evaluate(&Layers::SeccompAndNetns, Ok(0), &seen, None);
        assert!(!passes(&checks), "{name}");
    }
    assert!(!passes(&evaluate(
        &Layers::SeccompAndNetns,
        Ok(159),
        &good(),
        None
    )));
    assert!(!passes(&evaluate(
        &Layers::SeccompAndNetns,
        Err("no such program".to_owned()),
        &good(),
        None
    )));
}

/// Catches: a check that cannot tell what it saw: each odd value gets its own detail.
#[test]
fn details_name_what_was_seen() {
    let mut seen = good();
    seen.insert("loopback".to_owned(), "errno 13".to_owned());
    seen.insert("inet".to_owned(), "errno 110".to_owned());
    seen.insert("netlink".to_owned(), "ok".to_owned());
    seen.remove("packet");
    seen.insert("io_uring".to_owned(), "weird".to_owned());
    let checks = evaluate(&Layers::SeccompAndNetns, Ok(7), &seen, None);
    assert_eq!(by_name(&checks, "child").line(), "probe child: FAIL exit 7");
    assert_eq!(
        by_name(&checks, "loopback").line(),
        "probe loopback: FAIL errno 13"
    );
    assert_eq!(
        by_name(&checks, "inet_unreachable").line(),
        "probe inet_unreachable: FAIL ETIMEDOUT (110), wanted ENETUNREACH (101) (netns)"
    );
    assert_eq!(
        by_name(&checks, "netlink").line(),
        "probe netlink: FAIL not refused: ok"
    );
    assert_eq!(
        by_name(&checks, "packet").line(),
        "probe packet: FAIL the Command reported nothing"
    );
    assert_eq!(
        by_name(&checks, "io_uring").line(),
        "probe io_uring: FAIL not refused: weird"
    );
    let mut seen = good();
    seen.insert("inet".to_owned(), "ok".to_owned());
    assert_eq!(
        by_name(
            &evaluate(&Layers::SeccompAndNetns, Ok(0), &seen, None),
            "inet_unreachable"
        )
        .line(),
        "probe inet_unreachable: FAIL a connect to 192.0.2.1 worked"
    );
    seen.insert("inet".to_owned(), "garbage".to_owned());
    assert_eq!(
        by_name(
            &evaluate(&Layers::SeccompAndNetns, Ok(0), &seen, None),
            "inet_unreachable"
        )
        .line(),
        "probe inet_unreachable: FAIL garbage"
    );
    seen.remove("inet");
    seen.remove("loopback");
    let checks = evaluate(&Layers::SeccompAndNetns, Ok(0), &seen, None);
    assert_eq!(
        by_name(&checks, "inet_unreachable").line(),
        "probe inet_unreachable: FAIL the Command reported nothing"
    );
    assert_eq!(
        by_name(&checks, "loopback").line(),
        "probe loopback: FAIL the Command reported nothing"
    );
}

/// Catches: the credentials question being answered wrongly: readable and not readable are both
/// answers (neither fails), a path that is not there is not.
#[test]
fn the_secret_path_check_reports_readable_or_not_and_never_fails_on_either() {
    let path = Some(Path::new("/run/js-files"));
    let with = |value: Option<&str>| {
        let mut seen = good();
        if let Some(v) = value {
            seen.insert("secret".to_owned(), v.to_owned());
        }
        let checks = evaluate(&Layers::SeccompAndNetns, Ok(0), &seen, path);
        (by_name(&checks, "secret_path").line(), passes(&checks))
    };
    assert_eq!(
        with(Some("readable")),
        ("probe secret_path: ok readable".to_owned(), true)
    );
    assert_eq!(
        with(Some("errno 13")),
        (
            "probe secret_path: ok not readable (EACCES (13))".to_owned(),
            true
        )
    );
    let (line, pass) = with(Some("errno 2"));
    assert_eq!(
        line,
        "probe secret_path: FAIL ENOENT (2): the path is not there, so the question is unanswered"
    );
    assert!(!pass);
    assert!(line.starts_with("probe secret_path: FAIL"));
    assert!(with(Some("errno 20")).0.contains("FAIL"));
    assert_eq!(
        with(None).0,
        "probe secret_path: FAIL the Command reported nothing"
    );
    assert_eq!(with(Some("odd")).0, "probe secret_path: FAIL odd");
}

/// Catches: observation lines being parsed from anything but `observe <name> <value>`, so a
/// Command that prints other text could not forge or break a check.
#[test]
fn observations_are_only_the_observe_lines() {
    let seen = parse_observations(
        "noise\nobserve loopback ok\nobserve inet errno 101\nobserve lonely\nobserved x y\n",
    );
    assert_eq!(seen, obs(&[("loopback", "ok"), ("inet", "errno 101")]));
}

/// Catches: errno names drifting from the numbers the lines carry.
#[test]
fn errno_names() {
    assert_eq!(errno_name(1), "EPERM (1)");
    assert_eq!(errno_name(101), "ENETUNREACH (101)");
    assert_eq!(errno_name(13), "EACCES (13)");
    assert_eq!(errno_name(38), "ENOSYS (38)");
    assert_eq!(errno_name(97), "EAFNOSUPPORT (97)");
    assert_eq!(errno_name(111), "ECONNREFUSED (111)");
    assert_eq!(errno_name(2), "ENOENT (2)");
    assert_eq!(errno_name(110), "ETIMEDOUT (110)");
    assert_eq!(errno_name(9999), "errno 9999");
}

/// Catches: the facts of the userns question mis-read from `/proc`.
#[test]
fn proc_texts_become_one_line_facts() {
    assert_eq!(
        squeeze_map("         0       1000          1\n 5 6 7\n\n"),
        "0 1000 1;5 6 7"
    );
    assert_eq!(squeeze_map(""), "");
    let status = "Name:\tkbf-task\nUid:\t1000\t1001\t1000\t1000\nCapEff:\t0000000000000000\n";
    assert_eq!(cap_eff(status).as_deref(), Some("0000000000000000"));
    assert_eq!(euid(status).as_deref(), Some("1001"));
    assert_eq!(cap_eff("Name:\tx\n"), None);
    assert_eq!(euid("Name:\tx\n"), None);
    assert_eq!(
        fact("cap_eff", Some("0".to_owned())).line(),
        "probe cap_eff: ok 0"
    );
    assert_eq!(
        fact("max_user_namespaces", None).line(),
        "probe max_user_namespaces: n/a absent"
    );
}

/// Catches: the child's facts being shown under the wrong name or lost.
#[test]
fn the_childs_facts_are_prefixed_and_absent_ones_are_n_a() {
    let seen = obs(&[("fact.uid_map", "1000 1000 1")]);
    let all = facts(&seen);
    assert_eq!(
        by_name(&all, "child_uid_map").line(),
        "probe child_uid_map: ok 1000 1000 1"
    );
    assert_eq!(by_name(&all, "child_gid_map").status, Status::Na);
    assert_eq!(by_name(&all, "child_cap_eff").status, Status::Na);
    for name in [
        "apparmor_restrict_unprivileged_userns",
        "max_user_namespaces",
        "euid",
        "uid_map",
        "gid_map",
        "cap_eff",
    ] {
        let _ = by_name(&all, name);
    }
    // Facts never fail the probe, whatever they are.
    assert!(all.iter().all(|c| c.status != Status::Fail));
}

/// Catches: a Command that never started reading as a pass, and the failure text being lost.
#[test]
fn a_command_that_did_not_run_is_a_fail_with_its_error() {
    let checks = evaluate_failed("cannot make x".to_owned(), None);
    assert_eq!(
        by_name(&checks, "child").line(),
        "probe child: FAIL cannot make x"
    );
    assert!(by_name(&checks, "layers").status == Status::Fail);
    assert!(!passes(&checks));
}

/// Catches: the probe being selected by a flag of the task, or the task by a probe flag.
#[test]
fn only_the_two_probe_flags_select_the_probe() {
    assert!(selected(Some(OsStr::new("--probe-confinement"))));
    assert!(selected(Some(OsStr::new("--probe-confinement-child"))));
    assert!(!selected(Some(OsStr::new("--grant-url"))));
    assert!(!selected(None));
}

/// Catches: the probe running when the process could not be protected.
#[test]
fn a_failed_protection_stops_the_probe() {
    let cli = ProbeCli::parse_from(["kbf-task", FLAG]);
    let refused = std::io::Error::from_raw_os_error(1);
    assert_eq!(
        run_cli_with(Err(refused), &cli),
        ExitCode::from(crate::task::EXIT_SETUP)
    );
}

/// Catches: the flags the jobscheduler passes being misparsed.
#[test]
fn the_flags_parse() {
    let cli = ProbeCli::parse_from([
        "kbf-task",
        FLAG,
        "--probe-secret-path",
        "/run/js-files",
        "--probe-work-dir",
        "/scratch",
    ]);
    assert!(cli.probe_confinement && !cli.probe_confinement_child);
    assert_eq!(
        cli.probe_secret_path.as_deref(),
        Some(Path::new("/run/js-files"))
    );
    assert_eq!(cli.probe_work_dir.as_deref(), Some(Path::new("/scratch")));
}
