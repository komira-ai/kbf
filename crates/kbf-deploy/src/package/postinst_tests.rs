//! `package postinst` on a scratch root: what a first install builds, that a second run
//! and an upgrade plan nothing they should not, and that every refusal comes before the
//! first step.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use kbf_host::plan::HostError;

use super::super::testkit::*;
use super::super::*;
use super::*;

/// The releases the updater made current and previous.
const CURRENT: &str = "cccccccccccccccccccccccccccccccccccccccc";
const PREVIOUS: &str = "dddddddddddddddddddddddddddddddddddddddd";

fn runs(f: &Fixture) -> Vec<String> {
    f.steps()
        .into_iter()
        .filter(|s| s.starts_with("run "))
        .collect()
}

fn state_json(f: &Fixture) -> String {
    read(&f.exec, "/var/lib/kbf-deploy/state.json")
}

/// Catches a dry run that changes the host (it must be the default), and a plan that
/// leaves a part out: the account, the subordinate ids, lingering, the directories, the
/// trust files, the release and its link.
#[test]
fn a_dry_run_lists_every_part_and_changes_nothing() {
    let mut f = fixture("pkg-postinst-dry");
    let (out, r) = f.postinst(Mode::DryRun);
    r.expect("a dry run");
    assert!(f.exec.log.is_empty(), "{:?}", f.steps());
    assert!(f.world.borrow().users.is_empty());
    assert!(!exists(&f.exec, "/var/lib/kbf"));
    assert!(!exists(&f.exec, "/usr/local/kbf"));
    assert!(!exists(&f.exec, "/etc/subuid"));
    assert!(!exists(&f.exec, "/var/lib/kbf-deploy/state.json"));
    for line in [
        "DRY-RUN: run /usr/sbin/groupadd --system kbf",
        "DRY-RUN: run /usr/sbin/useradd --system --gid kbf --home-dir /var/lib/kbf --no-create-home --shell /usr/sbin/nologin --comment \"kbf build farm node\" kbf",
        "DRY-RUN: run /usr/sbin/usermod --add-subuids 200000-265535 --add-subgids 200000-265535 kbf",
        "DRY-RUN: run /usr/bin/loginctl enable-linger kbf",
        "/etc/kbf: 0750 root:kbf, planned once user kbf exists",
        "/var/lib/kbf/data: 0700 kbf:kbf, planned once user kbf exists",
        "DRY-RUN: mkdir /usr/local/libexec/kbf-deploy",
        "DRY-RUN: write /usr/local/libexec/kbf-deploy/gh",
        &format!("DRY-RUN: mkdir /usr/local/kbf/releases/{COMMIT}"),
        &format!("DRY-RUN: write /usr/local/kbf/releases/{COMMIT}/kbf"),
        &format!("DRY-RUN: write /usr/local/kbf/releases/{COMMIT}/units/kbf-node.service"),
        &format!("DRY-RUN: link /usr/local/kbf/current -> releases/{COMMIT}"),
        "kbf-node.service is not enabled: it needs /etc/kbf/kbf.toml and /var/lib/kbf/enroll.token",
    ] {
        assert!(out.contains(line), "{line}\n{out}");
    }
    assert!(out.contains("dry run: nothing changed. Re-run with --apply."));
}

