//! The readers of the tools' output, over what the tools really printed: the files under
//! `tests/fixtures/mac` (see its README) were captured by the `host-install-mac` workflow on
//! a GitHub-hosted macOS runner. The fake launchd, codesign and lsof of the other tests answer
//! with these same texts.

use super::fx::real;
use crate::mac::tools;
use crate::runner::Identity;

/// `launchctl print` of a job in each state the install tells apart: not started (`state =
/// not running`, `last exit code = (never exited)`), running (a pid), ended with exit code 0,
/// ended with exit code 1, and killed by signal 9, which has no `last exit code` line at all.
#[test]
fn launchctl_print_of_each_real_state() {
    let pid = real::pid_of(real::RUNNING).parse::<u32>().unwrap();
    // (listing, running, finished, pid)
    for (name, text, running, finished, pid) in [
        ("not started", real::IDLE, false, false, None),
        ("running", real::RUNNING, true, false, Some(pid)),
        ("clean exit", real::CLEAN_EXIT, false, true, None),
        ("exit 1", real::NONZERO_EXIT, false, true, None),
        ("kill -9", real::KILLED, false, true, None),
    ] {
        assert_eq!(tools::print_running(text), running, "{name}");
        assert_eq!(tools::print_finished(text), finished, "{name}");
        assert_eq!(tools::print_pid(text), pid, "{name}");
    }
    // The wording the readers rely on.
    assert!(real::IDLE.contains("\tlast exit code = (never exited)\n"));
    assert!(real::RUNNING.contains("\tlast exit code = (never exited)\n"));
    assert!(real::CLEAN_EXIT.contains("\tlast exit code = 0\n"));
    assert!(real::NONZERO_EXIT.contains("\tlast exit code = 1\n"));
    assert!(real::KILLED.contains("\tlast terminating signal = Killed: 9\n"));
    assert!(!real::KILLED.contains("last exit code"));
}

/// A real listing with nested blocks (`event triggers`, `endpoints`, `event channels`, each
/// with fields of its own) is read at its top level. No real listing of the system's own
/// services has `state`, `pid` or `last exit code` in a nested block (the workflow scanned
/// them all); lines of those names are put into the nested blocks here, to show that they are
/// not read: not as a state, not as a pid, not as an exit, and not when the top-level line
/// is the one that is missing.
#[test]
fn a_nested_block_is_never_read_as_the_services_own_field() {
    let text = real::NESTED;
    assert!(text.contains("\tendpoints = {\n\t\t\""));
    assert!(!tools::print_running(text) && !tools::print_finished(text));
    assert_eq!(tools::print_pid(text), None);

    let nested_fields = "\t\t\tstate = running\n\t\t\tpid = 99\n\t\t\tlast exit code = 7\n\t\t\tlast terminating signal = Killed: 9\n";
    let nested = text.replace(
        "\t\t\twatching = 1\n",
        &format!("\t\t\twatching = 1\n{nested_fields}"),
    );
    assert_ne!(nested, text);
    assert!(!tools::print_running(&nested));
    assert!(!tools::print_finished(&nested));
    assert_eq!(tools::print_pid(&nested), None);

    // The top-level lines gone: nothing is read from the blocks.
    let bare: String = nested
        .lines()
        .filter(|line| {
            !["\tstate = ", "\tlast exit code = "]
                .iter()
                .any(|key| line.starts_with(key))
        })
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(!tools::print_running(&bare) && !tools::print_finished(&bare));
    assert_eq!(tools::print_pid(&bare), None);
    // Indentation is part of the shape: spaces are not a top-level line either.
    assert!(!tools::print_running("    state = running\n"));
    assert!(!tools::print_running("system/x = {\nstate = running\n}\n"));
}

/// The real `launchctl print` of a label that is not loaded fails with status 113 and says so
/// on stderr; that failure is what "not loaded" means to the install.
#[test]
fn a_service_that_is_not_loaded_fails_to_print() {
    assert_eq!(real::MISSING_CODE.trim(), "113");
    assert!(real::MISSING_STDERR.contains("Could not find service"));
    let fx = super::fx::Fx::new("mac-real-missing");
    let sim = fx.sim();
    sim.loaded.borrow_mut().clear();
    let exec = crate::runner::Runner::run(&sim, &tools::print("com.x", std::time::Duration::ZERO))
        .unwrap();
    assert_eq!(exec.code, Some(113));
    assert!(
        exec.stderr_text()
            .contains("Could not find service \"com.x\"")
    );
}

/// `lsof -F n` of a running process: a `p` line, then an `f` and an `n` line for each file
/// mapped as text (the program, then the dynamic linker); only the `n` lines are paths.
#[test]
fn lsof_of_a_real_process_gives_its_program() {
    assert_eq!(
        tools::lsof_paths(real::LSOF),
        ["/bin/sleep", "/usr/lib/dyld"]
    );
    assert!(real::LSOF.starts_with('p'));
}

/// `codesign -d --verbose=4` writes on stderr and nothing on stdout; the code directory's
/// flags list `runtime` for a binary signed ad hoc with the hardened runtime and not for one
/// signed without it.
#[test]
fn codesign_describes_the_hardened_runtime_on_stderr() {
    assert!(real::CODESIGN_HARDENED.contains("flags=0x10002(adhoc,runtime)"));
    assert!(tools::has_hardened_runtime(real::CODESIGN_HARDENED));
    assert!(real::CODESIGN_PLAIN.contains("flags=0x2(adhoc)"));
    assert!(!tools::has_hardened_runtime(real::CODESIGN_PLAIN));
    // The line `Runtime Version=` is not the flag.
    assert!(real::CODESIGN_HARDENED.contains("Runtime Version="));
    assert!(!tools::has_hardened_runtime("Runtime Version=15.5.0\n"));
}

/// `dscl` prints the ids of `nobody` as `-2`, which is the 32-bit id 4294967294; positive ids
/// are read as they are, and numbers outside the 32-bit range are no id.
#[test]
fn dscl_prints_nobody_with_negative_ids() {
    let nobody = Identity {
        uid: 4_294_967_294,
        gid: 4_294_967_294,
    };
    assert_eq!(tools::parse_ids(real::DSCL_NOBODY), Some(nobody));
    let ids = |uid: &str, gid: &str| {
        tools::parse_ids(&format!("PrimaryGroupID: {gid}\nUniqueID: {uid}\n"))
    };
    assert_eq!(
        ids("-1", "20"),
        Some(Identity {
            uid: u32::MAX,
            gid: 20
        })
    );
    assert_eq!(ids("-2147483648", "0").map(|i| i.uid), Some(0x8000_0000));
    assert_eq!(ids("4294967295", "1").map(|i| i.uid), Some(u32::MAX));
    assert_eq!(ids("-2147483649", "1"), None);
    assert_eq!(ids("4294967296", "1"), None);
    assert_eq!(ids("1", "-"), None);
}

/// `xattr` lists the quarantine attribute by name, and lists nothing for a clean file.
#[test]
fn xattr_lists_the_quarantine_attribute_by_name() {
    assert!(tools::has_quarantine(real::XATTR_QUARANTINED));
    assert!(real::XATTR_CLEAN.is_empty());
    assert!(!tools::has_quarantine(real::XATTR_CLEAN));
}
