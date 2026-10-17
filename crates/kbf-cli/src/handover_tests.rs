//! `kbf frontend handover` against a fake `systemctl`: a shell script that keeps each
//! unit's `ActiveState` in a file, logs every call, and starts a unit by setting it
//! `active`, as a `Type=notify` start that was told `READY=1` does.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::*;

/// One test at a time: a script just written is executed, and a child forked by another
/// test between the write and the close would hold it open (ETXTBSY).
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

struct Fake {
    dir: PathBuf,
}

impl Fake {
    fn new(name: &str) -> Fake {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/handover-tests")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch");
        let script = format!(
            r#"#!/bin/sh
d='{dir}'
echo "$@" >> "$d/log"
case "$1" in
show)
    unit="$5"
    f="$d/state.$unit"
    [ -f "$d/show-fails" ] && {{ echo "no such unit" >&2; exit 1; }}
    if [ "$3" = Result ]; then
        if [ -f "$d/result.$unit" ]; then cat "$d/result.$unit"; else echo success; fi
        exit 0
    fi
    # `looks.UNIT` counts the looks left before the unit has exited.
    if [ -f "$d/looks.$unit" ]; then
        n=$(cat "$d/looks.$unit")
        if [ "$n" -le 0 ]; then rm "$d/looks.$unit"; echo inactive > "$f"
        else echo $((n - 1)) > "$d/looks.$unit"; fi
    fi
    if [ -f "$f" ]; then cat "$f"; else echo inactive; fi
    ;;
start)
    if [ -f "$d/start-fails" ]; then echo "Job for $2 failed." >&2; exit 1; fi
    echo active > "$d/state.$2"
    ;;
*)
    echo "unexpected verb $1" >&2
    exit 3
    ;;
esac
"#,
            dir = dir.display()
        );
        let path = dir.join("systemctl");
        fs::write(&path, script).expect("script");
        let mut perms = fs::metadata(&path).expect("meta").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&path, perms).expect("chmod");
        Fake { dir }
    }

    fn state(&self, instance: &str, state: &str) {
        fs::write(self.dir.join(format!("state.{}", unit(instance))), state).expect("state");
    }

    fn file(&self, name: &str, text: &str) {
        fs::write(self.dir.join(name), text).expect("file");
    }

    fn states(&self) -> (String, String) {
        let read = |i: &str| {
            fs::read_to_string(self.dir.join(format!("state.{}", unit(i))))
                .map_or_else(|_| "inactive".to_owned(), |s| s.trim().to_owned())
        };
        (read("a"), read("b"))
    }

    /// The calls made, one per line.
    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn handover(&self, wait: bool) -> (u8, String, String) {
        let h = Handover {
            systemctl: self.dir.join("systemctl"),
            wait,
            poll_ms: 1,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_to(&h, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("utf-8"),
            String::from_utf8(err).expect("utf-8"),
        )
    }
}

const SHOW_A: &str = "show -p ActiveState --value kbf-frontend@a.service";
const SHOW_B: &str = "show -p ActiveState --value kbf-frontend@b.service";

/// Catches a handover that starts the instance that serves (a restart of the serving
/// frontend under another name), starts both, calls any verb of systemctl other than
/// `show` and `start` (a stop, restart or kill would fail the builds in flight), or says
/// the old instance has exited when it has not.
#[test]
fn the_idle_instance_is_started_and_the_serving_one_is_left_alone() {
    let _one = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (serving, idle) in [("a", "b"), ("b", "a")] {
        let fake = Fake::new(&format!("idle-{idle}"));
        fake.state(serving, "active");
        let (code, out, err) = fake.handover(false);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        let start = format!("start kbf-frontend@{idle}.service");
        assert_eq!(fake.log(), [SHOW_A, SHOW_B, start.as_str()]);
        assert_eq!(
            out,
            format!(
                "kbf frontend handover: kbf-frontend@{serving}.service serves; starting kbf-frontend@{idle}.service to take over from it\n\
                 kbf frontend handover: kbf-frontend@{idle}.service is ready and serves; kbf-frontend@{serving}.service drains its open builds and exits when they are done\n"
            )
        );
        let (a, b) = fake.states();
        assert_eq!((a.as_str(), b.as_str()), ("active", "active"));
    }
}

/// Catches a start that failed reported as a success, a failed start that is followed by a
/// stop or restart of the old instance, and a message that does not say whether the old
/// one still serves (the operator's next step depends on it).
#[test]
fn a_start_that_fails_leaves_the_serving_instance_serving_and_says_so() {
    let _one = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fake = Fake::new("start-fails");
    fake.state("a", "active");
    fake.file("start-fails", "");
    let (code, out, err) = fake.handover(false);
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        err,
        "kbf frontend handover: kbf-frontend@b.service did not become ready: systemctl start exited 1: Job for kbf-frontend@b.service failed.\n\
         kbf frontend handover: kbf-frontend@a.service is still active; it stands down only for a new instance that is ready (see `journalctl -u kbf-frontend@b.service`)\n"
    );
    assert_eq!(
        fake.log(),
        [SHOW_A, SHOW_B, "start kbf-frontend@b.service", SHOW_A]
    );

    // The old instance is gone when the start fails: the message says no frontend serves.
    let fake = Fake::new("start-fails-old-gone");
    fake.state("a", "active");
    fake.file("start-fails", "");
    fake.file("looks.kbf-frontend@a.service", "1");
    let (code, _, err) = fake.handover(false);
    assert_eq!(code, 1);
    assert!(
        err.contains("kbf-frontend@a.service is no longer active either, and no frontend serves"),
        "{err}"
    );
}