/// Catches each part of a first install: the account, a subordinate block in both
/// files, lingering, the directories with their modes, the trust files with theirs, the
/// release, `current` and the deploy state naming it, and nothing enabled for a host
/// with no configuration.
#[test]
fn a_first_install_builds_the_account_directories_files_and_release() {
    let mut f = fixture("pkg-postinst-first");
    let (out, r) = f.postinst(Mode::Apply);
    r.unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert_eq!(
        runs(&f),
        [
            "run /usr/sbin/groupadd --system kbf".to_owned(),
            "run /usr/sbin/useradd --system --gid kbf --home-dir /var/lib/kbf --no-create-home --shell /usr/sbin/nologin --comment \"kbf build farm node\" kbf".to_owned(),
            "run /usr/sbin/usermod --add-subuids 200000-265535 --add-subgids 200000-265535 kbf".to_owned(),
            "run /usr/bin/loginctl enable-linger kbf".to_owned(),
        ]
    );
    assert!(f.world.borrow().users.contains_key("kbf"));
    for file in ["/etc/subuid", "/etc/subgid"] {
        assert_eq!(read(&f.exec, file), "kbf:200000:65536\n", "{file}");
    }
    assert!(exists(&f.exec, "/var/lib/systemd/linger/kbf"));
    for (dir, mode) in [
        ("/etc/kbf", 0o750),
        ("/var/lib/kbf", 0o700),
        ("/var/lib/kbf/hot", 0o700),
        ("/var/lib/kbf/meta", 0o700),
        ("/var/lib/kbf/data", 0o700),
        ("/usr/local/libexec/kbf-deploy", 0o755),
        ("/usr/local/kbf/releases", 0o755),
        (&format!("/usr/local/kbf/releases/{COMMIT}"), 0o755),
    ] {
        assert_eq!(mode_of(&f.exec, dir), mode, "{dir}");
    }
    assert!(!exists(&f.exec, "/var/lib/kbf/pki"), "the node makes pki/");
    for (file, text, mode) in [
        ("gh", "gh-1", 0o755),
        ("trusted_root.jsonl", "root-1", 0o644),
        ("installer.conf", "conf-1", 0o644),
    ] {
        let path = format!("/usr/local/libexec/kbf-deploy/{file}");
        assert_eq!(read(&f.exec, &path), text, "{path}");
        assert_eq!(mode_of(&f.exec, &path), mode, "{path}");
    }
    let rel = format!("/usr/local/kbf/releases/{COMMIT}");
    assert_eq!(read(&f.exec, &format!("{rel}/kbf")), "kbf-1");
    assert_eq!(mode_of(&f.exec, &format!("{rel}/kbf")), 0o755);
    assert_eq!(
        read(&f.exec, &format!("{rel}/units/kbf-node.service")),
        "# kbf-1\n"
    );
    assert_eq!(
        mode_of(&f.exec, &format!("{rel}/units/kbf-node.service")),
        0o644
    );
    assert_eq!(
        link_of(&f.exec, "/usr/local/kbf/current"),
        format!("releases/{COMMIT}")
    );
    assert!(!exists(&f.exec, "/usr/local/kbf/previous"));
    let state: serde_json::Value = serde_json::from_str(&state_json(&f)).expect("json");
    assert_eq!(state["current"], COMMIT);
    assert_eq!(state["previous"], serde_json::Value::Null);
    assert!(f.world.borrow().enabled.is_empty());
    assert!(
        out.contains("state: written to /var/lib/kbf-deploy/state.json"),
        "{out}"
    );
}

/// Catches a verb that is not idempotent: a second apply must plan no step at all, and
/// leave the deploy state byte for byte.
#[test]
fn a_second_run_plans_nothing() {
    let mut f = fixture("pkg-postinst-twice");
    f.postinst(Mode::Apply).1.expect("first");
    let state = state_json(&f);
    f.exec.log.clear();
    let (out, r) = f.postinst(Mode::Apply);
    r.expect("second");
    assert_eq!(f.steps(), Vec::<String>::new(), "{out}");
    assert_eq!(state_json(&f), state);
    // A dry run after an apply also has nothing to say but "nothing to do".
    let (out, r) = f.postinst(Mode::DryRun);
    r.expect("dry");
    assert!(!out.contains("DRY-RUN:"), "{out}");
    assert_eq!(out.matches("nothing to do").count(), 4, "{out}");
}

