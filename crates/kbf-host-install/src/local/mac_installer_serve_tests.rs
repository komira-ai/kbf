//! `--launchd-label`: a Mac served on the installer layout, the flag's grammar, and the
//! serving around it (the socket's account from `dscl`, the private state directory).

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use super::core_tests::{A, Fake, Fx};
use super::production::{
    LinuxEnv, MAC_INSTALLER_STATE, MAC_STATE_DIR, MAC_WORK_MODE, serve_linux_with, serve_mac,
    serve_mac_installer_with, work_dir,
};
use super::serve::{Layout, ServeConfig};
use crate::mac::tests::fx::Fx as MacFx;
use crate::runner::{Cmd, Exec, Runner};
use crate::trust::Trust;

fn parse(extra: &[&str]) -> Result<ServeConfig, String> {
    let mut words = vec!["--socket", "/s", "--socket-user", "_kbf-updater"];
    words.extend_from_slice(extra);
    ServeConfig::parse(&words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>())
}

/// Catches a Mac label taken that is not one, or given where it means nothing: the label of
/// the daemon's job is kept as given; one with a character outside letters, digits, `.`,
/// `-` and `_`, an empty one, one over 128 characters, a second one, and one beside
/// `--restart-unit`, `--frontend-container` or `--layout deb`/`rpm` are refused; without the
/// flag there is none. The usage names it.
#[test]
fn the_launchd_label_flag_names_the_daemons_job() {
    assert_eq!(parse(&[]).unwrap().launchd_label, None);
    let given = parse(&["--launchd-label", "com.komira.kbf-daemon"]).unwrap();
    assert_eq!(
        given.launchd_label.as_deref(),
        Some("com.komira.kbf-daemon")
    );
    assert_eq!(given.layout, Layout::Installer);
    assert_eq!(
        parse(&["--launchd-label=a_b-c.9"])
            .unwrap()
            .launchd_label
            .as_deref(),
        Some("a_b-c.9")
    );
    let long = "a".repeat(128);
    assert!(parse(&["--launchd-label", &long]).is_ok());
    for bad in ["", "system/com.komira", "a b", &"a".repeat(129)] {
        let err = parse(&["--launchd-label", bad]).unwrap_err();
        assert!(err.contains("is not a launchd label"), "{bad:?}: {err}");
    }
    assert!(
        parse(&["--launchd-label", "a", "--launchd-label", "b"])
            .unwrap_err()
            .contains("given twice")
    );
    for beside in [
        &["--restart-unit", "kbf-daemon.service"][..],
        &["--frontend-container"][..],
        &["--layout", "deb"][..],
        &["--layout", "rpm"][..],
    ] {
        let mut words = vec!["--launchd-label", "com.komira.kbf-daemon"];
        words.extend_from_slice(beside);
        assert_eq!(
            parse(&words).unwrap_err(),
            "--launchd-label is for a Mac on the installer layout: it is not given with \
             --restart-unit, --frontend-container or --layout deb or rpm",
            "{beside:?}"
        );
    }
    assert!(super::serve::USAGE.contains("--launchd-label LABEL"));
}

/// A runner whose programs cannot be started.
struct Broken;

impl Runner for Broken {
    fn run(&self, _cmd: &Cmd) -> std::io::Result<Exec> {
        Err(std::io::Error::other("cannot start"))
    }
}

