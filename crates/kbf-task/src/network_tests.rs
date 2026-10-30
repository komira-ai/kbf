use kbf_proto::reapi::{Platform, platform::Property};

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