/// The updater's host: `current` and `previous` point at releases it installed, and the
/// state says so. Returns the state it wrote.
fn updater_owns_the_links(f: &Fixture) -> String {
    let (c, d) = (CURRENT, PREVIOUS);
    for commit in [c, d] {
        write(
            &f.exec,
            &format!("/usr/local/kbf/releases/{commit}/kbf"),
            "kbf-x",
            0o755,
        );
    }
    for (link, commit) in [("current", c), ("previous", d)] {
        let at = f.exec.at(&format!("/usr/local/kbf/{link}"));
        let _ = fs::remove_file(&at);
        std::os::unix::fs::symlink(format!("releases/{commit}"), at).expect("link");
    }
    let state = format!(
        "{{\n  \"schema\": 1,\n  \"current\": \"{c}\",\n  \"previous\": \"{d}\",\n  \"staged\": null,\n  \"pending_confirm\": null,\n  \"order_floor\": 7\n}}\n"
    );
    write(&f.exec, "/var/lib/kbf-deploy/state.json", &state, 0o600);
    state
}

/// Catches the package taking `current` from the updater (the design's mutant "always
/// flip"): on a host where the updater has a release current, an install of another
/// package installs its release beside it and moves neither link nor the state.
#[test]
fn an_upgrade_installs_beside_and_never_moves_current() {
    let mut f = fixture("pkg-postinst-upgrade");
    f.postinst(Mode::Apply).1.expect("first install");
    f.exec.log.clear();
    let state = updater_owns_the_links(&f);
    // The package now ships another release and new trust files.
    ship(&f.exec, OTHER, "kbf-2");
    write(&f.exec, "/usr/lib/kbf/libexec/gh", "gh-2", 0o755);
    let (out, r) = f.postinst(Mode::Apply);
    r.unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert_eq!(
        link_of(&f.exec, "/usr/local/kbf/current"),
        format!("releases/{CURRENT}")
    );
    assert_eq!(
        link_of(&f.exec, "/usr/local/kbf/previous"),
        format!("releases/{PREVIOUS}")
    );
    assert_eq!(state_json(&f), state, "the deploy state is the updater's");
    assert!(
        f.steps().iter().all(|s| !s.starts_with("link ")),
        "{:?}",
        f.steps()
    );
    assert_eq!(read(&f.exec, "/usr/local/libexec/kbf-deploy/gh"), "gh-2");
    assert_eq!(
        read(
            &f.exec,
            &format!("/usr/local/kbf/releases/{OTHER}/units/kbf-node.service")
        ),
        "# kbf-2\n"
    );
    assert!(
        out.contains("current: exists, left to the updater"),
        "{out}"
    );
}

/// Catches a first-install flip when the deploy state already names a current release
/// whose link is gone: the state, not the package, decides, and the output says so.
#[test]
fn a_missing_link_the_state_names_is_left_to_the_updater() {
    let mut f = fixture("pkg-postinst-state-names");
    let state = updater_owns_the_links(&f);
    fs::remove_file(f.exec.at("/usr/local/kbf/current")).expect("rm");
    let (out, r) = f.postinst(Mode::Apply);
    r.expect("installs");
    assert!(!exists(&f.exec, "/usr/local/kbf/current"));
    assert_eq!(state_json(&f), state);
    assert!(
        out.contains(&format!(
            "the deploy state names {CURRENT} but the link is missing"
        )),
        "{out}"
    );
}

/// Catches a first install that died between the link and the state: the next run must
/// give the state to the link it made, and must not for a link to some other release.
#[test]
fn a_link_to_this_release_without_a_state_is_healed_but_another_is_not() {
    let mut f = fixture("pkg-postinst-heal");
    f.postinst(Mode::Apply).1.expect("first install");
    fs::remove_file(f.exec.at("/var/lib/kbf-deploy/state.json")).expect("rm");
    let (out, r) = f.postinst(Mode::Apply);
    r.unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert!(
        out.contains(&format!(
            "current: already {COMMIT}; writing the deploy state"
        )),
        "{out}"
    );
    let state: serde_json::Value = serde_json::from_str(&state_json(&f)).expect("json");
    assert_eq!(state["current"], COMMIT);

    let mut f = fixture("pkg-postinst-heal-other");
    write(
        &f.exec,
        &format!("/usr/local/kbf/releases/{OTHER}/kbf"),
        "kbf-x",
        0o755,
    );
    std::os::unix::fs::symlink(
        format!("releases/{OTHER}"),
        f.exec.at("/usr/local/kbf/current"),
    )
    .expect("link");
    f.postinst(Mode::Apply).1.expect("installs");
    assert!(!exists(&f.exec, "/var/lib/kbf-deploy/state.json"));
    assert_eq!(
        link_of(&f.exec, "/usr/local/kbf/current"),
        format!("releases/{OTHER}")
    );
}

