use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use super::*;

const NEW: &str = "/var/lib/kbf-installer/releases/current";

fn rules() -> Rules {
    Rules::release(rustix::process::geteuid().as_raw())
}

fn unit(name: &str) -> Unit {
    Unit::new(name).expect("a unit name")
}

/// Catches a name that reaches a path or a command line unchecked: only plain service and
/// launchd names are units.
#[test]
fn only_plain_unit_names_are_units() {
    for good in [
        "kbf-daemon.service",
        "kbf2.service",
        "com.komira.kbf-daemon.plist",
    ] {
        assert_eq!(unit(good).as_str(), good);
    }
    for bad in [
        "",
        ".service",
        "-kbf.service",
        "kbf-.service",
        "Kbf.service",
        "kbf daemon.service",
        "kbf/daemon.service",
        "../kbf.service",
        "kbf@a.service",
        "kbf.timer",
        "kbf.service.d",
        "kbf..plist",
        ".kbf.plist",
        "kbf.com.service",
    ] {
        assert!(Unit::new(bad).is_err(), "{bad:?}");
    }
    let named = unit("com.komira.kbf-daemon.plist");
    assert_eq!(
        (named.kind(), named.stem()),
        (Kind::Launchd, "com.komira.kbf-daemon")
    );
    let named = unit("kbf-daemon.service");
    assert_eq!((named.kind(), named.stem()), (Kind::Systemd, "kbf-daemon"));
    assert_eq!(named.to_string(), "kbf-daemon.service");
    let json = serde_json::to_string(&named).expect("json");
    assert_eq!(serde_json::from_str::<Unit>(&json).expect("back"), named);
    assert!(serde_json::from_str::<Unit>("\"../x.service\"").is_err());
}

/// Catches a rewrite that touches more than the path: every `Exec*=` and `Environment=`
/// line holding the old path as a whole is changed (also after `=` or `:`, and twice in a
/// line), and nothing else: not a comment, not `Description=`, not a longer path.
///
/// Mutants seen red: the `opened` test removed (`/opt/usr/local/kbf/current` rewritten);
/// the `closed` test removed (`current-old` rewritten).
#[test]
fn a_systemd_unit_changes_in_exec_and_environment_values_only() {
    let text = "\
[Unit]
Description=runs /usr/local/kbf/current/kbf
# ExecStart=/usr/local/kbf/current/kbf commented
[Service]
Environment=KBF_BIN_DIR=/usr/local/kbf/current PATH=/bin:/usr/local/kbf/current
ExecStartPre=/usr/local/kbf/current/kbf --check
ExecStart=\"/usr/local/kbf/current/kbf\" daemon /usr/local/kbf/current
ExecStop=/opt/usr/local/kbf/current/kbf
ExecReload=/usr/local/kbf/current-old/kbf
ExecCondition=/usr/local/kbf/currentX
";
    let out = rewrite_text(Kind::Systemd, text, NEW).expect("changed");
    let want = "\
[Unit]
Description=runs /usr/local/kbf/current/kbf
# ExecStart=/usr/local/kbf/current/kbf commented
[Service]
Environment=KBF_BIN_DIR=/var/lib/kbf-installer/releases/current PATH=/bin:/var/lib/kbf-installer/releases/current
ExecStartPre=/var/lib/kbf-installer/releases/current/kbf --check
ExecStart=\"/var/lib/kbf-installer/releases/current/kbf\" daemon /var/lib/kbf-installer/releases/current
ExecStop=/opt/usr/local/kbf/current/kbf
ExecReload=/usr/local/kbf/current-old/kbf
ExecCondition=/usr/local/kbf/currentX
";
    assert_eq!(out, want);
    assert_eq!(
        rewrite_text(Kind::Systemd, want, NEW),
        None,
        "a second pass changes nothing"
    );
    assert_eq!(
        rewrite_text(Kind::Systemd, "ExecStart=/bin/true\n", NEW),
        None
    );
    // Two paths with nothing between them: the second is not opened by anything.
    let glued = "ExecStart=/usr/local/kbf/current/usr/local/kbf/current\n";
    assert_eq!(
        rewrite_text(Kind::Systemd, glued, NEW).as_deref(),
        Some("ExecStart=/var/lib/kbf-installer/releases/current/usr/local/kbf/current\n")
    );
}

/// Catches a plist rewrite that changes keys or other strings: only a `<string>` line
/// holding the old path as a whole is changed, whether it ends the element or the path.
#[test]
fn a_plist_changes_in_string_lines_only() {
    let text = "\
<dict>
  <key>/usr/local/kbf/current</key>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/kbf/current/kbf-daemon</string>
    <string>/usr/local/kbf/current</string>
    <string>/usr/local/kbf/current2</string>
  </array>
  <key>Label</key><string>x</string>
</dict>
";
    let out = rewrite_text(
        Kind::Launchd,
        text,
        "/usr/local/kbf-installer/releases/current",
    )
    .expect("changed");
    assert!(out.contains("<key>/usr/local/kbf/current</key>"));
    assert!(out.contains("<string>/usr/local/kbf-installer/releases/current/kbf-daemon</string>"));
    assert!(out.contains("<string>/usr/local/kbf-installer/releases/current</string>"));
    assert!(out.contains("<string>/usr/local/kbf/current2</string>"));
    assert_eq!(
        rewrite_text(Kind::Systemd, text, NEW),
        None,
        "a plist is not read as a unit"
    );
}

