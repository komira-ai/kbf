//! A test helper, not part of the tool: runs a Command through `kbf_task::run::run` with the
//! network denied, from a process that first limits itself, so the tests can see what the run
//! does when the kernel refuses a step (`tests/network.rs`).
//!
//! `run_under <dir> <deny> <protect> <program> [args]`
//! - `deny`: `none`, `seccomp` or `unshare`: an outer seccomp filter that answers that system
//!   call with `EPERM`, installed before the run. The tool's own filter is then refused, or its
//!   namespace step is.
//! - `protect`: `yes` calls `network::protect_this_process`, as the binary does.
//! - In the arguments, `{FD}` is a UDP socket this process holds open and `{PID}` is its pid.
//!
//! It prints `layers=<..>` and `exit=<code>`, or `error=<..>` when the run did not start.

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;
    use std::process::ExitCode;
    use std::time::Duration;

    use kbf_proto::reapi::Command;
    use kbf_task::network::{self, Network};
    use kbf_task::run::{Run, run};

    pub fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let [dir, deny, protect, program, rest @ ..] = args.as_slice() else {
            eprintln!("usage: run_under <dir> <deny> <protect> <program> [args]");
            return ExitCode::from(2);
        };
        if protect == "yes" {
            network::protect_this_process().expect("non-dumpable");
        }
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("a socket");
        let fd = std::os::fd::AsRawFd::as_raw_fd(&socket).to_string();
        let pid = std::process::id().to_string();
        match deny.as_str() {
            "none" => {}
            "seccomp" => deny_syscall(libc::SYS_seccomp),
            "unshare" => deny_syscall(libc::SYS_unshare),
            other => panic!("unknown deny {other}"),
        }
        let arguments = std::iter::once(program)
            .chain(rest)
            .map(|a| a.replace("{FD}", &fd).replace("{PID}", &pid))
            .collect();
        let dir = Path::new(dir);
        let root = dir.join("root");
        std::fs::create_dir_all(&root).expect("root");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let result = runtime.block_on(run(
            &Command {
                arguments,
                ..Command::default()
            },
            &Run {
                root: &root,
                private: &dir.join("private"),
                stdout: &dir.join("stdout"),
                stderr: &dir.join("stderr"),
                deadline: Duration::from_secs(60),
                network: Network::Denied,
                unreadable: &[],
            },
        ));
        match result {
            Ok(ran) => {
                println!("layers={}", ran.layers);
                println!("exit={}", ran.exit_code);
            }
            Err(e) => {
                println!("error={e}");
                // The exit code the tool gives a run that fails this way.
                println!("code={}", kbf_task::task::Failure::from(e).exit_code());
            }
        }
        ExitCode::SUCCESS
    }

    /// Installs a filter that answers system call `nr` with `EPERM` and allows the rest.
    fn deny_syscall(nr: libc::c_long) {
        let insn = |code: u32, jt: u8, jf: u8, k: u32| libc::sock_filter {
            code: u16::try_from(code).expect("opcode"),
            jt,
            jf,
            k,
        };
        let mut filter = [
            insn(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
            insn(
                libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
                0,
                1,
                u32::try_from(nr).expect("number"),
            ),
            insn(
                libc::BPF_RET | libc::BPF_K,
                0,
                0,
                libc::SECCOMP_RET_ERRNO | 1,
            ),
            insn(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_ALLOW),
        ];
        let program = libc::sock_fprog {
            len: 4,
            filter: filter.as_mut_ptr(),
        };
        // SAFETY: integer arguments only, then a program that lives through the call.
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
            assert_eq!(
                libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER,
                    0,
                    &raw const program
                ),
                0
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}