/// Catches the neighbour cases through the whole verb: another user's range on the first
/// block, a different range in each file, a range of kbf's own in one file only, and
/// ranges in both (no `usermod` at all).
#[test]
fn the_block_avoids_neighbours_in_each_file_and_keeps_the_users_own() {
    let mut f = fixture("pkg-postinst-subid");
    write(&f.exec, "/etc/subuid", "alice:200000:65536\n", 0o644);
    write(
        &f.exec,
        "/etc/subgid",
        "bob:230000:10\n# c\n\nalice:100000:65536\n",
        0o644,
    );
    f.postinst(Mode::Apply).1.expect("installs");
    assert!(
        runs(&f).contains(
            &"run /usr/sbin/usermod --add-subuids 265536-331071 --add-subgids 230010-295545 kbf"
                .to_owned()
        ),
        "{:?}",
        runs(&f)
    );
    assert_eq!(
        read(&f.exec, "/etc/subuid"),
        "alice:200000:65536\nkbf:265536:65536\n"
    );

    // Only the gids are missing.
    let mut f = fixture("pkg-postinst-subid-gid-only");
    write(&f.exec, "/etc/subuid", "kbf:100000:65536\n", 0o644);
    f.postinst(Mode::Apply).1.expect("installs");
    assert!(
        runs(&f).contains(&"run /usr/sbin/usermod --add-subgids 200000-265535 kbf".to_owned()),
        "{:?}",
        runs(&f)
    );

    // Both there: no usermod, and a second kbf range is not added.
    let mut f = fixture("pkg-postinst-subid-both");
    f.subids("kbf:100000:65536\n");
    f.postinst(Mode::Apply).1.expect("installs");
    assert!(
        runs(&f).iter().all(|r| !r.contains("usermod")),
        "{:?}",
        runs(&f)
    );
    assert_eq!(read(&f.exec, "/etc/subuid"), "kbf:100000:65536\n");
}

