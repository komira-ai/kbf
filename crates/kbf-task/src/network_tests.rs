use kbf_proto::reapi::{Platform, platform::Property};

#[cfg(target_os = "linux")]
use super::seccomp::filter;
use super::*;

fn platform(pairs: &[(&str, &str)]) -> Platform {
    Platform {
        properties: pairs
            .iter()
            .map(|(name, value)| Property {
                name: (*name).to_owned(),
                value: (*value).to_owned(),
            })
            .collect(),
    }
}

#[allow(deprecated)] // `Command::platform`
fn read(action: Option<Platform>, command: Option<Platform>) -> Result<Network, RunError> {
    from_platform(
        &Action {
            platform: action,
            ..Action::default()
        },
        &Command {
            platform: command,
            ..Command::default()
        },
    )
}

#[test]
fn no_property_denies_and_allow_allows() {
    assert_eq!(read(None, None).unwrap(), Network::Denied);
    assert_eq!(
        read(Some(platform(&[("Pool", "x"), ("gpu", "1")])), None).unwrap(),
        Network::Denied
    );
    assert_eq!(
        read(Some(platform(&[("kbf-network", "allow")])), None).unwrap(),
        Network::Allowed
    );
    assert_eq!(
        read(Some(platform(&[("Kbf-NETWORK", "allow")])), None).unwrap(),
        Network::Allowed
    );
}

/// The Command's platform counts only when the Action has none, as the frontend reads them.
#[test]
fn the_commands_platform_is_read_only_when_the_action_has_none() {
    let allow = platform(&[("kbf-network", "allow")]);
    assert_eq!(read(None, Some(allow.clone())).unwrap(), Network::Allowed);
    assert_eq!(
        read(Some(platform(&[("Pool", "x")])), Some(allow)).unwrap(),
        Network::Denied
    );
}

#[test]
fn every_other_value_and_a_repeat_is_invalid() {
    for pairs in [
        vec![("kbf-network", "ALLOW")],
        vec![("kbf-network", "")],
        vec![("kbf-network", "off")],
        vec![("kbf-network", "allow"), ("kbf-network", "allow")],
        vec![("kbf-network", "allow"), ("KBF-network", "allow")],
    ] {
        let error = read(Some(platform(&pairs)), None).unwrap_err();
        assert!(matches!(error, RunError::Invalid(_)), "{pairs:?}");
        assert!(error.to_string().contains("kbf-network"), "{error}");
    }
}

#[test]
fn the_notice_names_the_action_and_the_property() {
    let digest = kbf_segments::sha256(b"an action");
    let line = allowed_notice(&digest);
    assert!(line.contains(&digest.to_string()), "{line}");
    assert!(line.contains("kbf-network=allow"), "{line}");
    assert!(!line.contains('\n'));
}

/// The line operators read to learn which layers confined a run.
#[test]
fn the_layers_notice_names_the_action_and_each_layer() {
    let digest = kbf_segments::sha256(b"an action");
    let line = |layers| layers_notice(&digest, &layers);
    assert_eq!(
        line(Layers::SeccompAndNetns),
        format!("kbf-task: action {digest}: network: netns (loopback only) + seccomp")
    );
    assert_eq!(
        line(Layers::SeccompOnly {
            step: "setgroups",
            errno: 13
        }),
        format!("kbf-task: action {digest}: network: seccomp only, no inet (netns: setgroups 13)")
    );
    assert!(line(Layers::Open).contains("open (kbf-network=allow)"));
}

