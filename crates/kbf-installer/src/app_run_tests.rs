//! The `run` loop and `boot-check` verbs, and the operator verbs, through the dispatcher.

use super::*;

/// Catches `run` not doing a pass in this build of the crate (the real binary does; this is
/// the same code run in the unit tests, so a change to it is seen by both): a pass on a host
/// reports and exits 0 even as an ordinary user, with an order record that is not one the
/// pass still ends, an unreadable state stops it before any pass (exit 1), and a memory file
/// it cannot read stops the loop with the file named.
///
/// Catches: the pass count ignored (the test would hang); `recover` errors dropped
/// (the ruined state is then not an exit 1); the cycle error dropped (helper.json is then not
/// reported).
#[test]
fn run_does_its_passes_and_stops_on_a_state_it_cannot_read() {
    let (layout, root) = seeded("run");
    let (exit, err) = on(&root, &RUN);
    assert_eq!(exit, Exit::Ok, "{err}");
    let status =
        fs::read(layout.join(&layout.update_dir()).join("status.json")).expect("status.json");
    let status: Value = serde_json::from_slice(&status).expect("json");
    assert_eq!(status["state"], "refused", "not root: the pass says so");
    let record = kbf_update::record::file_name(1, 2);
    fs::write(
        layout.join(&layout.orders_dir()).join(record),
        b"not an order",
    )
    .expect("record");
    let (exit, err) = on(&root, &RUN);
    assert_eq!(exit, Exit::Ok, "{err}");
    fs::write(
        layout.join(&layout.state_dir()).join("helper.json"),
        b"not json",
    )
    .expect("ruin");
    let (exit, err) = on(&root, &RUN);
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("helper.json"), "{err}");
    fs::remove_file(layout.join(&layout.state_dir()).join("helper.json")).expect("remove");
    fs::write(layout.join(&layout.state_file()), b"not json").expect("ruin");
    let (exit, err) = on(&root, &RUN);
    assert_eq!(
        exit,
        Exit::NeedsOperator,
        "no usable state: only an operator goes on"
    );
    assert!(err.starts_with("kbf-installer:"), "{err}");
}

/// Catches `run` not removing the scratch a killed stage left in `work/` when it starts: the
/// verb is run as an ordinary user over a host with a `<commit>.partial` directory (with a
/// download in it) and a `<commit>.check` directory, and both are gone afterwards while a
/// neighbour that is not scratch stays. The helper's tests drive `recover` directly; this
/// one pins that the verb calls it.
///
/// Red on main: nothing sweeps `work/`, so the directories are still there.
///
/// Catches: the `recover` call dropped from `run`
/// (the directories stay).
#[test]
fn run_removes_the_scratch_a_killed_stage_left_when_it_starts() {
    let (layout, root) = seeded("run-sweeps");
    let work = layout.join(&layout.work_dir());
    let partial = work.join(format!("{B}.partial"));
    fs::create_dir_all(partial.join("dl")).expect("a killed stage's directory");
    fs::write(partial.join("dl/kbf.tar.gz"), b"half a download").expect("its download");
    fs::create_dir_all(work.join(format!("{B}.check"))).expect("a killed stage's checks");
    fs::write(work.join("notes.txt"), b"not scratch").expect("a neighbour");
    let (exit, err) = on(&root, &RUN);
    assert_eq!(exit, Exit::Ok, "{err}");
    let (check, notes) = (work.join(format!("{B}.check")), work.join("notes.txt"));
    assert!(!partial.exists(), "the .partial directory stays");
    assert!(!check.exists(), "the .check directory stays");
    assert!(notes.is_file(), "a file that is not scratch went");
}

/// Catches `run` not waiting between passes, or not stopping after the last: two passes on a
/// host take one `PASS_INTERVAL` (the only wait, none after the last pass) and exit 0.
///
/// Catches: the wait dropped (the test finishes in well under the interval); the
/// wait taken after the last pass too (it takes two intervals).
#[test]
fn run_waits_one_interval_between_two_passes_and_not_after_the_last() {
    let (_, root) = seeded("run-two-passes");
    let began = std::time::Instant::now();
    let (exit, err) = on(&root, &["run", "--passes", "2", "--unit", "kbf.service"]);
    let took = began.elapsed();
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(took >= PASS_INTERVAL, "{took:?}");
    assert!(took < PASS_INTERVAL * 2, "{took:?}");
}