/// Catches a refusal that comes after a change: each of these leaves the host exactly as
/// it was (no step run, no user made, nothing written), and says why.
#[test]
fn every_refusal_comes_before_the_first_step() {
    type Plant = fn(&mut Fixture);
    let cases: Vec<(&str, Plant, &str)> = vec![
        (
            "no-crun",
            |f| f.world.borrow_mut().crun = None,
            "crun is missing or does not run (/usr/bin/crun: entity not found)",
        ),
        (
            "bad-crun",
            |f| f.world.borrow_mut().crun = Some(1),
            "crun is missing or does not run (/usr/bin/crun exited with exit status: 1)",
        ),
        (
            "no-newgidmap",
            |f| fs::remove_file(f.exec.at(NEWGIDMAP)).expect("rm"),
            "newgidmap is missing or does not run (no /usr/bin/newgidmap)",
        ),
        (
            "not-setuid",
            |f| {
                fs::set_permissions(f.exec.at(NEWUIDMAP), fs::Permissions::from_mode(0o755))
                    .expect("chmod")
            },
            "newuidmap is missing or does not run (/usr/bin/newuidmap is not an executable setuid root)",
        ),
        (
            "small-range",
            |f| write(&f.exec, "/etc/subgid", "kbf:100000:1000\n", 0o644),
            "/etc/subgid gives kbf:100000:1000 only 1000 ids, fewer than the 65536",
        ),
        (
            "bad-subuid",
            |f| write(&f.exec, "/etc/subuid", "alice:1:2\nbroken\n", 0o644),
            "/etc/subuid line 2 is not owner:start:count: \"broken\"",
        ),
        (
            "root-kbf",
            |f| {
                f.with_kbf();
                f.world.borrow_mut().users.insert("kbf".to_owned(), (0, 0));
            },
            "a user named kbf has uid 0",
        ),
        (
            "release-differs",
            |f| {
                write(
                    &f.exec,
                    &format!("/usr/local/kbf/releases/{COMMIT}/kbf"),
                    "other",
                    0o755,
                )
            },
            "differs from the release being installed",
        ),
        (
            "garbage-state",
            |f| write(&f.exec, "/var/lib/kbf-deploy/state.json", "{", 0o600),
            "is not a deploy state",
        ),
        (
            "no-manifest",
            |f| fs::remove_file(f.exec.at("/usr/lib/kbf/release/manifest.json")).expect("rm"),
            "cannot read",
        ),
        (
            "manifest-without-commit",
            |f| {
                write(
                    &f.exec,
                    "/usr/lib/kbf/release/manifest.json",
                    "{\"version\": \"1\"}",
                    0o644,
                )
            },
            "is not a manifest naming the release's commit: missing field `commit`",
        ),
        (
            "short-commit",
            |f| {
                write(
                    &f.exec,
                    "/usr/lib/kbf/release/manifest.json",
                    "{\"commit\": \"abc\"}",
                    0o644,
                )
            },
            "is not a manifest naming the release's commit: \"abc\" is not a commit id",
        ),
        (
            "libexec-link",
            |f| {
                std::os::unix::fs::symlink("/etc/passwd", f.exec.at("/usr/lib/kbf/libexec/passwd"))
                    .expect("link");
            },
            "libexec/passwd is not a regular file or directory the package ships",
        ),
        (
            "release-dir-is-a-file",
            |f| {
                write(
                    &f.exec,
                    &format!("/usr/local/kbf/releases/{COMMIT}"),
                    "x",
                    0o644,
                )
            },
            "is there and is not a directory",
        ),
    ];
    for (name, plant, want) in cases {
        let mut f = fixture(&format!("pkg-postinst-refuse-{name}"));
        plant(&mut f);
        let before = tree(&f);
        let (_, r) = f.postinst(Mode::Apply);
        let e = r.expect_err(name);
        assert!(e.to_string().contains(want), "{name}: {e}");
        assert_eq!(e.exit(), 2, "{name}");
        assert_eq!(f.steps(), Vec::<String>::new(), "{name}");
        assert_eq!(
            f.world.borrow().users.len(),
            usize::from(name == "root-kbf"),
            "{name}"
        );
        // The deploy lock file is the one thing an apply makes before it reads the state.
        let mut after = tree(&f);
        after.retain(|p| p != "run/kbf-deploy.lock");
        assert_eq!(after, before, "{name}");
    }
}