/// Catches a Mac on the installer layout that serves as the wrong account or with no state
/// directory of its own: the account of the socket comes from `dscl` and is never root's,
/// the private state directory is made (0700) and the backend's answers come over the
/// socket, the A/B state in a file of its own; a `dscl` that cannot run, an account with no
/// usable ids and a state directory that cannot be made are each an error that says which.
#[test]
fn a_mac_on_the_installer_layout_asks_dscl_and_makes_its_state_directory() {
    let fx = Fx::new("pr-macinst");
    let mac = MacFx::new("pr-macinst-sim");
    let sim = Rc::new(mac.sim());
    let run = fx.dir.join("run");
    fs::create_dir(&run).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = run.join("host.sock");
    let trust = Trust::owned_by(fs::metadata(&*fx.dir).unwrap().uid(), &fx.dir);
    let state_dir = fx.dir.join("state");
    let audit = fx.dir.join("audit.log");
    let config = parse(&["--launchd-label", "com.komira.kbf-daemon"]).unwrap();
    let config = ServeConfig {
        socket: socket.clone(),
        ..config
    };
    let backend = || Fake {
        world: Rc::clone(&fx.world),
        lock: fx.dir.join("lock2"),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let client = super::production_tests::ask_then_stop(socket.clone(), Arc::clone(&stop));
    serve_mac_installer_with(
        &config,
        &trust,
        &*sim,
        (&state_dir, &audit),
        backend(),
        &stop,
    )
    .unwrap();
    let answer: Value = serde_json::from_str(&client.join().unwrap()).unwrap();
    assert_eq!(answer["commit"], A);
    assert_eq!(
        fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777,
        MAC_WORK_MODE
    );
    assert_eq!(MAC_INSTALLER_STATE, "installer-state.json");

    let stopped = AtomicBool::new(true);
    let err = serve_mac_installer_with(
        &config,
        &trust,
        &Broken,
        (&state_dir, &audit),
        backend(),
        &stopped,
    )
    .unwrap_err();
    assert_eq!(err, "dscl: cannot start");
    sim.nobody_ids.set(Some((0, 0)));
    let root = ServeConfig {
        user: "nobody".to_owned(),
        ..config.clone()
    };
    let err = serve_mac_installer_with(
        &root,
        &trust,
        &*sim,
        (&state_dir, &audit),
        backend(),
        &stopped,
    )
    .unwrap_err();
    assert_eq!(err, "the account nobody has no usable ids");
    let file = fx.dir.join("state-file");
    fs::write(&file, "a file").unwrap();
    let err =
        serve_mac_installer_with(&config, &trust, &*sim, (&file, &audit), backend(), &stopped)
            .unwrap_err();
    assert!(err.starts_with(&file.display().to_string()), "{err}");
}

/// Catches the label taken on a host it means nothing on, or ignored by the program: a Linux
/// host refuses `--launchd-label` before it looks anything up, and the program's own Mac
/// serving with the label goes to the installer layout, whose first step, here on a host
/// with no `dscl` (Linux), is what fails.
#[test]
fn the_label_is_refused_on_linux_and_takes_a_mac_to_the_installer_layout() {
    let fx = Fx::new("pr-macinst-linux");
    let config = parse(&["--launchd-label", "com.komira.kbf-daemon"]).unwrap();
    let env = LinuxEnv {
        passwd: fx.dir.join("passwd"),
        state_dir: fx.dir.join("state"),
        state_name: "state.json".to_owned(),
        audit: fx.dir.join("audit.log"),
    };
    let backend = Fake {
        world: Rc::clone(&fx.world),
        lock: fx.dir.join("lock2"),
    };
    let trust = Trust::owned_by(fs::metadata(&*fx.dir).unwrap().uid(), &fx.dir);
    let err = serve_linux_with(&config, &env, &trust, backend, &AtomicBool::new(true)).unwrap_err();
    assert_eq!(
        err,
        "--launchd-label is for a Mac: a Linux host restarts its units"
    );
    assert!(!Path::new(&env.state_dir).exists());
    // On a Mac the program's own serving would ask the real `dscl`.
    if cfg!(target_os = "linux") {
        let err = serve_mac(&config).unwrap_err();
        assert!(err.starts_with("dscl: "), "{err}");
    }
}

/// The `<string>` values of the `ProgramArguments` of the plist `text`.
fn program_arguments(text: &str) -> Vec<String> {
    let start = text
        .find("<key>ProgramArguments</key>")
        .expect("ProgramArguments");
    let rest = &text[start..];
    let end = rest.find("</array>").expect("the end of the array");
    rest[..end]
        .split("<string>")
        .skip(1)
        .map(|part| {
            part.split("</string>")
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

/// Catches a state directory that locks the agent out of the shipped socket: the helper job
/// `packaging/host-agent/macos` ships puts `local.sock` in the very directory the installer
/// layout keeps its state in (`/var/db/kbf-host-install`), so that directory must let the
/// socket's account search it. It is made, and an existing 0700 one is changed, to 0711:
/// others may search it (and reach the socket, which is theirs, by name) and may not list or
/// write it; the directory is checked as root's; one that cannot be made says which.
#[test]
fn the_state_directory_lets_the_socket_account_reach_the_shipped_socket() {
    let shipped = include_str!(
        "../../../../packaging/host-agent/macos/com.komira.kbf-host-install-local.plist"
    );
    let args = program_arguments(shipped);
    let socket = args
        .windows(2)
        .find(|w| w[0] == "--socket")
        .map(|w| Path::new(&w[1]).to_owned())
        .expect("the socket flag");
    assert_eq!(socket.parent(), Some(Path::new(MAC_STATE_DIR)));
    assert!(args.windows(2).any(|w| w[0] == "--launchd-label"));

    let fx = Fx::new("pr-macinst-workdir");
    let trust = Trust::owned_by(fs::metadata(&*fx.dir).unwrap().uid(), &fx.dir);
    let mode = |dir: &Path| fs::metadata(dir).unwrap().permissions().mode() & 0o777;
    let made = fx.dir.join("made");
    work_dir(&trust, &made).unwrap();
    let existing = fx.dir.join("existing");
    fs::create_dir(&existing).unwrap();
    fs::set_permissions(&existing, fs::Permissions::from_mode(0o700)).unwrap();
    work_dir(&trust, &existing).unwrap();
    for dir in [&made, &existing] {
        let bits = mode(dir);
        assert_eq!(bits, 0o711, "{}", dir.display());
        assert_eq!(bits & 0o001, 0o001, "others search it");
        assert_eq!(bits & 0o066, 0, "no one else lists or writes it");
    }
    let err = work_dir(&trust, &fx.dir.join("no/such")).unwrap_err();
    assert!(
        err.starts_with(&fx.dir.join("no/such").display().to_string()),
        "{err}"
    );
}