/// Catches the use site of the root rule being wrong: a plan for euid 0 skips the namespace
/// steps (its outcome is `SKIPPED_FOR_ROOT`, with nothing done) and the strict program is the
/// one installed after it; a plan for anyone else does not skip.
#[cfg(target_os = "linux")]
#[test]
fn a_plan_for_root_skips_the_namespace_and_gets_the_strict_filter() {
    let (root, _report) = super::netns::plan(0, 0, super::netns::make_pipe).unwrap();
    assert!(root.skips());
    // Nothing is done for root, so this is safe to run in the test process.
    let outcome = root.apply();
    assert_eq!(outcome, (super::netns::SKIPPED_FOR_ROOT, 0));
    let strict = super::seccomp::filter(false);
    let in_netns = super::seccomp::filter(true);
    assert!(std::ptr::eq(
        super::pick(outcome, &strict, &in_netns),
        strict.as_slice()
    ));
    for uid in [1, 1000, u32::MAX] {
        let (plan, _report) = super::netns::plan(uid, uid, super::netns::make_pipe).unwrap();
        assert!(!plan.skips(), "{uid}");
    }
    // Only a clean outcome gets the program that lets the internet domains through.
    let clean = (super::netns::OK, 0);
    assert!(std::ptr::eq(
        super::pick(clean, &strict, &in_netns),
        in_netns.as_slice()
    ));
    assert!(std::ptr::eq(
        super::pick((5, 1), &strict, &in_netns),
        strict.as_slice()
    ));
}

#[test]
fn an_unreported_child_is_named_unknown() {
    let digest = kbf_segments::sha256(b"an action");
    assert_eq!(
        layers_notice(&digest, &Layers::Unreported),
        format!("kbf-task: action {digest}: network: unknown (no report from the child)")
    );
}

/// Catches the namespace layer being made for root (whose one-id map breaks `chown`, `setuid`
/// and `setgroups`) or left out for an ordinary user.
#[cfg(target_os = "linux")]
#[test]
fn only_root_skips_the_namespace_layer() {
    assert!(super::netns::skips_namespace(0));
    assert!(!super::netns::skips_namespace(1));
    assert!(!super::netns::skips_namespace(1000));
    assert!(!super::netns::skips_namespace(u32::MAX));
}

#[test]
fn the_root_line_says_the_namespace_was_skipped_for_uid_0() {
    let digest = kbf_segments::sha256(b"an action");
    assert_eq!(
        layers_notice(&digest, &Layers::SeccompOnlyForRoot),
        format!(
            "kbf-task: action {digest}: network: seccomp only, no inet (netns: skipped for uid 0)"
        )
    );
}

/// What the kernel does with a filter: the classic BPF subset the program uses, run on one
/// system call. Returns the filter's verdict.
#[cfg(target_os = "linux")]
fn verdict(program: &[libc::sock_filter], arch: u32, nr: u32, arg0: u32) -> u32 {
    let data = |offset: u32| match offset {
        0 => nr,
        4 => arch,
        16 => arg0,
        other => panic!("the program reads offset {other}"),
    };
    let mut acc = 0;
    let mut pc = 0;
    loop {
        let insn = &program[pc];
        pc += 1;
        match u32::from(insn.code) {
            c if c == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS => acc = data(insn.k),
            c if c == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K => {
                pc += usize::from(if acc == insn.k { insn.jt } else { insn.jf });
            }
            c if c == libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K => {
                pc += usize::from(if acc >= insn.k { insn.jt } else { insn.jf });
            }
            c if c == libc::BPF_RET | libc::BPF_K => return insn.k,
            other => panic!("the program uses opcode {other:#x}"),
        }
    }
}

#[cfg(target_os = "linux")]
mod rule {
    use super::*;

    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xC000_00B7;
    const EPERM: u32 = libc::SECCOMP_RET_ERRNO | 1;
    const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
    const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;

    fn nr(n: libc::c_long) -> u32 {
        u32::try_from(n).unwrap()
    }

