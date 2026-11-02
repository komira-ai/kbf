//! A hook runs with exactly the rights it declares, under the real systemd, as root.
//!
//! These tests need Linux, systemd and root, so they are `#[ignore]`d and run in the
//! `update-rights-linux` workflow on a GitHub-hosted Ubuntu runner through `cargo xtask sudo-test
//! -p update-rights --test rights_root`. Each raw result goes to `target/update-rights`, which
//! the workflow uploads.
//!
//! Two kinds of test. **Rights**: a hook declared with a narrow set tries to exceed it and must
//! fail, and the same hook with the right widened must succeed, so the failure is the declared
//! limit and not something else. **Probes**: the six assumptions of docs/design/host-updates.md,
//! section 11, that the design rests on; each probe is a test that can fail, and a probe the
//! image cannot run says so by name and fails (`refused`).
//!
//! No test here decides anything by how long it takes: every step ends by its own exit.

#![cfg(target_os = "linux")]

mod root_rig;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use root_rig::{
    Account, Group, PROBE, Rig, SYSTEMCTL, give_to_nobody, make_dir, nobody, ok, record, refused,
    said, said_by,
};
use update_hook::{Class, Outcome, Step};
use update_rights::{Accounts, EtcAccounts, Launch, Policy, Refusal, load_dir};

fn caps_text(mask: u64) -> String {
    format!("eff={mask:016x} amb={mask:016x} bnd={mask:016x}")
}

/// `CAP_NET_ADMIN` is capability 12.
const NET_ADMIN: u64 = 1 << 12;

fn detail(run: &update_rights::StepRun) -> String {
    run.report.detail.clone().unwrap_or_default()
}

/// Catches a hook that can write where it was not given: the whole tree is read-only to it
/// except the paths it declares and its record directory. The directory it tries is one its
/// account owns, so the filesystem and not the permissions is what refuses. The same hook with
/// the path declared writes.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_hook_cannot_write_outside_its_write_paths() {
    let rig = Rig::new("writes");
    let outside = rig.path("ro/out.txt");
    let run = rig.run(&rig.spec(), Step::Prepare, &["write", &outside, "--"]);
    assert_eq!(run.outcome, Outcome::Failure, "{}", said_by(&run));
    assert!(detail(&run).contains("ead-only"), "{}", said_by(&run));
    assert!(!Path::new(&outside).exists());

    let mut widened = rig.spec();
    widened.write.push(rig.path("ro"));
    let run = rig.run(&widened, Step::Prepare, &["write", &outside, "--"]);
    assert_eq!(run.outcome, Outcome::Success, "{}", said_by(&run));
    assert!(Path::new(&outside).exists());

    // What was declared, and the record directory without being declared.
    for inside in ["w/out.txt", "state/out.txt"] {
        let target = rig.path(inside);
        let run = rig.run(&rig.spec(), Step::Cleanup, &["write", &target, "--"]);
        assert_eq!(run.outcome, Outcome::Success, "{inside}: {}", said_by(&run));
    }
    record(
        "rights-write",
        "a write outside write_paths failed with a read-only filesystem; declared, it succeeded; \
         the record directory was writable without being declared",
    );
}

/// Catches a capability held where it was not granted: with no grant the capability sets are
/// empty (effective, ambient and bounding), a grant for one step is that capability and no other
/// in exactly that step, and a step with no entry holds the hook-wide grant.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_hook_holds_the_capabilities_of_its_step_and_no_others() {
    let rig = Rig::new("caps");
    let empty = caps_text(0);
    let run = rig.run(&rig.spec(), Step::Prepare, &["caps", "--"]);
    assert_eq!(detail(&run), empty, "{}", said_by(&run));

    let mut one_step = rig.spec();
    one_step
        .steps
        .push((Step::Prepare, vec!["CAP_NET_ADMIN".to_owned()]));
    let run = rig.run(&one_step, Step::Prepare, &["caps", "--"]);
    assert_eq!(detail(&run), caps_text(NET_ADMIN), "{}", said_by(&run));
    for other in [Step::Cutover, Step::Verify, Step::Rollback, Step::Cleanup] {
        let run = rig.run(&one_step, other, &["caps", "--"]);
        assert_eq!(detail(&run), empty, "{other}: {}", said_by(&run));
    }

    let mut wide = rig.spec();
    wide.caps.push("CAP_NET_ADMIN".to_owned());
    wide.steps.push((Step::Cleanup, Vec::new()));
    let run = rig.run(&wide, Step::Verify, &["caps", "--"]);
    assert_eq!(detail(&run), caps_text(NET_ADMIN), "{}", said_by(&run));
    let run = rig.run(&wide, Step::Cleanup, &["caps", "--"]);
    assert_eq!(
        detail(&run),
        empty,
        "an entry with no capability holds none: {}",
        said_by(&run)
    );
    record(
        "rights-caps",
        "the capability sets of a step are exactly its declaration",
    );
}

/// Where `nft` is, or the probe is refused: the workflow installs it.
fn nft() -> &'static str {
    ["/usr/sbin/nft", "/sbin/nft", "/usr/bin/nft"]
        .into_iter()
        .find(|path| Path::new(path).exists())
        .unwrap_or_else(|| refused(1, "no nft on the image"))
}

/// Removes the table a test makes, whatever the test ends in.
struct Table(String);

impl Drop for Table {
    fn drop(&mut self) {
        let _ = Command::new(nft())
            .args(["delete", "table", "inet", &self.0])
            .output();
    }
}

