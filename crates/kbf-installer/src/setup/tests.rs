//! Tests of `setup` on a fake host. Each says the defect it catches; the mutants planted to
//! see them go red are in the pull request.

use kbf_host::testkit::{FakeExec, RUNNER};
use kbf_host::{Entry, HostPath, Owner, Perms, Step};

use super::fake::{FakeOs, user};
use super::plan::{KBF_TOML, TOKEN, release_binary};
use super::validate::Raw;
use super::{SetupError, Start, run};
use crate::layout::Platform;

const SRC: &str = "/root/kbf.token";

fn raw(role: &str) -> Raw {
    Raw {
        frontend_url: "https://farm.example:8443".to_owned(),
        role: role.to_owned(),
        token_file: SRC.to_owned(),
        release_source: "frontend".to_owned(),
        ..Raw::default()
    }
}

fn hp(p: &str) -> HostPath {
    HostPath::new(p).expect("a path")
}

fn put(exec: &mut FakeExec, path: &str, perms: u32, bytes: &str) {
    exec.entries.insert(
        hp(path),
        Entry::File {
            perms: Perms::new(perms),
            owner: RUNNER,
            bytes: bytes.as_bytes().to_vec(),
        },
    );
}

fn host() -> FakeExec {
    let mut exec = FakeExec::new(0);
    put(&mut exec, SRC, 0o600, "tok-abc123\n");
    exec
}

fn released(mut exec: FakeExec, platform: Platform) -> FakeExec {
    put(&mut exec, release_binary(platform), 0o755, "binary");
    exec
}

fn go(os: &FakeOs, exec: &mut FakeExec, raw: &Raw, start: Start) -> Result<String, SetupError> {
    let mut out = Vec::new();
    run(os, exec, raw, start, &mut out)?;
    Ok(String::from_utf8(out).expect("text"))
}

fn go_ok(os: &FakeOs, exec: &mut FakeExec, raw: &Raw) -> String {
    go(os, exec, raw, Start::default()).expect("setup")
}

/// The commands the steps ran, as printed.
fn cmds(exec: &FakeExec) -> Vec<String> {
    exec.log
        .iter()
        .filter_map(|s| match s {
            Step::Run(c) => Some(c.to_string()),
            _ => None,
        })
        .collect()
}

fn write_index(exec: &FakeExec, path: &str) -> Option<usize> {
    exec.log
        .iter()
        .position(|s| matches!(s, Step::WriteFile { path: p, .. } if p.as_str() == path))
}

/// What a finished daemon setup leaves, as the host's facts.
fn settled_daemon() -> FakeOs {
    let mut os = FakeOs::linux();
    os.users.push(user("kbf-fetch", 991));
    os.subuid = "kbf:100000:65536\n".to_owned();
    os.subgid = os.subuid.clone();
    os.lingering.insert("kbf".to_owned());
    for u in ["kbf-daemon.service", "kbf-installer.service"] {
        os.enabled.insert(u.to_owned());
    }
    os
}

const DAEMON_TOML: &str = "# Written by `kbf-installer setup`; run it again to change this file.
role = \"daemon\"

[frontend]
url = \"https://farm.example:8443\"

[enroll]
token_file = \"/etc/kbf/enroll.token\"

[update]
source = \"frontend\"
";