struct Dirs {
    root: PathBuf,
    units: OwnedFd,
    backup: OwnedFd,
}

fn dirs(name: &str) -> Dirs {
    crate::migrate::rig::root_umask();
    let root = kbf_host::testkit::scratch(&format!("migrate-units-{name}"));
    fs::create_dir(root.join("units")).expect("units");
    fs::create_dir(root.join("backup")).expect("backup");
    Dirs {
        units: tree::open_root(&root.join("units")).expect("open"),
        backup: tree::open_root(&root.join("backup")).expect("open"),
        root,
    }
}

/// Catches a rewrite that loses the original or keeps only a later version of it: the
/// first rewrite keeps the original byte for byte, a second run says `Already` and keeps
/// that same original, and `restore` puts the original back.
///
/// Mutant seen red: the backup written on every rewrite (the original becomes the rewritten
/// text on a repeated run that finds the old path again, here the rewritten-then-edited unit).
#[test]
fn the_first_original_is_kept_and_restored() {
    let d = dirs("keep");
    let original = "[Service]\nExecStart=/usr/local/kbf/current/kbf daemon\n";
    fs::write(d.root.join("units/kbf-daemon.service"), original).expect("unit");
    let u = unit("kbf-daemon.service");
    let go = || rewrite(&rules(), &d.units, &d.backup, &u, NEW).expect("rewrite");
    assert_eq!(go(), UnitOutcome::Rewritten);
    let after = fs::read_to_string(d.root.join("units/kbf-daemon.service")).expect("unit");
    assert!(after.contains(NEW) && !after.contains(OLD_BIN));
    assert_eq!(
        fs::read_to_string(d.root.join("backup/kbf-daemon.service")).expect("backup"),
        original
    );
    assert_eq!(go(), UnitOutcome::Already);
    // An operator edit that brings the old path back is rewritten again; the backup is still the first.
    fs::write(
        d.root.join("units/kbf-daemon.service"),
        "ExecStart=/usr/local/kbf/current/kbf x\n",
    )
    .expect("edit");
    assert_eq!(go(), UnitOutcome::Rewritten);
    assert_eq!(
        fs::read_to_string(d.root.join("backup/kbf-daemon.service")).expect("backup"),
        original
    );
    assert!(restore(&rules(), &d.units, &d.backup, &u).expect("restore"));
    assert_eq!(
        fs::read_to_string(d.root.join("units/kbf-daemon.service")).expect("unit"),
        original
    );
    assert!(!restore(&rules(), &d.units, &d.backup, &unit("other.service")).expect("none kept"));
}

/// Catches a unit that does not run kbf from a release being rewritten or reported as
/// done, and a missing file being an error: they are `NotOurs` and `Missing`.
#[test]
fn units_that_are_not_ours_or_not_there_are_left_alone() {
    let d = dirs("notours");
    fs::write(
        d.root.join("units/other.service"),
        "[Service]\nExecStart=/bin/true\n",
    )
    .expect("unit");
    assert_eq!(
        rewrite(&rules(), &d.units, &d.backup, &unit("other.service"), NEW).expect("rewrite"),
        UnitOutcome::NotOurs
    );
    assert_eq!(
        rewrite(&rules(), &d.units, &d.backup, &unit("absent.service"), NEW).expect("rewrite"),
        UnitOutcome::Missing
    );
    assert!(
        d.root
            .join("backup")
            .read_dir()
            .expect("dir")
            .next()
            .is_none(),
        "nothing kept for nothing changed"
    );
}

/// Catches a unit file followed through a link or one anyone else could have written:
/// a symlinked unit and a group-writable unit are refused and nothing is written.
///
/// Mutant seen red: `NOFOLLOW` removed from `read_file` (the link's target is rewritten).
#[test]
fn a_linked_or_writable_unit_is_refused() {
    let d = dirs("refuse");
    fs::write(
        d.root.join("target.service"),
        "ExecStart=/usr/local/kbf/current/kbf\n",
    )
    .expect("target");
    symlink(
        d.root.join("target.service"),
        d.root.join("units/linked.service"),
    )
    .expect("link");
    assert!(rewrite(&rules(), &d.units, &d.backup, &unit("linked.service"), NEW).is_err());
    fs::write(
        d.root.join("units/open.service"),
        "ExecStart=/usr/local/kbf/current/kbf\n",
    )
    .expect("unit");
    fs::set_permissions(
        d.root.join("units/open.service"),
        fs::Permissions::from_mode(0o666),
    )
    .expect("mode");
    assert!(rewrite(&rules(), &d.units, &d.backup, &unit("open.service"), NEW).is_err());
    assert_eq!(
        fs::read_to_string(d.root.join("target.service")).expect("target"),
        "ExecStart=/usr/local/kbf/current/kbf\n"
    );
}