/// Catches a boot check that does not count a start or that guesses on a state it cannot
/// read: with a swap on trial the first start is counted (exit 0), the last start's revert
/// needs root and so refuses an ordinary user (exit 1), a ruined state refuses (exit 1), and
/// a relative root is refused for both verbs before anything is read.
///
/// Catches: the decision printed but the error arm returning `Ok` (the refusals
/// above are then exit 0).
#[test]
fn boot_check_counts_a_start_and_refuses_what_it_cannot_read() {
    let (layout, root) = seeded("boot");
    let release = layout.join(&layout.release_dir(&c(B)));
    crate::testkit::mkdirs(&root, &release, 0o755);
    fs::write(release.join("kbf"), b"x").expect("binary");
    let lock = Locked::acquire(&layout).expect("lock");
    lock.write(&State {
        current: Some(c(B)),
        previous: Some(c(A)),
        pending_confirm: Some(PendingConfirm {
            commit: c(B),
            previous: Some(c(A)),
            activation: "5".to_owned(),
            deadline_wall: None,
            deadline_mono_s: None,
            starts_left: Some(1),
        }),
        ..State::default()
    })
    .expect("state");
    drop(lock);
    let (exit, err) = on(&root, &["boot-check", "--window-secs", "600"]);
    assert_eq!(exit, Exit::Ok, "{err}");
    let (exit, err) = on(&root, &["boot-check"]);
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("as root"), "{err}");
    for file in [layout.state_file(), layout.state_backup()] {
        fs::write(layout.join(&file), b"not json").expect("ruin");
    }
    let (exit, err) = on(&root, &["boot-check"]);
    assert_eq!(exit, Exit::NeedsOperator, "{err}");
    for verb in [&RUN[..], &["boot-check"][..]] {
        let mut full = vec!["--root", "relative"];
        full.extend_from_slice(verb);
        let (exit, _, _) = run_as(1000, &full);
        assert_eq!(exit, Exit::Failed, "{verb:?}");
    }
}

/// Asserts that a bundled host's boot check reads no state the host does not have and runs the
/// kbf its unit names. With `--kbf` and `--mode` the verb never calls `activate::boot_check`
/// (both state files are ruined here, which is exit 5 without the flags), and a kbf that is
/// not a file ends the check with exit 1, the kbf named, for a mode that keeps no state in
/// the database as for one that does.
#[test]
fn a_bundled_boot_check_runs_the_kbf_it_names_and_never_reads_the_state() {
    let (layout, root) = seeded("boot-bundled");
    for file in [layout.state_file(), layout.state_backup()] {
        fs::write(layout.join(&file), b"not json").expect("ruin");
    }
    let (exit, err) = on(&root, &["boot-check"]);
    assert_eq!(exit, Exit::NeedsOperator, "{err}");

    let flag = format!("--kbf={}", root.join("no-such-kbf").display());
    for mode in ["--mode=daemon", "--mode=frontend"] {
        let (exit, err) = on(&root, &["boot-check", flag.as_str(), mode]);
        assert_eq!(exit, Exit::Failed, "{mode}: {err}");
        assert!(err.contains("cannot run"), "{mode}: {err}");
        assert!(err.contains("no-such-kbf"), "{mode}: {err}");
    }
}