/// Probe 1, and the rights test for a capability: a non-root hook account holding
/// `CAP_NET_ADMIN` as an ambient capability under `NoNewPrivileges=yes`, `RestrictNamespaces=yes`
/// and `ProtectKernelTunables=yes` runs `nft` in the host network namespace and changes the
/// ruleset; without the capability the same `nft` call fails, in the steps that do not hold it.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn probe_1_ambient_net_admin_lets_a_non_root_hook_run_nft() {
    let rig = Rig::new("nft");
    let nft = nft();
    let table = format!("urprobe{}", std::process::id());
    let _guard = Table(table.clone());
    let mut spec = rig.spec();
    spec.families = vec!["AF_UNIX".to_owned(), "AF_NETLINK".to_owned()];
    spec.steps
        .push((Step::Prepare, vec!["CAP_NET_ADMIN".to_owned()]));

    // Without the capability, in a step that does not hold it.
    let denied = rig.run(
        &spec,
        Step::Verify,
        &["run", nft, "add", "table", "inet", &table, "--"],
    );
    assert_eq!(denied.outcome, Outcome::Failure, "{}", said_by(&denied));
    assert!(
        detail(&denied).contains("not permitted"),
        "{}",
        said_by(&denied)
    );
    assert!(
        !Command::new(nft)
            .args(["list", "table", "inet", &table])
            .status()
            .unwrap()
            .success()
    );

    // With it, in the step that does.
    let added = rig.run(
        &spec,
        Step::Prepare,
        &["run", nft, "add", "table", "inet", &table, "--"],
    );
    assert_eq!(added.outcome, Outcome::Success, "{}", said_by(&added));
    ok(nft, &["list", "table", "inet", &table]);
    let removed = rig.run(
        &spec,
        Step::Prepare,
        &["run", nft, "delete", "table", "inet", &table, "--"],
    );
    assert_eq!(removed.outcome, Outcome::Success, "{}", said_by(&removed));
    assert!(
        !Command::new(nft)
            .args(["list", "table", "inet", &table])
            .status()
            .unwrap()
            .success()
    );
    record(
        "probe-1",
        "PROBE 1 RAN: an ambient CAP_NET_ADMIN let a non-root hook (nobody) add and delete an nft table \
         under NoNewPrivileges, RestrictNamespaces and ProtectKernelTunables; the same call without it failed",
    );
}

/// Catches a socket family opened that was not declared: with only `AF_UNIX` an internet socket
/// is refused by the kernel, with it declared it opens, and with no family declared not even a
/// unix socket opens.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_hook_cannot_open_a_socket_family_it_does_not_have() {
    let rig = Rig::new("sockets");
    let spec = rig.spec();
    let run = rig.run(&spec, Step::Prepare, &["socket", "inet", "--"]);
    assert_eq!(run.outcome, Outcome::Failure, "{}", said_by(&run));
    assert!(detail(&run).contains("not supported"), "{}", said_by(&run));
    let run = rig.run(&spec, Step::Prepare, &["socket", "unix", "--"]);
    assert_eq!(run.outcome, Outcome::Success, "{}", said_by(&run));

    let mut inet = rig.spec();
    inet.families.push("AF_INET".to_owned());
    let run = rig.run(&inet, Step::Prepare, &["socket", "inet", "--"]);
    assert_eq!(run.outcome, Outcome::Success, "{}", said_by(&run));
    let run = rig.run(&inet, Step::Prepare, &["socket", "inet6", "--"]);
    assert_eq!(run.outcome, Outcome::Failure, "{}", said_by(&run));

    let mut none = rig.spec();
    none.families.clear();
    let run = rig.run(&none, Step::Prepare, &["socket", "unix", "--"]);
    assert_eq!(
        run.outcome,
        Outcome::Failure,
        "no families: {}",
        said_by(&run)
    );
    assert!(detail(&run).contains("not supported"), "{}", said_by(&run));
    record(
        "rights-families",
        "a socket family not declared was refused by the kernel; declared, it opened",
    );
}

/// Catches an environment that is inherited: the hook sees the variables the declaration lists
/// and not others.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_hook_sees_the_declared_environment_and_nothing_of_the_updaters() {
    let rig = Rig::new("env");
    let mut spec = rig.spec();
    spec.env
        .push(("UPDATER_PROBE".to_owned(), "declared".to_owned()));
    let run = rig.run(&spec, Step::Prepare, &["env", "UPDATER_PROBE", "--"]);
    assert_eq!(detail(&run), "declared", "{}", said_by(&run));
    // The harness holds the two secrets in its own environment, so each "unset" below is an answer
    // and not an absence of a question: the spawner clears the environment of `systemd-run`.
    // SAFETY: the root job runs one test at a time (`--test-threads=1`), so nothing else reads or
    // writes the environment while it is changed.
    unsafe {
        std::env::set_var("HARNESS_SECRET", "planted");
        std::env::set_var("GITHUB_TOKEN", "planted");
    }
    // And a `systemd-run` started with a secret it was not asked to drop: its unit does not have it.
    let command = rig.command(&spec, Step::Prepare, &["env", "HARNESS_SECRET", "--"]);
    let done = Command::new("/usr/bin/systemd-run")
        .args(&command.args)
        .output()
        .unwrap();
    let seen = update_hook::Report::read(&String::from_utf8_lossy(&done.stdout));
    assert_eq!(
        seen.detail.as_deref(),
        Some("unset"),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
    for absent in [
        "HARNESS_SECRET",
        "SUDO_USER",
        "SUDO_COMMAND",
        "GITHUB_TOKEN",
    ] {
        let run = rig.run(&spec, Step::Prepare, &["env", absent, "--"]);
        assert_eq!(detail(&run), "unset", "{absent}: {}", said_by(&run));
    }
}