    /// Catches a jump that lands one instruction off: the verdict for every domain of `socket`
    /// and `socketpair` (the family numbers of `linux/socket.h`, 0 to 45 and some beyond).
    #[test]
    fn only_the_unix_domain_may_make_a_socket() {
        let program = filter(false);
        for call in [libc::SYS_socket, libc::SYS_socketpair] {
            for domain in 0..=64u32 {
                let want = if domain == 1 { ALLOW } else { EPERM };
                assert_eq!(verdict(&program, ARCH, nr(call), domain), want, "{domain}");
            }
            // A domain whose low 32 bits are AF_UNIX: the kernel reads an int.
            assert_eq!(verdict(&program, ARCH, nr(call), 1), ALLOW);
            assert_eq!(verdict(&program, ARCH, nr(call), u32::MAX), EPERM);
        }
    }

    /// Catches the namespace filter letting through more than the loopback needs: it adds
    /// `AF_INET` and `AF_INET6` and nothing else.
    #[test]
    fn the_namespace_filter_adds_only_the_two_internet_domains() {
        let program = filter(true);
        for call in [libc::SYS_socket, libc::SYS_socketpair] {
            for domain in 0..=64u32 {
                let want = if matches!(domain, 1 | 2 | 10) {
                    ALLOW
                } else {
                    EPERM
                };
                assert_eq!(verdict(&program, ARCH, nr(call), domain), want, "{domain}");
            }
        }
        for call in [
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            assert_eq!(verdict(&program, ARCH, nr(call), 0), EPERM);
        }
        assert_eq!(verdict(&program, 0, nr(libc::SYS_socket), 2), KILL);
        assert_eq!(verdict(&program, ARCH, nr(libc::SYS_getpid), 0), ALLOW);
    }

    #[test]
    fn the_io_uring_calls_are_refused_whatever_their_arguments() {
        let program = filter(false);
        for call in [
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            for arg0 in [0, 1] {
                assert_eq!(verdict(&program, ARCH, nr(call), arg0), EPERM);
            }
        }
    }

    /// Catches the filter taking a call that matters for a build away: other system calls
    /// (`connect` is not one: it needs a socket, which cannot be made) pass.
    #[test]
    fn other_system_calls_pass() {
        let program = filter(false);
        for call in [
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_execve,
            libc::SYS_connect,
            libc::SYS_getpid,
            libc::SYS_clone,
        ] {
            assert_eq!(verdict(&program, ARCH, nr(call), 2), ALLOW, "{call}");
        }
    }

    /// Catches a system call of another ABI getting past the number tests: the architecture is
    /// compared first, so the 32-bit x86 ABI (whose `socketcall` is the one socket entry) and
    /// the other 64-bit one are killed, not matched by number.
    #[test]
    fn another_architecture_is_killed() {
        let program = filter(false);
        for arch in [0x4000_0003, 0xC000_00B7 ^ 0xC000_003E ^ ARCH, 0, u32::MAX] {
            assert_eq!(verdict(&program, arch, nr(libc::SYS_getpid), 0), KILL);
            assert_eq!(verdict(&program, arch, nr(libc::SYS_socket), 2), KILL);
        }
    }

    /// Catches an x32 call (bit 30 of the number) reaching `socket` under another number.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn an_x32_number_is_killed() {
        let program = filter(false);
        let x32 = 0x4000_0000;
        assert_eq!(verdict(&program, ARCH, x32 | nr(libc::SYS_socket), 2), KILL);
        assert_eq!(verdict(&program, ARCH, x32 | nr(libc::SYS_getpid), 0), KILL);
        assert_eq!(verdict(&program, ARCH, x32 - 1, 0), ALLOW);
    }
}

#[test]
fn every_layer_has_a_line() {
    for (layers, text) in [
        (Layers::Open, "open (kbf-network=allow)"),
        (Layers::Unavailable, "none (this system has no filter)"),
        (Layers::SeccompAndNetns, "netns (loopback only) + seccomp"),
        (
            Layers::Seatbelt,
            "sandbox-exec profile (no network but loopback and unix sockets)",
        ),
    ] {
        assert_eq!(layers.to_string(), text);
    }
}

