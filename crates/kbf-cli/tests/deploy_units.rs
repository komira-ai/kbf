//! The units `kbf-deploy linux render-units --mode` writes, against this binary's own
//! parser, in-process: each mode's rendered `ExecStart=` is a command line `kbf` takes,
//! for the mode the unit is named after, and kbf-deploy renders exactly kbf's modes;
//! and `check-units --mode` runs this build's `kbf check-args` on a built mode's flags.

use kbf_cli::names::{Entry, MODES};
use kbf_cli::{Invocation, parse_from};
use kbf_deploy::linux::modes::{TRUSTED_NOTE, render};
use kbf_deploy::linux::{LinuxConfig, Mode, check_mode_unit};
use kbf_deploy::unit::UnitFile;
use kbf_host::exec::{Entry as HostEntry, Exec, HostExec, Meta};
use kbf_host::path::{HostPath, Root};
use kbf_host::step::{Cmd, HttpUrl, Step};
use kbf_host::trust::Untrusted;

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
            Invocation::Storage(flags) => {
                assert_eq!(mode, Mode::Storage);
                // The unit's certificate flags are read as the paths they spell (not
                // dropped, and not another flag's), so the node starts with mutual TLS.
                let paths = flags.tls.expect("the storage unit turns on mutual TLS");
                assert_eq!(
                    paths.map(|p| p.display().to_string()),
                    [
                        "/etc/kbf/storage.pem",
                        "/etc/kbf/storage.key",
                        "/etc/kbf/ca.pem"
                    ]
                );
                // Its stores and listener are the settings', read as kbf reads them:
                // the bucket, the hot store and the metadata log, on the pinned address.
                assert_eq!(flags.listen.to_string(), "192.0.2.10:8983");
                assert_eq!(
                    flags.server.meta_dir.as_deref(),
                    Some(std::path::Path::new("/var/lib/kbf-server/raft"))
                );
                assert!(flags.server.cold_store.is_some(), "no cold store");
                assert!(flags.server.hot_store.is_some(), "no hot store");
            }
            Invocation::NotBuilt { mode: m, .. } => {
                assert_eq!(m, mode.name());
                assert_eq!(argv.len(), 2, "kbf {mode} is not built: {argv:?}");
            }
        }
    }
}

