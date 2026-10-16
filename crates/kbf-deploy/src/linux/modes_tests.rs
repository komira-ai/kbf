//! The mode units: each renders from the example settings into a unit that passes
//! every rule and matches its golden, each planted defect turns its rule red, and the
//! converged units `render-units` writes without `--mode` are unchanged.

use std::cell::RefCell;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{ExitStatus, Output};
use std::rc::Rc;

use kbf_host::exec::Entry;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::{FakeExec, reply};

use super::*;
use crate::linux::render::{Templates, render as render_converged};

const EXAMPLE: &str = include_str!("../../templates/linux/config.example");

fn config() -> LinuxConfig {
    LinuxConfig::parse(EXAMPLE).expect("valid settings")
}

fn golden(dir: &str, name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/linux")
        .join(dir)
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn lines(r: &Report) -> Vec<String> {
    r.failures.iter().map(ToString::to_string).collect()
}

/// Catches a mode template that breaks a rule as shipped (an old binary's path, a lost
/// `Delegate=yes`, a flag in an unbuilt mode's unit), a summary that does not name the
/// mode, and a template edit that changes a rendered unit without its golden moving in
/// the same change (the goldens are what a reviewer reads).
#[test]
fn each_mode_renders_a_unit_that_passes_every_rule_and_matches_its_golden() {
    let c = config();
    for mode in Mode::ALL {
        let text = render(&c, Some(1500), mode).expect("renders");
        let r = check(&c, mode, &text, true).expect("a unit");
        assert_eq!(lines(&r), Vec::<String>::new(), "{mode}");
        assert_eq!(
            r.summary,
            format!("UNIT OK: kbf-{mode}.service runs kbf {mode}")
        );
        assert!(
            text == golden("modes", &mode.unit_name()),
            "{mode}: the render differs from tests/golden/linux/modes/{}:\n{text}",
            mode.unit_name()
        );
    }
}

/// Catches a change to the units `render-units` writes without `--mode` (kbf-server's
/// and kbf-daemon's, MinIO's drop-in and the slice): the mode units are new files and
/// leave the converged host's units byte for byte as they were.
#[test]
fn the_converged_units_are_unchanged() {
    let units = render_converged(&config(), Some(1500), &Templates::BUILT_IN).expect("renders");
    for (name, text) in units.files() {
        let file = name.replace('/', "_");
        assert!(
            text == golden("converged", &file),
            "{name} differs from tests/golden/linux/converged/{file}:\n{text}"
        );
    }
}

/// Catches an unbuilt mode marked built (its unit's flags would go unchecked by kbf's
/// parser, which takes anything there) and a mode that runs builds without its cgroup
/// delegated.
#[test]
fn the_built_modes_are_daemon_and_all_in_one() {
    let built: Vec<_> = Mode::ALL.iter().filter(|m| m.is_built()).collect();
    assert_eq!(built, [&Mode::Daemon, &Mode::AllInOne]);
    let builds: Vec<_> = Mode::ALL.iter().filter(|m| m.runs_builds()).collect();
    assert_eq!(
        builds,
        [&Mode::Daemon, &Mode::StorageAndDaemon, &Mode::AllInOne]
    );
    let names: Vec<_> = Mode::ALL.iter().map(|m| m.unit_name()).collect();
    assert_eq!(
        names,
        [
            "kbf-frontend.service",
            "kbf-storage.service",
            "kbf-daemon.service",
            "kbf-storage-and-daemon.service",
            "kbf-all-in-one.service"
        ]
    );
}

/// Each row plants one defect in one mode's template; the check must fail exactly the
/// rule meant for it, with this message. `from` must occur once in the template.
#[test]
fn each_planted_defect_turns_its_rule_red() {
    let c = config();
    let rows: [(Mode, &str, &str, &str); 10] = [
        (
            Mode::Daemon,
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\n",
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\nEnvironmentFile=-@KBF_ETC@/s3.env\n",
            "FAIL credential: kbf-daemon.service gives a build node the cold store's key (EnvironmentFile=-/etc/kbf/s3.env): a build node holds no credential, and its builds run as its user",
        ),
        (
            Mode::StorageAndDaemon,
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\n",
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@ AWS_SECRET_ACCESS_KEY=planted\n",
            "FAIL credential: kbf-storage-and-daemon.service gives a build node the cold store's key (Environment= sets AWS_SECRET_ACCESS_KEY): a build node holds no credential, and its builds run as its user",
        ),
        (
            Mode::Daemon,
            "Delegate=yes\n",
            "Delegate=cpu memory pids\n",
            "FAIL delegate: kbf-daemon.service runs builds and delegates cpu memory pids, not cpuset: the daemon makes its own cgroups and needs cpu, cpuset, memory and pids delegated (Delegate=yes), cpuset to pin each lease to one NUMA node (--lease-placement=numa-node)",
        ),
        (
            Mode::Frontend,
            "/kbf frontend\n",
            "/kbf frontend --node-id=n1\n",
            "FAIL flags: kbf frontend is not built yet and takes no flag kbf can check, but kbf-frontend.service gives '--node-id=n1'",
        ),
        (
            Mode::StorageAndDaemon,
            "/kbf storage-and-daemon\n",
            "/kbf storage-and-daemon --data-dir=/var/lib/kbf\n",
            "FAIL flags: kbf storage-and-daemon is not built yet and takes no flag kbf can check, but kbf-storage-and-daemon.service gives '--data-dir=/var/lib/kbf'",
        ),
        (
            Mode::Daemon,
            "/kbf daemon \\",
            "/kbf-daemon \\",
            "FAIL exec: kbf-daemon.service starts '/usr/local/bin/kbf-daemon --driver=container', want '/usr/local/bin/kbf daemon ...'",
        ),
        (
            Mode::Storage,
            "/kbf storage\n",
            "/kbf storage-and-daemon\n",
            "FAIL exec: kbf-storage.service starts '/usr/local/bin/kbf storage-and-daemon', want '/usr/local/bin/kbf storage ...'",
        ),
        (
            Mode::AllInOne,
            "Delegate=yes\n",
            "Delegate=no\n",
            "FAIL delegate: kbf-all-in-one.service runs builds and delegates no controller, not cpu cpuset memory pids: the daemon makes its own cgroups and needs cpu, cpuset, memory and pids delegated (Delegate=yes), cpuset to pin each lease to one NUMA node (--lease-placement=numa-node)",
        ),
        (
            Mode::Frontend,
            "Slice=kbf.slice\n",
            "Slice=system.slice\n",
            "FAIL slice: kbf-frontend.service is not in kbf.slice",
        ),
        (
            Mode::Daemon,
            "@KBF_UID@",
            "<kbf uid>",
            "FAIL render: kbf-daemon.service keeps a <...> placeholder: <kbf uid>",
        ),
    ];
    for (mode, from, to, want) in rows {
        let t = mode.template();
        assert_eq!(
            t.matches(from).count(),
            1,
            "{mode}: {from:?} not there once"
        );
        let text = render_template(&c, Some(1500), mode, &t.replace(from, to)).expect("renders");
        let r = check(&c, mode, &text, true).expect("a unit");
        assert_eq!(lines(&r), [want], "{mode}: {from:?} -> {to:?}");
        assert_eq!(r.summary, "UNIT BROKEN: 1 failed check(s)");
        // Without --installable a `<...>` placeholder passes; nothing else does.
        let lenient = check(&c, mode, &text, false).expect("a unit");
        assert_eq!(
            lenient.failures.is_empty(),
            want.contains("<...>"),
            "{want}"
        );
    }
    let left = check(
        &c,
        Mode::Storage,
        "[Service]\nSlice=kbf.slice\nExecStart=/usr/local/bin/kbf storage\nUser=@KBF_USER@\n",
        false,
    )
    .expect("a unit");
    assert_eq!(
        lines(&left),
        ["FAIL render: kbf-storage.service keeps the placeholder @KBF_USER@"]
    );
    let none = check(&c, Mode::Storage, "[Service]\nSlice=kbf.slice\n", false).expect("a unit");
    assert_eq!(
        lines(&none),
        ["FAIL exec: kbf-storage.service starts '', want '/usr/local/bin/kbf storage ...'"]
    );
}

/// Catches `--mode` rendering into another directory than `--out`, writing other files
/// than the slice and the mode's unit (the converged units would be installed beside
/// it), check reading another file than render wrote, and a garbled or missing unit
/// checked as if it were empty.
#[test]
fn the_mode_verbs_write_the_slice_and_one_unit_and_read_it_back() {
    let dir = kbf_host::testkit::scratch("linux_mode_verbs");
    let cfg = dir.join("config.local");
    std::fs::write(&cfg, EXAMPLE).expect("write config");
    let out = dir.join("units");
    let line =
        crate::linux::render_mode_unit(&cfg, &out, Some(1500), Mode::AllInOne).expect("renders");
    assert_eq!(
        line,
        format!(
            "rendered kbf.slice kbf-all-in-one.service into {} (kbf all-in-one)",
            out.display()
        )
    );
    let mut names: Vec<_> = std::fs::read_dir(&out)
        .expect("out")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
        .collect();
    names.sort();
    assert_eq!(names, ["kbf-all-in-one.service", "kbf.slice"]);
    let slice = std::fs::read_to_string(out.join("kbf.slice")).expect("slice");
    assert_eq!(slice, golden("converged", "kbf.slice"));
    let r =
        crate::linux::check_mode_unit(&FakeExec::new(501), &cfg, &out, Mode::AllInOne, true, None)
            .expect("checks");
    assert_eq!(lines(&r), Vec::<String>::new());

    let e =
        crate::linux::check_mode_unit(&FakeExec::new(501), &cfg, &out, Mode::Daemon, true, None)
            .err()
            .map(|e| e.to_string());
    assert!(
        e.as_deref()
            .is_some_and(|e| e.starts_with("cannot read ") && e.contains("kbf-daemon.service")),
        "{e:?}"
    );
    std::fs::write(
        out.join("kbf-all-in-one.service"),
        "[Service]\nSlice kbf.slice\n",
    )
    .expect("garble");
    let e =
        crate::linux::check_mode_unit(&FakeExec::new(501), &cfg, &out, Mode::AllInOne, true, None)
            .err()
            .map(|e| e.to_string());
    assert_eq!(
        e.as_deref(),
        Some("kbf-all-in-one.service: line 2: not [Section], a comment or Key=value")
    );
    let blocked = dir.join("file");
    std::fs::write(&blocked, "").expect("write");
    let e = crate::linux::render_mode_unit(&cfg, &blocked, None, Mode::Daemon).err();
    assert!(
        matches!(e, Some(crate::linux::Refusal::Write { .. })),
        "{e:?}"
    );
    // A directory where the mode's unit goes: the slice is written, the unit's write fails.
    std::fs::create_dir_all(dir.join("taken/kbf-daemon.service")).expect("dir in the way");
    let e = crate::linux::render_mode_unit(&cfg, &dir.join("taken"), None, Mode::Daemon).err();
    assert!(
        matches!(e, Some(crate::linux::Refusal::Write { .. })),
        "{e:?}"
    );
    assert!(dir.join("taken/kbf.slice").is_file());
    // No settings file: refused before anything is written or read.
    let none = dir.join("nope");
    let e = crate::linux::render_mode_unit(&none, &dir.join("unwritten"), None, Mode::Daemon).err();
    assert!(matches!(e, Some(crate::linux::Refusal::Config(_))), "{e:?}");
    assert!(!dir.join("unwritten").exists());
    let e =
        crate::linux::check_mode_unit(&FakeExec::new(501), &none, &out, Mode::AllInOne, true, None)
            .err();
    assert!(matches!(e, Some(crate::linux::Refusal::Config(_))), "{e:?}");
}

/// A fake host whose every command answers `answer`, and the commands it was asked to run.
fn host(answer: io::Result<Output>) -> (FakeExec, Rc<RefCell<Vec<Vec<String>>>>) {
    let asked = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&asked);
    let mut exec = FakeExec::new(501);
    let answer = RefCell::new(Some(answer));
    exec.answer = Box::new(move |cmd: &Cmd| {
        let mut argv = vec![cmd.program.as_str().to_owned()];
        argv.extend(cmd.args.iter().cloned());
        seen.borrow_mut().push(argv);
        answer.borrow_mut().take().expect("asked once")
    });
    (exec, asked)
}