/// Asserts that a bundled host's boot check runs `<kbf> boot-check --mode=<mode>` once, for
/// each of the five modes, with the host's `installer.toml` under the root named by
/// `--installer-config`, as the caller with an empty environment in `/` and for at most 120 s,
/// and passes when it exits 0. A relative root is refused before anything is run.
#[test]
fn the_bundled_boot_check_runs_kbf_boot_check_for_the_mode_and_passes_when_it_does() {
    let (layout, root) = seeded("boot-bundled-args");
    let installer_toml = layout.join(&layout.installer_toml());
    let config = format!("--installer-config={}", installer_toml.display());
    let modes = [
        (Mode::Frontend, "frontend"),
        (Mode::Storage, "storage"),
        (Mode::Daemon, "daemon"),
        (Mode::StorageAndDaemon, "storage-and-daemon"),
        (Mode::AllInOne, "all-in-one"),
    ];
    for (mode, name) in modes {
        let runner = FakeRunner::new(|_| Ok(ran(0, "", "kbf boot-check: skipped\n")));
        let kbf = Path::new("/usr/bin/kbf");
        bundled_boot_check(root.clone(), &runner, kbf, mode).expect("passes");
        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "{name}");
        let spawn = &calls[0];
        let flag = format!("--mode={name}");
        assert_eq!(spawn.program, kbf);
        assert_eq!(spawn.args, ["boot-check", flag.as_str(), config.as_str()]);
        assert!(spawn.env.is_empty(), "{name}");
        assert_eq!(spawn.cwd, Path::new("/"), "{name}");
        assert_eq!(spawn.run_as, None, "{name}");
        assert_eq!(spawn.timeout, Duration::from_secs(120), "{name}");
    }
    let runner = FakeRunner::new(|_| Ok(ran(0, "", "")));
    let kbf = Path::new("/usr/bin/kbf");
    let refused = bundled_boot_check("relative".into(), &runner, kbf, Mode::Daemon);
    assert_eq!(refused.unwrap_err().exit, Exit::Failed);
    assert!(runner.calls().is_empty());
}

/// Asserts that a bundled host's boot check ends as the kbf it ran ended: an exit status other
/// than 0, a signal, a run past 120 s and a kbf that could not be started are each exit 1 with
/// the kbf named and what happened, and an exit status 2 (a kbf without the verb) says to
/// upgrade the kbf package.
#[test]
fn the_bundled_boot_check_fails_when_kbf_boot_check_does_not_exit_0() {
    let (_, root) = seeded("boot-bundled-ended");
    let mut timed_out = ran(0, "", "");
    timed_out.timed_out = true;
    let mut signalled = ran(0, "", "");
    signalled.code = None;
    let ended = |answer: Ran| FakeRunner::new(move |_| Ok(answer.clone()));
    let cases = [
        (ended(ran(1, "", "busy\n")), "ended with Some(1)"),
        (ended(ran(2, "", "")), "upgrade the kbf package"),
        (ended(signalled), "ended with None"),
        (ended(timed_out), "did not end in 120 s"),
        (
            FakeRunner::new(|_| Err(std::io::ErrorKind::NotFound.into())),
            "cannot run /usr/bin/kbf: entity not found",
        ),
    ];
    let kbf = Path::new("/usr/bin/kbf");
    for (runner, says) in cases {
        let failure = bundled_boot_check(root.clone(), &runner, kbf, Mode::Frontend);
        let failure = failure.unwrap_err();
        assert_eq!(failure.exit, Exit::Failed, "{says}");
        let why = &failure.why;
        assert!(why.contains("/usr/bin/kbf"), "{says}: {why}");
        assert!(why.contains(says), "{says}: {why}");
    }
}