/// Every path under the scratch root.
fn tree(f: &Fixture) -> Vec<String> {
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

/// Catches a kbf user that already exists being made again, and its directories being
/// owned by anyone but it: the ids come from the host's user database.
#[test]
fn an_existing_user_is_kept_and_owns_its_directories() {
    let mut f = fixture("pkg-postinst-existing");
    f.with_kbf();
    f.subids("kbf:100000:65536\n");
    write(&f.exec, "/var/lib/systemd/linger/kbf", "", 0o644);
    let (out, r) = f.postinst(Mode::Apply);
    r.unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert_eq!(runs(&f), Vec::<String>::new());
    let m = me();
    assert!(
        f.steps().contains(&format!(
            "mkdir /var/lib/kbf (0700, owner {}:{})",
            m.uid, m.gid
        )),
        "{:?}",
        f.steps()
    );
    assert!(
        out.contains(&format!("user kbf: uid {} gid {}, kept", m.uid, m.gid)),
        "{out}"
    );
    assert!(out.contains("/etc/subuid: kbf has a range, kept"), "{out}");
}

/// Catches a verb that runs `loginctl` or `systemctl` where no systemd runs (a container
/// build): lingering is then the file `loginctl` would make, and the units are left.
#[test]
fn without_systemd_lingering_is_a_file_and_no_systemctl_runs() {
    let mut f = fixture("pkg-postinst-nosystemd");
    fs::remove_dir(f.exec.at("/run/systemd/system")).expect("rm");
    let (out, r) = f.postinst(Mode::Apply);
    r.unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert!(exists(&f.exec, "/var/lib/systemd/linger/kbf"));
    assert_eq!(mode_of(&f.exec, "/var/lib/systemd/linger/kbf"), 0o644);
    assert!(
        runs(&f)
            .iter()
            .all(|r| !r.contains("loginctl") && !r.contains("systemctl")),
        "{:?}",
        runs(&f)
    );
    assert!(
        out.contains("systemd is not running: units are left as they are"),
        "{out}"
    );
    f.exec.log.clear();
    f.postinst(Mode::Apply).1.expect("again");
    assert_eq!(f.steps(), Vec::<String>::new());
}

/// Catches `kbf-node` enabled for a host that cannot start it (no config, or no token
/// and no earlier enrollment), not enabled for one that can, enabled twice, and a unit
/// reload that is skipped when systemd holds an old copy or run when it does not.
#[test]
fn the_unit_is_enabled_only_when_the_node_can_start() {
    let enabled = |f: &Fixture| {
        runs(f).contains(&"run /usr/bin/systemctl enable kbf-node.service".to_owned())
    };
    for (name, files, want) in [
        ("none", vec![], false),
        ("config-only", vec!["/etc/kbf/kbf.toml"], false),
        ("token-only", vec!["/var/lib/kbf/enroll.token"], false),
        (
            "config-token",
            vec!["/etc/kbf/kbf.toml", "/var/lib/kbf/enroll.token"],
            true,
        ),
        (
            "config-enrolled",
            vec!["/etc/kbf/kbf.toml", "/var/lib/kbf/pki/node.json"],
            true,
        ),
    ] {
        let mut f = fixture(&format!("pkg-postinst-enable-{name}"));
        for file in files {
            write(&f.exec, file, "x", 0o600);
        }
        let (out, r) = f.postinst(Mode::Apply);
        r.unwrap_or_else(|e| panic!("{name}: {e}\n{out}"));
        assert_eq!(enabled(&f), want, "{name}: {:?}", runs(&f));
        assert_eq!(
            out.contains("kbf-node.service is not enabled: it needs /etc/kbf/kbf.toml and /var/lib/kbf/enroll.token (or an enrolled node's /var/lib/kbf/pki/node.json)"),
            !want,
            "{name}\n{out}"
        );
        // Enabling is done once.
        f.exec.log.clear();
        f.postinst(Mode::Apply).1.expect("again");
        assert!(!enabled(&f), "{name}: enabled again");
    }
    // The reload comes before the enable, only when systemd asks for it.
    let mut f = fixture("pkg-postinst-reload");
    for file in ["/etc/kbf/kbf.toml", "/var/lib/kbf/enroll.token"] {
        write(&f.exec, file, "x", 0o600);
    }
    f.world.borrow_mut().need_reload = true;
    f.postinst(Mode::Apply).1.expect("installs");
    let r = runs(&f);
    let at = |s: &str| {
        r.iter()
            .position(|x| x == s)
            .unwrap_or_else(|| panic!("{s} in {r:?}"))
    };
    assert!(
        at("run /usr/bin/systemctl daemon-reload")
            < at("run /usr/bin/systemctl enable kbf-node.service")
    );
    assert!(!f.world.borrow().need_reload);
    f.exec.log.clear();
    f.postinst(Mode::Apply).1.expect("again");
    assert_eq!(f.steps(), Vec::<String>::new());
}

/// Catches an apply by a user who is not root: refused as the other kits' are, before any
/// step, with exit 2.
#[test]
fn an_apply_by_a_user_who_is_not_root_is_refused() {
    let mut f = fixture("pkg-postinst-not-root");
    f.exec.euid = 1000;
    let e = f.postinst(Mode::Apply).1.expect_err("refused");
    assert!(
        matches!(e, PackageError::Host(HostError::NotRoot { euid: 1000 })),
        "{e}"
    );
    assert_eq!(e.exit(), 2);
    assert_eq!(f.steps(), Vec::<String>::new());
}

/// Catches a `useradd` that exits 0 and leaves no user: the directories need its ids, and
/// the verb must say so rather than plan them for nobody.
#[test]
fn a_useradd_that_makes_no_user_is_an_error() {
    let mut f = fixture("pkg-postinst-no-user");
    f.world.borrow_mut().useradd_creates = false;
    let e = f.postinst(Mode::Apply).1.expect_err("no user");
    assert!(e.to_string().contains("no user kbf"), "{e}");
}

/// Catches a step that fails being reported as a refusal: exit 1, and the plan stops at
/// the failed step (`useradd` exits 6 when its group is not there).
#[test]
fn a_failed_step_is_exit_1() {
    let mut f = fixture("pkg-postinst-step-fails");
    f.world.borrow_mut().groupadd_creates = false;
    let e = f.postinst(Mode::Apply).1.expect_err("useradd fails");
    assert!(
        matches!(e, PackageError::Host(HostError::Step { index: 1, .. })),
        "{e}"
    );
    assert_eq!(e.exit(), 1);
    assert_eq!(f.steps().len(), 2, "{:?}", f.steps());
}

/// Catches a `newuidmap` that is setuid but not root's (any user could have replaced it,
/// and the kernel honours the bit for its owner): the same refusal as a missing bit.
#[test]
fn a_setuid_newuidmap_that_root_does_not_own_is_refused() {
    let f = fixture("pkg-postinst-not-root-owned");
    let env = Env {
        root: f.env.root.clone(),
        root_owner: Owner {
            uid: me().uid + 1,
            gid: me().gid,
        },
    };
    let e = check_dependencies(&f.exec, &env).expect_err("not root's");
    assert!(
        e.to_string().contains("newuidmap is missing or does not run (/usr/bin/newuidmap is not an executable setuid root)"),
        "{e}"
    );
}

/// Catches the model of the system tools answering for a command it does not know: the
/// tests would then pass on a step they never meant to allow.
#[test]
fn the_fake_host_refuses_a_command_it_does_not_know() {
    let f = fixture("pkg-postinst-unknown-cmd");
    let e = f
        .exec
        .query(&Cmd::new(tool("/usr/bin/nope")))
        .expect_err("unknown");
    assert!(e.to_string().contains("does not know"), "{e}");
}

/// An `Exec` over a scratch host that damages the release binary right after it is
/// copied, as a short write or a full disk would.
struct Corrupting<'a>(&'a mut kbf_host::testkit::ScratchHost);

