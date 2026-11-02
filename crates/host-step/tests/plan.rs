//! The planner and the executor on the fake host.

use host_step::fake::FakeHost;
use host_step::{
    Account, Cmd, DRY_RUN_NOTE, ExecError, GETENT, Host, Kind, Mode, NOTHING_TO_DO, Planner,
    SYSTEMCTL, Step, USERADD, UnitVerb, execute, ids,
};

fn run(plan: &host_step::Plan, mode: Mode, host: &FakeHost) -> (Result<(), ExecError>, String) {
    let mut out = Vec::new();
    let result = execute(plan, mode, host, &mut out);
    (result, String::from_utf8(out).expect("utf8"))
}

fn svc() -> Account {
    Account::new("svc", "svc")
}

/// Catches a plan line that prints a file's contents: a secret written by a step would
/// show in a dry run. The line names path, mode, owner and size only.
#[test]
fn a_dry_run_lists_names_sizes_and_modes_and_never_contents() {
    let host = FakeHost::new(0);
    host.add_user("svc", 900);
    let mut p = Planner::new(&host);
    assert!(p.dir("/srv/x", 0o700, &svc()).unwrap());
    assert!(
        p.file("/srv/x/key", 0o600, &Account::root(), b"TOPSECRET")
            .unwrap()
    );
    p.unit(UnitVerb::DaemonReload, &[]);
    p.unit(UnitVerb::Enable, &["a.service", "b.timer"]);
    p.run(Cmd::new("/bin/tool").arg("go"));
    host.reply(&[GETENT, "passwd", "other"], 2, "");
    assert!(p.user("other", "/var/lib/other").unwrap());
    assert!(!p.absent("/nope").unwrap());
    let plan = p.finish();
    let (result, shown) = run(&plan, Mode::DryRun, &host);
    result.unwrap();
    let want = [
        "DRY-RUN: mkdir /srv/x mode 0700 owner svc:svc",
        "DRY-RUN: write /srv/x/key mode 0600 owner root:root (9 bytes)",
        "DRY-RUN: systemctl daemon-reload",
        "DRY-RUN: systemctl enable -- a.service b.timer",
        "DRY-RUN: run /bin/tool go",
        "DRY-RUN: useradd other (system user and group, home /var/lib/other, no login)",
        DRY_RUN_NOTE,
    ];
    assert_eq!(shown, want.map(|l| format!("{l}\n")).concat());
    assert!(!shown.contains("TOPSECRET"));
    // Nothing changed and nothing but lookups ran.
    assert!(host.node("/srv/x").is_none());
    for call in host.calls() {
        assert_eq!(call[0], GETENT, "{call:?}");
    }
}

/// Catches an apply that skips a step, does them out of order, or sets a mode other than the
/// one asked for.
#[test]
fn apply_does_each_step_with_exact_modes_and_owners() {
    let host = FakeHost::new(0);
    host.add_user("svc", 900);
    host.reply(&[SYSTEMCTL, "disable", "--now", "--", "x.service"], 0, "");
    host.reply(&[SYSTEMCTL, "restart", "--", "x.service"], 0, "");
    host.reply(&["/bin/tool", "go"], 0, "");
    let mut p = Planner::new(&host);
    p.dir("/srv/x", 0o750, &svc()).unwrap();
    p.file("/srv/x/f", 0o640, &svc(), b"abc").unwrap();
    p.unit(UnitVerb::DisableNow, &["x.service"]);
    p.unit(UnitVerb::Restart, &["x.service"]);
    p.run(Cmd::new("/bin/tool").arg("go"));
    let (result, shown) = run(&p.finish(), Mode::Apply, &host);
    result.unwrap();
    assert!(
        shown.starts_with("+ mkdir /srv/x mode 0750 owner svc:svc\n"),
        "{shown}"
    );
    let dir = host.node("/srv/x").unwrap();
    assert_eq!(
        (dir.kind, dir.mode, dir.uid, dir.gid),
        (Kind::Dir, 0o750, 900, 900)
    );
    let file = host.node("/srv/x/f").unwrap();
    assert_eq!(
        (file.mode, file.uid, file.gid, file.bytes),
        (0o640, 900, 900, b"abc".to_vec())
    );
    let ran: Vec<String> = host
        .calls()
        .iter()
        .filter(|c| c[0] != GETENT)
        .map(|c| c.join(" "))
        .collect();
    assert_eq!(
        ran,
        [
            "/usr/bin/systemctl disable --now -- x.service",
            "/usr/bin/systemctl restart -- x.service",
            "/bin/tool go"
        ]
    );
}

