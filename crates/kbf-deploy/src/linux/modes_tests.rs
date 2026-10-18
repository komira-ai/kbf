//! The mode units: each renders from the example settings into a unit that passes
//! every rule and matches its golden, and each planted defect turns its rule red.

use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::process::Output;
use std::rc::Rc;

use kbf_host::exec::Entry;
use kbf_host::step::{Cmd, Owner, Perms};
use kbf_host::testkit::{FakeExec, reply};

use super::*;
use crate::linux::shared;

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
            format!("UNIT OK: {} runs kbf {mode}", mode.unit_name())
        );
        assert!(
            text == golden("modes", &mode.unit_name()),
            "{mode}: the render differs from tests/golden/linux/modes/{}:\n{text}",
            mode.unit_name()
        );
    }
}

/// The images a unit that runs builds passes: one `--image=docker://<repo>@sha256:<digest>`
/// per `KBF_IMAGES` entry, in order (kbf's `--image` parser takes that form only, and
/// exits 2 at start on a bare one), none in a mode that runs no builds. Catches the
/// `docker://` prefix dropped or doubled in the rendering, an entry lost or reordered,
/// and an image flag in a unit of a mode that has no containers.
#[test]
fn a_build_units_images_are_passed_in_the_docker_form() {
    let first = format!("gcr.io/distroless/base-debian12@sha256:{}", "a".repeat(64));
    let second = format!("registry.example/kbf/ci@sha256:{}", "b".repeat(64));
    let text = EXAMPLE.replacen(
        "KBF_IMAGES=\n",
        &format!("KBF_IMAGES={first} {second}\n"),
        1,
    );
    assert_ne!(text, EXAMPLE, "the example has a KBF_IMAGES= line to set");
    let c = LinuxConfig::parse(&text).expect("valid settings");
    let want = [format!("docker://{first}"), format!("docker://{second}")];
    for mode in Mode::ALL {
        let rendered = render(&c, Some(1500), mode).expect("renders");
        let unit = UnitFile::parse(&rendered).expect("a unit");
        let got: Vec<&str> = unit.flag_values("image");
        if mode.runs_builds() {
            assert_eq!(got, want, "{mode}");
            assert!(
                rendered.contains(&format!(
                    "    --image={} \\\n    --image={} \\\n",
                    want[0], want[1]
                )),
                "{mode}: {rendered}"
            );
        } else {
            assert_eq!(got, Vec::<&str>::new(), "{mode}");
        }
    }
}

/// The smallest `LimitNOFILE=` a kbf unit may set. systemd starts a unit that sets none at
/// a soft limit of 1024, and a storage node under a build's burst of uploads (a file per
/// open write-tier block, a socket per peer, client and cold-store request) hit EMFILE
/// there ("sync_dir ...: Too many open files").
const MIN_NOFILE: u64 = 65536;

fn nofile(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("LimitNOFILE="))
        .and_then(|v| v.parse().ok())
}

/// Catches a kbf unit with no `LimitNOFILE=` (the process runs at a
/// soft open-file limit of 1024) or one below `MIN_NOFILE`. Mutant: the line deleted from
/// one template turns this red for that unit.
#[test]
fn every_kbf_unit_sets_an_open_file_limit() {
    let c = config();
    for mode in Mode::ALL {
        let text = render(&c, Some(1500), mode).expect("renders");
        assert!(
            nofile(&text).is_some_and(|n| n >= MIN_NOFILE),
            "{mode}: LimitNOFILE= is {:?}",
            nofile(&text)
        );
    }
}

