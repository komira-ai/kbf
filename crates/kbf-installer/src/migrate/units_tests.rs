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

const PG: &str = "postgres:///kbf?host=/run/postgresql";

/// A frontend unit as the kit renders it: the flags continue over lines, the state store is
/// on a continuation line, and the comments above name the flag too.
const FRONTEND: &str = "[Service]\n\
# the state store is --state-store=sqlite:///var/lib/not/a/flag.db in this comment\n\
ExecStart=/usr/local/kbf/current/kbf frontend \\\n    --listen=127.0.0.1:8980 \\\n    --state-store=sqlite:///var/lib/kbf-server/state/state.db \\\n    --handover-socket=/run/kbf-frontend/control.sock\n\
Description=--state-store=sqlite:///var/lib/not/a/command.db\n";

/// Catches a store switch that changes more than the store, or misses where the flag is: the
/// value of `--state-store=sqlite://...` on a line the `Exec*=` setting continues is replaced
/// and nothing else moves; a comment, another setting, a flag that is not a whole word and a
/// store that is not SQLite are left as they are; the flag is found in quotes, twice, and in a
/// launchd `<string>`; a second pass finds nothing to do.
///
/// Mutants seen red: continuation lines not followed (the flag of a kit unit is on one); the
/// word-start check removed; `starts_with("sqlite://")` removed (a Postgres URL replaced).
#[test]
fn the_store_flag_is_switched_where_the_command_line_is_and_nowhere_else() {
    let switched = switch_store_text(Kind::Systemd, FRONTEND, PG).expect("changes");
    assert_eq!(
        switched,
        FRONTEND.replace(
            "--state-store=sqlite:///var/lib/kbf-server/state/state.db",
            &format!("--state-store={PG}")
        )
    );
    assert_eq!(switch_store_text(Kind::Systemd, &switched, PG), None);

    // Not a word, not SQLite, not a command line: each stays.
    for same in [
        "ExecStart=/bin/kbf x--state-store=sqlite:///a.db\n",
        "ExecStart=/bin/kbf --state-store=file:///a\n",
        "ExecStart=/bin/kbf --state-store=postgres://other/db\n",
        "[Service]\nDescription=kbf --state-store=sqlite:///a.db\n",
        "# ExecStart=/bin/kbf --state-store=sqlite:///a.db\n",
        "ExecStart=/bin/kbf --state-store sqlite:///a.db\n",
    ] {
        assert_eq!(switch_store_text(Kind::Systemd, same, PG), None, "{same}");
    }

    // Two settings and a quoted word, and a last line with no newline.
    let two = "ExecStartPre=/bin/kbf check --state-store=sqlite:///a.db\n\
               ExecStart=/bin/kbf \"--state-store=sqlite:///a.db\" --x";
    assert_eq!(
        switch_store_text(Kind::Systemd, two, PG).expect("changes"),
        format!(
            "ExecStartPre=/bin/kbf check --state-store={PG}\n\
             ExecStart=/bin/kbf \"--state-store={PG}\" --x"
        )
    );

    let plist =
        "<array>\n<string>--state-store=sqlite:///a.db</string>\n<string>--x</string>\n</array>\n";
    assert_eq!(
        switch_store_text(Kind::Launchd, plist, PG).expect("changes"),
        format!("<array>\n<string>--state-store={PG}</string>\n<string>--x</string>\n</array>\n")
    );
}

/// Catches a switch that loses the unit's original or reports the wrong thing: the first
/// switch keeps the original byte for byte (also after the path was rewritten first, as the
/// migration does), a second pass says `Already` and keeps that original, a unit with no
/// store is `NotOurs`, a missing file is `Missing`, and `restore` puts the SQLite store back.
///
/// Mutants seen red: the backup written on every switch (the path-rewritten text replaces
/// the original); `Already` reported as `NotOurs`.
#[test]
fn a_switched_unit_keeps_its_original_and_a_second_pass_finds_it_done() {
    let d = dirs("store");
    fs::write(d.root.join("units/kbf-frontend.service"), FRONTEND).expect("unit");
    let u = unit("kbf-frontend.service");
    assert_eq!(
        rewrite(&rules(), &d.units, &d.backup, &u, NEW).expect("path"),
        UnitOutcome::Rewritten
    );
    let go = || switch_store(&rules(), &d.units, &d.backup, &u, PG).expect("switch");
    assert_eq!(go(), UnitOutcome::Rewritten);
    let after = fs::read_to_string(d.root.join("units/kbf-frontend.service")).expect("unit");
    assert!(after.contains(NEW) && after.contains(&format!("--state-store={PG} ")));
    assert!(!after.contains("--state-store=sqlite:///var/lib/kbf-server"));
    assert_eq!(go(), UnitOutcome::Already);
    assert_eq!(
        fs::read_to_string(d.root.join("backup/kbf-frontend.service")).expect("backup"),
        FRONTEND
    );
    assert!(restore(&rules(), &d.units, &d.backup, &u).expect("restore"));
    assert_eq!(
        fs::read_to_string(d.root.join("units/kbf-frontend.service")).expect("unit"),
        FRONTEND
    );

    fs::write(
        d.root.join("units/kbf-daemon.service"),
        "[Service]\nExecStart=/usr/local/kbf/current/kbf daemon\n",
    )
    .expect("unit");
    for (name, want) in [
        ("kbf-daemon.service", UnitOutcome::NotOurs),
        ("absent.service", UnitOutcome::Missing),
    ] {
        let outcome = switch_store(&rules(), &d.units, &d.backup, &unit(name), PG).expect("pass");
        assert_eq!(outcome, want, "{name}");
    }
    assert!(!d.root.join("backup/kbf-daemon.service").exists());
}

/// Catches a switch that loses the original of a unit the path pass left alone: a unit that
/// never ran from the old link (`NotOurs` to `rewrite`) is first changed by the switch, which
/// keeps that original itself.
///
/// Mutant seen red: the backup made from the new text instead of the original.
#[test]
fn a_unit_the_path_pass_left_alone_keeps_its_original_when_its_store_is_switched() {
    let d = dirs("store-first");
    let original = "[Service]\nExecStart=/opt/kbf/bin/kbf frontend --state-store=sqlite:///a.db\n";
    fs::write(d.root.join("units/kbf-front.service"), original).expect("unit");
    let u = unit("kbf-front.service");
    assert_eq!(
        rewrite(&rules(), &d.units, &d.backup, &u, NEW).expect("path"),
        UnitOutcome::NotOurs
    );
    assert_eq!(
        switch_store(&rules(), &d.units, &d.backup, &u, PG).expect("switch"),
        UnitOutcome::Rewritten
    );
    assert_eq!(
        fs::read_to_string(d.root.join("backup/kbf-front.service")).expect("backup"),
        original
    );
    assert_eq!(
        fs::read_to_string(d.root.join("units/kbf-front.service")).expect("unit"),
        original.replace("sqlite:///a.db", PG)
    );
}