/// Catches a setup that leaves a prerequisite out or in the wrong order: the whole daemon
/// plan on a bare host, step for step. The golden text is the file W2's loader must read.
#[test]
fn a_bare_daemon_host_gets_accounts_dirs_token_config_then_units() {
    let mut exec = released(host(), Platform::Linux);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw("daemon"));
    assert_eq!(
        cmds(&exec),
        [
            "run /usr/sbin/useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin -- kbf-fetch",
            "run /usr/sbin/usermod --add-subuids 100000-165535 -- kbf",
            "run /usr/sbin/usermod --add-subgids 100000-165535 -- kbf",
            "run /usr/bin/loginctl enable-linger kbf",
            "run /usr/bin/systemctl enable --now -- kbf-daemon.service",
            "run /usr/bin/systemctl enable --now -- kbf-installer.service",
        ]
    );
    let dirs: Vec<_> = exec
        .log
        .iter()
        .filter_map(|s| match s {
            Step::Mkdir { path, perms, owner } => Some((path.as_str(), perms.bits(), *owner)),
            _ => None,
        })
        .collect();
    let (root, kbf) = (Owner { uid: 0, gid: 0 }, Owner { uid: 990, gid: 990 });
    assert_eq!(
        dirs,
        [
            ("/etc/kbf", 0o755, Some(root)),
            ("/var/lib/kbf-installer", 0o755, Some(root)),
            ("/var/lib/kbf-installer/orders", 0o700, Some(kbf)),
        ]
    );
    let Some(Entry::File { perms, owner, bytes }) = exec.entries.get(&hp(TOKEN)) else {
        panic!("no token");
    };
    assert_eq!(bytes, b"tok-abc123\n");
    assert_eq!((perms.bits(), *owner), (0o640, Owner { uid: 0, gid: 990 }));
    let Some(Entry::File { perms, bytes, .. }) = exec.entries.get(&hp(KBF_TOML)) else {
        panic!("no kbf.toml");
    };
    assert_eq!(String::from_utf8_lossy(bytes), DAEMON_TOML);
    assert_eq!(perms.bits(), 0o644);
    assert!(!out.contains("tok-abc123"), "the token is never printed: {out}");
}

/// Catches a config written before the token (or before the directories): a crash between
/// the two would leave a `kbf.toml` that passes the units' condition and a token that is not
/// there. The config is the last file, and no unit is enabled before it.
#[test]
fn kbf_toml_is_written_after_the_token_and_before_any_unit() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw("daemon"));
    let (token, toml) = (write_index(&exec, TOKEN), write_index(&exec, KBF_TOML));
    assert!(token < toml, "token {token:?} then config {toml:?}");
    let first_enable = exec
        .log
        .iter()
        .position(|s| matches!(s, Step::Run(c) if c.to_string().contains("systemctl")));
    assert!(toml < first_enable, "config {toml:?} before units {first_enable:?}");
}

/// Catches a setup that is not safe to interrupt: a failure while writing the token must
/// leave no `kbf.toml` behind.
#[test]
fn a_failed_token_write_leaves_no_config() {
    let mut exec = released(host(), Platform::Linux);
    let at = {
        let mut probe = released(host(), Platform::Linux);
        go_ok(&FakeOs::linux(), &mut probe, &raw("daemon"));
        write_index(&probe, TOKEN).expect("token step")
    };
    exec.fail_at = Some(at);
    let err = go(&FakeOs::linux(), &mut exec, &raw("daemon"), Start::default());
    assert!(matches!(err, Err(SetupError::Host(_))), "{err:?}");
    assert!(!exec.entries.contains_key(&hp(KBF_TOML)));
}

/// Catches a re-run that does work: the same flags on a finished host change nothing, and
/// say so. Also catches a plan that compares the file's owner or mode loosely.
#[test]
fn a_rerun_with_the_same_flags_is_a_no_op() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw("daemon"));
    let steps = exec.log.len();
    let out = go_ok(&settled_daemon(), &mut exec, &raw("daemon"));
    assert_eq!(exec.log.len(), steps, "a step ran: {:?}", &exec.log[steps..]);
    assert_eq!(out, "nothing to do\n");
}

/// Catches a re-run that ignores a changed flag, or one that rewrites more than it must:
/// only `kbf.toml` is rewritten (atomically, by the step), the token and units are left, and
/// the operator is told a running kbf keeps the old file.
#[test]
fn a_changed_flag_rewrites_only_the_config() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw("daemon"));
    let steps = exec.log.len();
    let mut changed = raw("daemon");
    changed.frontend_url = "https://other.example".to_owned();
    changed.min_protocol = Some(7);
    let out = go_ok(&settled_daemon(), &mut exec, &changed);
    assert_eq!(exec.log.len(), steps + 1, "{:?}", &exec.log[steps..]);
    let Some(Entry::File { bytes, .. }) = exec.entries.get(&hp(KBF_TOML)) else {
        panic!("no kbf.toml");
    };
    let text = String::from_utf8_lossy(bytes);
    assert!(text.contains("url = \"https://other.example\""), "{text}");
    assert!(text.ends_with("min_protocol = 7\n"), "{text}");
    assert!(out.contains("restarted") || out.contains("restart"), "{out}");
}