/// Catches a mode that runs builds without its cgroup delegated, a mode left out of the
/// credential rule's two sets (it would be checked by neither), and a unit name that is
/// not `kbf-<mode>.service`.
#[test]
fn the_modes_are_listed_with_the_sets_the_rules_use() {
    let builds: Vec<_> = Mode::ALL.iter().filter(|m| m.runs_builds()).collect();
    assert_eq!(
        builds,
        [&Mode::Daemon, &Mode::StorageAndDaemon, &Mode::AllInOne]
    );
    let no_key: Vec<_> = Mode::ALL.iter().filter(|m| m.holds_no_key()).collect();
    assert_eq!(no_key, [&Mode::Daemon, &Mode::AllInOne]);
    let beside: Vec<_> = Mode::ALL
        .iter()
        .filter(|m| m.holds_key_beside_builds())
        .collect();
    assert_eq!(beside, [&Mode::StorageAndDaemon]);
    let names: Vec<_> = Mode::ALL.iter().map(|m| m.unit_name()).collect();
    assert_eq!(
        names,
        [
            "kbf-frontend@.service",
            "kbf-storage.service",
            "kbf-daemon.service",
            "kbf-storage-and-daemon.service",
            "kbf-all-in-one.service"
        ]
    );
}

/// Catches rule `stop` skipped for a mode that drains (a storage node finishes its large
/// Writes at stop; systemd's 90 s default would cut them) or applied to the daemon,
/// which abandons its leases on SIGTERM.
#[test]
fn every_mode_but_the_daemon_drains() {
    let drains: Vec<_> = Mode::ALL.iter().filter(|m| m.drains()).collect();
    assert_eq!(
        drains,
        [
            &Mode::Frontend,
            &Mode::Storage,
            &Mode::StorageAndDaemon,
            &Mode::AllInOne
        ]
    );
}