/// Catches the bundled flags of `boot-check` being taken singly or with a mode that does not
/// exist: `--kbf` needs `--mode` and `--mode` needs `--kbf` (a missing argument), a mode `kbf`
/// does not have is an invalid value, neither flag is the connected host's check, and both
/// give the path and the mode back together.
///
/// Catches: either `requires` dropped from the flags; `bundled` returning a fixed
/// mode, or `Some` for a lone `--kbf`.
#[test]
fn the_bundled_flags_of_boot_check_come_together_or_not_at_all() {
    use clap::error::ErrorKind;
    let refused = |args: &[&str]| {
        let mut full = vec!["kbf-installer", "boot-check"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).err().map(|e| e.kind())
    };
    let missing = Some(ErrorKind::MissingRequiredArgument);
    assert_eq!(refused(&["--kbf", "/usr/bin/kbf"]), missing);
    assert_eq!(refused(&["--mode", "frontend"]), missing);
    let node = refused(&["--kbf", "/usr/bin/kbf", "--mode", "node"]);
    assert_eq!(node, Some(ErrorKind::InvalidValue));
    assert_eq!(refused(&[]), None);
    let both = refused(&["--kbf", "/usr/bin/kbf", "--mode", "all-in-one"]);
    assert_eq!(both, None);

    let cli = parse(&["boot-check", "--kbf=/usr/bin/kbf", "--mode=all-in-one"]);
    let Verb::BootCheck(args) = cli.verb else {
        panic!("boot-check");
    };
    assert_eq!(
        args.bundled(),
        Some((Path::new("/usr/bin/kbf"), Mode::AllInOne))
    );
    let Verb::BootCheck(args) = parse(&["boot-check"]).verb else {
        panic!("boot-check");
    };
    assert_eq!(args.bundled(), None);
}

/// Catches an exit that does not tell an operator's job from a retry: only an unusable state
/// (both files damaged) ends `needs an operator` (5); every other state error, a memory file
/// that cannot be read, and a plain message end `failed` (1). Both of the error types a verb
/// can end with are checked.
///
/// Catches: the guard dropped (every state error asks for an operator).
#[test]
fn only_an_unusable_state_asks_for_an_operator() {
    use crate::helper::cycle::HelperError;
    use crate::release::ReleaseError;
    use crate::state::StateError;
    let unusable = || StateError::Unusable {
        primary: "p".to_owned(),
        backup: "b".to_owned(),
    };
    let other = || StateError::NoSchema {
        path: PathBuf::from("/x"),
    };
    let exit = |f: Failure| f.exit;
    assert_eq!(
        exit(HelperError::State(unusable()).into()),
        Exit::NeedsOperator
    );
    assert_eq!(exit(HelperError::State(other()).into()), Exit::Failed);
    assert_eq!(
        exit(HelperError::Memory("m".to_owned()).into()),
        Exit::Failed
    );
    assert_eq!(
        exit(ReleaseError::State(unusable()).into()),
        Exit::NeedsOperator
    );
    assert_eq!(exit(ReleaseError::State(other()).into()), Exit::Failed);
    assert_eq!(exit(ReleaseError::Pending(c(A)).into()), Exit::Failed);
    assert_eq!(exit("message".to_owned().into()), Exit::Failed);
}

/// Catches the loop of `run` going on with a file that was replaced under it: the file it
/// watches is another file (here one that is no program), so the turn says re-exec, the
/// loop tries to start it, and the failure is the reason it ends with (exit 1).
///
/// Catches: the watch never consulted; the failed start taken for a success.
#[test]
fn a_loop_whose_file_was_replaced_tries_the_new_one_and_ends_when_it_cannot_start() {
    let (_, root) = seeded("replaced");
    let dir = root.join("copy");
    fs::create_dir_all(&dir).expect("dir");
    let me = dir.join("kbf-installer");
    fs::write(&me, b"the program").expect("the program");
    let watch = FileIdentity::of(&me).expect("identity");
    let other = dir.join("other");
    fs::write(&other, b"not a program").expect("a file that is no program");
    fs::rename(&other, &me).expect("replaced");
    let cli = parse(&["run", "--passes", "3", "--unit", "kbf.service"]);
    let Verb::Run(args) = &cli.verb else {
        panic!("a run");
    };
    let Err(failure) = run_from(root, args, &me, watch) else {
        panic!("the new file cannot start");
    };
    assert_eq!(failure.exit, Exit::Failed);
    assert!(failure.why.contains("cannot start"), "{}", failure.why);
    // A start that fails leaves this process as it was: `Command::exec` would have put
    // SIGPIPE back to its default (the standard library does that before it execs), and a
    // later write to a pipe nobody reads would then end the whole process by a signal
    // instead of failing with an error. Nothing else in the lib tests reaches the re-exec.
    assert_eq!(sigpipe_handler(), libc::SIG_IGN, "SIGPIPE is ignored again");
}