/// Catches the arguments of a step not reaching the hook through `systemd-run`: the hook is run
/// with the declared arguments, then the step, the staged release for `prepare` and the params
/// file.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn the_hook_is_given_the_step_and_its_arguments() {
    let rig = Rig::new("args");
    let params = rig.dir.join("params.toml");
    fs::write(&params, "a = 1\n").unwrap();
    let done = rig.raw(&rig.spec(), Step::Prepare, &["args", "--"], Some(&params));
    let out = String::from_utf8_lossy(&done.stdout).into_owned();
    let release = rig.path("release");
    let expected = format!("prepare {release} --params {}", params.display());
    assert!(
        out.contains(&expected),
        "{out}\n{}",
        String::from_utf8_lossy(&done.stderr)
    );
    let done = rig.raw(&rig.spec(), Step::Verify, &["args", "--"], None);
    assert!(String::from_utf8_lossy(&done.stdout).contains("\"detail\":\"verify\""));
}

/// Probe 3: `systemd-run --wait --collect --pipe` as a non-root `User=` preserves the exit code
/// and the last line of standard output, whatever noise comes before it.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn probe_3_systemd_run_pipe_keeps_the_exit_code_and_the_last_line() {
    let rig = Rig::new("pipe");
    let spec = rig.spec();
    for (code, outcome) in [
        (0, Outcome::Success),
        (1, Outcome::Failure),
        (2, Outcome::Failure),
        (42, Outcome::Failure),
        (75, Outcome::Deferred),
        (76, Outcome::Halt),
    ] {
        let text = code.to_string();
        let args = ["exit", text.as_str(), "--"];
        let raw = rig.raw(&spec, Step::Cutover, &args, None);
        assert_eq!(
            raw.code,
            Some(code),
            "exit {code}: {}",
            String::from_utf8_lossy(&raw.stderr)
        );
        let stdout = String::from_utf8_lossy(&raw.stdout).into_owned();
        let last = stdout.lines().last().unwrap_or_default();
        assert!(
            last.contains(&format!("\"detail\":\"exit {code}\"")),
            "{stdout:?}"
        );
        assert!(stdout.starts_with("a line of noise"), "{stdout:?}");
        let run = rig.run(&spec, Step::Cutover, &args);
        assert_eq!(run.outcome, outcome, "exit {code}");
        assert_eq!(
            run.report.detail.as_deref(),
            Some(format!("exit {code}").as_str())
        );
        assert_eq!(run.report.class, Class::Release);
    }
    let ids = rig.run(&spec, Step::Cutover, &["ids", "--"]);
    let (uid, _) = nobody();
    assert!(
        detail(&ids).contains(&format!("uid={uid}")),
        "the hook did not run as nobody: {}",
        said_by(&ids)
    );
    record(
        "probe-3",
        "PROBE 3 RAN: systemd-run --wait --collect --pipe as User=nobody returned the exit codes 0, 1, 2, 42, 75 and 76 \
         and the last line of standard output of each",
    );
}

/// Probe 4: a binary is executed through a link in a directory that only a group can read: the
/// kernel checks traversal for the account that executes it, by the group, at `execve`. The
/// directory is owned by the hook account and its group, the account that executes is a member
/// of that group, and the account that is not a member is refused.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn probe_4_exec_through_a_link_in_a_group_readable_directory() {
    let rig = Rig::new("slots");
    let pid = std::process::id();
    let group = Group::new(&format!("urp4g{pid}"));
    let hook = Account::new(&format!("urp4h{pid}"), &[]);
    let service = Account::new(&format!("urp4s{pid}"), &[&group.name]);
    let outsider = Account::new(&format!("urp4o{pid}"), &[]);
    let slots = rig.dir.join("slots");
    make_dir(&slots, 0o750);
    let hook_ids = EtcAccounts::system().unwrap().user(&hook.name).unwrap();
    let group_id = EtcAccounts::system().unwrap().group(&group.name).unwrap();
    chown(&slots, Some(hook_ids.uid), Some(group_id)).unwrap();
    symlink(rig.dir.join("release/probe"), slots.join("current")).unwrap();

    let mut spec = rig.spec();
    spec.release = Some(slots);
    spec.binary = "current".to_owned();
    spec.user = service.name.clone();
    spec.group = service.name.clone();
    let run = rig.run(&spec, Step::Verify, &["ok", "--"]);
    assert_eq!(run.outcome, Outcome::Success, "{}", said_by(&run));
    assert_eq!(detail(&run), "ran");

    spec.user = outsider.name.clone();
    spec.group = outsider.name.clone();
    let run = rig.run(&spec, Step::Verify, &["ok", "--"]);
    assert_eq!(
        run.outcome,
        Outcome::Failure,
        "an account outside the group executed it: {}",
        said_by(&run)
    );
    record(
        "probe-4",
        "PROBE 4 RAN: an account in the group of a 0750 directory executed a binary through a link in it; \
         an account outside the group was refused",
    );
}

/// Probe 5: `/proc/locks` is readable by an unprivileged hook, and names the process that holds
/// a lock on a file: the harness takes the lock and the hook, as `nobody`, finds its own process
/// number against the file's device and inode.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn probe_5_proc_locks_is_readable_by_an_unprivileged_hook() {
    use rustix::fs::{FlockOperation, flock, major, minor};
    let rig = Rig::new("locks");
    let locked = fs::File::create(rig.dir.join("w/locked")).unwrap();
    flock(&locked, FlockOperation::NonBlockingLockExclusive).unwrap();
    let meta = locked.metadata().unwrap();
    let key = format!(
        "{:02x}:{:02x}:{}",
        major(meta.dev()),
        minor(meta.dev()),
        meta.ino()
    );
    let raw = rig.raw(&rig.spec(), Step::Verify, &["locks", "--"], None);
    assert_eq!(
        raw.code,
        Some(0),
        "{}",
        String::from_utf8_lossy(&raw.stderr)
    );
    let table = String::from_utf8_lossy(&raw.stdout).into_owned();
    let line = table
        .lines()
        .find(|line| line.contains(&key))
        .unwrap_or_else(|| panic!("no lock on {key} in\n{table}"));
    let holder = std::process::id().to_string();
    assert!(
        line.split_whitespace().any(|word| word == holder),
        "the holder {holder} is not named in {line:?}"
    );
    let ids = rig.run(&rig.spec(), Step::Verify, &["ids", "--"]);
    assert!(
        !detail(&ids).contains("uid=0"),
        "the reader was root: {}",
        said_by(&ids)
    );
    record(
        "probe-5",
        &format!(
            "PROBE 5 RAN: /proc/locks read by a hook as nobody named the holder {holder} of {key}: {line}"
        ),
    );
}