/// Catches a second run that plans again what the first did: a rerun must find nothing.
#[test]
fn a_second_plan_after_an_apply_is_empty() {
    let host = FakeHost::new(0);
    host.add_user("svc", 900);
    let plan_it = |host: &FakeHost| {
        let mut p = Planner::new(host);
        p.dir("/srv/x", 0o750, &svc()).unwrap();
        p.file("/srv/x/f", 0o640, &svc(), b"abc").unwrap();
        p.finish()
    };
    let first = plan_it(&host);
    assert_eq!(first.steps.len(), 2);
    run(&first, Mode::Apply, &host).0.unwrap();
    let second = plan_it(&host);
    assert!(second.steps.is_empty());
    let (result, shown) = run(&second, Mode::Apply, &host);
    result.unwrap();
    assert_eq!(shown, format!("{NOTHING_TO_DO}\n"));
}

/// Catches a planner that treats a differing mode, owner, content or kind as "the same":
/// each is a step.
#[test]
fn a_different_mode_owner_content_or_kind_is_planned() {
    let host = FakeHost::new(0);
    host.add_user("svc", 900);
    host.put_dir("/d", 0o750, (900, 900));
    host.put_file("/f", 0o640, (900, 900), b"abc");
    host.reply(&[GETENT, "passwd", "later"], 2, "");
    host.reply(&[GETENT, "group", "later"], 2, "");
    let steps = |f: &dyn Fn(&mut Planner<'_>)| {
        let mut p = Planner::new(&host);
        f(&mut p);
        p.finish().steps.len()
    };
    assert_eq!(
        steps(&|p| {
            p.dir("/d", 0o750, &svc()).unwrap();
        }),
        0
    );
    assert_eq!(
        steps(&|p| {
            p.dir("/d", 0o700, &svc()).unwrap();
        }),
        1
    );
    assert_eq!(
        steps(&|p| {
            p.dir("/d", 0o750, &Account::root()).unwrap();
        }),
        1
    );
    assert_eq!(
        steps(&|p| {
            p.dir("/f", 0o640, &svc()).unwrap();
        }),
        1
    );
    assert_eq!(
        steps(&|p| {
            p.file("/f", 0o640, &svc(), b"abc").unwrap();
        }),
        0
    );
    assert_eq!(
        steps(&|p| {
            p.file("/f", 0o640, &svc(), b"abd").unwrap();
        }),
        1
    );
    assert_eq!(
        steps(&|p| {
            p.file("/f", 0o600, &svc(), b"abc").unwrap();
        }),
        1
    );
    assert_eq!(
        steps(&|p| {
            p.file("/f", 0o640, &Account::root(), b"abc").unwrap();
        }),
        1
    );
    assert_eq!(
        steps(&|p| {
            p.file("/d", 0o640, &svc(), b"").unwrap();
        }),
        1
    );
    assert_eq!(
        steps(&|p| {
            p.absent("/f").unwrap();
        }),
        1
    );
    // A user that does not exist yet cannot own anything yet: the step is planned.
    assert_eq!(
        steps(&|p| {
            p.dir("/d", 0o750, &Account::new("later", "later")).unwrap();
        }),
        1
    );
}