/// The disposition of SIGPIPE in this process.
fn sigpipe_handler() -> libc::sighandler_t {
    // SAFETY: sigaction with a null new action only reads the current one into `old`, which
    // a zeroed `sigaction` is valid for.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        assert_eq!(
            libc::sigaction(libc::SIGPIPE, std::ptr::null(), &raw mut old),
            0
        );
        old.sa_sigaction
    }
}

/// Catches the operator's verbs not being wired to the dispatcher, or their exits being
/// remapped on the way: through the dispatcher on a host with nothing on trial, `confirm`,
/// `revert` and `rollback` need the operator (5), `quiesce` is at rest (0) and a dry-run `gc`
/// succeeds; each failure of the operator keeps its exit code through the conversion.
///
/// Catches: a verb dispatched to another verb's function; the refused exit folded
/// into failed.
#[test]
fn the_operators_verbs_run_through_the_dispatcher_with_their_own_exits() {
    use crate::helper::operator::Failure as Operator;
    let (_, root) = seeded("operator-verbs");
    let with = |verb: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = verb.iter().map(ToString::to_string).collect();
        v.extend(["--unit".to_owned(), "kbf.service".to_owned()]);
        v
    };
    let run = |args: Vec<String>| {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        on(&root, &refs).0
    };
    assert_eq!(run(with(&["confirm", "--id=1-5"])), Exit::NeedsOperator);
    assert_eq!(run(with(&["revert", "--id=1-5"])), Exit::NeedsOperator);
    assert_eq!(run(with(&["rollback"])), Exit::NeedsOperator);
    assert_eq!(run(vec!["quiesce".to_owned()]), Exit::Ok);
    assert_eq!(run(vec!["gc".to_owned()]), Exit::Ok);
    let exit = |f: Operator| Failure::from(f).exit;
    assert_eq!(exit(Operator::Failed("x".to_owned())), Exit::Failed);
    assert_eq!(exit(Operator::Refused("x".to_owned())), Exit::Refused);
    assert_eq!(exit(Operator::Busy("x".to_owned())), Exit::Busy);
    assert_eq!(
        exit(Operator::NeedsOperator("x".to_owned())),
        Exit::NeedsOperator
    );
}

/// Catches `record-release` not running through the dispatcher, or its refusals leaving with
/// another exit: as root, files that are not a release (a release that was never placed, a
/// binary that is not the executable this host runs, a host root that is not absolute) end with
/// the exit of the refusal and the program on the line; a release error keeps the exit of
/// the release verbs.
///
/// Catches: the verb dispatched to `gc`; the refusal folded into the failure exit.
#[test]
fn record_release_runs_through_the_dispatcher_and_refuses_what_is_not_a_release() {
    use crate::record_release::RecordError;
    use crate::release::ReleaseError;
    let (_, root) = seeded("record-release");
    let root_arg = root.to_str().expect("text");
    for commit in [A, B] {
        let (exit, _, err) = run_as(
            0,
            &[
                "--root",
                root_arg,
                "--apply",
                "record-release",
                "--commit",
                commit,
            ],
        );
        assert_eq!(exit, Exit::Refused, "{commit}: {err}");
        assert!(err.starts_with("kbf-installer: "), "{err}");
    }
    let (exit, _, err) = run_as(0, &["--root", "relative", "record-release", "--commit", A]);
    assert_eq!(exit, Exit::Failed, "{err}");

    let release = |e: ReleaseError| Failure::from(RecordError::Release(e)).exit;
    assert_eq!(release(ReleaseError::NotFirst(c(A))), Exit::Failed);
    let read = io::Error::from(io::ErrorKind::NotFound);
    let refused = Failure::from(RecordError::Read {
        path: "p".to_owned(),
        source: read,
    });
    assert_eq!(refused.exit, Exit::Refused);
    assert!(refused.why.contains("cannot read p"), "{}", refused.why);
}