/// The parts of the child's work that change the process they run in, or that only a forked
/// copy of the test process can show: each runs in a forked child that leaves through `exit`,
/// so its coverage data is written like any process's, and reports through its exit code.
#[cfg(target_os = "linux")]
mod forked {
    use std::ffi::CString;
    use std::io;
    use std::os::fd::OwnedFd;

    use super::super::{child_setup, confine_with, netns, pick, seccomp};
    use super::*;

    const EPERM: i32 = 1;
    /// What a child that panicked exits with.
    const PANICKED: i32 = 99;

    /// Runs `f` in a forked copy of this process and returns its exit code.
    fn in_child(f: impl FnOnce() -> i32) -> i32 {
        // SAFETY: the child runs `f` and leaves through `exit`; nothing else of the parent
        // runs in it. The tests are one process each (nextest) or tolerate the usual fork
        // caveats of a threaded parent for the few allocations `f` makes.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork");
        if pid == 0 {
            let code =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(PANICKED);
            // SAFETY: `exit` runs the handlers that write the coverage profile.
            unsafe { libc::exit(code) };
        }
        let mut status = 0;
        // SAFETY: `status` is writable; `pid` is our child.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            128 + libc::WTERMSIG(status)
        }
    }

    /// The errno of `socket(domain)`, or 0.
    fn socket(domain: libc::c_int) -> i32 {
        // SAFETY: integer arguments only.
        if unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) } >= 0 {
            0
        } else {
            io::Error::last_os_error().raw_os_error().unwrap_or(-1)
        }
    }

    fn dumpable() -> i32 {
        // SAFETY: integer arguments only.
        unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) }
    }

    fn temp(name: &str) -> kbf_testkit::dir::TestDir {
        kbf_testkit::dir::TestDir::under(std::env::temp_dir(), &format!("kbf-task-netns-{name}"))
    }

    #[test]
    fn the_strict_filter_refuses_internet_sockets_and_keeps_unix_ones() {
        let strict = seccomp::filter(false);
        assert_eq!(
            in_child(|| {
                seccomp::install(&strict).unwrap();
                socket(libc::AF_INET) * 10 + socket(libc::AF_UNIX)
            }),
            EPERM * 10
        );
    }

    #[test]
    fn the_namespace_filter_lets_internet_sockets_and_nothing_more_through() {
        let in_netns = seccomp::filter(true);
        let code = in_child(|| {
            seccomp::install(&in_netns).unwrap();
            i32::from(socket(libc::AF_INET) != 0)
                | i32::from(socket(libc::AF_NETLINK) != EPERM) << 1
        });
        assert_eq!(code, 0);
    }

    #[test]
    fn a_filter_that_cannot_be_installed_is_an_error() {
        let too_long = vec![
            libc::sock_filter {
                code: 0,
                jt: 0,
                jf: 0,
                k: 0
            };
            70_000
        ];
        assert_eq!(
            seccomp::install(&too_long).unwrap_err().raw_os_error(),
            Some(libc::E2BIG)
        );
        let garbage = [libc::sock_filter {
            code: 0xffff,
            jt: 0,
            jf: 0,
            k: 0,
        }];
        assert_eq!(
            in_child(|| i32::from(seccomp::install(&garbage).is_err())),
            1
        );
    }

    /// Catches the child's setup not choosing, installing and reporting as one: whatever the
    /// host allows, the layer it reports is the one in force (an internet socket is made
    /// exactly when the namespace is), and the child ends non-dumpable.
    #[test]
    fn the_child_setup_installs_the_filter_its_report_names() {
        // SAFETY: no arguments.
        let uid = unsafe { libc::geteuid() };
        let (plan, report) = netns::plan(uid, uid, netns::make_pipe).unwrap();
        let (strict, in_netns) = (seccomp::filter(false), seccomp::filter(true));
        let code = in_child(|| {
            child_setup(&plan, &strict, &in_netns).unwrap();
            if dumpable() != 0 {
                return 98;
            }
            socket(libc::AF_INET)
        });
        let layers = report.read();
        let want = if layers == Layers::SeccompAndNetns {
            0
        } else {
            EPERM
        };
        assert_eq!(code, want, "{layers}");
    }

    /// Catches a skip for root still doing the steps: with a plan for euid 0 the child does
    /// nothing, reports the skip, and gets the strict filter.
    #[test]
    fn a_child_setup_for_root_reports_the_skip() {
        let (plan, report) = netns::plan(0, 0, netns::make_pipe).unwrap();
        let (strict, in_netns) = (seccomp::filter(false), seccomp::filter(true));
        let code = in_child(|| {
            child_setup(&plan, &strict, &in_netns).unwrap();
            socket(libc::AF_INET)
        });
        assert_eq!(report.read(), Layers::SeccompOnlyForRoot);
        assert_eq!(code, EPERM);
    }

    #[test]
    fn a_setup_whose_filter_is_refused_fails_before_the_report() {
        let (plan, report) = netns::plan(0, 0, netns::make_pipe).unwrap();
        let garbage = [libc::sock_filter {
            code: 0xffff,
            jt: 0,
            jf: 0,
            k: 0,
        }];
        assert_eq!(
            in_child(|| i32::from(child_setup(&plan, &garbage, &garbage).is_err())),
            1
        );
        assert_eq!(report.read(), Layers::Unreported);
    }

    /// Catches the steps or their order changing: the child must be dumpable to write its own
    /// `/proc` files, which need the namespace to exist first, and must be non-dumpable again
    /// before `lo` and the filter. Root's plan has no steps. Each step has the number the
    /// parent reads in the report.
    #[test]
    fn the_plan_is_these_steps_in_this_order() {
        use netns::Step;
        let path = |p: &str| CString::new(p).unwrap();
        let (plan, _report) = netns::plan(1000, 2000, netns::make_pipe).unwrap();
        assert!(!plan.skips());
        assert_eq!(
            plan.steps(),
            [
                (7, Step::Dumpable(1)),
                (1, Step::Unshare),
                (
                    2,
                    Step::Write(path("/proc/self/setgroups"), b"deny".to_vec())
                ),
                (
                    3,
                    Step::Write(path("/proc/self/uid_map"), b"1000 1000 1".to_vec())
                ),
                (
                    4,
                    Step::Write(path("/proc/self/gid_map"), b"2000 2000 1".to_vec())
                ),
                (7, Step::Dumpable(0)),
                (5, Step::Loopback),
            ]
        );
        let (root, _report) = netns::plan(0, 0, netns::make_pipe).unwrap();
        assert!(root.skips());
        assert!(root.steps().is_empty());
    }

    #[test]
    fn the_dumpable_step_and_the_protection_set_the_flag() {
        assert_eq!(
            in_child(|| {
                netns::run_step(&netns::Step::Dumpable(1)).unwrap();
                let was = dumpable();
                crate::network::protect_this_process().unwrap();
                was * 10 + dumpable()
            }),
            10
        );
    }

    /// The `unshare` step ends in one of the two ways a host can answer, and either way the
    /// child lives to report it.
    #[test]
    fn the_unshare_step_succeeds_or_fails_with_an_errno() {
        let code = in_child(|| match netns::run_step(&netns::Step::Unshare) {
            Ok(()) => 0,
            Err(e) => netns::code(&e).into(),
        });
        assert!(
            code == 0 || code == EPERM || code == libc::EINVAL || code == libc::ENOSPC,
            "{code}"
        );
    }

    #[test]
    fn a_map_file_is_written_in_one_write_or_the_errno_is_returned() {
        let dir = temp("write");
        let file = dir.join("file");
        let path = CString::new(file.to_str().unwrap()).unwrap();
        std::fs::write(&file, b"").unwrap();
        netns::run_step(&netns::Step::Write(path.clone(), b"deny".to_vec())).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"deny");
        std::fs::remove_file(&file).unwrap();
        let missing = netns::write_file(&path, b"x").unwrap_err();
        assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
        let full = CString::new("/dev/full").unwrap();
        let error = netns::write_file(&full, b"x").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(netns::code(&error), u8::try_from(libc::ENOSPC).unwrap());
    }

    #[test]
    fn steps_run_in_order_up_to_the_first_that_fails() {
        let dir = temp("steps");
        let (a, b) = (dir.join("a"), dir.join("b"));
        let c = |p: &std::path::Path| CString::new(p.to_str().unwrap()).unwrap();
        std::fs::write(&a, b"").unwrap();
        assert_eq!(netns::run_steps(&[]), (netns::OK, 0));
        let ok = (3, netns::Step::Write(c(&a), b"1".to_vec()));
        assert_eq!(netns::run_steps(&[ok]), (netns::OK, 0));
        let steps = [
            (3, netns::Step::Write(c(&a), b"2".to_vec())),
            (4, netns::Step::Write(c(&b), b"x".to_vec())),
            (5, netns::Step::Write(c(&a), b"3".to_vec())),
        ];
        let want = (4, u8::try_from(libc::ENOENT).unwrap());
        assert_eq!(netns::run_steps(&steps), want);
        assert_eq!(
            std::fs::read(&a).unwrap(),
            b"2",
            "the step after the failure ran"
        );
        std::fs::remove_file(&a).unwrap();
    }

    /// `lo` is raised where the process may, and refused with the errno where it may not.
    #[test]
    fn the_loopback_step_raises_lo_or_returns_the_errno() {
        match netns::run_step(&netns::Step::Loopback).and(netns::raise_loopback()) {
            Ok(()) => {}
            Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EPERM)),
        }
    }

    #[test]
    fn what_the_child_wrote_is_what_the_parent_reads() {
        let say = |bytes: Option<(u8, u8)>| {
            let (plan, report) = netns::plan(1000, 1000, netns::make_pipe).unwrap();
            if let Some(outcome) = bytes {
                plan.tell(outcome);
            }
            report.read()
        };
        assert_eq!(say(Some((netns::OK, 0))), Layers::SeccompAndNetns);
        assert_eq!(
            say(Some((netns::SKIPPED_FOR_ROOT, 0))),
            Layers::SeccompOnlyForRoot
        );
        assert_eq!(
            say(Some((3, 13))),
            Layers::SeccompOnly {
                step: "uid_map",
                errno: 13
            }
        );
        assert_eq!(
            say(Some((7, 1))),
            Layers::SeccompOnly {
                step: "dumpable",
                errno: 1
            }
        );
        assert_eq!(say(Some((9, 1))), Layers::Unreported);
        assert_eq!(say(None), Layers::Unreported);
    }

    fn no_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        Err(io::Error::from_raw_os_error(libc::EMFILE))
    }

    #[test]
    fn a_report_pipe_that_cannot_be_made_is_an_io_error() {
        let mut process = Process::new("true");
        let Err(error) = confine_with(&mut process, Network::Denied, no_pipe) else {
            panic!("confined without a pipe");
        };
        assert!(matches!(error, RunError::Io { .. }), "{error}");
        assert!(confine_with(&mut process, Network::Allowed, no_pipe).is_ok());
    }

    #[test]
    fn only_a_clean_outcome_picks_the_namespace_program() {
        let (strict, in_netns) = (seccomp::filter(false), seccomp::filter(true));
        assert!(std::ptr::eq(
            pick((netns::OK, 0), &strict, &in_netns),
            in_netns.as_slice()
        ));
        assert!(std::ptr::eq(
            pick((netns::OK, 1), &strict, &in_netns),
            strict.as_slice()
        ));
    }
}