/// Catches a service plan that restarts what it should only start, or starts what runs:
/// enabled and active with no change is no step; a change restarts; idle starts.
#[test]
fn a_service_is_enabled_started_or_restarted_as_needed() {
    let host = FakeHost::new(0);
    let on = |unit: &str, enabled: i32, active: i32| {
        host.reply(&[SYSTEMCTL, "is-enabled", unit], enabled, "");
        host.reply(&[SYSTEMCTL, "is-active", unit], active, "");
    };
    on("up.service", 0, 0);
    on("idle.service", 1, 3);
    let mut p = Planner::new(&host);
    assert!(!p.service("up.service", false).unwrap());
    assert!(p.service("up.service", true).unwrap());
    assert!(p.service("idle.service", false).unwrap());
    assert!(p.service("idle.service", true).unwrap());
    let lines: Vec<String> = p.finish().steps.iter().map(Step::describe).collect();
    assert_eq!(
        lines,
        [
            "systemctl restart -- up.service",
            "systemctl enable -- idle.service",
            "systemctl start -- idle.service",
            "systemctl enable -- idle.service",
            "systemctl restart -- idle.service",
        ]
    );
}

/// Catches an apply without root: it must refuse before any step, and a dry run needs no
/// root at this layer (the verbs decide).
#[test]
fn applying_without_root_is_refused_and_does_nothing() {
    let host = FakeHost::new(1000);
    host.add_user("svc", 900);
    let mut p = Planner::new(&host);
    p.dir("/srv/x", 0o700, &svc()).unwrap();
    let plan = p.finish();
    let (result, shown) = run(&plan, Mode::Apply, &host);
    let err = result.unwrap_err();
    assert!(matches!(err, ExecError::NotRoot(1000)), "{err}");
    assert_eq!(
        err.to_string(),
        "applying a plan needs root (effective uid is 1000)"
    );
    assert_eq!(shown, "");
    assert!(host.node("/srv/x").is_none());
    run(&plan, Mode::DryRun, &host).0.unwrap();
}

/// Catches an executor that goes on after a failed step, or that prints what the failed
/// program wrote: the error names the step and the exit status only.
#[test]
fn a_failing_step_stops_the_plan_and_names_itself() {
    let host = FakeHost::new(0);
    host.reply(&["/bin/bad"], 7, "SECRET-IN-OUTPUT");
    host.reply(&["/bin/good"], 0, "");
    let mut p = Planner::new(&host);
    p.run(Cmd::new("/bin/bad"));
    p.run(Cmd::new("/bin/good"));
    let (result, shown) = run(&p.finish(), Mode::Apply, &host);
    let err = result.unwrap_err().to_string();
    assert_eq!(err, "step 1 (run /bin/bad) failed: exit status 7");
    assert!(!shown.contains("SECRET") && !err.contains("SECRET"));
    assert!(!host.calls().iter().any(|c| c[0] == "/bin/good"));
}

/// Catches a runner that lets an unconfigured command through: it fails closed, with an
/// error that names the line, and the line is still recorded.
#[test]
fn the_fake_runner_refuses_what_it_was_not_told() {
    let host = FakeHost::new(0);
    let err = host.run(&Cmd::new("/bin/surprise").arg("x")).unwrap_err();
    assert!(
        err.to_string()
            .contains("unexpected command /bin/surprise x"),
        "{err}"
    );
    assert_eq!(
        host.calls(),
        [vec!["/bin/surprise".to_owned(), "x".to_owned()]]
    );
    host.clear_calls();
    assert!(host.calls().is_empty());
    // A planner over it surfaces the failure instead of guessing.
    let mut p = Planner::new(&host);
    assert!(p.user("nobody-such", "/x").is_err());
    // Prefix replies answer a family of lines; an exact reply wins.
    host.reply_prefix(&["/bin/fam"], 0, "any");
    host.reply(&["/bin/fam", "one"], 5, "one");
    assert_eq!(
        host.run(&Cmd::new("/bin/fam").arg("two")).unwrap().stdout,
        "any"
    );
    assert_eq!(host.run(&Cmd::new("/bin/fam").arg("one")).unwrap().code, 5);
}