/// Probe 6: the js-agent hook's claim. A hook that runs as root with no capabilities (an empty
/// bounding set) writes only where it owns, because it has no override of file permissions, and
/// still stops a unit, because systemd decides by its uid.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn probe_6_root_with_no_capabilities_writes_only_where_it_owns_and_keeps_systemds_authority() {
    let rig = Rig::new("rootless");
    let pid = std::process::id();
    let own = rig.dir.join("own");
    make_dir(&own, 0o755);
    let foreign = own.join("foreign.txt");
    fs::write(&foreign, "someone else's\n").unwrap();
    give_to_nobody(&foreign);
    fs::set_permissions(&foreign, fs::Permissions::from_mode(0o644)).unwrap();
    let nobodys_dir = rig.dir.join("nobodys");
    make_dir(&nobodys_dir, 0o755);
    give_to_nobody(&nobodys_dir);

    let mut spec = rig.spec();
    spec.user = "root".to_owned();
    spec.group = "root".to_owned();
    spec.write = vec![own.display().to_string(), nobodys_dir.display().to_string()];
    let run = rig.run(&spec, Step::Cutover, &["caps", "--"]);
    assert_eq!(
        detail(&run),
        caps_text(0),
        "root kept a capability: {}",
        said_by(&run)
    );
    let ids = rig.run(&spec, Step::Cutover, &["ids", "--"]);
    assert!(
        detail(&ids).starts_with("uid=0\t0\t0\t0"),
        "not root: {}",
        said_by(&ids)
    );

    // It writes what it owns.
    let mine = own.join("mine.txt").display().to_string();
    let run = rig.run(&spec, Step::Cutover, &["write", &mine, "--"]);
    assert_eq!(run.outcome, Outcome::Success, "{}", said_by(&run));
    // It cannot write a file of another account, nor create one in another account's directory.
    let theirs = foreign.display().to_string();
    let run = rig.run(&spec, Step::Cutover, &["write", &theirs, "--"]);
    assert_eq!(run.outcome, Outcome::Failure, "{}", said_by(&run));
    assert!(
        detail(&run).contains("ermission denied"),
        "{}",
        said_by(&run)
    );
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "someone else's\n");
    let created = nobodys_dir.join("created.txt").display().to_string();
    let run = rig.run(&spec, Step::Cutover, &["write", &created, "--"]);
    assert_eq!(run.outcome, Outcome::Failure, "{}", said_by(&run));
    assert!(!Path::new(&created).exists());

    // And still stops a unit, by its uid.
    let unit = format!("urp6victim{pid}");
    ok(
        "/usr/bin/systemd-run",
        &[&format!("--unit={unit}"), "--collect", PROBE, "park", "--"],
    );
    assert_eq!(said(SYSTEMCTL, &["is-active", &unit]), "active");
    let stop = rig.run(
        &spec,
        Step::Cutover,
        &[
            "run",
            SYSTEMCTL,
            "--no-ask-password",
            "stop",
            &format!("{unit}.service"),
            "--",
        ],
    );
    let state = said(SYSTEMCTL, &["is-active", &unit]);
    let _ = Command::new(SYSTEMCTL).args(["stop", &unit]).output();
    assert_eq!(stop.outcome, Outcome::Success, "{}", said_by(&stop));
    assert_eq!(state, "inactive");
    record(
        "probe-6",
        "PROBE 6 RAN: a hook as root with an empty capability set wrote a file it owns, could not write a file of \
         nobody (Permission denied) nor create one in nobody's directory, and stopped a unit through systemd by its uid",
    );
}

/// Removes what the polkit probe makes, whatever the test ends in.
struct PolkitProbe {
    units: Vec<String>,
    files: Vec<String>,
}

impl Drop for PolkitProbe {
    fn drop(&mut self) {
        for unit in &self.units {
            let _ = Command::new(SYSTEMCTL)
                .args(["unmask", "--runtime", unit])
                .output();
            let _ = Command::new(SYSTEMCTL).args(["stop", unit]).output();
        }
        for file in &self.files {
            let _ = fs::remove_file(file);
        }
        let _ = Command::new(SYSTEMCTL).arg("daemon-reload").output();
        let _ = Command::new(SYSTEMCTL)
            .args(["restart", "polkit.service"])
            .output();
    }
}