/// Each row plants one defect in one mode's template; the check must fail exactly the
/// rule meant for it, with this message. `from` must occur once in the template.
#[test]
fn each_planted_defect_turns_its_rule_red() {
    let c = config();
    let rows: [(Mode, &str, &str, &str); 24] = [
        (
            Mode::Daemon,
            "RestartSec=5\n",
            "",
            "FAIL restart: kbf-daemon.service gives no RestartSec=5: systemd restarts after 100 ms, and a failure that repeats quickly ends in start-limit-hit with the unit down",
        ),
        (
            Mode::Storage,
            "StartLimitIntervalSec=0\n",
            "StartLimitIntervalSec=10\n",
            "FAIL restart: kbf-storage.service has StartLimitIntervalSec=10, not StartLimitIntervalSec=0: five starts in that interval end in start-limit-hit with the unit down",
        ),
        (
            Mode::StorageAndDaemon,
            "RestartSec=5\n",
            "RestartSec=100ms\n",
            "FAIL restart: kbf-storage-and-daemon.service has RestartSec=100ms, not RestartSec=5: a failure that repeats quickly must not use up the start limit",
        ),
        (
            Mode::Frontend,
            "TimeoutStopSec=infinity\n",
            "TimeoutStopSec=90s\n",
            "FAIL stop: kbf-frontend@.service has TimeoutStopSec=90s, not TimeoutStopSec=infinity: systemd kills kbf when it runs out, before the drain ends, and every build in flight fails",
        ),
        (
            Mode::AllInOne,
            "KillSignal=SIGTERM\n",
            "KillSignal=SIGINT\n",
            "FAIL stop: kbf-all-in-one.service has KillSignal=SIGINT, not KillSignal=SIGTERM: kbf drains on SIGTERM",
        ),
        (
            Mode::Storage,
            "TimeoutStopSec=infinity\n",
            "",
            "FAIL stop: kbf-storage.service gives no TimeoutStopSec=infinity: systemd kills kbf after its default 90 s, before the drain ends, and every build in flight fails",
        ),
        (
            Mode::StorageAndDaemon,
            "KillSignal=SIGTERM\n",
            "KillSignal=SIGINT\n",
            "FAIL stop: kbf-storage-and-daemon.service has KillSignal=SIGINT, not KillSignal=SIGTERM: kbf drains on SIGTERM",
        ),
        (
            Mode::AllInOne,
            "--api-listen=127.0.0.1:@KBF_API_PORT@",
            "--api-listen=127.0.0.1:@KBF_WORKER_PORT@",
            "FAIL ports: kbf-all-in-one.service: --worker-listen and --api-listen are both on port 8981: give each listener its own (KBF_REAPI_PORT, KBF_WORKER_PORT, KBF_API_PORT, KBF_STORAGE_PORT)",
        ),
        (
            Mode::Daemon,
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\n",
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\nEnvironmentFile=-@KBF_ETC@/s3.env\n",
            "FAIL credential: kbf-daemon.service gives a build node the cold store's key (EnvironmentFile=-/etc/kbf/s3.env): a build node holds no credential, and its builds run as its user",
        ),
        (
            Mode::AllInOne,
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\n",
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\nEnvironmentFile=@KBF_ETC@/s3.env\n",
            "FAIL credential: kbf-all-in-one.service gives a build node the cold store's key (EnvironmentFile=/etc/kbf/s3.env): a build node holds no credential, and its builds run as its user",
        ),
        (
            Mode::Daemon,
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\n",
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@ \"AWS_SESSION_TOKEN=a b\"\n",
            "FAIL credential: kbf-daemon.service gives a build node the cold store's key (Environment= sets AWS_SESSION_TOKEN): a build node holds no credential, and its builds run as its user",
        ),
        (
            Mode::Daemon,
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\n",
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\nEnvironmentFile=-@KBF_ETC@/*.env\n",
            "FAIL credential: kbf-daemon.service gives a build node the cold store's key (EnvironmentFile=-/etc/kbf/*.env): a build node holds no credential, and its builds run as its user",
        ),
        (
            Mode::StorageAndDaemon,
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\n",
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\nLoadCredential=s3:@KBF_ETC@/s3.env\n",
            "FAIL credential: kbf-storage-and-daemon.service gives a build node the cold store's key (LoadCredential=s3:/etc/kbf/s3.env, a systemd credential): a build node holds no credential, and its builds run as its user",
        ),
        (
            Mode::Daemon,
            "Delegate=yes\n",
            "Delegate=yes\nDisableControllers=cpuset\n",
            "FAIL delegate: kbf-daemon.service disables cpuset for its own cgroups (DisableControllers=): the daemon needs cpu, cpuset, memory and pids in supervisor/ and actions/",
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
            "--state-store=sqlite://@KBF_STATE_DIR@/state.db",
            "--state-store=postgres://db.example/kbf",
            "FAIL state-store: kbf-frontend@.service: --state-store is a postgres URI: postgres is not wired yet; the state store is sqlite:///<absolute path>",
        ),
        (
            Mode::StorageAndDaemon,
            "EnvironmentFile=@KBF_ETC@/s3.env\n",
            "",
            "FAIL storage: kbf-storage-and-daemon.service has an s3:// cold store and no EnvironmentFile=/etc/kbf/s3.env: kbf storage starts without the bucket's key pair and refuses",
        ),
        (
            Mode::StorageAndDaemon,
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\n",
            "Environment=XDG_RUNTIME_DIR=/run/user/@KBF_UID@\nEnvironmentFile=@KBF_ETC@/*.env\n",
            "FAIL credential: kbf-storage-and-daemon.service gives a build node the cold store's key (EnvironmentFile=/etc/kbf/*.env): a build node holds no credential, and its builds run as its user",
        ),
        (
            Mode::Daemon,
            "/kbf daemon \\",
            "/kbf-daemon \\",
            "FAIL exec: kbf-daemon.service starts '/usr/local/bin/kbf-daemon --driver=container', want '/usr/local/bin/kbf daemon ...'",
        ),
        (
            Mode::Storage,
            "/kbf storage \\\n",
            "/kbf storage-and-daemon \\\n",
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
            "FAIL slice: kbf-frontend@.service is not in kbf.slice",
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
        Mode::Frontend,
        "[Service]\nSlice=kbf.slice\nUser=@KBF_USER@\nTimeoutStopSec=infinity\nKillSignal=SIGTERM\n\
         Type=notify\nRuntimeDirectory=kbf-frontend\nRuntimeDirectoryPreserve=yes\n\
         ExecStart=/usr/local/bin/kbf frontend --handover-socket=/run/kbf-frontend/control.sock\n",
        false,
    )
    .expect("a unit");
    assert_eq!(
        lines(&left),
        ["FAIL render: kbf-frontend@.service keeps the placeholder @KBF_USER@"]
    );
    let none = check(
        &c,
        Mode::Frontend,
        "[Service]\nSlice=kbf.slice\nTimeoutStopSec=infinity\nKillSignal=SIGTERM\n",
        false,
    )
    .expect("a unit");
    // The rule `handover` fails the unit with no command line too; the others pass.
    let exec = "FAIL exec: kbf-frontend@.service starts '', want '/usr/local/bin/kbf frontend ...'";
    assert_eq!(lines(&none)[0], exec);
    assert!(
        lines(&none)[1..]
            .iter()
            .all(|l| l.starts_with("FAIL handover: ")),
        "{:?}",
        lines(&none)
    );
}

/// Catches `--mode` rendering into another directory than `--out`, writing other files
/// than the slice and the mode's unit, check reading another file than render wrote, and a garbled or missing unit
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
    assert_eq!(slice, golden("modes", "kbf.slice"));
    let r = crate::linux::check_mode_unit(&FakeExec::new(501), &cfg, &out, Mode::AllInOne, true)
        .expect("checks");
    assert_eq!(lines(&r), Vec::<String>::new());

    let e = crate::linux::check_mode_unit(&FakeExec::new(501), &cfg, &out, Mode::Daemon, true)
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
    let e = crate::linux::check_mode_unit(&FakeExec::new(501), &cfg, &out, Mode::AllInOne, true)
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
        crate::linux::check_mode_unit(&FakeExec::new(501), &none, &out, Mode::AllInOne, true).err();
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

/// [`check_on_host`] on the unit `text` of `mode` alone, beside the rendered slice.
fn on_host<E: Exec>(
    exec: &E,
    c: &LinuxConfig,
    mode: Mode,
    text: &str,
) -> Result<Report, ParseError> {
    let unit = UnitText::new(mode.unit_name(), text);
    check_on_host(exec, c, mode, &unit, &slice(c), true)
}

/// The rendered `kbf.slice`.
fn slice(c: &LinuxConfig) -> UnitText {
    UnitText::new(SLICE, shared::render_slice(c, Some(1500)).expect("renders"))
}

/// Catches a check that runs `kbf` (rule `parse` is retired: whether `kbf` takes a unit's
/// flags is kbf-cli's tests', crates/kbf-cli/tests/deploy_units.rs), a binary at the
/// unit's path that makes a note, and a garbled unit read as an empty one.
#[test]
fn no_kbf_is_run_and_a_garbled_unit_is_refused() {
    let c = config();
    let text = render(&c, Some(1500), Mode::AllInOne).expect("renders");
    let (mut exec, asked) = host(Ok(reply(2, "", "never asked")));
    let entry = Entry::File {
        perms: Perms::new(0o755),
        owner: Owner { uid: 0, gid: 0 },
        bytes: Vec::new(),
    };
    exec.entries.insert(
        HostPath::new("/usr/local/bin/kbf").expect("absolute"),
        entry,
    );
    let r = on_host(&exec, &c, Mode::AllInOne, &text).expect("runs");
    assert_eq!(lines(&r), Vec::<String>::new());
    assert_eq!(r.notes, [TRUSTED_NOTE]);
    assert_eq!(
        r.summary,
        "UNIT OK: kbf-all-in-one.service runs kbf all-in-one"
    );
    assert!(asked.borrow().is_empty(), "{:?}", asked.borrow());

    let e = on_host(&exec, &c, Mode::AllInOne, "[Service]\nSlice kbf.slice\n");
    assert!(
        matches!(&e, Err(ParseError::Unit { name, .. }) if name == "kbf-all-in-one.service"),
        "{e:?}"
    );
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
        // A list after `yes` adds to every controller: cpuset is kept (systemd's
        // config_parse_delegate ORs the list into the mask `yes` set).
        ("Delegate=yes\nDelegate=memory\n", true),
        ("Delegate=YES\n", true),
        ("Delegate=On\n", true),
        ("Delegate=yes\nDelegate=no\nDelegate=memory\n", false),
        ("Delegate=yes\nDisableControllers=cpuset\n", false),
        ("Delegate=yes\nDisableControllers=io\n", true),
        (
            "Delegate=yes\nDisableControllers=cpuset\nDisableControllers=\n",
            true,
        ),
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
/// modes' units read the key from (a rename of `s3.env` in one place), refusing every
/// environment file (or one whose directory, not its name, says `s3.env`), and flagging
/// the two modes that hold the storage half's key (the second by this one file).
#[test]
fn the_credential_rule_names_the_file_the_storage_units_read() {
    let c = config();
    let line = format!("EnvironmentFile=@KBF_ETC@/{COLD_STORE_KEY_FILE}\n");
    for mode in Mode::ALL {
        let has = mode.template().contains(&line);
        assert_eq!(
            has,
            matches!(mode, Mode::Storage | Mode::StorageAndDaemon),
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
    // Another environment file is not the key: a build node may read its own settings.
    let daemon = render(&c, Some(1500), Mode::Daemon).expect("renders");
    let own = daemon.replace(
        "Delegate=yes\n",
        "Delegate=yes\nEnvironmentFile=-/etc/kbf/s3.env.example.d/daemon.env\n",
    );
    assert_ne!(own, daemon);
    let r = check(&c, Mode::Daemon, &own, true).expect("a unit");
    assert_eq!(lines(&r), Vec::<String>::new());
    let holds: Vec<_> = Mode::ALL.iter().filter(|m| m.holds_no_key()).collect();
    assert_eq!(holds, [&Mode::Daemon, &Mode::AllInOne]);
}

/// Catches a comment inside a continued `ExecStart=` passed: systemd 239 and older
/// join it into the command line (the daemon's template carried one, its TODO, until
/// it moved above the directive), and an indented one, which 240 joins too.
#[test]
fn a_comment_inside_a_continued_line_fails_rule_continuation() {
    let c = config();
    for comment in ["# planted", "    ; planted"] {
        let t = Mode::Daemon.template().replace(
            "    --driver=container \\\n",
            &format!("    --driver=container \\\n{comment}\n"),
        );
        let text = render_template(&c, Some(1500), Mode::Daemon, &t).expect("renders");
        let line = text.lines().position(|l| l == comment).expect("planted") + 1;
        let r = check(&c, Mode::Daemon, &text, true).expect("a unit");
        assert_eq!(
            lines(&r),
            [format!(
                "FAIL continuation: kbf-daemon.service line {line} is a comment inside a continued line: systemd 239 and older read it into the value (240 an indented one); put it before the directive"
            )]
        );
    }
    // Above the directive, after its last line, or after a blank line: not inside.
    let unit = "[Service]\n# a\nExecStart=/bin/a \\\n  --b\n# c\nX=1 \\\n\n# d\n";
    assert_eq!(
        crate::unit::comments_in_continuations(unit),
        Vec::<usize>::new()
    );
    assert_eq!(
        crate::unit::comments_in_continuations("A=1 \\\n# x\n; y\n  2\n"),
        [2, 3]
    );
}

/// Catches `all-in-one` checked without its warning (it runs builds beside the farm's
/// state as one user, so it is for a trusted box), and the warning on another mode.
#[test]
fn all_in_one_is_labelled_trusted() {
    let c = config();
    for mode in Mode::ALL {
        let r = check(
            &c,
            mode,
            &render(&c, Some(1500), mode).expect("renders"),
            true,
        )
        .expect("a unit");
        let want: &[&str] = if mode == Mode::AllInOne {
            &[TRUSTED_NOTE]
        } else {
            &[]
        };
        assert_eq!(r.notes, want, "{mode}");
    }
    assert!(
        Mode::AllInOne
            .template()
            .contains("TRUSTED, SINGLE BOX OR DEVELOPMENT ONLY")
    );
    let all = render(&c, Some(1500), Mode::AllInOne).expect("renders");
    let unit = UnitFile::parse(&all).expect("a unit");
    assert_eq!(unit.flag("cold-store"), Some("file:///var/lib/kbf/cold"));
    assert_eq!(unit.get("EnvironmentFile"), None);
}

/// Catches a drop-in not applied (one that hands a build node the key, or turns
/// delegation off, would pass), drop-ins applied out of systemd's file-name order, a
/// file that is not `*.conf` read as a drop-in, `kbf.slice`'s drop-ins not read
/// (one that disables cpuset below the slice), and a garbled drop-in named wrongly.
#[test]
fn drop_ins_apply_after_the_unit_in_file_name_order() {
    let dir = kbf_host::testkit::scratch("linux_mode_drop_ins");
    let cfg = dir.join("config.local");
    std::fs::write(&cfg, EXAMPLE).expect("write config");
    let out = dir.join("units");
    crate::linux::render_mode_unit(&cfg, &out, Some(1500), Mode::Daemon).expect("renders");
    let run = || {
        crate::linux::check_mode_unit(&FakeExec::new(0), &cfg, &out, Mode::Daemon, true)
            .map(|r| lines(&r))
            .map_err(|e| e.to_string())
    };
    assert_eq!(run(), Ok(Vec::new()));
    let d = out.join("kbf-daemon.service.d");
    std::fs::create_dir_all(&d).expect("drop-in dir");
    std::fs::write(d.join("10-a.conf"), "[Service]\nDelegate=no\n").expect("write");
    std::fs::write(d.join("20-b.conf"), "[Service]\nDelegate=yes\n").expect("write");
    std::fs::write(d.join("30-c.conf.off"), "[Service]\nDelegate=no\n").expect("write");
    assert_eq!(run(), Ok(Vec::new()));
    std::fs::write(d.join("05-a.conf"), "[Service]\nDelegate=yes\n").expect("write");
    std::fs::rename(d.join("20-b.conf"), d.join("01-b.conf")).expect("rename");
    let r = run().expect("checks");
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(
        r[0].starts_with(
            "FAIL delegate: kbf-daemon.service runs builds and delegates no controller"
        ),
        "{r:?}"
    );
    std::fs::remove_file(d.join("10-a.conf")).expect("remove");
    std::fs::write(
        d.join("50-key.conf"),
        "[Service]\nEnvironmentFile=/etc/kbf/s3.env\n",
    )
    .expect("write");
    assert_eq!(
        run(),
        Ok(vec![
            "FAIL credential: kbf-daemon.service gives a build node the cold store's key (EnvironmentFile=/etc/kbf/s3.env): a build node holds no credential, and its builds run as its user".to_owned()
        ])
    );
    std::fs::write(d.join("60-reset.conf"), "[Service]\nEnvironmentFile=\n").expect("write");
    assert_eq!(run(), Ok(Vec::new()));
    let s = out.join("kbf.slice.d");
    std::fs::create_dir_all(&s).expect("slice drop-in dir");
    std::fs::write(
        s.join("x.conf"),
        "[Slice]\nDisableControllers=memory cpuset\n",
    )
    .expect("write");
    assert_eq!(
        run(),
        Ok(vec![
            "FAIL delegate: kbf.slice disables cpuset memory below it (DisableControllers=), so kbf-daemon.service cannot have it delegated: the daemon needs cpu, cpuset, memory and pids".to_owned()
        ])
    );
    std::fs::write(s.join("x.conf"), "[Slice]\nDisableControllers cpuset\n").expect("write");
    assert_eq!(
        run(),
        Err("kbf.slice.d/x.conf: line 2: not [Section], a comment or Key=value".to_owned())
    );
    std::fs::remove_file(s.join("x.conf")).expect("remove");
    // A drop-in directory that is not one: refused, not read as none.
    std::fs::remove_dir(&s).expect("remove");
    std::fs::write(&s, "").expect("a file in its place");
    assert!(run().is_err_and(|e| e.starts_with("cannot read ") && e.contains("kbf.slice.d")));
    std::fs::remove_file(&s).expect("remove");
    std::fs::remove_file(out.join("kbf.slice")).expect("remove");
    assert!(run().is_err_and(|e| e.starts_with("cannot read ") && e.ends_with("(os error 2)")));
}

/// Catches rule `cpuset` not run on a mode that runs builds (its unit's or the slice's
/// narrower `AllowedCPUs=` passed), run on one that does not, and the slice's lists not
/// compared.
#[test]
fn a_build_modes_unit_and_slice_keep_every_cpu_of_the_node() {
    let c = config();
    let mut exec = FakeExec::new(0);
    for (path, text) in [
        ("/sys/devices/system/cpu/online", "0-7\n"),
        ("/sys/devices/system/node/online", "0-1\n"),
    ] {
        let entry = Entry::File {
            perms: Perms::new(0o444),
            owner: Owner { uid: 0, gid: 0 },
            bytes: text.as_bytes().to_vec(),
        };
        exec.entries
            .insert(HostPath::new(path).expect("absolute"), entry);
    }
    let daemon = render(&c, Some(1500), Mode::Daemon).expect("renders");
    let narrow = UnitText::new(Mode::Daemon.unit_name(), daemon.as_str()).with_drop_in(
        "kbf-daemon.service.d/cpus.conf",
        "[Service]\nAllowedCPUs=0-3\n",
    );
    let r = check_on_host(&exec, &c, Mode::Daemon, &narrow, &slice(&c), true).expect("runs");
    // `caps` refuses any AllowedCPUs= without reading the node; `cpuset` is the one that
    // reads it (a list of every CPU is no cap, and only this rule can tell).
    let rules: Vec<&str> = r.failures.iter().map(|f| f.rule).collect();
    assert_eq!(rules, ["caps", "cpuset"], "{:?}", lines(&r));
    let wide_slice =
        slice(&c).with_drop_in("kbf.slice.d/n.conf", "[Slice]\nAllowedMemoryNodes=1\n");
    let unit = UnitText::new(Mode::Daemon.unit_name(), daemon.as_str());
    let r = check_on_host(&exec, &c, Mode::Daemon, &unit, &wide_slice, true).expect("runs");
    assert_eq!(
        lines(&r),
        [
            "FAIL cpuset: kbf.slice sets AllowedMemoryNodes=1, narrower than the node's NUMA nodes 0-1: the daemon places leases on every one of them (--lease-placement=numa-node pins a lease to one NUMA node), and the unit's cgroup keeps them off the rest"
        ]
    );
    let frontend = render(&c, Some(1500), Mode::Frontend).expect("renders");
    let f = UnitText::new(Mode::Frontend.unit_name(), frontend).with_drop_in(
        "kbf-frontend@.service.d/cpus.conf",
        "[Service]\nAllowedCPUs=0\n",
    );
    let r = check_on_host(&exec, &c, Mode::Frontend, &f, &wide_slice, true).expect("runs");
    assert_eq!(lines(&r), Vec::<String>::new());
}

/// Catches `check-units` on the node not reading the unit's environment files: a copy
/// of the key under another name, given by a drop-in, passes without it; and the
/// storage mode, whose half reads the key, checked for it.
#[test]
fn on_the_node_an_environment_file_holding_the_key_fails_rule_credential() {
    let c = config();
    let mut exec = FakeExec::new(0);
    let copy = Entry::File {
        perms: Perms::new(0o600),
        owner: Owner { uid: 0, gid: 0 },
        bytes: b"AWS_ACCESS_KEY_ID=a\nAWS_SECRET_ACCESS_KEY=b\n".to_vec(),
    };
    exec.entries.insert(
        HostPath::new("/etc/kbf/daemon.env").expect("absolute"),
        copy,
    );
    let drop_in = "[Service]\nEnvironmentFile=/etc/kbf/daemon.env\n";
    for mode in [Mode::Daemon, Mode::Storage] {
        let text = render(&c, Some(1500), mode).expect("renders");
        let unit = UnitText::new(mode.unit_name(), text)
            .with_drop_in(format!("{}.d/env.conf", mode.unit_name()), drop_in);
        let r = check_on_host(&exec, &c, mode, &unit, &slice(&c), true).expect("runs");
        let want: Vec<String> = if mode == Mode::Daemon {
            vec!["FAIL credential: kbf-daemon.service gives a build node the cold store's key (EnvironmentFile=/etc/kbf/daemon.env, which sets AWS_ACCESS_KEY_ID): a build node holds no credential, and its builds run as its user".to_owned()]
        } else {
            Vec::new()
        };
        assert_eq!(lines(&r), want, "{mode}");
    }
}

/// Catches rule `stop` reading the unit file alone: a drop-in's `TimeoutSec=` (which
/// sets the stop timeout too) or `KillSignal=` passed because the unit has the right
/// lines, and a later drop-in that restores them not read.
#[test]
fn rule_stop_reads_the_drop_ins() {
    let c = config();
    let text = render(&c, Some(1500), Mode::AllInOne).expect("renders");
    let exec = FakeExec::new(0);
    let fails = |unit: &UnitText| -> Vec<String> {
        lines(&check_on_host(&exec, &c, Mode::AllInOne, unit, &slice(&c), true).expect("runs"))
    };
    let unit = UnitText::new(Mode::AllInOne.unit_name(), text);
    assert_eq!(fails(&unit), Vec::<String>::new());
    let cut = unit.clone().with_drop_in(
        "kbf-all-in-one.service.d/10.conf",
        "[Service]\nTimeoutSec=5min\nKillSignal=SIGKILL\n",
    );
    assert_eq!(
        fails(&cut),
        [
            "FAIL stop: kbf-all-in-one.service has TimeoutSec=5min, not TimeoutStopSec=infinity: systemd kills kbf when it runs out, before the drain ends, and every build in flight fails",
            "FAIL stop: kbf-all-in-one.service has KillSignal=SIGKILL, not KillSignal=SIGTERM: kbf drains on SIGTERM",
        ]
    );
    let restored = cut.with_drop_in(
        "kbf-all-in-one.service.d/20.conf",
        "[Service]\nTimeoutStopSec=infinity\nKillSignal=TERM\n",
    );
    assert_eq!(fails(&restored), Vec::<String>::new());
}

/// Catches a storage unit rendered with the loopback MinIO when the
/// settings name no `MINIO_S3_ENDPOINT` (a storage node's bucket is elsewhere), and the
/// refusal applied to a mode whose unit has no S3 cold store.
#[test]
fn the_storage_mode_needs_its_s3_endpoint_set() {
    let text: String = EXAMPLE
        .lines()
        .filter(|l| !l.starts_with("MINIO_S3_ENDPOINT="))
        .map(|l| format!("{l}\n"))
        .collect();
    let c = LinuxConfig::parse(&text).expect("valid settings");
    assert_eq!(
        render(&c, Some(1500), Mode::Storage).map_err(|e| e.to_string()),
        Err("MINIO_S3_ENDPOINT must be set for --mode storage: the storage node's cold store is the bucket at that endpoint, and no MinIO runs beside it to default to".to_owned())
    );
    for mode in Mode::ALL.into_iter().filter(|m| *m != Mode::Storage) {
        assert_eq!(
            render(&c, Some(1500), mode).expect("renders"),
            render(&config(), Some(1500), mode).expect("renders"),
            "{mode}"
        );
    }
    let set = render(&config(), Some(1500), Mode::Storage).expect("renders");
    assert!(set.contains("?endpoint=http://127.0.0.1:9000&"), "{set}");
}

/// A listen address with a colon is an IPv6 address and goes into a `--listen=` flag in
/// brackets (`[2001:db8::10]:8080`); an IPv4 one goes in bare. Catches an IPv6 address
/// rendered bare (`2001:db8::10:8080`, which kbf cannot split into host and port) and a
/// bracket put round an IPv4 one.
#[test]
fn an_ipv6_listen_address_is_bracketed_in_the_flags_and_an_ipv4_one_is_not() {
    let mut c = config();
    assert_eq!(c.listen_host(), c.listen_address, "an IPv4 address is bare");
    let v4 = render(&c, Some(1500), Mode::Frontend).expect("renders");
    assert!(
        v4.contains(&format!("--listen={}:{}", c.listen_address, c.reapi_port)),
        "{v4}"
    );
    c.listen_address = "2001:db8::10".to_owned();
    assert_eq!(c.listen_host(), "[2001:db8::10]");
    let v6 = render(&c, Some(1500), Mode::Frontend).expect("renders");
    assert!(
        v6.contains(&format!("--listen=[2001:db8::10]:{}", c.reapi_port)),
        "{v6}"
    );
    assert!(
        v6.contains(&format!("--worker-listen=[2001:db8::10]:{}", c.worker_port)),
        "{v6}"
    );
    assert!(!v6.contains("--listen=2001"), "{v6}");
}
