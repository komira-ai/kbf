//! `package prerm` and `package postrm` on a scratch root, after a real first install.

use std::fs;

use super::super::testkit::*;
use super::super::*;

/// A host the package is installed on, with an enrolled node's files and some data.
fn installed(name: &str) -> Fixture {
    let mut f = fixture(name);
    f.postinst(Mode::Apply).1.expect("installs");
    for (file, text) in [
        ("/etc/kbf/kbf.toml", "[frontend]\n"),
        ("/var/lib/kbf/enroll.token", "secret"),
        ("/var/lib/kbf/pki/node.json", "{}"),
        ("/var/lib/kbf/pki/storage.key", "key"),
        ("/var/lib/kbf/hot/blob", "data"),
        ("/var/lib/kbf/meta/log", "log"),
        ("/var/lib/kbf/data/x/y", "work"),
    ] {
        write(&f.exec, file, text, 0o600);
    }
    f.exec.log.clear();
    f
}

fn runs(f: &Fixture) -> Vec<String> {
    f.steps()
        .into_iter()
        .filter(|s| s.starts_with("run "))
        .collect()
}

fn prerm(f: &mut Fixture, upgrade: bool, mode: Mode) -> (String, Result<(), PackageError>) {
    let mut out = Vec::new();
    let r = remove::prerm(&mut f.exec, upgrade, mode, &mut out);
    (String::from_utf8(out).expect("utf-8"), r)
}

fn postrm(
    f: &mut Fixture,
    purge: bool,
    purge_data: bool,
    mode: Mode,
) -> (String, Result<(), PackageError>) {
    let mut out = Vec::new();
    let what = remove::Postrm { purge, purge_data };
    let r = remove::postrm(&mut f.exec, &f.env, what, mode, &mut out);
    (String::from_utf8(out).expect("utf-8"), r)
}

/// Catches `prerm` stopping a node on an upgrade (the running release is the updater's
/// and keeps serving), not stopping one on a remove, and stopping one that is not
/// running or where no systemd runs.
#[test]
fn prerm_stops_the_node_on_a_remove_and_not_on_an_upgrade() {
    let mut f = installed("pkg-prerm");
    f.world.borrow_mut().active.insert(UNIT.to_owned());

    let (out, r) = prerm(&mut f, true, Mode::Apply);
    r.expect("upgrade");
    assert_eq!(f.steps(), Vec::<String>::new());
    assert!(
        out.contains("upgrade: kbf-node.service keeps running"),
        "{out}"
    );

    let (out, r) = prerm(&mut f, false, Mode::DryRun);
    r.expect("dry");
    assert!(
        out.contains("DRY-RUN: run /usr/bin/systemctl stop kbf-node.service"),
        "{out}"
    );
    assert!(
        f.world.borrow().active.contains(UNIT),
        "a dry run stops nothing"
    );

    let (_, r) = prerm(&mut f, false, Mode::Apply);
    r.expect("remove");
    assert_eq!(runs(&f), ["run /usr/bin/systemctl stop kbf-node.service"]);
    assert!(f.world.borrow().active.is_empty());

    // Stopped already: nothing to do, and it says so.
    f.exec.log.clear();
    let (out, r) = prerm(&mut f, false, Mode::Apply);
    r.expect("again");
    assert_eq!(f.steps(), Vec::<String>::new());
    assert!(out.contains("kbf-node.service is not running"), "{out}");

    // No systemd: never asked.
    f.world.borrow_mut().active.insert(UNIT.to_owned());
    fs::remove_dir(f.exec.at("/run/systemd/system")).expect("rm");
    let (out, r) = prerm(&mut f, false, Mode::Apply);
    r.expect("no systemd");
    assert_eq!(f.steps(), Vec::<String>::new());
    assert!(out.contains("is not running"), "{out}");
}