/// Probe 2: `systemctl mask --runtime` followed by `stop` on an active templated instance, driven
/// through polkit as a hook account that is not root. The rule lets the account manage the unit
/// files at large (the action carries no unit) and manage the one listed instance (the action
/// names it); another instance of the same template is refused.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn probe_2_mask_runtime_then_stop_an_active_instance_through_polkit() {
    let rig = Rig::new("polkit");
    if !["/usr/lib/polkit-1/polkitd", "/usr/libexec/polkitd"]
        .iter()
        .any(|p| Path::new(p).exists())
    {
        refused(2, "polkitd is not installed");
    }
    let pid = std::process::id();
    let account = Account::new(&format!("urp2h{pid}"), &[]);
    let template = "/run/systemd/system/probe-polkit@.service".to_owned();
    let rule = "/etc/polkit-1/rules.d/49-probe-polkit.rules".to_owned();
    let guard = PolkitProbe {
        units: vec![
            "probe-polkit@a.service".to_owned(),
            "probe-polkit@b.service".to_owned(),
        ],
        files: vec![template.clone(), rule.clone()],
    };
    fs::write(
        &template,
        format!(
            "[Service]\nExecStart={} park --\n",
            rig.path("release/probe")
        ),
    )
    .unwrap();
    make_dir(Path::new("/etc/polkit-1/rules.d"), 0o755);
    fs::write(
        &rule,
        format!(
            "polkit.addRule(function(action, subject) {{\n\
             \x20 if (subject.user != \"{user}\") return polkit.Result.NOT_HANDLED;\n\
             \x20 if (action.id == \"org.freedesktop.systemd1.manage-unit-files\") return polkit.Result.YES;\n\
             \x20 if (action.id == \"org.freedesktop.systemd1.manage-units\" &&\n\
             \x20     action.lookup(\"unit\") == \"probe-polkit@a.service\") return polkit.Result.YES;\n\
             \x20 return polkit.Result.NOT_HANDLED;\n}});\n",
            user = account.name
        ),
    )
    .unwrap();
    ok(SYSTEMCTL, &["daemon-reload"]);
    // The rules are read when polkit starts: restart it, so no wait is needed to know they are in.
    let _ = Command::new(SYSTEMCTL)
        .args(["reset-failed", "polkit.service"])
        .output();
    ok(SYSTEMCTL, &["restart", "polkit.service"]);
    ok(
        SYSTEMCTL,
        &[
            "start",
            "probe-polkit@a.service",
            "probe-polkit@b.service",
            "probe-polkit@c.service",
        ],
    );
    let started: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|i| {
            said(
                SYSTEMCTL,
                &["is-active", &format!("probe-polkit@{i}.service")],
            )
        })
        .collect();
    assert_eq!(started, ["active", "active", "active"]);
    assert_eq!(
        said(SYSTEMCTL, &["is-active", "probe-polkit@a.service"]),
        "active"
    );

    let mut spec = rig.spec();
    spec.user = account.name.clone();
    spec.group = account.name.clone();
    spec.units = vec!["probe-polkit@a.service".to_owned()];
    let systemctl = |verb: &[&str], unit: &str| {
        let mut args = vec!["run", SYSTEMCTL, "--no-ask-password"];
        args.extend(verb);
        args.push(unit);
        args.push("--");
        rig.run(&spec, Step::Cutover, &args)
    };

    let masked = systemctl(&["mask", "--runtime"], "probe-polkit@a.service");
    assert_eq!(masked.outcome, Outcome::Success, "{}", said_by(&masked));
    assert_eq!(
        said(SYSTEMCTL, &["is-enabled", "probe-polkit@a.service"]),
        "masked-runtime"
    );
    assert_eq!(
        said(SYSTEMCTL, &["is-active", "probe-polkit@a.service"]),
        "active",
        "mask must not stop it"
    );
    let stopped = systemctl(&["stop"], "probe-polkit@a.service");
    assert_eq!(stopped.outcome, Outcome::Success, "{}", said_by(&stopped));
    assert_eq!(
        said(SYSTEMCTL, &["is-active", "probe-polkit@a.service"]),
        "inactive"
    );

    // An account with no rule at all is refused by the default: polkit is what decides.
    let stranger = Account::new(&format!("urp2s{pid}"), &[]);
    let mut stranger_spec = rig.spec();
    stranger_spec.user = stranger.name.clone();
    stranger_spec.group = stranger.name.clone();
    let stranger_stop = rig.run(
        &stranger_spec,
        Step::Cutover,
        &[
            "run",
            SYSTEMCTL,
            "--no-ask-password",
            "stop",
            "probe-polkit@c.service",
            "--",
        ],
    );
    assert_eq!(
        stranger_stop.outcome,
        Outcome::Failure,
        "{}",
        said_by(&stranger_stop)
    );
    assert!(
        detail(&stranger_stop).contains("authentication"),
        "{}",
        said_by(&stranger_stop)
    );
    assert_eq!(
        said(SYSTEMCTL, &["is-active", "probe-polkit@c.service"]),
        "active"
    );
    drop(guard);
    record(
        "probe-2",
        "PROBE 2 RAN: through polkit a non-root hook account masked --runtime an active templated instance, \
         left it running, then stopped it; an account with no rule was refused",
    );
}

/// Catches a declaration or a directory of declarations trusted by something other than its
/// owner, under the real root: a file owned by another account, a file others can write, and a
/// directory owned by another account are refused, each only for what it is.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_declaration_not_owned_by_root_or_writable_by_others_is_refused() {
    let rig = Rig::new("owner");
    let dir = rig.dir.join("updater.d");
    make_dir(&dir, 0o755);
    let spec = rig.spec();
    for (name, mode, owner) in [
        ("good", 0o644, false),
        ("loose", 0o664, false),
        ("other", 0o644, true),
    ] {
        let text = rig.toml(name, &spec, &["ok", "--"]).replace(
            &format!("state_dir = \"{}\"", rig.path("state")),
            &format!("state_dir = \"{}\"", rig.path(&format!("state-{name}"))),
        );
        let path = dir.join(format!("{name}.toml"));
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        if owner {
            give_to_nobody(&path);
        }
    }
    let accounts = EtcAccounts::system().unwrap();
    let policy = Policy {
        trusted_uid: 0,
        accounts: &accounts,
    };
    let loaded = load_dir(&dir, &policy).unwrap();
    let refused: Vec<(&str, &Refusal)> = loaded
        .refused
        .iter()
        .map(|r| (r.service.as_str(), &r.reason))
        .collect();
    assert_eq!(
        loaded.services.keys().collect::<Vec<_>>(),
        ["good"],
        "{refused:?}"
    );
    assert_eq!(refused.len(), 2, "{refused:?}");
    for (service, reason) in refused {
        match reason {
            Refusal::File(why) => {
                let word = if service == "other" {
                    "owned by uid"
                } else {
                    "write"
                };
                assert!(why.contains(word), "{service}: {why}");
            }
            other => panic!("{service}: {other:?}"),
        }
    }
    // A directory of another account: every file is refused for the directory.
    give_to_nobody(&dir);
    let loaded = load_dir(&dir, &policy).unwrap();
    assert!(loaded.services.is_empty());
    assert_eq!(loaded.refused.len(), 3);
    assert!(
        loaded
            .refused
            .iter()
            .all(|r| matches!(r.reason, Refusal::DirectoryUnsafe(_)))
    );
    record(
        "rights-owner",
        "declarations not owned by root, or writable by others, were refused; so was a directory not owned by root",
    );
}