/// Where no confine is built, a Command denied the network is refused with `NoConfine`, and nothing
/// is printed or run for it.
#[test]
fn without_a_confine_a_denied_command_is_refused() {
    let error = without_confine(Network::Denied, false).expect_err("refused");
    assert!(matches!(error, RunError::NoConfine), "{error}");
}

/// A Command that is allowed the network needs no confine, with the test feature or without it,
/// and no warning is made for it.
#[test]
fn without_a_confine_an_allowed_command_is_unaffected() {
    assert_eq!(without_confine(Network::Allowed, false).unwrap(), None);
    assert_eq!(without_confine(Network::Allowed, true).unwrap(), None);
}

/// With the test-only feature a denied Command is let through with the warning line, which says
/// the Command has the network and names the feature.
#[test]
fn the_test_feature_warns_and_lets_a_denied_command_run() {
    let line = without_confine(Network::Denied, true)
        .unwrap()
        .expect("a warning");
    assert_eq!(line, UNCONFINED_WARNING);
    assert!(line.contains("runs with it"), "{line}");
    assert!(line.contains("unconfined-for-tests"), "{line}");
}

/// The system's own `confine` where neither Linux nor macOS has one: refused without the feature,
/// run with it.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn confine_refuses_a_denied_command_unless_the_test_feature_is_on() {
    let mut process = Process::new("true");
    let result = confine(&mut process, Network::Denied);
    if cfg!(feature = "unconfined-for-tests") {
        assert_eq!(result.expect("tolerated").layers(), Layers::Unavailable);
    } else {
        assert!(matches!(result, Err(RunError::NoConfine)));
    }
    let mut process = Process::new("true");
    let allowed = confine(&mut process, Network::Allowed).expect("allowed");
    assert_eq!(allowed.layers(), Layers::Open);
}

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