/// Catches a user step that runs useradd with the wrong flags or that cannot own what it
/// creates in the same plan: the owner is looked up when the step runs, after the user is made.
#[test]
fn a_user_is_made_before_what_it_owns_and_with_no_login() {
    let host = FakeHost::new(0);
    host.reply(&[GETENT, "passwd", "svc"], 2, "");
    host.reply(&[GETENT, "group", "svc"], 2, "");
    let mut p = Planner::new(&host);
    assert!(p.user("svc", "/var/lib/svc").unwrap());
    p.dir("/var/lib/svc", 0o700, &svc()).unwrap();
    let plan = p.finish();
    // Before the user exists, the directory step fails: no such user.
    host.reply(
        &[
            USERADD,
            "--system",
            "--user-group",
            "--home-dir",
            "/var/lib/svc",
            "--no-create-home",
            "--shell",
            "/usr/sbin/nologin",
            "svc",
        ],
        0,
        "",
    );
    let (result, _) = run(&plan, Mode::Apply, &host);
    assert_eq!(
        result.unwrap_err().to_string(),
        "step 2 (mkdir /var/lib/svc mode 0700 owner svc:svc) failed: no user svc or group svc"
    );
    host.add_user("svc", 901);
    run(&plan, Mode::Apply, &host).0.unwrap();
    assert_eq!(host.node("/var/lib/svc").unwrap().uid, 901);
    // A failing useradd is a failed step.
    let host = FakeHost::new(0);
    host.reply(&[GETENT, "passwd", "svc"], 2, "");
    host.reply_prefix(&[USERADD], 9, "");
    let mut p = Planner::new(&host);
    p.user("svc", "/h").unwrap();
    let (result, _) = run(&p.finish(), Mode::Apply, &host);
    assert!(result.unwrap_err().to_string().ends_with("exit status 9"));
}

/// Catches `Cmd::as_user` that puts the user's command ahead of the user switch, and a
/// lookup that reads a failing getent as success.
#[test]
fn as_user_wraps_and_lookups_read_getent_exit_codes() {
    let cmd = Cmd::new("/bin/x").arg("a").as_user("svc");
    assert_eq!(cmd.line(), "/usr/sbin/runuser -u svc -- /bin/x a");
    let host = FakeHost::new(0);
    host.reply(&[GETENT, "passwd", "ghost"], 2, "");
    host.reply(&[GETENT, "group", "ghost"], 0, "ghost:x:5:\n");
    assert_eq!(ids(&host, &Account::new("ghost", "ghost")).unwrap(), None);
    host.reply(&[GETENT, "passwd", "odd"], 0, "garbage");
    host.reply(&[GETENT, "group", "odd"], 0, "odd:x:5:\n");
    assert_eq!(ids(&host, &Account::new("odd", "odd")).unwrap(), None);
    assert_eq!(ids(&host, &Account::root()).unwrap(), Some((0, 0)));
}

