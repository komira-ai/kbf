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
        ("netns", "net:[2]"),
        ("parent.netns", "net:[1]"),
        // CAP_NET_RAW (bit 13) is held, so `packet` is a proof of the filter.
        ("fact.cap_eff", "0000000000002000"),
        ("fact.io_uring_disabled", "0"),
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
            "probe netns_distinct: ok net:[2] != net:[1]",
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

/// Catches: the Command's argv losing the hidden flag or the secret path.
#[test]
fn the_childs_argv_is_this_program_with_the_hidden_flag() {
    let bare = child_argv(None);
    assert_eq!(bare.len(), 2);
    assert_eq!(bare[1], CHILD_FLAG);
    let with = child_argv(Some(Path::new("/run/js-files")));
    assert_eq!(
        with[1..],
        [CHILD_FLAG, "--probe-secret-path", "/run/js-files"]
    );
}

#[cfg(target_os = "linux")]
mod confined {
    use kbf_testkit::dir::TestDir;

    use super::*;

    /// A scratch directory shaped as `run_probe` makes it, and the checks of `argv` run in it.
    async fn checks_of(name: &str, argv: &[&str], deadline: Duration) -> Vec<Check> {
        let dir = TestDir::under(std::env::temp_dir(), &format!("kbf-task-probe-unit-{name}"));
        std::fs::create_dir(dir.join("root")).expect("root");
        let argv = argv.iter().map(|a| (*a).to_owned()).collect();
        run_confined(&dir, argv, deadline, None).await
    }

    /// Catches: a Command that cannot start passing, or its error being lost.
    #[tokio::test]
    async fn a_command_that_cannot_start_fails_the_child_check() {
        let checks = checks_of(
            "nostart",
            &["/nonexistent/kbf-probe"],
            Duration::from_secs(5),
        )
        .await;
        let child = by_name(&checks, "child").line();
        assert!(child.starts_with("probe child: FAIL "), "{child}");
        assert!(child.contains("/nonexistent/kbf-probe"), "{child}");
        assert_eq!(by_name(&checks, "layers").status, Status::Fail);
        assert!(!passes(&checks));
    }

    /// Catches: a Command that outlives its deadline being reported as having exited.
    #[tokio::test]
    async fn a_command_past_its_deadline_is_a_timeout() {
        let checks = checks_of("timeout", &["/bin/sleep", "30"], Duration::from_millis(200)).await;
        assert_eq!(
            by_name(&checks, "child").line(),
            "probe child: FAIL timed out"
        );
        assert!(!passes(&checks));
    }

    /// Catches: a Command that exits 0 and says nothing passing: every call it owns fails.
    #[tokio::test]
    async fn a_command_that_reports_nothing_fails_the_checks_it_owns() {
        let checks = checks_of("silent", &["/bin/true"], Duration::from_secs(30)).await;
        assert_eq!(by_name(&checks, "child").line(), "probe child: ok exit 0");
        for name in ["inet_unreachable", "netlink", "packet", "io_uring"] {
            assert_eq!(
                by_name(&checks, name).line(),
                format!("probe {name}: FAIL the Command reported nothing")
            );
        }
        assert!(!passes(&checks));
        // The facts follow the checks, whatever the Command did.
        assert_eq!(
            checks.last().map(|c| c.name.as_str()),
            Some("child_cap_eff")
        );
    }

    /// Catches: an error with no OS errno being written as `errno 0`, which reads as a refusal.
    #[test]
    fn an_error_with_no_os_errno_is_written_as_other() {
        let timed_out = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert_eq!(child::errno(&timed_out), "other TimedOut");
        assert_eq!(
            child::errno(&std::io::Error::from_raw_os_error(13)),
            "errno 13"
        );
    }

    /// Catches: a descriptor leaked or an error lost by the call helper both sockets and the ring use.
    #[test]
    fn a_call_that_returns_a_descriptor_is_closed_and_one_that_fails_names_its_errno() {
        // SAFETY: opens a file read-only; the descriptor is handed to the helper, which closes it.
        let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
        assert!(fd >= 0);
        assert_eq!(child::fd_outcome(libc::c_long::from(fd)), "ok");
        // SAFETY: `fcntl` on a closed descriptor only fails with `EBADF`.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        // SAFETY: `__errno_location` is this thread's `errno`, which is writable.
        unsafe { *libc::__errno_location() = libc::EBADF };
        assert_eq!(child::fd_outcome(-1), format!("errno {}", libc::EBADF));
    }
}

/// Catches: `packet` counted as a proof of the filter when the Command has no `CAP_NET_RAW`
/// (the kernel refuses it then), and a proof that is not required being allowed to fail.
#[test]
fn packet_is_a_proof_only_with_cap_net_raw() {
    let with = |cap: &str, packet: &str| {
        let mut seen = good();
        seen.insert("fact.cap_eff".to_owned(), cap.to_owned());
        seen.insert("packet".to_owned(), packet.to_owned());
        let checks = evaluate(&Layers::SeccompAndNetns, Ok(0), &seen, None);
        (by_name(&checks, "packet").line(), passes(&checks))
    };
    assert_eq!(
        with("0000000000002000", "errno 1"),
        ("probe packet: ok EPERM (1)".to_owned(), true)
    );
    // Held, but refused by something else: not the filter's answer.
    assert_eq!(
        with("0000000000002000", "errno 38"),
        (
            "probe packet: FAIL refused, not by the filter: ENOSYS (38)".to_owned(),
            false
        )
    );
    assert_eq!(
        with("0000000000000000", "errno 1"),
        (
            "probe packet: ok EPERM (1) (no CAP_NET_RAW: not a filter proof)".to_owned(),
            true
        )
    );
    // Not a proof: another errno does not fail the probe, a call that worked does.
    assert!(with("0000000000000000", "errno 13").1);
    assert!(!with("0000000000000000", "ok").1);
    // No capability report is no capability.
    assert!(!has_net_raw(None));
    assert!(!has_net_raw(Some(&"zz".to_owned())));
    assert!(has_net_raw(Some(&"ffffffffffffffff".to_owned())));
}