/// Catches the profile of a denied Command lacking the network denial, and that of an allowed one
/// having it: the two Commands differ in nothing else.
#[test]
fn the_seatbelt_profile_denies_the_network_only_to_a_denied_command() {
    let denied = seatbelt_profile(Network::Denied, &[]).expect("denied");
    let allowed = seatbelt_profile(Network::Allowed, &[]).expect("allowed");
    assert!(denied.contains("(deny network*)"), "{denied}");
    assert!(
        !allowed.contains("(deny network*)") && !allowed.contains("localhost"),
        "{allowed}"
    );
    assert_eq!(
        denied.replace(kbf_seatbelt::DENY_NETWORK_RULES, ""),
        allowed
    );
}

/// Catches a credential file left readable: one read deny per file, for a denied Command and for
/// an allowed one.
#[test]
fn the_seatbelt_profile_denies_reading_each_credential_file() {
    for network in [Network::Denied, Network::Allowed] {
        let text = seatbelt_profile(network, &paths(&["/w/grant", "/w/put urls"])).expect("ok");
        let denies: Vec<&str> = text.lines().filter(|l| l.contains("file-read*")).collect();
        assert_eq!(
            denies,
            [
                "(deny file-read* (literal \"/w/grant\"))",
                "(deny file-read* (literal \"/w/put urls\"))"
            ]
        );
    }
}