/// Catches the record directory check being made against paths the test runner can see only:
/// with a service account that owns the directory the loader refuses, with root's it does not.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_state_directory_the_service_can_write_is_refused_under_the_real_accounts() {
    let rig = Rig::new("statedir");
    let dir = rig.dir.join("updater.d");
    make_dir(&dir, 0o755);
    let spec = rig.spec();
    let (nobody_uid, _) = nobody();
    // `nobody` as the service and `daemon` as the hook; the record directory is nobody's.
    let text = rig
        .toml("probe-statedir", &spec, &["ok", "--"])
        .replace("uid = \"daemon\"", "uid = \"nobody\"")
        .replace("user = \"nobody\"", "user = \"daemon\"")
        .replace("group = \"nogroup\"", "group = \"daemon\"");
    let path = dir.join("probe-statedir.toml");
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let accounts = EtcAccounts::system().unwrap();
    let policy = Policy {
        trusted_uid: 0,
        accounts: &accounts,
    };
    let loaded = load_dir(&dir, &policy).unwrap();
    assert_eq!(
        fs::metadata(rig.dir.join("state")).unwrap().uid(),
        nobody_uid
    );
    match &loaded.refused[..] {
        [one] => assert!(
            matches!(one.reason, Refusal::StateDirWritable { .. }),
            "{:?}",
            one.reason
        ),
        other => panic!("{other:?}"),
    }
}

/// The answer of polkit (`pkcheck`: 0 authorized, 1 not, 2 needs authentication) for the
/// `manage-units` action on three units, for an account owning a process, under a rule.
fn manage_units_answers(rig: &Rig, rule_body: &str, tag: &str) -> Vec<Option<i32>> {
    use std::os::unix::process::CommandExt;
    let account = Account::new(&format!("urpk{tag}{}", std::process::id()), &[]);
    let rule = format!("/etc/polkit-1/rules.d/48-probe-{tag}.rules");
    let _guard = PolkitProbe {
        units: Vec::new(),
        files: vec![rule.clone()],
    };
    let ids = EtcAccounts::system().unwrap().user(&account.name).unwrap();
    let mut child = Command::new(rig.path("release/probe"))
        .args(["park", "--"])
        .uid(ids.uid)
        .gid(ids.gid)
        .spawn()
        .unwrap();
    let pid = child.id().to_string();
    let body = format!(
        "polkit.addRule(function(action, subject) {{\n  if (subject.user != \"{user}\") return polkit.Result.NOT_HANDLED;\n  {rule_body}\n  return polkit.Result.NOT_HANDLED;\n}});\n",
        user = account.name
    );
    fs::write(&rule, body).unwrap();
    let _ = Command::new(SYSTEMCTL)
        .args(["reset-failed", "polkit.service"])
        .output();
    ok(SYSTEMCTL, &["restart", "polkit.service"]);
    let answers = ["a", "b", "c"]
        .iter()
        .map(|unit| {
            Command::new("/usr/bin/pkcheck")
                .args([
                    "--action-id",
                    "org.freedesktop.systemd1.manage-units",
                    "--process",
                    &pid,
                ])
                .args(["--detail", "unit", &format!("probe-polkit@{unit}.service")])
                .output()
                .unwrap()
                .status
                .code()
        })
        .collect();
    child.kill().unwrap();
    child.wait().unwrap();
    answers
}

const UNITS_RULE: &str = "if (action.id == \"org.freedesktop.systemd1.manage-units\" && action.lookup(\"unit\") == \"probe-polkit@a.service\") return polkit.Result.YES;";
const FILES_RULE: &str =
    "if (action.id == \"org.freedesktop.systemd1.manage-unit-files\") return polkit.Result.YES;";

/// Probe 2, the scope: a rule that grants the `manage-units` action for one unit authorizes that
/// unit and no other; the other instances need authentication.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn probe_2_a_rule_for_manage_units_scopes_the_account_to_the_listed_unit() {
    if !["/usr/lib/polkit-1/polkitd", "/usr/libexec/polkitd"]
        .iter()
        .any(|p| Path::new(p).exists())
    {
        refused(2, "polkitd is not installed");
    }
    let rig = Rig::new("pkscope");
    let answers = manage_units_answers(&rig, UNITS_RULE, "scope");
    record(
        "probe-2-scope",
        &format!("manage-units only, units a b c: {answers:?}"),
    );
    assert_eq!(answers, [Some(0), Some(2), Some(2)]);
}

/// Probe 2, what the scope costs: `mask --runtime` needs the `manage-unit-files` action, and the
/// policy of systemd says that action implies `manage-units`, so an account that holds it is
/// authorized to manage every unit, not only the listed ones. The declaration's `units` list is
/// therefore not a limit that a polkit rule can enforce for a hook that masks.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn probe_2_granting_manage_unit_files_authorizes_every_unit() {
    if !["/usr/lib/polkit-1/polkitd", "/usr/libexec/polkitd"]
        .iter()
        .any(|p| Path::new(p).exists())
    {
        refused(2, "polkitd is not installed");
    }
    let rig = Rig::new("pkfiles");
    let answers = manage_units_answers(&rig, &format!("{UNITS_RULE} {FILES_RULE}"), "files");
    record(
        "probe-2-files",
        &format!("manage-units for a, manage-unit-files for all, units a b c: {answers:?}"),
    );
    assert_eq!(answers, [Some(0), Some(0), Some(0)]);
}