impl kbf_host::exec::Exec for Corrupting<'_> {
    fn euid(&self) -> u32 {
        self.0.euid()
    }
    fn inspect(&self, path: &HostPath) -> std::io::Result<Option<kbf_host::exec::Entry>> {
        self.0.inspect(path)
    }
    fn stat(&self, path: &HostPath) -> std::io::Result<Option<kbf_host::exec::Meta>> {
        self.0.stat(path)
    }
    fn query(&self, cmd: &Cmd) -> std::io::Result<std::process::Output> {
        self.0.query(cmd)
    }
    fn http_status(&self, url: &kbf_host::step::HttpUrl) -> std::io::Result<u16> {
        self.0.http_status(url)
    }
    fn apply(&mut self, step: &kbf_host::step::Step) -> std::io::Result<()> {
        self.0.apply(step)?;
        if let kbf_host::step::Step::WriteFile { path, .. } = step
            && path.as_str().ends_with("/kbf")
        {
            fs::write(self.0.at(path.as_str()), "short")?;
        }
        Ok(())
    }
    fn check_binary(&self) -> Result<(), kbf_host::Untrusted> {
        self.0.check_binary()
    }
}

/// Catches a copy that is never read back (the verb would report a release it cannot
/// run): a binary damaged after the copy fails the verb with exit 1, naming both sums,
/// before `current` is trusted and before the deploy state names the release.
#[test]
fn a_damaged_copy_fails_the_apply_before_the_state_is_written() {
    let mut f = fixture("pkg-postinst-damaged");
    let mut out = Vec::new();
    let e = postinst::run(&mut Corrupting(&mut f.exec), &f.env, Mode::Apply, &mut out)
        .expect_err("damaged");
    assert!(matches!(e, PackageError::Copy { .. }), "{e}");
    assert_eq!(e.exit(), 1);
    assert!(!exists(&f.exec, "/var/lib/kbf-deploy/state.json"));
}