/// Catches a path that would end the profile string, and a relative one, reaching the sandbox: the
/// run is refused with `Confine`, which fails the run (exit 11), not the Command.
#[test]
fn the_seatbelt_profile_refuses_a_path_it_cannot_carry() {
    for bad in [
        "/w/a\"b",
        "/w/a\\b",
        "/w/a\")(allow default)(\"",
        "relative",
    ] {
        let error = seatbelt_profile(Network::Denied, &paths(&[bad])).expect_err(bad);
        assert!(matches!(error, RunError::Confine(_)), "{bad}: {error}");
        assert!(error.to_string().starts_with("cannot confine the Command"));
    }
}

/// Catches `launch` changing a Command that is not wrapped (any system but macOS).
#[cfg(not(target_os = "macos"))]
#[test]
fn off_macos_the_command_is_started_as_it_is() {
    let launch = launch(
        Path::new("/bin/echo"),
        &["a".to_owned(), "b c".to_owned()],
        Network::Denied,
        &paths(&["/w/grant"]),
    )
    .expect("launch");
    assert_eq!(launch.program, Path::new("/bin/echo"));
    assert_eq!(launch.args, ["a", "b c"]);
}

/// Catches a macOS Command started without `sandbox-exec`, and one whose arguments are altered:
/// the program is `sandbox-exec`, then `-p`, the profile, the Command's program and its arguments
/// in order.
#[cfg(target_os = "macos")]
#[test]
fn on_macos_the_command_is_started_under_sandbox_exec() {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-task-launch");
    let grant = dir.join("grant");
    std::fs::write(&grant, "x").expect("grant");
    let launch = launch(
        Path::new("/bin/echo"),
        &["a".to_owned(), "b c".to_owned()],
        Network::Denied,
        std::slice::from_ref(&grant),
    )
    .expect("launch");
    assert_eq!(launch.program, Path::new("/usr/bin/sandbox-exec"));
    assert_eq!(launch.args[0], "-p");
    let profile = launch.args[1].to_str().expect("profile text");
    assert!(profile.contains("(deny network*)"), "{profile}");
    // The file is named by its real path: a link in the temporary directory is resolved.
    let real = std::fs::canonicalize(&grant).expect("real path");
    assert!(
        profile.contains(&format!("(literal \"{}\")", real.display())),
        "{profile}"
    );
    assert_eq!(launch.args[2..], ["/bin/echo", "a", "b c"]);
}