/// Catches a setup that runs as an ordinary user: it refuses before it reads or writes
/// anything.
#[test]
fn a_non_root_run_is_refused_before_any_step() {
    let mut os = FakeOs::linux();
    os.euid = 1000;
    let mut exec = host();
    let err = go(&os, &mut exec, &raw("daemon"), Start::default());
    assert!(matches!(err, Err(SetupError::NotRoot { euid: 1000 })), "{err:?}");
    assert!(exec.log.is_empty() && exec.queries.borrow().is_empty());
}

/// Catches root running a binary that anyone else can replace.
#[test]
fn an_untrusted_binary_is_refused() {
    let mut os = FakeOs::linux();
    os.untrusted = true;
    let err = go(&os, &mut host(), &raw("daemon"), Start::default());
    assert!(matches!(err, Err(SetupError::Untrusted(_))), "{err:?}");
}

/// Catches a restart of the frontend nobody asked for (it fails every build in flight):
/// the frontend and the all-in-one are enabled, not started, until `--start-frontend`.
#[test]
fn the_frontend_is_enabled_but_not_started_unasked() {
    for role in ["frontend", "all-in-one"] {
        let mut exec = released(host(), Platform::Linux);
        let out = go_ok(&FakeOs::linux(), &mut exec, &raw(role));
        let enables: Vec<_> = cmds(&exec)
            .into_iter()
            .filter(|c| c.contains("systemctl"))
            .collect();
        assert!(enables.len() >= 2, "{enables:?}");
        for c in enables.iter().filter(|c| !c.contains("kbf-installer")) {
            assert!(c.contains("enable -- "), "{role}: {c}");
        }
        assert!(out.contains("--start-frontend"), "{out}");

        let mut exec = released(host(), Platform::Linux);
        let asked = Start { start_frontend: true, ..Start::default() };
        go(&FakeOs::linux(), &mut exec, &raw(role), asked).expect("setup");
        assert!(
            cmds(&exec).iter().any(|c| c.contains("enable --now -- kbf-")),
            "{role}: {:?}",
            cmds(&exec)
        );
    }
}

/// Catches a unit started where its binary does not exist: with no release installed the
/// units are enabled only, and `--no-start` does the same with one.
#[test]
fn nothing_starts_without_a_release_or_with_no_start() {
    let mut bare = host();
    go_ok(&FakeOs::linux(), &mut bare, &raw("daemon"));
    assert!(bare.log.iter().all(|s| !s.to_string().contains("--now")
        || s.to_string().contains("kbf-installer")));
    let mut exec = released(host(), Platform::Linux);
    let none = Start { no_start: true, ..Start::default() };
    go(&FakeOs::linux(), &mut exec, &raw("daemon"), none).expect("setup");
    assert!(cmds(&exec).iter().all(|c| !c.contains("--now")), "{:?}", cmds(&exec));
}

/// Catches an enable of a unit that is already enabled (a `--now` would then start a unit
/// the operator stopped).
#[test]
fn an_enabled_unit_is_left_alone() {
    let mut exec = released(host(), Platform::Linux);
    let mut os = FakeOs::linux();
    os.enabled.insert("kbf-daemon.service".to_owned());
    go_ok(&os, &mut exec, &raw("daemon"));
    assert!(cmds(&exec).iter().all(|c| !c.contains("kbf-daemon.service")));
}

/// Catches a new subid range that overlaps another user's (a container escape of one
/// account into another), and the grant of a file that already has one.
#[test]
fn a_subid_range_starts_above_every_existing_one() {
    let mut os = FakeOs::linux();
    os.subuid = "alice:100000:65536\nbob:165536:65536\n".to_owned();
    os.subgid = "kbf:200000:65536\n".to_owned();
    let mut exec = released(host(), Platform::Linux);
    go_ok(&os, &mut exec, &raw("storage-and-daemon"));
    let c = cmds(&exec);
    assert!(
        c.contains(&"run /usr/sbin/usermod --add-subuids 231072-296607 -- kbf".to_owned()),
        "{c:?}"
    );
    assert!(c.iter().all(|c| !c.contains("--add-subgids")), "{c:?}");
}

/// A range keyed by the uid counts as the user's own (subuid(5) allows either).
#[test]
fn a_range_keyed_by_uid_is_the_users() {
    let mut os = FakeOs::linux();
    os.subuid = "990:100000:65536\n".to_owned();
    os.subgid = os.subuid.clone();
    let mut exec = released(host(), Platform::Linux);
    go_ok(&os, &mut exec, &raw("daemon"));
    assert!(cmds(&exec).iter().all(|c| !c.contains("--add-sub")));
}