fn kbf_at(exec: &mut FakeExec, path: &str) {
    let entry = Entry::File {
        perms: Perms::new(0o755),
        owner: Owner { uid: 0, gid: 0 },
        bytes: Vec::new(),
    };
    exec.entries
        .insert(HostPath::new(path).expect("absolute"), entry);
}

/// Catches rule `parse` not run (the mutant: no `kbf check-args` at all, so a unit with
/// a flag kbf refuses passes), run with other words than `check-args <mode> <flags>`,
/// a refusal that does not fail the unit or loses kbf's message, a mode not built yet
/// or a unit that fails `exec` handed to kbf, and `--kbf-bin` not preferred over the
/// unit's own binary. The real kbf's answers are crates/kbf-cli/tests/deploy_units.rs's.
#[test]
fn the_parse_rule_runs_kbf_check_args_on_a_built_modes_flags() {
    let c = config();
    let text = render(&c, Some(1500), Mode::Daemon).expect("renders");
    let flags: Vec<String> = UnitFile::parse(&text).expect("a unit").exec_start()[2..]
        .iter()
        .map(|w| (*w).to_owned())
        .collect();
    let given = HostPath::new("/opt/kbf/bin/kbf").expect("absolute");

    let (exec, asked) = host(Ok(reply(0, "", "")));
    let r = check_on_host(&exec, &c, Mode::Daemon, &text, true, Some(&given)).expect("runs");
    assert_eq!((lines(&r), r.notes.len()), (Vec::<String>::new(), 0));
    let mut want = vec![
        "/opt/kbf/bin/kbf".to_owned(),
        "check-args".into(),
        "daemon".into(),
    ];
    want.extend(flags);
    assert_eq!(*asked.borrow(), [want]);

    let refusal = "error: unexpected argument '--bogus-flag' found\n\nUsage: kbf daemon\n";
    let (mut exec, asked) = host(Ok(reply(2, "", refusal)));
    kbf_at(&mut exec, "/usr/local/bin/kbf");
    let r = check_on_host(&exec, &c, Mode::Daemon, &text, true, None).expect("runs");
    assert_eq!(
        lines(&r),
        [
            "FAIL parse: kbf daemon refuses the flags of kbf-daemon.service (/usr/local/bin/kbf check-args: exit 2): error: unexpected argument '--bogus-flag' found"
        ]
    );
    assert_eq!(r.summary, "UNIT BROKEN: 1 failed check(s)");
    assert_eq!(asked.borrow()[0][0], "/usr/local/bin/kbf");

    let killed = Output {
        status: ExitStatus::from_raw(9),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    let (exec, _) = host(Ok(killed));
    let r = check_on_host(&exec, &c, Mode::Daemon, &text, true, Some(&given)).expect("runs");
    assert_eq!(
        lines(&r),
        [
            "FAIL parse: kbf daemon refuses the flags of kbf-daemon.service (/opt/kbf/bin/kbf check-args: killed by a signal): (nothing on stderr)"
        ]
    );

    // A mode not built yet, and a unit that starts an old binary: kbf is not asked.
    for (mode, text) in [
        (
            Mode::Storage,
            render(&c, Some(1500), Mode::Storage).expect("renders"),
        ),
        (
            Mode::Daemon,
            text.replace("/kbf daemon \\", "/kbf-daemon \\"),
        ),
    ] {
        let (exec, asked) = host(Ok(reply(2, "", "never asked")));
        let r = check_on_host(&exec, &c, mode, &text, true, Some(&given)).expect("runs");
        assert_eq!(r.notes.len(), 0, "{mode}");
        assert!(asked.borrow().is_empty(), "{mode}");
    }
}

/// Catches a missing kbf binary read as a failure or a pass with no word (the operator
/// must see that the flags went unparsed), a `--kbf-bin` that cannot run passed or
/// skipped instead of refused, and a garbled unit read as an empty one.
#[test]
fn the_parse_rule_is_skipped_without_kbf_and_refused_when_kbf_cannot_run() {
    let c = config();
    let text = render(&c, Some(1500), Mode::AllInOne).expect("renders");
    let (exec, asked) = host(Ok(reply(0, "", "")));
    let r = check_on_host(&exec, &c, Mode::AllInOne, &text, true, None).expect("runs");
    assert_eq!(lines(&r), Vec::<String>::new());
    assert_eq!(
        r.notes,
        [
            "SKIP parse: no kbf binary at /usr/local/bin/kbf, so the flags of kbf all-in-one were not parsed; give --kbf-bin PATH to parse them with another"
        ]
    );
    assert_eq!(
        r.summary,
        "UNIT OK: kbf-all-in-one.service runs kbf all-in-one"
    );
    assert!(asked.borrow().is_empty());

    let given = HostPath::new("/nonexistent/kbf").expect("absolute");
    let (exec, _) = host(Err(io::Error::from(io::ErrorKind::NotFound)));
    let e = check_on_host(&exec, &c, Mode::AllInOne, &text, true, Some(&given)).err();
    assert_eq!(
        e.map(|e| e.to_string()).as_deref(),
        Some("cannot run /nonexistent/kbf check-args: entity not found")
    );
    let e = check_on_host(
        &exec,
        &c,
        Mode::AllInOne,
        "[Service]\nSlice kbf.slice\n",
        true,
        None,
    );
    assert!(matches!(e, Err(ParseError::Unit(_))), "{e:?}");
}

/// Catches a `Delegate=` read other than systemd reads it: a later list adding to an
/// earlier one, an empty value starting over, a boolean meaning all or nothing, and the
/// lines of another section counted.
#[test]
fn delegate_is_read_as_systemd_reads_it() {
    let c = config();
    let base = render(&c, Some(1500), Mode::Daemon).expect("renders");
    for (lines_, ok) in [
        ("Delegate=cpu cpuset memory pids\n", true),
        ("Delegate=cpu memory\nDelegate=cpuset pids\n", true),
        ("Delegate=true\n", true),
        ("Delegate=yes\nDelegate=\nDelegate=cpu memory pids\n", false),
        ("Delegate=cpu cpuset memory pids\nDelegate=off\n", false),
        ("Delegate=cpu cpuset\n", false),
    ] {
        let text = base.replace("Delegate=yes\n", lines_);
        let r = check(&c, Mode::Daemon, &text, true).expect("a unit");
        assert_eq!(r.failures.is_empty(), ok, "{lines_:?}: {:?}", lines(&r));
    }
    let elsewhere = base
        .replace("Delegate=yes\n", "")
        .replace("[Install]\n", "[Install]\nDelegate=yes\n");
    let r = check(&c, Mode::Daemon, &elsewhere, true).expect("a unit");
    assert_eq!(r.failures.len(), 1, "{:?}", lines(&r));
}

/// Catches the rule `credential` looking for another file than the one the storage
/// modes' units read the key from (a rename of `s3.env` in one place), and flagging a
/// mode that holds the storage half's key today.
#[test]
fn the_credential_rule_names_the_file_the_storage_units_read() {
    let c = config();
    let line = format!("EnvironmentFile=@KBF_ETC@/{COLD_STORE_KEY_FILE}\n");
    for mode in Mode::ALL {
        let has = mode.template().contains(&line);
        assert_eq!(
            has,
            matches!(mode, Mode::Storage | Mode::AllInOne),
            "{mode}"
        );
        if has {
            let r = check(
                &c,
                mode,
                &render(&c, Some(1500), mode).expect("renders"),
                true,
            )
            .expect("a unit");
            assert_eq!(lines(&r), Vec::<String>::new(), "{mode}");
        }
    }
    let holds: Vec<_> = Mode::ALL.iter().filter(|m| m.holds_no_key()).collect();
    assert_eq!(holds, [&Mode::Daemon, &Mode::StorageAndDaemon]);
}