/// Catches fake mkdir that leaves a parent out, which a later `stat` of it would miss, and a
/// fake that ignores `deny_read`.
#[test]
fn the_fake_tree_makes_parents_and_can_deny_reads() {
    let host = FakeHost::new(0);
    assert_eq!((host.euid(), host.egid()), (0, 0));
    host.mkdir("/a/b/c", 0o700, (1, 2)).unwrap();
    assert_eq!(host.paths(), ["/a", "/a/b", "/a/b/c"]);
    assert_eq!(host.node("/a").unwrap().mode, 0o755);
    host.write("/a/f", 0o600, (0, 0), b"x").unwrap();
    assert_eq!(host.stat("/a/f").unwrap().unwrap().size, 1);
    host.deny_read("/a/f");
    assert_eq!(
        host.read("/a/f").unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(host.read("/a/none").unwrap(), None);
    host.remove("/a/f").unwrap();
    assert!(host.stat("/a/f").unwrap().is_none());
}

/// Catches a fake that cannot stand for a link, which the verbs must refuse to follow, and
/// an effect that fires for a command other than its own.
#[test]
fn the_fake_can_hold_a_link_and_a_command_can_leave_a_file() {
    let host = FakeHost::new(0);
    host.put_link("/l");
    assert_eq!(host.stat("/l").unwrap().unwrap().kind, Kind::Other);
    host.reply(&["/bin/make"], 0, "");
    host.reply(&["/bin/other"], 0, "");
    host.effect(&["/bin/make"], "/made", 0o600, (1, 2), b"data");
    host.run(&Cmd::new("/bin/other")).unwrap();
    assert!(host.node("/made").is_none());
    host.run(&Cmd::new("/bin/make")).unwrap();
    let made = host.node("/made").unwrap();
    assert_eq!(
        (made.mode, made.uid, made.gid, made.bytes),
        (0o600, 1, 2, b"data".to_vec())
    );
}

/// Catches a plan that shows a secret when it is formatted by an assertion, a panic or a log
/// line: `Debug` of every step prints the length of a file's bytes and never the bytes.
#[test]
fn a_plan_formatted_with_debug_shows_no_file_contents() {
    let plan = host_step::Plan {
        steps: vec![
            Step::Dir {
                path: "/d".to_owned(),
                mode: 0o700,
                owner: Account::root(),
            },
            Step::File {
                path: "/s".to_owned(),
                mode: 0o600,
                owner: Account::root(),
                bytes: b"TOPSECRET-VALUE".to_vec(),
            },
            Step::Absent {
                path: "/a".to_owned(),
            },
            Step::User {
                name: "svc".to_owned(),
                home: "/h".to_owned(),
            },
            Step::Unit {
                verb: UnitVerb::Restart,
                units: vec!["a.service".to_owned()],
            },
            Step::Run(Cmd::new("/bin/x").arg("go")),
        ],
    };
    for shown in [format!("{plan:?}"), format!("{:#?}", plan.steps)] {
        assert!(!shown.contains("TOPSECRET"), "{shown}");
        assert!(
            shown.contains("File") && shown.contains("15 bytes") && shown.contains("0600"),
            "{shown}"
        );
        for word in [
            "Dir",
            "Absent",
            "User",
            "Unit",
            "Run",
            "Restart",
            "a.service",
            "/bin/x",
        ] {
            assert!(shown.contains(word), "{word} missing in {shown}");
        }
    }
    // A failed equality assertion prints through Debug too.
    let other = Step::File {
        path: "/s".to_owned(),
        mode: 0o600,
        owner: Account::root(),
        bytes: b"OTHER-SECRET".to_vec(),
    };
    let panicked = std::panic::catch_unwind(|| assert_eq!(plan.steps[1], other));
    assert!(panicked.is_err());
}

/// Catches a fake that cannot resolve links the way the real host does: a link in the middle
/// of a path is replaced by its target, the part that does not exist is kept, and a loop
/// of links is an error rather than a hang.
#[test]
fn the_fake_resolves_symbolic_links() {
    let host = FakeHost::new(0);
    host.put_symlink("/srv/op/archives", "/data/real");
    host.put_symlink("/data/real/inner", "/elsewhere");
    assert_eq!(host.canonical("/plain/path").unwrap(), "/plain/path");
    assert_eq!(host.canonical("/").unwrap(), "/");
    assert_eq!(
        host.canonical("/srv/op/archives/x").unwrap(),
        "/data/real/x"
    );
    assert_eq!(
        host.canonical("/srv/op/archives/inner/y").unwrap(),
        "/elsewhere/y"
    );
    host.put_symlink("/loop/a", "/loop/b");
    host.put_symlink("/loop/b", "/loop/a");
    assert!(host.canonical("/loop/a").is_err());
}

/// Catches a fake that prints file contents in `Debug`, and one that reads a link or a file
/// above a limit as fine, which the real host refuses.
#[test]
fn the_fake_node_hides_contents_and_reads_like_the_real_host() {
    let host = FakeHost::new(0);
    host.put_file("/s", 0o600, (0, 0), b"TOPSECRET");
    let shown = format!("{:?}", host.node("/s").unwrap());
    assert!(
        !shown.contains("TOPSECRET") && shown.contains("9 bytes") && shown.contains("0600"),
        "{shown}"
    );
    assert_eq!(host.read_limited("/s", 9).unwrap().unwrap(), b"TOPSECRET");
    assert_eq!(
        host.read_limited("/s", 8).unwrap_err().kind(),
        std::io::ErrorKind::FileTooLarge
    );
    assert_eq!(host.read_limited("/none", 8).unwrap(), None);
    host.put_link("/l");
    assert!(host.read("/l").unwrap_err().to_string().contains("refused"));
    host.deny_read("/s");
    assert_eq!(
        host.read("/s").unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}