/// Catches rootless prerequisites asked of a host that runs no actions.
#[test]
fn a_storage_host_gets_no_subids_or_lingering() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw("storage"));
    assert!(cmds(&exec).iter().all(|c| !c.contains("usermod") && !c.contains("linger")));
}

/// Catches a setup on a host the package has not prepared: it names the missing user
/// instead of writing files for a service that cannot run.
#[test]
fn a_missing_kbf_user_is_refused_before_any_write() {
    let mut os = FakeOs::linux();
    os.users.clear();
    let mut exec = host();
    let err = go(&os, &mut exec, &raw("daemon"), Start::default());
    assert!(matches!(err, Err(SetupError::NoUser { name: "kbf", .. })), "{err:?}");
    assert!(exec.log.is_empty());
}

/// Catches the bad token files an operator hands over: world-readable (the secret is
/// already out), missing, empty, several words. Each refuses before any write.
#[test]
fn bad_token_files_are_refused() {
    let cases: [(&str, Box<dyn Fn(&mut FakeExec)>); 4] = [
        ("lets others read", Box::new(|e| put(e, SRC, 0o644, "tok\n"))),
        ("no such file", Box::new(|e| {
            e.entries.remove(&hp(SRC));
        })),
        ("empty", Box::new(|e| put(e, SRC, 0o600, " \n"))),
        ("more than one word", Box::new(|e| put(e, SRC, 0o600, "a b\n"))),
    ];
    for (want, plant) in cases {
        let mut exec = host();
        plant(&mut exec);
        let err = go(&FakeOs::linux(), &mut exec, &raw("daemon"), Start::default())
            .expect_err(want)
            .to_string();
        assert!(err.contains(want), "{want}: {err}");
        assert!(exec.log.is_empty(), "{want}");
    }
}

/// Catches a flag check that stops at the first problem, or lets a bad one through.
#[test]
fn every_bad_flag_is_named_and_nothing_is_written() {
    let bad = Raw {
        frontend_url: "http://user@farm.example/x?y#z".to_owned(),
        role: "queen".to_owned(),
        token_file: "rel/token".to_owned(),
        release_source: "url".to_owned(),
        release_url: None,
        min_commit: Some("abc".to_owned()),
        min_protocol: Some(0),
    };
    let mut exec = host();
    let err = go(&FakeOs::linux(), &mut exec, &bad, Start::default()).expect_err("refused");
    let SetupError::Invalid(problems) = err else {
        panic!("{err:?}");
    };
    assert_eq!(problems.len(), 6, "{problems:#?}");
    assert!(exec.log.is_empty());
}

fn problem(mutate: impl Fn(&mut Raw)) -> String {
    let mut r = raw("daemon");
    mutate(&mut r);
    r.validate().expect_err("refused").join("; ")
}

/// Catches URLs that would send the installer, or a mirror fetch, somewhere unintended.
#[test]
fn urls_are_https_with_nothing_hidden() {
    for url in [
        "http://farm.example",
        "https://u:p@farm.example",
        "https://farm.example/path",
        "https://farm.example?q=1",
        "https://farm.example#f",
        "https://farm.example:0",
        "https://farm.example:99999",
        "https://-bad.example",
        "https://fa rm.example",
        "https://",
        "farm.example",
    ] {
        assert!(!problem(|r| r.frontend_url = url.to_owned()).is_empty(), "{url}");
    }
    let ok = |url: &str| {
        let mut r = raw("daemon");
        r.frontend_url = url.to_owned();
        r.validate().expect("valid").frontend_url
    };
    assert_eq!(ok("https://Farm.Example/"), "https://farm.example");
    assert_eq!(ok("https://[::1]:8443"), "https://[::1]:8443");
}

#[test]
fn the_release_source_flags_agree() {
    let url = |s: &str, u: Option<&str>| {
        let mut r = raw("daemon");
        r.release_source = s.to_owned();
        r.release_url = u.map(str::to_owned);
        r.validate()
    };
    assert!(url("url", None).is_err());
    assert!(url("frontend", Some("https://m.example")).is_err());
    assert!(url("ftp", None).is_err());
    assert!(url("url", Some("http://m.example")).is_err());
    assert!(url("url", Some("https://m.example/a/../b")).is_err());
    let ok = url("url", Some("https://M.example:8443/releases/")).expect("valid");
    assert_eq!(
        ok.source,
        super::validate::ReleaseSource::Url("https://m.example:8443/releases".to_owned())
    );
}

