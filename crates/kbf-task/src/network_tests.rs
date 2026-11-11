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

/// Catches the use site of the root rule being wrong: a plan for euid 0 makes no user or
/// network namespace (its `unshare` has neither flag) and gets the strict program whatever its
/// steps did; a plan for anyone else makes both, and gets the program that lets the internet
/// domains through only when no step of the network namespace failed.
#[cfg(target_os = "linux")]
#[test]
fn a_plan_for_root_makes_no_network_namespace_and_gets_the_strict_filter() {
    use crate::child::{self, Outcome, Step};
    let outcome = |step, errno| Outcome {
        step,
        errno,
        unshared: true,
    };
    let strict = super::seccomp::filter(false);
    let in_netns = super::seccomp::filter(true);
    let (root, _report) = child::plan(
        0,
        0,
        true,
        rustix::process::getpgrp(),
        Vec::new(),
        child::make_pipe,
    )
    .unwrap();
    assert_eq!(
        root.steps(),
        [(
            child::UNSHARE,
            Step::Unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID)
        )]
    );
    for done in [outcome(child::OK, 0), outcome(child::PRIVATE, 1)] {
        assert!(!root.netns_made(done));
        assert!(std::ptr::eq(
            super::pick(root.netns_made(done), &strict, &in_netns),
            strict.as_slice()
        ));
    }
    for uid in [1, 1000, u32::MAX] {
        let (plan, _report) = child::plan(
            uid,
            uid,
            true,
            rustix::process::getpgrp(),
            Vec::new(),
            child::make_pipe,
        )
        .unwrap();
        assert!(
            plan.steps().contains(&(
                child::UNSHARE,
                Step::Unshare(
                    libc::CLONE_NEWUSER
                        | libc::CLONE_NEWNS
                        | libc::CLONE_NEWPID
                        | libc::CLONE_NEWNET
                )
            )),
            "{uid}"
        );
        // A clean outcome, or one whose failure is past the network namespace (a mount step),
        // gets the program that lets the internet domains through; a failed namespace step not.
        assert!(plan.netns_made(outcome(child::OK, 0)), "{uid}");
        assert!(plan.netns_made(outcome(child::PRIVATE, 1)), "{uid}");
        assert!(!plan.netns_made(outcome(child::LOOPBACK, 1)), "{uid}");
    }
    // A Command allowed the network gets no network namespace, and no filter is picked for it.
    let (open, _report) = child::plan(
        1000,
        1000,
        false,
        rustix::process::getpgrp(),
        Vec::new(),
        child::make_pipe,
    )
    .unwrap();
    assert!(!open.netns_made(outcome(child::OK, 0)));
    assert!(open.steps().contains(&(
        child::UNSHARE,
        Step::Unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID)
    )));
    assert!(!open.steps().iter().any(|(n, _)| *n == child::LOOPBACK));
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
    assert!(crate::child::skips_namespace(0));
    assert!(!crate::child::skips_namespace(1));
    assert!(!crate::child::skips_namespace(1000));
    assert!(!crate::child::skips_namespace(u32::MAX));
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

/// The parts of the child's work that change the process they run in: each runs in a forked
/// child ([`crate::in_child`]) and reports through its exit code.
#[cfg(target_os = "linux")]
mod forked {
    use std::io;
    use std::os::fd::OwnedFd;

    use super::super::{Filters, child_setup, confine_with, pick, seccomp};
    use super::*;
    use crate::child;
    use crate::in_child::{dumpable, in_child, socket};

    const EPERM: i32 = 1;