/// What a test plants outside the rig and removes when it ends.
struct Planted(Vec<std::path::PathBuf>);

impl Drop for Planted {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_dir_all(path);
            let _ = fs::remove_file(path);
        }
    }
}

/// Runs the probe as `nobody` without any of the sandbox, to show what the sandbox took away.
fn as_nobody(rig: &Rig, args: &[&str]) -> update_hook::Report {
    use std::os::unix::process::CommandExt;
    let (uid, gid) = nobody();
    let done = Command::new(rig.path("release/probe"))
        .args(args)
        .uid(uid)
        .gid(gid)
        .output()
        .unwrap();
    update_hook::Report::read(&String::from_utf8_lossy(&done.stdout))
}

/// Catches `RuntimeMaxSec=` being dropped: a step that never ends on its own passes its bound and
/// systemd kills it, which the updater reads as a failure with no report. The bound is one second
/// and nothing here waits for it: the step ends when systemd ends it. A step that ends within the
/// bound is not touched.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_step_that_passes_its_bound_is_killed_and_read_as_a_failure() {
    let rig = Rig::new("bound");
    let mut spec = rig.spec();
    spec.timeout_secs = 1;
    let quick = rig.run(&spec, Step::Verify, &["ok", "--"]);
    assert_eq!(quick.outcome, Outcome::Success, "{}", said_by(&quick));
    let stuck = rig.run(&spec, Step::Verify, &["park", "--"]);
    assert_eq!(stuck.outcome, Outcome::Failure, "{}", said_by(&stuck));
    assert_eq!(
        stuck.report,
        update_hook::Report::default(),
        "a killed step wrote no report"
    );
    record(
        "rights-bound",
        "a step that never ends was killed at its RuntimeMaxSec and read as a failure with no report",
    );
}

/// Catches `NoNewPrivileges=` being dropped: a setuid-root program, started as the hook account,
/// keeps the hook's identity. Started the same way outside the sandbox it becomes root.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_setuid_program_does_not_gain_privilege_in_a_step() {
    let rig = Rig::new("nnp");
    let suid = rig.dir.join("suid");
    make_dir(&suid, 0o755);
    fs::copy(PROBE, suid.join("probe")).unwrap();
    fs::set_permissions(suid.join("probe"), fs::Permissions::from_mode(0o4755)).unwrap();
    let (uid, _) = nobody();
    let outside = {
        use std::os::unix::process::CommandExt;
        let done = Command::new(suid.join("probe"))
            .args(["ids", "--"])
            .uid(uid)
            .output()
            .unwrap();
        update_hook::Report::read(&String::from_utf8_lossy(&done.stdout))
    };
    assert!(
        outside
            .detail
            .as_deref()
            .unwrap_or_default()
            .starts_with(&format!("uid={uid}\t0\t")),
        "the setuid program did not become root outside the sandbox: {outside:?}"
    );
    let mut spec = rig.spec();
    spec.release = Some(suid);
    let run = rig.run(&spec, Step::Verify, &["ids", "--"]);
    assert!(
        detail(&run).starts_with(&format!("uid={uid}\t{uid}\t")),
        "the setuid program gained privilege: {}",
        said_by(&run)
    );
    record(
        "rights-nnp",
        "a setuid-root program stayed the hook account in a step and became root outside it",
    );
}

/// Catches `ProtectHome=` being dropped: a file under /home that the hook account can read is not
/// there for the hook.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_hook_cannot_see_home() {
    let rig = Rig::new("home");
    let home = Path::new("/home").join(format!("urprobe-{}", std::process::id()));
    let _planted = Planted(vec![home.clone()]);
    make_dir(&home, 0o755);
    let secret = home.join("secret");
    fs::write(&secret, "in a home\n").unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    let path = secret.display().to_string();
    let outside = as_nobody(&rig, &["read", &path, "--"]);
    assert_eq!(outside.detail.as_deref(), Some("in a home"), "{outside:?}");
    let run = rig.run(&rig.spec(), Step::Verify, &["read", &path, "--"]);
    assert_eq!(run.outcome, Outcome::Failure, "{}", said_by(&run));
}

/// Catches `PrivateTmp=` being dropped: a file in the host's /tmp that the hook account can read
/// is not in the hook's /tmp.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_hook_has_a_private_tmp() {
    let rig = Rig::new("tmp");
    let file = Path::new("/tmp").join(format!("urprobe-{}.txt", std::process::id()));
    let _planted = Planted(vec![file.clone()]);
    fs::write(&file, "in the hosts tmp\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let path = file.display().to_string();
    let outside = as_nobody(&rig, &["read", &path, "--"]);
    assert_eq!(
        outside.detail.as_deref(),
        Some("in the hosts tmp"),
        "{outside:?}"
    );
    let run = rig.run(&rig.spec(), Step::Verify, &["read", &path, "--"]);
    assert_eq!(run.outcome, Outcome::Failure, "{}", said_by(&run));
}

/// Catches a write path swapped for a link after the load: the link points somewhere allowed
/// when the file is read and at `/etc` when a step is about to run, and the recheck refuses it
/// under its own field. Put back, it passes.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_write_path_pointed_at_etc_after_the_load_is_refused_by_the_recheck() {
    let rig = Rig::new("swap");
    let real = rig.dir.join("real");
    make_dir(&real, 0o755);
    let link = rig.dir.join("swap");
    symlink(&real, &link).unwrap();
    let mut spec = rig.spec();
    spec.write = vec![link.display().to_string()];
    let declared = rig.declared(&spec, &["ok", "--"]);
    assert_eq!(declared.recheck(), Ok(()));
    fs::remove_file(&link).unwrap();
    symlink("/etc", &link).unwrap();
    match declared.recheck() {
        Err(Refusal::Reserved { field, .. }) => assert_eq!(field, "hook.write_paths"),
        other => panic!("{other:?}"),
    }
    fs::remove_file(&link).unwrap();
    symlink("/var/log", &link).unwrap();
    match declared.recheck() {
        Err(Refusal::Reserved { field, why, .. }) => {
            assert_eq!(field, "hook.write_paths");
            assert_eq!(why, update_rights::reserved::Violation::NotAllowed);
        }
        other => panic!("a link to somewhere outside the write roots: {other:?}"),
    }
}

