//! The units `kbf-deploy linux render-units --mode` writes, against this binary's own
//! parser, in-process: each mode's rendered `ExecStart=` is a command line `kbf` takes,
//! for the mode the unit is named after, and kbf-deploy renders exactly kbf's modes.

use kbf_cli::names::{Entry, MODES};
use kbf_cli::{Invocation, parse_from};
use kbf_deploy::linux::modes::render;
use kbf_deploy::linux::{LinuxConfig, Mode};
use kbf_deploy::unit::UnitFile;

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