    fn filters() -> Filters {
        Filters {
            strict: seccomp::filter(false),
            in_netns: seccomp::filter(true),
        }
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
    /// exactly when the namespace is), and the child ends non-dumpable. Where the `unshare`
    /// succeeds the setup also makes the PID namespace's processes, and the code below runs in
    /// the one that is to become the Command; its exit code comes back up through them.
    #[test]
    fn the_child_setup_installs_the_filter_its_report_names() {
        let uid = rustix::process::geteuid().as_raw();
        let (mut plan, report) = child::plan(
            uid,
            uid,
            true,
            rustix::process::getpgrp(),
            Vec::new(),
            child::make_pipe,
        )
        .unwrap();
        let filters = filters();
        let code = in_child(|| {
            child_setup(&mut plan, Some(&filters), libc::exit).unwrap();
            if dumpable() != 0 {
                return 98;
            }
            socket(libc::AF_INET)
        });
        let (layers, _) = report.read();
        let want = if layers == Layers::SeccompAndNetns {
            0
        } else {
            EPERM
        };
        assert_eq!(code, want, "{layers}");
    }

    /// Catches a plan for root being given a network namespace or the open filter: with a plan
    /// for euid 0 the child reports root's layers and gets the strict filter.
    #[test]
    fn a_child_setup_for_root_reports_the_strict_filter() {
        let (mut plan, report) = child::plan(
            0,
            0,
            true,
            rustix::process::getpgrp(),
            Vec::new(),
            child::make_pipe,
        )
        .unwrap();
        let filters = filters();
        let code = in_child(|| {
            child_setup(&mut plan, Some(&filters), libc::exit).unwrap();
            socket(libc::AF_INET)
        });
        assert_eq!(report.read().0, Layers::SeccompOnlyForRoot);
        assert_eq!(code, EPERM);
    }

    /// Catches a filter installed for a Command allowed the network: its setup installs none, so
    /// it makes an internet socket, and its report says the network is open.
    #[test]
    fn a_child_setup_allowed_the_network_installs_no_filter() {
        let uid = rustix::process::geteuid().as_raw();
        let (mut plan, report) = child::plan(
            uid,
            uid,
            false,
            rustix::process::getpgrp(),
            Vec::new(),
            child::make_pipe,
        )
        .unwrap();
        let code = in_child(|| {
            child_setup(&mut plan, None, libc::exit).unwrap();
            socket(libc::AF_INET)
        });
        assert_eq!(report.read().0, Layers::Open);
        assert_eq!(code, 0);
    }

    #[test]
    fn a_setup_whose_filter_is_refused_fails_before_the_report() {
        let (mut plan, report) = child::plan(
            0,
            0,
            true,
            rustix::process::getpgrp(),
            Vec::new(),
            child::make_pipe,
        )
        .unwrap();
        let garbage = [libc::sock_filter {
            code: 0xffff,
            jt: 0,
            jf: 0,
            k: 0,
        }];
        let refused = Filters {
            strict: garbage.to_vec(),
            in_netns: garbage.to_vec(),
        };
        assert_eq!(
            in_child(|| i32::from(child_setup(&mut plan, Some(&refused), libc::exit).is_err())),
            1
        );
        assert_eq!(report.read().0, Layers::Unreported);
    }

    #[test]
    fn the_protection_makes_the_process_non_dumpable() {
        assert_eq!(
            in_child(|| {
                child::set_dumpable(true).unwrap();
                let was = dumpable();
                crate::network::protect_this_process().unwrap();
                was * 10 + dumpable()
            }),
            10
        );
    }

    fn no_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        Err(io::Error::from_raw_os_error(libc::EMFILE))
    }

    /// Catches a Command started without the report its sandbox needs: whatever the network,
    /// a pipe that cannot be made fails the start.
    #[test]
    fn a_report_pipe_that_cannot_be_made_is_an_io_error() {
        for network in [Network::Denied, Network::Allowed] {
            let mut process = Process::new("true");
            let Err(error) = confine_with(&mut process, network, Layout::default(), no_pipe) else {
                panic!("confined without a pipe");
            };
            assert!(matches!(error, RunError::Io { .. }), "{error}");
        }
    }

    #[test]
    fn only_a_whole_network_namespace_picks_the_namespace_program() {
        let (strict, in_netns) = (seccomp::filter(false), seccomp::filter(true));
        assert!(std::ptr::eq(
            pick(true, &strict, &in_netns),
            in_netns.as_slice()
        ));
        assert!(std::ptr::eq(
            pick(false, &strict, &in_netns),
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
    let result = confine(&mut process, Network::Denied, Layout::default());
    if cfg!(feature = "unconfined-for-tests") {
        assert_eq!(
            result.expect("tolerated").read(),
            (Layers::Unavailable, Sandbox::Unavailable)
        );
    } else {
        assert!(matches!(result, Err(RunError::NoConfine)));
    }
    let mut process = Process::new("true");
    let allowed = confine(&mut process, Network::Allowed, Layout::default()).expect("allowed");
    assert_eq!(allowed.read(), (Layers::Open, Sandbox::Unavailable));
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
/// would run it with the network on macOS; a process that is `sandbox-exec` passes, and layers say
/// so. There is no sandbox on macOS.
#[cfg(target_os = "macos")]
#[test]
fn on_macos_confine_refuses_a_denied_command_that_is_not_under_sandbox_exec() {
    let mut plain = Process::new("/bin/echo");
    assert!(matches!(
        confine(&mut plain, Network::Denied, Layout::default()),
        Err(RunError::NoConfine)
    ));
    let mut wrapped = Process::new("/usr/bin/sandbox-exec");
    assert_eq!(
        confine(&mut wrapped, Network::Denied, Layout::default())
            .expect("wrapped")
            .read(),
        (Layers::Seatbelt, Sandbox::Unavailable)
    );
    let mut allowed = Process::new("/bin/echo");
    assert_eq!(
        confine(&mut allowed, Network::Allowed, Layout::default())
            .expect("allowed")
            .read(),
        (Layers::Open, Sandbox::Unavailable)
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