/// Catches a mirror written to kbf.toml that the loader would not read back, and a floor
/// that is dropped.
#[test]
fn a_mirror_and_a_floor_reach_the_file() {
    let mut r = raw("daemon");
    r.release_source = "url".to_owned();
    r.release_url = Some("https://m.example/r".to_owned());
    r.min_commit = Some("a".repeat(40));
    r.min_protocol = Some(3);
    let mut exec = host();
    go_ok(&FakeOs::linux(), &mut exec, &r);
    let Some(Entry::File { bytes, .. }) = exec.entries.get(&hp(KBF_TOML)) else {
        panic!("no kbf.toml");
    };
    let text = String::from_utf8_lossy(bytes);
    let tail = format!(
        "[update]\nsource = \"url\"\nurl = \"https://m.example/r\"\nmin_commit = \"{}\"\nmin_protocol = 3\n",
        "a".repeat(40)
    );
    assert!(text.ends_with(&tail), "{text}");
}

/// A Mac is a node: no frontend, no subids, a hidden fetch account made with `dscl` at a
/// free id, the launchd daemon bootstrapped only with a release; and the orders directory
/// and token are root's (the daemon is root there).
#[test]
fn a_mac_node_gets_its_fetch_account_and_a_launchd_daemon() {
    let mut os = FakeOs::mac();
    os.dscl_users = "root 0\n_kbf-pool-0 300\n_other 301\n".to_owned();
    os.dscl_groups = "wheel 0\n_x 302\n".to_owned();
    let mut exec = released(host(), Platform::MacOs);
    go_ok(&os, &mut exec, &raw("daemon"));
    let c = cmds(&exec);
    assert_eq!(c.len(), 7, "{c:?}");
    assert_eq!(c[0], "run /usr/bin/dscl . -create /Groups/_kbf-fetch PrimaryGroupID 303");
    assert_eq!(c[1], "run /usr/bin/dscl . -create /Users/_kbf-fetch UniqueID 303");
    assert_eq!(
        c[6],
        "run /bin/launchctl bootstrap system /Library/LaunchDaemons/com.komira.kbf-daemon.plist"
    );
    let Some(Entry::File { perms, owner, .. }) = exec.entries.get(&hp(TOKEN)) else {
        panic!("no token");
    };
    assert_eq!((perms.bits(), *owner), (0o600, Owner { uid: 0, gid: 0 }));
    let orders = exec.entries.get(&hp("/var/db/kbf-installer/orders"));
    assert!(matches!(orders, Some(Entry::Dir { owner: Owner { uid: 0, .. }, .. })));
}

#[test]
fn a_mac_refuses_the_frontend_and_waits_for_a_release() {
    let err = go(&FakeOs::mac(), &mut host(), &raw("frontend"), Start::default());
    assert!(matches!(err, Err(SetupError::Unsupported(_))), "{err:?}");
    let mut exec = host();
    let out = go_ok(&FakeOs::mac(), &mut exec, &raw("daemon"));
    assert!(cmds(&exec).iter().all(|c| !c.contains("launchctl")));
    assert!(out.contains("no release"), "{out}");
    let mut os = FakeOs::mac();
    os.loaded.insert("com.komira.kbf-daemon".to_owned());
    let mut exec = released(host(), Platform::MacOs);
    go_ok(&os, &mut exec, &raw("daemon"));
    assert!(cmds(&exec).iter().all(|c| !c.contains("launchctl")));
}

#[test]
fn no_free_mac_id_is_a_refusal() {
    let mut os = FakeOs::mac();
    os.dscl_users = (300..400).map(|i| format!("u{i} {i}\n")).collect();
    let err = go(&os, &mut host(), &raw("daemon"), Start::default());
    assert!(matches!(err, Err(SetupError::NoFreeId)), "{err:?}");
}

#[test]
fn subid_space_exhaustion_is_a_refusal() {
    let mut os = FakeOs::linux();
    os.subuid = "x:4294900000:60000\n".to_owned();
    let err = go(&os, &mut host(), &raw("daemon"), Start::default());
    assert!(matches!(err, Err(SetupError::SubIdSpace("subuid"))), "{err:?}");
}