/// Catches ownership read from the wrong place, which a scratch root cannot show (it
/// can chown only to its own user, so root's and kbf's ids are one there): on a host
/// where they differ, `/etc/kbf` is root's with the group `kbf`, the node's directories
/// are `kbf`'s, and what the package installs for root to run is root's.
#[test]
fn directories_and_installed_files_have_their_owners() {
    use kbf_host::testkit::{FakeExec, reply};
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(|cmd| {
        Ok(match cmd.program.as_str() {
            kbf_host::user::GETENT => {
                reply(0, "kbf:x:999:998::/var/lib/kbf:/usr/sbin/nologin\n", "")
            }
            _ => reply(0, "998\n", ""),
        })
    });
    let root = Owner { uid: 0, gid: 0 };
    let plan = plan_directories(&exec, root, Mode::Apply, &mut Vec::new()).expect("plans");
    let steps: Vec<String> = plan.steps().iter().map(ToString::to_string).collect();
    assert_eq!(
        steps,
        [
            "mkdir /etc/kbf (0750, owner 0:998)",
            "mkdir /var/lib/kbf (0700, owner 999:998)",
            "mkdir /var/lib/kbf/hot (0700, owner 999:998)",
            "mkdir /var/lib/kbf/meta (0700, owner 999:998)",
            "mkdir /var/lib/kbf/data (0700, owner 999:998)",
        ]
    );
    let f = fixture("pkg-postinst-owners");
    let shipped = release::read_shipped(&f.env.root).expect("reads");
    let trust = release::read_trust(&f.env.root).expect("reads");
    let mut planner = kbf_host::plan::Planner::new(&exec);
    release::plan_trust(&mut planner, root, &trust).expect("plans");
    release::plan_release(
        &exec,
        &mut planner,
        root,
        &shipped,
        &crate::host::state::State::default(),
        &mut Vec::new(),
    )
    .expect("plans");
    let plan = planner.finish();
    let owned: Vec<String> = plan
        .steps()
        .iter()
        .map(ToString::to_string)
        .filter(|s| !s.starts_with("link "))
        .collect();
    assert!(owned.len() > 10, "{owned:?}");
    assert!(owned.iter().all(|s| s.contains("owner 0:0")), "{owned:?}");
    // And the lingering file, where no systemd makes it.
    let plan = account::plan(&exec, false, root, &mut Vec::new()).expect("plans");
    let linger: Vec<String> = plan
        .steps()
        .iter()
        .map(ToString::to_string)
        .filter(|s| s.starts_with("write ") && s.contains("linger"))
        .collect();
    assert_eq!(linger.len(), 1, "{linger:?}");
    assert!(linger[0].contains("owner 0:0"), "{linger:?}");
}
