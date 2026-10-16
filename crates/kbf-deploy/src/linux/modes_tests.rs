//! The mode units: each renders from the example settings into a unit that passes
//! every rule and matches its golden, each planted defect turns its rule red, and the
//! converged units `render-units` writes without `--mode` are unchanged.

use std::path::Path;

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
    let rows: [(Mode, &str, &str, &str); 7] = [
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
            "FAIL slice: kbf-all-in-one.service runs builds and needs Delegate=yes for their cgroups",
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
    let r = crate::linux::check_mode_unit(&cfg, &out, Mode::AllInOne, true).expect("checks");
    assert_eq!(lines(&r), Vec::<String>::new());

    let e = crate::linux::check_mode_unit(&cfg, &out, Mode::Daemon, true)
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
    let e = crate::linux::check_mode_unit(&cfg, &out, Mode::AllInOne, true)
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
    let e = crate::linux::check_mode_unit(&none, &out, Mode::AllInOne, true).err();
    assert!(matches!(e, Some(crate::linux::Refusal::Config(_))), "{e:?}");
}
