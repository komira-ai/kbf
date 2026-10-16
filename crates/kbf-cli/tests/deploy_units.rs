//! The units `kbf-deploy linux render-units --mode` writes, against this binary's own
//! parser, in-process: each mode's rendered `ExecStart=` is a command line `kbf` takes,
//! for the mode the unit is named after, and kbf-deploy renders exactly kbf's modes;
//! and `check-units --mode` runs this build's `kbf check-args` on a built mode's flags.

use kbf_cli::names::{Entry, MODES};
use kbf_cli::{Invocation, parse_from};
use kbf_deploy::linux::modes::render;
use kbf_deploy::linux::{LinuxConfig, Mode, check_mode_unit};
use kbf_deploy::unit::UnitFile;
use kbf_host::exec::HostExec;
use kbf_host::path::Root;

const EXAMPLE: &str = include_str!("../../kbf-deploy/templates/linux/config.example");

/// The example settings with one pinned image, so the image flags render too.
fn config() -> LinuxConfig {
    let image = format!("docker.io/library/busybox@sha256:{}", "a".repeat(64));
    let text = EXAMPLE.replace("KBF_IMAGES=\n", &format!("KBF_IMAGES={image}\n"));
    assert_ne!(
        text, EXAMPLE,
        "config.example has no empty KBF_IMAGES= line"
    );
    LinuxConfig::parse(&text).expect("valid settings")
}

/// Catches a mode unit naming a flag `kbf <mode>` does not take or refuses as another
/// mode's (a store flag in the daemon's unit, `--cas` in the all-in-one's), a value
/// its parser refuses, a unit that starts another mode than its name says, and a flag
/// in the unit of a mode not built yet (kbf takes anything there and exits 2 at start,
/// so nothing would have checked it).
#[test]
fn each_rendered_mode_unit_parses_with_kbfs_own_parser() {
    let c = config();
    for mode in Mode::ALL {
        let text = render(&c, Some(1500), mode).expect("renders");
        let unit = UnitFile::parse(&text).expect("a unit");
        let argv = unit.exec_start();
        assert_eq!(argv[..2], ["/usr/local/bin/kbf", mode.name()], "{mode}");
        let parsed = match parse_from(argv.iter().copied()) {
            Ok(parsed) => parsed,
            Err(e) => panic!("kbf {mode} refuses its rendered ExecStart= {argv:?}:\n{e}"),
        };
        match parsed {
            Invocation::Daemon(_) => assert_eq!(mode, Mode::Daemon),
            Invocation::AllInOne(_) => assert_eq!(mode, Mode::AllInOne),
            Invocation::NotBuilt { mode: m, .. } => {
                assert_eq!(m, mode.name());
                assert_eq!(argv.len(), 2, "kbf {mode} is not built: {argv:?}");
            }
        }
    }
}

/// Catches kbf-deploy rendering a mode kbf does not have (a sixth, or an old name),
/// missing one kbf has, listing them in another order than `kbf --help`, and saying a
/// mode is built (its unit's flags checked) when kbf has no command line for it, or
/// the other way round once kbf builds one.
#[test]
fn kbf_deploy_renders_exactly_the_modes_kbf_takes() {
    let kbf = kbf_cli::command();
    let help: Vec<&str> = kbf.get_subcommands().map(clap::Command::get_name).collect();
    let deploy: Vec<&str> = Mode::ALL.iter().map(|m| m.name()).collect();
    assert_eq!(deploy, help);
    for (mode, row) in Mode::ALL.iter().zip(MODES) {
        let built = !matches!(row.entry, Entry::NotBuilt);
        assert_eq!(mode.is_built(), built, "{mode}");
    }
}

/// The scratch directory `name` under the workspace's `target/`, empty.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// `check-units --mode` on the unit `text` of `mode`, with this build's kbf as
/// `--kbf-bin`: the failure lines and the notes.
fn check_units(name: &str, mode: Mode, text: &str) -> (Vec<String>, Vec<String>) {
    let dir = scratch(name);
    let cfg = dir.join("config.local");
    std::fs::write(&cfg, EXAMPLE).expect("config");
    std::fs::write(dir.join(mode.unit_name()), text).expect("unit");
    let exec = HostExec::new(Root::host());
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_kbf"));
    let r = check_mode_unit(&exec, &cfg, &dir, mode, true, Some(bin)).expect("checks");
    (
        r.failures.iter().map(ToString::to_string).collect(),
        r.notes,
    )
}

/// Catches `check-units --mode` passing a built mode's unit whose flags kbf refuses (an
/// operator's edit that adds a flag `kbf daemon` does not have, or another mode's flag;
/// red before rule `parse`, which runs `kbf check-args`), failing a unit kbf takes, and
/// the two crates spelling `check-args` differently.
#[test]
fn check_units_parses_a_built_modes_flags_with_this_kbf() {
    assert_eq!(kbf_deploy::linux::modes::CHECK_ARGS, kbf_cli::CHECK_ARGS);
    let c = config();
    for mode in [Mode::Daemon, Mode::AllInOne] {
        let text = render(&c, Some(1500), mode).expect("renders");
        let none: (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        assert_eq!(
            check_units(&format!("clean_{mode}"), mode, &text),
            none,
            "{mode}"
        );
    }
    let bin = env!("CARGO_BIN_EXE_kbf");
    let text = render(&c, Some(1500), Mode::Daemon).expect("renders");
    for (flag, said) in [
        (
            "--bogus-flag",
            "error: unexpected argument '--bogus-flag' found",
        ),
        (
            "--cold-store=file:///x",
            "error: kbf daemon refuses --cold-store: a build node holds no store and no credential (docs/design/cluster.md, section 3.3)",
        ),
    ] {
        let edited = text.replace("--driver=container", &format!("--driver=container {flag}"));
        assert_ne!(edited, text);
        let (failures, notes) = check_units("edited", Mode::Daemon, &edited);
        assert_eq!(
            failures,
            [format!(
                "FAIL parse: kbf daemon refuses the flags of kbf-daemon.service ({bin} check-args: exit 2): {said}"
            )]
        );
        assert_eq!(notes, Vec::<String>::new());
    }
}