/// Catches: a refusal by another errno being taken for the filter's, and `io_uring` counted as
/// a proof where the host switch alone refuses it.
#[test]
fn netlink_and_io_uring_need_the_filters_own_errno() {
    let with = |name: &str, value: &str, disabled: &str| {
        let mut seen = good();
        seen.insert(name.to_owned(), value.to_owned());
        seen.insert("fact.io_uring_disabled".to_owned(), disabled.to_owned());
        let checks = evaluate(&Layers::SeccompAndNetns, Ok(0), &seen, None);
        (by_name(&checks, name).line(), passes(&checks))
    };
    assert_eq!(
        with("netlink", "errno 13", "0"),
        (
            "probe netlink: FAIL refused, not by the filter: EACCES (13)".to_owned(),
            false
        )
    );
    assert_eq!(
        with("io_uring", "errno 38", "0"),
        (
            "probe io_uring: FAIL refused, not by the filter: ENOSYS (38)".to_owned(),
            false
        )
    );
    assert_eq!(
        with("io_uring", "errno 1", "2"),
        (
            "probe io_uring: ok EPERM (1) (io_uring_disabled=2: not a filter proof)".to_owned(),
            true
        )
    );
    assert_eq!(
        with("io_uring", "errno 1", "1").0,
        "probe io_uring: ok EPERM (1) (io_uring_disabled=1: not a filter proof)"
    );
    assert!(!with("io_uring", "ok", "2").1);
}

/// Catches: a Command that is in the probe's own namespace, or that did not say, passing.
#[test]
fn the_command_must_be_in_a_namespace_of_its_own() {
    let with = |child: Option<&str>, probe: Option<&str>| {
        let mut seen = good();
        seen.remove("netns");
        seen.remove("parent.netns");
        seen.extend(child.map(|v| ("netns".to_owned(), v.to_owned())));
        seen.extend(probe.map(|v| ("parent.netns".to_owned(), v.to_owned())));
        let checks = evaluate(&Layers::SeccompAndNetns, Ok(0), &seen, None);
        (by_name(&checks, "netns_distinct").line(), passes(&checks))
    };
    assert_eq!(
        with(Some("net:[2]"), Some("net:[1]")),
        (
            "probe netns_distinct: ok net:[2] != net:[1]".to_owned(),
            true
        )
    );
    let (line, pass) = with(Some("net:[1]"), Some("net:[1]"));
    assert!(
        line.starts_with("probe netns_distinct: FAIL the Command's namespace is not a new one"),
        "{line}"
    );
    assert!(!pass);
    assert!(!with(Some("errno 13"), Some("net:[1]")).1);
    assert_eq!(
        with(None, Some("net:[1]")).0,
        "probe netns_distinct: FAIL the Command reported nothing"
    );
    assert_eq!(
        with(Some("net:[2]"), None).0,
        "probe netns_distinct: FAIL cannot read the probe's own /proc/self/ns/net"
    );
    // On the fallback there is no namespace to compare.
    let fallback = Layers::SeccompOnly {
        step: "unshare",
        errno: 1,
    };
    let checks = evaluate(&fallback, Ok(0), &good(), None);
    assert_eq!(
        by_name(&checks, "netns_distinct").line(),
        "probe netns_distinct: n/a the namespace path did not run"
    );
}

/// Catches: an error with no OS errno (a timeout) or `errno 0` counting as a refusal.
#[test]
fn an_error_without_an_errno_is_not_a_refusal() {
    assert_eq!(errno_of(Some(&"errno 0".to_owned())), None);
    assert_eq!(errno_of(Some(&"other TimedOut".to_owned())), None);
    let mut seen = good();
    seen.insert("netlink".to_owned(), "other TimedOut".to_owned());
    let checks = evaluate(&Layers::SeccompAndNetns, Ok(0), &seen, None);
    assert_eq!(
        by_name(&checks, "netlink").line(),
        "probe netlink: FAIL not refused: other TimedOut"
    );
}

/// Catches: a host with no confinement (the run refuses a network-denied Command, exit 15 in a
/// task) reading as anything but a FAIL that says why.
#[test]
fn a_run_refused_for_want_of_a_confine_is_a_fail_with_the_reason() {
    let checks = after_run(Err(RunError::NoConfine), "", None);
    let child = by_name(&checks, "child").line();
    assert!(child.starts_with("probe child: FAIL "), "{child}");
    assert!(child.contains(&RunError::NoConfine.to_string()), "{child}");
    assert_eq!(by_name(&checks, "layers").status, Status::Fail);
    assert!(!passes(&checks));
    assert_eq!(result_line(&checks), "probe result: FAIL");
}