/// Catches the ancestor rule for a root hook under the real root: below a directory root owns and
/// closes, a write path passes; below one anyone can write, one the group can write, or one an
/// ordinary account owns, it is refused, and the ancestor is named.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_root_hook_path_below_a_directory_others_control_is_refused() {
    let rig = Rig::new("above");
    let closed = rig.dir.join("closed");
    make_dir(&closed, 0o755);
    let mut spec = rig.spec();
    spec.user = "root".to_owned();
    spec.group = "root".to_owned();
    spec.write = vec![closed.join("work").display().to_string()];
    let declared = rig.declared(&spec, &["ok", "--"]);
    assert_eq!(declared.recheck(), Ok(()), "below a closed root directory");
    for (name, mode, owned_by_nobody) in [
        ("everyone", 0o777, false),
        ("group", 0o775, false),
        ("theirs", 0o755, true),
    ] {
        let dir = rig.dir.join(name);
        make_dir(&dir, mode);
        if owned_by_nobody {
            give_to_nobody(&dir);
        }
        spec.write = vec![dir.join("work").display().to_string()];
        let declared = rig.declared(&spec, &["ok", "--"]);
        match declared.recheck() {
            Err(Refusal::Ancestor { ancestor, .. }) => {
                assert_eq!(ancestor, dir.display().to_string(), "{name}")
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

/// Catches a root hook reaching the directory of another declared service through a link: the
/// service `probe-trespass` may write in `/var/lib/probe-trespass-*`, a link there that points
/// at `/var/lib/probe-trespass-b`, the directory of the service `probe-trespass-b`, is that
/// service's. At the load the declaration is refused as a conflict, and a link made or
/// re-pointed after the load is refused at the recheck, and so at the launch, which spawns
/// nothing.
#[test]
#[ignore = "needs root and systemd; run by cargo xtask sudo-test"]
fn a_root_hook_cannot_write_in_another_services_directory_through_a_link() {
    let rig = Rig::new("trespass");
    let (mine, other, link) = (
        PathBuf::from("/var/lib/probe-trespass-mine"),
        PathBuf::from("/var/lib/probe-trespass-b"),
        PathBuf::from("/var/lib/probe-trespass-x"),
    );
    let _planted = Planted(vec![mine.clone(), other.clone(), link.clone()]);
    for dir in [&mine, &other] {
        make_dir(dir, 0o755);
    }
    symlink(&mine, &link).unwrap();

    // After the load: the link is made to point at the other service's directory.
    let mut spec = rig.spec();
    spec.user = "root".to_owned();
    spec.group = "root".to_owned();
    spec.write = vec![link.display().to_string()];
    let declared = update_rights::Declared::by_hand(
        rig.declaration(&spec, &["ok", "--"]),
        0,
        &["probe-trespass", "probe-trespass-b"],
    );
    assert_eq!(
        declared.recheck(),
        Ok(()),
        "the link points into its own directory"
    );
    fs::remove_file(&link).unwrap();
    symlink(&other, &link).unwrap();
    match declared.recheck() {
        Err(Refusal::Conflict { what, with }) => {
            assert_eq!(what, "a write path in the directory of another service");
            assert_eq!(with, "probe-trespass-b");
        }
        other => panic!("{other:?}"),
    }
    let release = rig.dir.join("release");
    let launch = Launch::of(&declared, Step::Verify, 1, &release, None);
    assert!(matches!(
        update_rights::run_step(&update_rights::Systemd::system(), &launch),
        Err(update_rights::RunError::Recheck(Refusal::Conflict { .. }))
    ));

    // At the load: the same link, already there.
    let dir = rig.dir.join("updater.d");
    make_dir(&dir, 0o755);
    let declarations = [
        ("probe-trespass", "daemon", link.display().to_string()),
        ("probe-trespass-b", "bin", other.display().to_string()),
    ];
    for (name, account, path) in declarations {
        let mut spec = rig.spec();
        spec.user = "root".to_owned();
        spec.group = "root".to_owned();
        spec.write = vec![path];
        let text = rig
            .toml(name, &spec, &["ok", "--"])
            .replace("uid = \"daemon\"", &format!("uid = \"{account}\""))
            .replace(
                &format!("state_dir = \"{}\"", rig.path("state")),
                &format!("state_dir = \"{}\"", rig.path(&format!("state-{name}"))),
            );
        let file = dir.join(format!("{name}.toml"));
        fs::write(&file, text).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    }
    let accounts = EtcAccounts::system().unwrap();
    let policy = Policy {
        trusted_uid: 0,
        accounts: &accounts,
    };
    let loaded = load_dir(&dir, &policy).unwrap();
    let refused: Vec<_> = loaded
        .refused
        .iter()
        .map(|r| (&r.service, &r.reason))
        .collect();
    assert_eq!(
        loaded.services.keys().collect::<Vec<_>>(),
        ["probe-trespass-b"],
        "{refused:?}"
    );
    match &loaded.refused[..] {
        [one] => assert!(
            matches!(&one.reason, Refusal::Conflict { with, .. } if with == "probe-trespass-b"),
            "{:?}",
            one.reason
        ),
        other => panic!("{other:?}"),
    }
}