/// Catches a start that goes ahead when the instances are not in a state that can hand
/// over: both active (the old one is still draining, and starting an instance would stop
/// nothing but would take the listeners from a frontend that cannot give them), one
/// activating or deactivating (a handover or a stop is under way), and a systemctl that
/// cannot be run or cannot show a unit.
#[test]
fn nothing_is_started_while_a_handover_is_draining_or_an_instance_is_changing() {
    let _one = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rows = [
        (
            "both-active",
            ("active", "active"),
            "kbf-frontend@a.service and kbf-frontend@b.service are both active: the one an earlier handover replaced is still draining its open builds and exits when they are done; run this again then\n",
        ),
        (
            "activating",
            ("active", "activating"),
            "kbf-frontend@b.service is activating: run this again when it is active or stopped\n",
        ),
        (
            "deactivating",
            ("deactivating", "inactive"),
            "kbf-frontend@a.service is deactivating: run this again when it is active or stopped\n",
        ),
    ];
    for (name, (a, b), why) in rows {
        let fake = Fake::new(name);
        fake.state("a", a);
        fake.state("b", b);
        let (code, out, err) = fake.handover(false);
        assert_eq!((code, out.as_str()), (1, ""), "{name}");
        assert_eq!(err, format!("kbf frontend handover: {why}"), "{name}");
        assert_eq!(fake.log(), [SHOW_A, SHOW_B], "{name}: nothing was started");
    }
    let fake = Fake::new("show-fails");
    fake.file("show-fails", "");
    let (code, _, err) = fake.handover(false);
    assert_eq!(code, 1);
    assert!(
        err.contains("show kbf-frontend@a.service: no such unit"),
        "{err}"
    );
    let h = Handover {
        systemctl: PathBuf::from("/nonexistent/systemctl"),
        wait: false,
        poll_ms: 1,
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(run_to(&h, &mut out, &mut err), 1);
    assert!(
        String::from_utf8_lossy(&err).contains("cannot run /nonexistent/systemctl"),
        "{err:?}"
    );
}

/// Catches a host with no frontend up that is left without one (the command is how the
/// first start and a start after a crash are done too), and a first start that picks the
/// second instance (the one that is enabled at boot is the first).
#[test]
fn with_no_instance_up_the_first_is_started() {
    let _one = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (name, (a, b)) in [
        ("none", ("inactive", "inactive")),
        ("failed", ("failed", "inactive")),
    ] {
        let fake = Fake::new(name);
        fake.state("a", a);
        fake.state("b", b);
        let (code, out, err) = fake.handover(false);
        assert_eq!((code, err.as_str()), (0, ""), "{name}");
        assert_eq!(
            out,
            "kbf frontend handover: no frontend serves; starting kbf-frontend@a.service\n\
             kbf frontend handover: kbf-frontend@a.service is ready and serves\n"
        );
        assert_eq!(fake.log(), [SHOW_A, SHOW_B, "start kbf-frontend@a.service"]);
    }
}

/// Catches `--wait` that returns before the old instance has exited, one that hangs on a
/// stopped instance, and a result other than `success` (the old frontend crashed while it
/// drained: its builds failed) reported as a clean end.
#[test]
fn wait_follows_the_old_instance_to_its_exit() {
    let _one = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fake = Fake::new("wait");
    fake.state("a", "active");
    // It is active for three looks (the first of them the command's own), then it has exited.
    fake.file("looks.kbf-frontend@a.service", "3");
    let (code, out, err) = fake.handover(true);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert!(
        out.ends_with("kbf-frontend@a.service has exited\n"),
        "{out}"
    );
    let log = fake.log();
    let looks = log.iter().filter(|l| l.as_str() == SHOW_A).count();
    assert_eq!(
        looks, 4,
        "three looks while it is active, one inactive: {log:?}"
    );
    assert_eq!(fake.states().0, "inactive");

    let fake = Fake::new("wait-failed");
    fake.state("a", "active");
    fake.file("looks.kbf-frontend@a.service", "1");
    fake.file("result.kbf-frontend@a.service", "exit-code\n");
    let (code, _, err) = fake.handover(true);
    assert_eq!(code, 1);
    assert!(
        err.contains("kbf-frontend@a.service ended with result exit-code"),
        "{err}"
    );
}

/// Catches the unit names the command starts drifting from the template's, and a default
/// that reads the wrong `systemctl`.
#[test]
fn the_defaults_name_the_template_and_the_systemctl_on_the_path() {
    use clap::FromArgMatches as _;
    let cmd = Handover::augment_args(clap::Command::new("handover"));
    let m = cmd.try_get_matches_from(["handover"]).expect("parses");
    let h = Handover::from_arg_matches(&m).expect("flags");
    assert_eq!(
        h,
        Handover {
            systemctl: PathBuf::from("systemctl"),
            wait: false,
            poll_ms: 1000
        }
    );
    assert_eq!(unit("a"), "kbf-frontend@a.service");
}