/// Catches a plain remove deleting configuration, keys, data or the account (they stay
/// for a reinstall), and a systemd left holding a unit the package took away.
#[test]
fn a_remove_keeps_everything_and_reloads_systemd_if_it_asks() {
    let mut f = installed("pkg-postrm-remove");
    let before = tree_of(&f);
    let (out, r) = postrm(&mut f, false, false, Mode::Apply);
    r.expect("remove");
    assert_eq!(f.steps(), Vec::<String>::new());
    assert_eq!(tree_of(&f), before);
    assert!(f.world.borrow().users.contains_key("kbf"));
    assert!(out.contains("nothing to do"), "{out}");

    f.world.borrow_mut().need_reload = true;
    postrm(&mut f, false, false, Mode::Apply).1.expect("remove");
    assert_eq!(runs(&f), ["run /usr/bin/systemctl daemon-reload"]);
    assert!(!f.world.borrow().need_reload);
}

fn tree_of(f: &Fixture) -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).expect("dir") {
            let p = e.expect("entry").path();
            out.push(p.strip_prefix(root).expect("under").display().to_string());
            if fs::symlink_metadata(&p).expect("meta").is_dir() {
                walk(&p, root, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&f.exec.root, &f.exec.root, &mut out);
    out.sort();
    out
}

/// Catches a purge that leaves a key, a credential or the account behind, removes the
/// node's data (hot store, metadata log, data, and the releases with the deploy state), or
/// runs its steps in an order that fails on a real host: the user's manager is stopped
/// and its ranges released before `userdel`, and the group goes with the user.
#[test]
fn a_purge_removes_the_keys_and_the_account_and_keeps_the_data() {
    let mut f = installed("pkg-postrm-purge");
    let uid = me().uid;
    let (out, r) = postrm(&mut f, true, false, Mode::Apply);
    r.unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert_eq!(
        runs(&f),
        [
            "run /usr/bin/loginctl disable-linger kbf".to_owned(),
            format!("run /usr/bin/systemctl stop user@{uid}.service"),
            "run /usr/sbin/usermod --del-subuids 200000-265535 --del-subgids 200000-265535 kbf"
                .to_owned(),
            "run /usr/sbin/userdel kbf".to_owned(),
        ]
    );
    for gone in [
        "/etc/kbf",
        "/var/lib/kbf/enroll.token",
        "/var/lib/kbf/pki",
        "/usr/local/libexec/kbf-deploy",
        "/var/lib/systemd/linger/kbf",
    ] {
        assert!(!exists(&f.exec, gone), "{gone}");
    }
    for kept in [
        "/var/lib/kbf",
        "/var/lib/kbf/hot/blob",
        "/var/lib/kbf/meta/log",
        "/var/lib/kbf/data/x/y",
        "/usr/local/kbf/current",
        &format!("/usr/local/kbf/releases/{COMMIT}/kbf"),
        "/var/lib/kbf-deploy/state.json",
        "/usr/local/libexec",
    ] {
        assert!(exists(&f.exec, kept), "{kept}");
    }
    let w = f.world.borrow();
    assert!(
        w.users.is_empty() && w.groups.is_empty(),
        "{:?}",
        (&w.users, &w.groups)
    );
    drop(w);
    assert_eq!(read(&f.exec, "/etc/subuid"), "\n");

    // A second purge has nothing left to do.
    f.exec.log.clear();
    let (out, r) = postrm(&mut f, true, false, Mode::Apply);
    r.expect("again");
    assert_eq!(f.steps(), Vec::<String>::new(), "{out}");
    assert!(out.contains("user kbf: not there"), "{out}");
}

/// Catches `--purge-data` leaving the node's data or the releases, and a plain purge
/// being allowed to take them (the test above).
#[test]
fn a_purge_with_data_removes_the_state_and_the_releases_too() {
    let mut f = installed("pkg-postrm-purge-data");
    postrm(&mut f, true, true, Mode::Apply).1.expect("purge");
    for gone in [
        "/var/lib/kbf",
        "/usr/local/kbf",
        "/var/lib/kbf-deploy",
        "/etc/kbf",
    ] {
        assert!(!exists(&f.exec, gone), "{gone}");
    }
    // What the package ships is dpkg's to remove.
    assert!(exists(&f.exec, "/usr/lib/kbf/release/kbf"));
}

/// Catches a group left behind where `userdel` keeps it (a host without per-user
/// groups), and a `groupdel` planned where `userdel` already removed it.
#[test]
fn the_group_is_removed_after_the_user_where_userdel_leaves_it() {
    let mut f = installed("pkg-postrm-group");
    f.world.borrow_mut().userdel_removes_group = false;
    postrm(&mut f, true, false, Mode::Apply).1.expect("purge");
    assert_eq!(
        runs(&f).last().map(String::as_str),
        Some("run /usr/sbin/groupdel kbf")
    );
    assert!(f.world.borrow().groups.is_empty());

    let mut f = installed("pkg-postrm-group-gone");
    postrm(&mut f, true, false, Mode::Apply).1.expect("purge");
    assert!(
        runs(&f).iter().all(|r| !r.contains("groupdel")),
        "{:?}",
        runs(&f)
    );
}

/// Catches a purge dry run that changes anything, and one that does not say what it
/// would remove.
#[test]
fn a_purge_dry_run_lists_the_removals_and_changes_nothing() {
    let mut f = installed("pkg-postrm-dry");
    let before = tree_of(&f);
    let (out, r) = postrm(&mut f, true, false, Mode::DryRun);
    r.expect("dry");
    assert_eq!(f.steps(), Vec::<String>::new());
    assert_eq!(tree_of(&f), before);
    for line in [
        "DRY-RUN: remove /etc/kbf/kbf.toml",
        "DRY-RUN: remove /etc/kbf",
        "DRY-RUN: remove /var/lib/kbf/pki/storage.key",
        "DRY-RUN: remove /var/lib/kbf/enroll.token",
        "DRY-RUN: run /usr/sbin/userdel kbf",
        "group kbf: removed with the user, or after it if the host keeps it",
    ] {
        assert!(out.contains(line), "{line}\n{out}");
    }
    assert!(!out.contains("remove /var/lib/kbf/hot"), "{out}");
    assert!(f.world.borrow().users.contains_key("kbf"));
}

/// Catches a purge that follows a link out of the tree it removes (a link in `/etc/kbf`
/// to a file elsewhere must be removed as a link), and lingering left as a file where no
/// systemd runs.
#[test]
fn a_link_is_removed_not_followed_and_lingering_is_a_file_without_systemd() {
    let mut f = installed("pkg-postrm-link");
    fs::remove_dir(f.exec.at("/run/systemd/system")).expect("rm");
    std::os::unix::fs::symlink(f.exec.at("/usr/lib/kbf/release"), f.exec.at("/etc/kbf/rel"))
        .expect("link");
    std::os::unix::fs::symlink(
        f.exec.at("/usr/lib/kbf/libexec/gh"),
        f.exec.at("/etc/kbf/gh"),
    )
    .expect("link");
    postrm(&mut f, true, false, Mode::Apply).1.expect("purge");
    assert!(
        exists(&f.exec, "/usr/lib/kbf/release/kbf"),
        "followed a link"
    );
    assert!(
        exists(&f.exec, "/usr/lib/kbf/libexec/gh"),
        "followed a link"
    );
    assert!(!exists(&f.exec, "/etc/kbf"));
    assert!(!exists(&f.exec, "/var/lib/systemd/linger/kbf"));
    assert!(
        runs(&f)
            .iter()
            .all(|r| !r.contains("loginctl") && !r.contains("systemctl")),
        "{:?}",
        runs(&f)
    );
}

/// Catches ranges recorded under the user's uid (not its name) being left behind, and
/// every range of the user, not only the first, being released.
#[test]
fn every_range_of_the_user_is_released() {
    let mut f = installed("pkg-postrm-ranges");
    let uid = me().uid;
    write(
        &f.exec,
        "/etc/subuid",
        &format!("alice:1:5\nkbf:200000:65536\n{uid}:300000:100\n"),
        0o644,
    );
    postrm(&mut f, true, false, Mode::Apply).1.expect("purge");
    assert!(
        runs(&f).contains(&"run /usr/sbin/usermod --del-subuids 200000-265535 --del-subuids 300000-300099 --del-subgids 200000-265535 kbf".to_owned()),
        "{:?}",
        runs(&f)
    );
}

/// Catches a tree walk that lists a parent before its children (a directory step that
/// removes only an empty directory would then fail), loses the top, or invents entries
/// for a path that is not there.
#[test]
fn a_tree_is_listed_children_first() {
    let f = installed("pkg-tree");
    let top = HostPath::new("/var/lib/kbf").expect("path");
    let all: Vec<String> = remove::tree_entries(&f.env.root, &top)
        .expect("lists")
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(all.last().map(String::as_str), Some("/var/lib/kbf"));
    for (i, p) in all.iter().enumerate() {
        for later in &all[i + 1..] {
            assert!(
                !later.starts_with(&format!("{p}/")),
                "{p} before its child {later}"
            );
        }
    }
    for p in [
        "/var/lib/kbf/pki/node.json",
        "/var/lib/kbf/data/x/y",
        "/var/lib/kbf/data/x",
    ] {
        assert!(all.iter().any(|a| a == p), "{p} in {all:?}");
    }
    let none = HostPath::new("/var/lib/not-there").expect("path");
    assert_eq!(remove::tree_entries(&f.env.root, &none).expect("lists"), []);
    let file = HostPath::new("/var/lib/kbf/enroll.token").expect("path");
    assert_eq!(
        remove::tree_entries(&f.env.root, &file).expect("lists"),
        [file]
    );
}

/// Catches an entry whose name is not UTF-8 being skipped (it would stay behind and the
/// directory step fail), and unreadable directories being read as empty.
#[test]
fn a_name_that_is_not_utf8_is_refused() {
    use std::os::unix::ffi::OsStrExt;
    let f = installed("pkg-tree-utf8");
    let odd = f
        .exec
        .at("/var/lib/kbf/hot")
        .join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
    fs::write(odd, "x").expect("write");
    let top = HostPath::new("/var/lib/kbf").expect("path");
    let e = remove::tree_entries(&f.env.root, &top).expect_err("refused");
    assert!(matches!(e, PackageError::NotUtf8(_)), "{e}");
}

/// Catches a `usermod` with no range to release being run (it would fail on a real host,
/// and the purge with it), for a user that never had a block or whose block was removed
/// by hand.
#[test]
fn a_user_without_ranges_needs_no_usermod() {
    let mut f = installed("pkg-postrm-no-ranges");
    f.subids("alice:1:5\n");
    postrm(&mut f, true, false, Mode::Apply).1.expect("purge");
    assert!(
        runs(&f).iter().all(|r| !r.contains("usermod")),
        "{:?}",
        runs(&f)
    );
    assert!(runs(&f).contains(&"run /usr/sbin/userdel kbf".to_owned()));
}

/// Catches `disable-linger` run for a user that does not linger (`loginctl` exits 1
/// there), and a lingering file left removed twice.
#[test]
fn a_user_that_does_not_linger_is_not_told_to_stop() {
    let mut f = installed("pkg-postrm-no-linger");
    fs::remove_file(f.exec.at("/var/lib/systemd/linger/kbf")).expect("rm");
    postrm(&mut f, true, false, Mode::Apply).1.expect("purge");
    assert!(
        runs(&f).iter().all(|r| !r.contains("linger")),
        "{:?}",
        runs(&f)
    );
}

/// Catches a tree walk that reports a directory it cannot read as empty.
#[test]
fn an_unreadable_tree_is_an_error() {
    let e = remove::walk(
        std::path::Path::new("/no/such/kbf-dir"),
        "/no/such/kbf-dir",
        &mut Vec::new(),
    )
    .expect_err("missing");
    assert!(
        e.to_string().contains("cannot read /no/such/kbf-dir"),
        "{e}"
    );
}