/// Catches a credential file that is not there being dropped from the denies, or refusing the run:
/// it holds nothing to read, and is named as given.
#[cfg(target_os = "macos")]
#[test]
fn on_macos_a_credential_file_that_does_not_exist_is_still_named() {
    let launch = launch(
        Path::new("/bin/echo"),
        &[],
        Network::Denied,
        &paths(&["/no/such/grant"]),
    )
    .expect("launch");
    let profile = launch.args[1].to_str().expect("profile text");
    assert!(
        profile.contains("(deny file-read* (literal \"/no/such/grant\"))"),
        "{profile}"
    );
}

/// Catches `confine` accepting a denied Command whose process was not made by `launch`, which
/// would run it with the network on macOS; a process that is `sandbox-exec` passes, and layers say so.
#[cfg(target_os = "macos")]
#[test]
fn on_macos_confine_refuses_a_denied_command_that_is_not_under_sandbox_exec() {
    let mut plain = Process::new("/bin/echo");
    assert!(matches!(
        confine(&mut plain, Network::Denied),
        Err(RunError::NoConfine)
    ));
    let mut wrapped = Process::new("/usr/bin/sandbox-exec");
    assert_eq!(
        confine(&mut wrapped, Network::Denied)
            .expect("wrapped")
            .layers(),
        Layers::Seatbelt
    );
    let mut allowed = Process::new("/bin/echo");
    assert_eq!(
        confine(&mut allowed, Network::Allowed)
            .expect("allowed")
            .layers(),
        Layers::Open
    );
}

/// Catches an error that names only the wrapper, or only the Command's program: a Command started
/// as it is has one name, one started through another program has both.
#[test]
fn a_launch_names_the_program_it_runs_and_the_one_it_runs_through() {
    let own = Launch {
        program: PathBuf::from("/bin/echo"),
        args: Vec::new(),
    };
    assert_eq!(own.name(Path::new("/bin/echo")), Path::new("/bin/echo"));
    let wrapped = Launch {
        program: PathBuf::from("/usr/bin/sandbox-exec"),
        args: Vec::new(),
    };
    assert_eq!(
        wrapped.name(Path::new("/bin/echo")),
        Path::new("/usr/bin/sandbox-exec for /bin/echo")
    );
}