/// The all-in-one unit gives no `--state-store`, and kbf all-in-one, parsing the rendered
/// command line, takes the SQLite file `state.db` in the unit's `--data-dir`: the state
/// the template's comment promises, and one rule `state-store` passes. Catches the unit
/// growing a `--state-store` (a Postgres URI, say) or kbf's default moving out of the
/// data directory, or off SQLite.
#[test]
fn the_all_in_one_unit_leaves_its_state_store_to_kbfs_sqlite_default() {
    let text = render(&config(), Some(1500), Mode::AllInOne).expect("renders");
    let unit = UnitFile::parse(&text).expect("a unit");
    let argv = unit.exec_start();
    assert!(
        !argv.iter().any(|a| a.starts_with("--state-store")),
        "{argv:?}"
    );
    let data_dir = argv
        .iter()
        .find_map(|a| a.strip_prefix("--data-dir="))
        .expect("the all-in-one unit gives --data-dir");
    match parse_from(argv.iter().copied()) {
        Ok(Invocation::AllInOne(flags)) => assert_eq!(
            flags.server.state_store,
            Some(kbf_server::StateStoreUri::Sqlite(
                std::path::Path::new(data_dir).join("state.db")
            ))
        ),
        Ok(_) => panic!("not the all-in-one"),
        Err(e) => panic!("kbf all-in-one refuses its rendered unit: {e}"),
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

/// The real host, but a command it runs keeps this test's `LLVM_PROFILE_FILE`:
/// [`HostExec`] clears the environment, and an instrumented `kbf` would then write its
/// profile into the working directory (the crate's), where the repo lints refuse it.
/// With `old` set, every command answers as a `kbf` from before `check-args` would:
/// this build's parser without the word intercepted, which that `kbf` ran on every
/// command line.
struct Profiled {
    host: HostExec,
    old: bool,
}

impl Exec for Profiled {
    fn euid(&self) -> u32 {
        self.host.euid()
    }
    fn inspect(&self, path: &HostPath) -> std::io::Result<Option<HostEntry>> {
        self.host.inspect(path)
    }
    fn stat(&self, path: &HostPath) -> std::io::Result<Option<Meta>> {
        self.host.stat(path)
    }
    fn query(&self, cmd: &Cmd) -> std::io::Result<std::process::Output> {
        if self.old {
            return Ok(old_kbf(cmd));
        }
        match std::env::var("LLVM_PROFILE_FILE") {
            Ok(file) => self.host.query(&cmd.clone().env("LLVM_PROFILE_FILE", file)),
            Err(_) => self.host.query(cmd),
        }
    }
    fn http_status(&self, url: &HttpUrl) -> std::io::Result<u16> {
        self.host.http_status(url)
    }
    fn apply(&mut self, step: &Step) -> std::io::Result<()> {
        self.host.apply(step)
    }
    fn check_binary(&self) -> Result<(), Untrusted> {
        self.host.check_binary()
    }
}

/// `check-units --mode` on the unit `text` of `mode`, with this build's kbf as
/// `--kbf-bin`: the failure lines and the notes.
fn check_units(name: &str, mode: Mode, text: &str) -> (Vec<String>, Vec<String>) {
    let exec = Profiled {
        host: HostExec::new(Root::host()),
        old: false,
    };
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_kbf"));
    check_units_with(&exec, bin, name, mode, text)
}

/// `check-units --mode` on the unit `text` of `mode`, beside the rendered `kbf.slice`,
/// on the host `exec` with `bin` as `--kbf-bin`: the failure lines and the notes.
fn check_units_with<E: Exec>(
    exec: &E,
    bin: &std::path::Path,
    name: &str,
    mode: Mode,
    text: &str,
) -> (Vec<String>, Vec<String>) {
    let dir = scratch(name);
    let cfg = dir.join("config.local");
    std::fs::write(&cfg, EXAMPLE).expect("config");
    kbf_deploy::linux::render_mode_unit(&cfg, &dir, Some(1500), mode).expect("renders");
    std::fs::write(dir.join(mode.unit_name()), text).expect("unit");
    let r = check_mode_unit(exec, &cfg, &dir, mode, true, Some(bin)).expect("checks");
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
    for mode in [Mode::Storage, Mode::Daemon, Mode::AllInOne] {
        let text = render(&c, Some(1500), mode).expect("renders");
        // all-in-one is labelled trusted; the storage unit's key file is not on this
        // machine; nothing else is said.
        let notes: Vec<String> = match mode {
            Mode::AllInOne => vec![TRUSTED_NOTE.to_owned()],
            Mode::Storage => vec![
                "SKIP credential: no key file at /etc/kbf/s3.env yet, so its owner and mode were not checked"
                    .to_owned(),
            ],
            _ => Vec::new(),
        };
        assert_eq!(
            check_units(&format!("clean_{mode}"), mode, &text),
            (Vec::new(), notes),
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
    // The storage node's parser, which refuses kbf-server's other flags by name.
    let text = render(&c, Some(1500), Mode::Storage).expect("renders");
    let edited = text.replace(
        "--meta-dir=",
        "--worker-listen=192.0.2.10:8981 \\\n    --meta-dir=",
    );
    assert_ne!(edited, text);
    let (failures, _) = check_units("edited_storage", Mode::Storage, &edited);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].starts_with(&format!(
            "FAIL parse: kbf storage refuses the flags of kbf-storage.service ({bin} check-args: exit 2): error: kbf storage refuses --worker-listen"
        )),
        "{failures:?}"
    );
}

/// What a `kbf` without `check-args` answers to `cmd`: clap's refusal, exit 2.
fn old_kbf(cmd: &Cmd) -> std::process::Output {
    use std::os::unix::process::ExitStatusExt as _;
    let argv = std::iter::once(cmd.program.as_str().to_owned()).chain(cmd.args.clone());
    let e = parse_from(argv)
        .map(drop)
        .expect_err("an old kbf refuses check-args");
    std::process::Output {
        status: std::process::ExitStatus::from_raw(e.exit_code() << 8),
        stdout: Vec::new(),
        stderr: e.render().to_string().into_bytes(),
    }
}

/// The upgrade note in docs/deploy/kbf-modes.md: an older `kbf`, without `check-args`,
/// fails rule `parse` with the line the doc quotes. Catches the doc's line going stale
/// (clap's wording, the rule's message) and an older kbf's refusal not failing the rule.
#[test]
fn an_older_kbf_fails_the_parse_rule_with_the_line_the_docs_quote() {
    let doc = include_str!("../../../docs/deploy/kbf-modes.md");
    let text = render(&config(), Some(1500), Mode::Daemon).expect("renders");
    let bin = std::path::Path::new("/usr/local/bin/kbf");
    let (failures, notes) = check_units_with(
        &Profiled {
            host: HostExec::new(Root::host()),
            old: true,
        },
        bin,
        "old_kbf",
        Mode::Daemon,
        &text,
    );
    assert_eq!(notes, Vec::<String>::new());
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        doc.contains(&format!("\n{}\n", failures[0])),
        "docs/deploy/kbf-modes.md does not quote:\n{}",
        failures[0]
    );
}
