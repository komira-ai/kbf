//! The clean of a slot between two leases, for real (docs/design/macos-end-state.md,
//! step 4): after every lease the daemon cleans its slot's user, checks the clean, makes
//! the user again when the check fails, and quarantines the slot when that fails too.
//!
//! The gate is [`a_lease_finds_nothing_the_lease_before_it_left_on_its_slot`]: with
//! one slot, lease A leaves a marker in each place a lease can leave state, and lease B,
//! on the same user, looks for each through the tool that would use it and finds none.
//! The other tests: a process the uid cannot kill fails the clean within its deadline
//! and quarantines the slot; a home the wipe cannot empty gets the user made again; a
//! user that cannot be made again quarantines the slot; and the start cleans what a
//! killed daemon left.
//!
//! Every test is `#[ignore]`: each needs root and real users. The macOS CI job runs them
//! with `cargo xtask sudo-test -p kbf-driver-native --test slot_clean` (from
//! `tools/ci/mac-session-tests.sh`), as `tests/slots.rs` runs, with the users of
//! `tests/support/pool.rs`.

#![cfg(target_os = "macos")]

mod support;

use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_daemon::Runtime as _;
use kbf_daemon::uid;
use kbf_driver_native::NativeRuntime;
use kbf_driver_native::slots::SlotUser;
use support::pool::{Scratch, Users, config, home_of, ids_of, name_of};
use support::{MemoryCas, PROMPT, Spec, stdout};

fn assert_root() {
    support::pool::assert_root("slot_clean");
}

/// Where a recreate moves a home it could not empty: `<homes>/<prefix><user>-...`.
const ASIDE: &str = ".kbf-recreated-";

/// The rows of the freshness test: each place lease A leaves a marker, and what finds
/// it in lease B.
const ROWS: [(&str, &str); 17] = [
    ("lease-dir", "a file in lease A's directory (cat)"),
    ("home", "a file in the slot user's real home (cat)"),
    ("defaults", "a preference (defaults read)"),
    (
        "keychain",
        "a generic password in a keychain on the search list (security)",
    ),
    ("T", "a file in DARWIN_USER_TEMP_DIR (cat)"),
    ("C", "a file in DARWIN_USER_CACHE_DIR (cat)"),
    ("tmp", "a file in /private/tmp (cat)"),
    ("var-tmp", "a file in /private/var/tmp (cat)"),
    ("shared", "a file in /Users/Shared (cat)"),
    (
        "agent",
        "a KeepAlive LaunchAgent in user/<uid> (launchctl print, ps)",
    ),
    ("crontab", "a crontab line (crontab -l)"),
    ("at", "an at job (atq)"),
    (
        "sleeper",
        "a sleep in a session of its own (setsid) whose parent exited (ps)",
    ),
    ("setuid", "a setuid-root sleep the slot user started (ps)"),
    ("image", "an attached disk image (hdiutil info)"),
    ("sem", "a System V semaphore (ipcs)"),
    (
        "processes",
        "any process of the uid, by effective or real uid (libproc)",
    ),
];

/// Lease A's part inside the sandbox, as the slot user: `$1` the marker, `$2` the
/// setuid-root sleep. Each place written prints `planted <row>`; then it waits for the
/// test's `go`.
const PLANT_INSIDE: &str = r#"m=$1
uid=$(id -u)
eval "h=~$(id -un)"
t=$(getconf DARWIN_USER_TEMP_DIR)
c=$(getconf DARWIN_USER_CACHE_DIR)
ok() { echo "planted $*"; }
ps -o command= -p "$(cat sleeper)" | grep -q 'sleep 3004$' && ok sleeper
printf %s "$m" > kbf-fresh && ok lease-dir
printf %s "$m" > "$h/kbf-fresh" && ok home
defaults write kbf.ci.fresh v "$m" && ok defaults
kc="$h/Library/Keychains/kbf-fresh.keychain-db"
mkdir -p "$h/Library/Keychains" && security create-keychain -p "" "$kc" &&
  security add-generic-password -a kbf-ci -s kbf-ci-fresh -w "$m" "$kc" &&
  security list-keychains -d user -s "$kc" &&
  [ "$(security find-generic-password -s kbf-ci-fresh -w)" = "$m" ] && ok keychain
printf %s "$m" > "${t}kbf-fresh" && ok T
printf %s "$m" > "${c}kbf-fresh" && ok C
"$2" 3005 </dev/null >/dev/null 2>&1 &
s=$!
sleep 0.5
[ "$(ps -o uid=,ruid= -p "$s" | awk '{print $1, $2}')" = "0 $uid" ] && ok setuid
hdiutil create -quiet -size 1m -fs HFS+ -volname kbffresh "$m.dmg" &&
  hdiutil attach -quiet -nomount "$m.dmg" && hdiutil info | grep -q "$m" && ok image
sem=$(/usr/bin/perl -e 'my $id = semget(0, 1, 0600 | 01000); defined $id or die "semget: $!\n"; print $id') &&
  ipcs -s | awk -v id="$sem" '$1 == "s" && $2 == id' | grep -q . && ok sem "$sem"
: > planted
i=0
while [ ! -e go ]; do i=$((i + 1)); [ "$i" -lt 600 ] || exit 9; sleep 0.1; done
exit 0
"#;

/// Lease A's part outside the sandbox, which denies an action these writes: run by the
/// test as the slot user (as a job that left the sandbox would be), `$1` the marker.
const PLANT_OUTSIDE: &str = r#"m=$1
ok() { echo "planted $*"; }
printf %s "$m" > "/private/tmp/$m" && ok tmp
printf %s "$m" > "/private/var/tmp/$m" && ok var-tmp
printf %s "$m" > "/Users/Shared/$m" && ok shared
echo "0 0 1 1 * /usr/bin/true # $m" | crontab - && crontab -l | grep -q "$m" && ok crontab
echo /usr/bin/true | at now + 2 hours && [ -n "$(atq)" ] && ok at
exit 0
"#;

/// Lease B, as the same slot user: `$1` the marker, `$2` lease A's semaphore id, `$3`
/// lease A's directory. Prints `found <row> ...` for each marker it finds, then
/// `checked`.
const CHECK: &str = r#"m=$1
uid=$(id -u)
eval "h=~$(id -un)"
t=$(getconf DARWIN_USER_TEMP_DIR)
c=$(getconf DARWIN_USER_CACHE_DIR)
found() { echo "found $*"; }
[ -e "$3/kbf-fresh" ] && found lease-dir
[ -e "$h/kbf-fresh" ] && found home
v=$(defaults read kbf.ci.fresh v 2>/dev/null) && found defaults "$v"
v=$(security find-generic-password -s kbf-ci-fresh -w 2>/dev/null) && found keychain "$v"
[ -e "$h/Library/Keychains/kbf-fresh.keychain-db" ] && found keychain file
[ -e "${t}kbf-fresh" ] && found T
[ -e "${c}kbf-fresh" ] && found C
[ -e "/private/tmp/$m" ] && found tmp
[ -e "/private/var/tmp/$m" ] && found var-tmp
[ -e "/Users/Shared/$m" ] && found shared
launchctl print "user/$uid/kbf.ci.fresh" >/dev/null 2>&1 && found agent in user/$uid
[ -e "$h/Library/LaunchAgents/kbf.ci.fresh.plist" ] && found agent plist
ps -axo command= | grep -qE 'sleep 3003$' && found agent process
crontab -l 2>/dev/null | grep -q "$m" && found crontab
q=$(atq 2>/dev/null) && [ -n "$q" ] && found at "$q"
ps -axo command= | grep -qE '/bin/sleep 3004$' && found sleeper
ps -axo command= | grep -qE 'setuid-sleep 3005$' && found setuid
hdiutil info 2>/dev/null | grep -q "$m" && found image
[ -n "$2" ] && ipcs -s | awk -v id="$2" '$1 == "s" && $2 == id' | grep -q . && found sem
echo checked
"#;

/// A KeepAlive LaunchAgent that runs `sleep 3003`.
const AGENT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>kbf.ci.fresh</string>
  <key>ProgramArguments</key><array><string>/bin/sleep</string><string>3003</string></array>
  <key>KeepAlive</key><true/>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#;

/// The rows `text` names after `word` (`planted` or `found`), and what each line said.
fn rows(text: &str, word: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.strip_prefix(word)?.strip_prefix(' '))
        .map(|rest| {
            let (row, what) = rest.split_once(' ').unwrap_or((rest, ""));
            (row.to_owned(), what.to_owned())
        })
        .collect()
}

/// `program` with `args` as `user`, outside any sandbox, with a plain environment.
fn as_user(user: &SlotUser, program: &str, args: &[&str]) -> std::process::Output {
    Command::new(program)
        .args(args)
        .uid(user.uid)
        .gid(user.gid)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", home_of(user))
        .env("USER", &user.name)
        .env("LOGNAME", &user.name)
        .current_dir("/")
        .stdin(Stdio::null())
        .output()
        .expect(program)
}

/// `program` with `args` as root; its stdout and stderr, and whether it exited 0.
fn root(program: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(program).args(args).output().expect(program);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// Background Task Management's entries for `uid`, from `sfltool dumpbtm`.
fn btm(uid: u32) -> Vec<String> {
    let (_, text) = root("/usr/bin/sfltool", &["dumpbtm"]);
    let want = format!("uid={uid} ");
    kbf_mac_session::scan::parse_dumpbtm(&text)
        .unwrap_or_else(|why| vec![format!("unreadable: {why}")])
        .into_iter()
        .filter(|item| item.contains(&want))
        .collect()
}

/// What the Mac says of `user` where a marker could hide, for a failure message.
fn diagnosis(user: &SlotUser) -> String {
    let domain = format!("user/{}", user.uid);
    let mut out = String::new();
    for (what, program, args) in [
        ("hdiutil info", "/usr/bin/hdiutil", vec!["info", "-plist"]),
        ("ipcs", "/usr/bin/ipcs", vec!["-a"]),
        (
            "launchctl print",
            "/bin/launchctl",
            vec!["print", domain.as_str()],
        ),
    ] {
        let (_, text) = root(program, &args);
        let head: Vec<&str> = text.lines().take(60).collect();
        out.push_str(&format!("--- {what}\n{}\n", head.join("\n")));
    }
    out.push_str(&format!(
        "--- btm of uid {}\n{:?}\n",
        user.uid,
        btm(user.uid)
    ));
    let home = home_of(user);
    let (_, listing) = root("/bin/ls", &["-laR", &home.to_string_lossy()]);
    let head: Vec<&str> = listing.lines().take(60).collect();
    out.push_str(&format!("--- {}\n{}\n", home.display(), head.join("\n")));
    out
}

/// The slot count the runtime reports.
fn slots_reported(rt: &NativeRuntime<MemoryCas>) -> Option<String> {
    rt.capabilities()
        .into_iter()
        .find(|(key, _)| key == kbf_driver_native::slots::CAPABILITY)
        .map(|(_, value)| value)
}

/// The freshness test. With one slot, lease A leaves a marker in each place of
/// [`ROWS`]: its own directory, the real home, a preference, a keychain item, `T/`
/// and `C/` of its `/var/folders` entry, a setsid sleeper, a setuid-root sleep, an
/// attached disk image and a System V semaphore from inside the sandbox; and, as the
/// slot user outside the sandbox (which denies an action these writes), files in
/// `/private/tmp`, `/private/var/tmp` and `/Users/Shared`, a crontab line, an `at` job,
/// and a KeepAlive LaunchAgent bootstrapped into `user/<uid>` from its home. Lease B, as
/// the same user, finds none of them, each through the tool that would use it, and no
/// process of the uid is left between the two leases. Background Task Management entries
/// of the uid, if the agent made any, are gone too.
///
/// Catches every step of the clean skipped, each turning its own rows red: the
/// bootout (the agent stays in `user/<uid>`), the crontab and `at` sweep, the image
/// detach, the semaphore removal, the home wipe (home, defaults, keychain, agent
/// plist), the `/var/folders` wipe (T, C), the shared sweep (tmp, var-tmp, shared).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_lease_finds_nothing_the_lease_before_it_left_on_its_slot() {
    assert_root();
    let scratch = Scratch::new("fresh");
    let users = Users::new("fresh", 1);
    let user = users.0[0].clone();
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config(&scratch, users.pool()), Arc::clone(&cas)).expect("runtime");
    let mark = format!("kbf-fresh-{}", std::process::id());
    let setuid = scratch.setuid_sleep().to_string_lossy().into_owned();
    let act = scratch.act();
    let a = Spec::argv(&[
        &act,
        "spawn-child",
        "setsid",
        ";;",
        "run",
        "/bin/sh",
        "-c",
        "/bin/sleep 3004 </dev/null >/dev/null 2>&1 & echo $! > sleeper",
        ";",
        "wait-for",
        "sleeper",
        "10",
        ";",
        "run",
        "/bin/sh",
        "-c",
        PLANT_INSIDE,
        "sh",
        &mark,
        &setuid,
    ]);
    let a_root = scratch.lease_root(1);
    let outside = {
        let (user, mark, a_root) = (user.clone(), mark.clone(), a_root.clone());
        async move {
            let give_up = Instant::now() + PROMPT * 3;
            while !a_root.join("planted").exists() {
                if Instant::now() > give_up {
                    return "lease A never planted its markers".to_owned();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let planted = tokio::task::spawn_blocking(move || plant_outside(&user, &mark));
            let planted = planted.await.expect("planted");
            let _ = std::fs::write(a_root.join("go"), b"");
            planted
        }
    };
    let (first, outside) = tokio::join!(support::run(&rt, &cas, 1, &a), outside);
    let first = first.expect("lease A ran");
    let a_out = stdout(&cas, &first);
    let a_err = support::stderr(&cas, &first);
    // Between the leases: the clean has run before lease A's run returned.
    let left = uid::processes(user.uid).expect("list");
    let btm_after = btm(user.uid);
    let mut planted: BTreeSet<String> = rows(&a_out, "planted")
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    planted.extend(rows(&outside, "planted").into_iter().map(|(r, _)| r));
    planted.insert("processes".to_owned());
    let sem = rows(&a_out, "planted")
        .into_iter()
        .find(|(row, _)| row == "sem")
        .map(|(_, id)| id)
        .unwrap_or_default();
    let a_dir = a_root.to_string_lossy().into_owned();
    let b = Spec::argv(&["/bin/sh", "-c", CHECK, "sh", &mark, &sem, &a_dir]);
    let second = support::run(&rt, &cas, 2, &b).await.expect("lease B ran");
    let b_out = stdout(&cas, &second);
    let b_err = support::stderr(&cas, &second);
    let mut found: Vec<(String, String)> = rows(&b_out, "found");
    if !left.is_empty() {
        found.push(("processes".to_owned(), format!("{left:?}")));
    }
    let btm_planted = btm_planted_note(&outside);
    if !btm_after.is_empty() {
        found.push(("btm".to_owned(), format!("{btm_after:?}")));
    }
    let diagnosis = diagnosis(&user);
    let not_planted: Vec<&str> = ROWS
        .iter()
        .map(|(row, _)| *row)
        .filter(|row| !planted.contains(*row))
        .collect();
    let red: BTreeSet<&str> = found.iter().map(|(row, _)| row.as_str()).collect();
    eprintln!("freshness: planted {planted:?}; {btm_planted}; red rows {red:?}; found {found:?}");
    assert_eq!(first.exit_code, 0, "lease A: {a_out}{a_err}\n{outside}");
    assert!(
        not_planted.is_empty(),
        "lease A did not plant {not_planted:?} (so those rows prove nothing):\n{a_out}{a_err}\n{outside}\n{diagnosis}"
    );
    assert_eq!(second.exit_code, 0, "lease B: {b_out}{b_err}");
    assert!(b_out.ends_with("checked\n"), "lease B: {b_out}{b_err}");
    assert!(
        found.is_empty(),
        "lease B found what lease A left, rows {red:?}: {found:?}\n{diagnosis}"
    );
}

/// The part of lease A [`PLANT_OUTSIDE`] makes, as the slot user outside the sandbox,
/// and its LaunchAgent, bootstrapped by root into `user/<uid>` from the home. Its
/// `planted` lines, and what failed.
fn plant_outside(user: &SlotUser, mark: &str) -> String {
    let out = as_user(user, "/bin/sh", &["-c", PLANT_OUTSIDE, "sh", mark]);
    let mut text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let agents = home_of(user).join("Library").join("LaunchAgents");
    let plist = agents.join("kbf.ci.fresh.plist");
    let made = as_user(user, "/bin/mkdir", &["-p", &agents.to_string_lossy()]);
    if !made.status.success() {
        text.push_str("mkdir LaunchAgents failed\n");
        return text;
    }
    std::fs::write(&plist, AGENT).expect("agent plist");
    std::os::unix::fs::chown(&plist, Some(user.uid), Some(user.gid)).expect("chown");
    std::fs::set_permissions(&plist, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let domain = format!("user/{}", user.uid);
    let path = plist.to_string_lossy().into_owned();
    let (booted, said) = root("/bin/launchctl", &["bootstrap", &domain, &path]);
    text.push_str(&format!("launchctl bootstrap {domain}: {booted} {said}\n"));
    let service = format!("{domain}/kbf.ci.fresh");
    let (printed, _) = root("/bin/launchctl", &["print", &service]);
    if printed {
        text.push_str("planted agent\n");
    }
    let made_btm = btm(user.uid);
    text.push_str(&format!("btm after the agent: {made_btm:?}\n"));
    text
}

/// Whether the agent made a Background Task Management entry, as [`plant_outside`]
/// noted: the clean must remove one if it did; it proves nothing if it did not.
fn btm_planted_note(outside: &str) -> String {
    outside
        .lines()
        .find(|line| line.starts_with("btm after the agent: "))
        .map_or_else(|| "btm: not looked at".to_owned(), str::to_owned)
}

/// A process with the real uid 0 and the effective uid of a slot: listed among the
/// uid's processes, and out of reach of a kill from the uid (macOS lets a process
/// signal one whose real or saved uid is the sender's, and this one's are root's).
/// Killed and reaped when dropped.
struct Unkillable(libc::pid_t);

impl Unkillable {
    fn new(uid: u32) -> Self {
        // SAFETY: the child calls only async-signal-safe functions (seteuid, pause,
        // _exit), as a fork of a threaded process must.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            // SAFETY: async-signal-safe calls only; neither returns to Rust code.
            unsafe {
                if libc::seteuid(uid) != 0 {
                    libc::_exit(1);
                }
                loop {
                    libc::pause();
                }
            }
        }
        Self(pid)
    }
}

impl Drop for Unkillable {
    fn drop(&mut self) {
        // SAFETY: plain integers; `self.0` is this process's child, reaped here once.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
            libc::waitpid(self.0, std::ptr::null_mut(), 0);
        }
    }
}

/// The deadline. Catches a clean whose kill loops with no deadline (the lease never
/// ends: a process stuck in the kernel would hang the slot), and a slot whose
/// processes survived every kill given back to the pool (the next lease would run
/// beside them): with a process of the uid that the uid's kill cannot reach, the lease
/// ends within its deadlines, failed, and the node reports no slot.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_process_the_kill_cannot_end_fails_the_clean_within_its_deadline() {
    assert_root();
    let scratch = Scratch::new("deadline");
    let users = Users::new("deadline", 1);
    let user = &users.0[0];
    let cas = Arc::new(MemoryCas::default());
    let mut config = config(&scratch, users.pool());
    config.kill_wait = Duration::from_secs(2);
    let rt = NativeRuntime::new(config, Arc::clone(&cas)).expect("runtime");
    let before = slots_reported(&rt);
    // After the start, which would fail on it.
    let stuck = Unkillable::new(user.uid);
    let mut ids = None;
    let give_up = Instant::now() + PROMPT;
    while ids != Some((user.uid, 0)) && Instant::now() < give_up {
        std::thread::sleep(Duration::from_millis(20));
        ids = ids_of(stuck.0);
    }
    let listed = uid::processes(user.uid).expect("list").contains(&stuck.0);
    let act = scratch.act();
    let action = Spec::argv(&[&act, "id"]).store(&cas);
    let started = Instant::now();
    let ended =
        tokio::time::timeout(Duration::from_secs(90), rt.run(support::work(1, action, 0))).await;
    let took = started.elapsed();
    let after = slots_reported(&rt);
    drop(stuck);
    assert_eq!(
        ids,
        Some((user.uid, 0)),
        "the stuck process is not root's by its real uid and the slot's by its effective one"
    );
    assert!(
        listed,
        "the stuck process is not among uid {}'s processes",
        user.uid
    );
    let ended = ended.expect("the lease never ended: the kill has no deadline");
    let why = ended.expect_err("a lease whose processes survived succeeded");
    assert!(why.to_string().contains("survived"), "{why}");
    assert!(took < Duration::from_secs(60), "the lease took {took:?}");
    assert_eq!(before.as_deref(), Some("1"));
    assert_eq!(
        after.as_deref(),
        Some("0"),
        "a slot whose processes survived is still reported"
    );
}

/// The entries of `dir`, by name.
fn entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir).map_or_else(
        |_| vec!["<unreadable>".to_owned()],
        |list| {
            list.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        },
    )
}

/// The homes a recreate moved aside for `user`.
fn moved_aside(user: &SlotUser) -> Vec<PathBuf> {
    let prefix = format!("{ASIDE}{}-", user.name);
    let homes = Path::new(support::pool::HOMES);
    std::fs::read_dir(homes)
        .expect("homes")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path())
        .collect()
}

/// Removes what [`moved_aside`] finds, its flags cleared first.
fn remove_aside(user: &SlotUser) {
    for dir in moved_aside(user) {
        let _ = Command::new("/usr/bin/chflags")
            .args(["-R", "nouchg"])
            .arg(&dir)
            .status();
        let _ = kbf_outputs::remove_tree(&dir);
    }
}

/// A root-owned file in `user`'s home, locked (`uchg`): the wipe clears flags only on
/// the departing uid's own entries, so it cannot remove it.
fn plant_stuck(user: &SlotUser) -> PathBuf {
    let stuck = home_of(user).join("kbf-ci-stuck");
    std::fs::write(&stuck, b"stuck").expect("stuck file");
    support::pool::run("/usr/bin/chflags", &["uchg", &stuck.to_string_lossy()]);
    stuck
}

/// A failed clean makes the user again. Catches a clean that ignores its check (the
/// file it could not remove stays in the home for the next lease), and a slot
/// quarantined although the user could be made again: with a root-owned locked file in
/// the home, the lease's clean fails, the user is made again with the same name and uid
/// and an empty home of its own (the old one moved aside, root's, mode 0700), and the
/// next lease runs on the slot as that uid.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_slot_whose_home_cannot_be_emptied_gets_its_user_made_again() {
    assert_root();
    let scratch = Scratch::new("recreate");
    let users = Users::new("recreate", 1);
    let user = users.0[0].clone();
    remove_aside(&user);
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config(&scratch, users.pool()), Arc::clone(&cas)).expect("runtime");
    plant_stuck(&user);
    let act = scratch.act();
    let first = support::run(&rt, &cas, 1, &Spec::argv(&[&act, "id"])).await;
    let home = home_of(&user);
    let meta = std::fs::symlink_metadata(&home);
    let in_home = entries(&home);
    let aside = moved_aside(&user);
    let aside_meta: Vec<(u32, u32, Vec<String>)> = aside
        .iter()
        .map(|dir| {
            let m = std::fs::symlink_metadata(dir).expect("aside");
            (m.uid(), m.mode() & 0o7777, entries(dir))
        })
        .collect();
    let named = name_of(user.uid);
    let reported = slots_reported(&rt);
    let second = tokio::time::timeout(
        PROMPT * 3,
        support::run(&rt, &cas, 2, &Spec::argv(&[&act, "id"])),
    )
    .await;
    remove_aside(&user);
    let first = first.expect("the first lease ran");
    assert_eq!(first.exit_code, 0, "{}", stdout(&cas, &first));
    let meta = meta.expect("no home after the recreate");
    assert!(meta.is_dir(), "the home is not a directory");
    assert_eq!(
        (meta.uid(), meta.gid(), meta.mode() & 0o7777),
        (user.uid, user.gid, 0o700),
        "the new home is not the user's, mode 0700"
    );
    assert!(
        in_home.is_empty(),
        "the home after the recreate holds {in_home:?}"
    );
    assert_eq!(
        aside_meta,
        vec![(0, 0o700, vec!["kbf-ci-stuck".to_owned()])],
        "the old home was not moved aside, root's, mode 0700, with the stuck file: {aside:?}"
    );
    assert_eq!(
        named.as_deref(),
        Some(user.name.as_str()),
        "uid {} lost its name",
        user.uid
    );
    assert_eq!(reported.as_deref(), Some("1"), "the slot was quarantined");
    let second = second
        .expect("the next lease never got the slot")
        .expect("the next lease ran");
    assert_eq!(second.exit_code, 0, "{}", support::stderr(&cas, &second));
    assert_eq!(stdout(&cas, &second), format!("{}\n", user.uid));
}

/// A failed recreate quarantines the slot. Catches a slot given back although its user
/// could not be made again (the next lease would find what the clean could not
/// remove): with the home itself root's and locked, so it can be neither emptied nor
/// moved aside, the node reports no slot after the lease, and the next lease waits.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_slot_whose_user_cannot_be_made_again_is_quarantined() {
    assert_root();
    let scratch = Scratch::new("quarantine");
    let users = Users::new("quarantine", 1);
    let user = users.0[0].clone();
    remove_aside(&user);
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config(&scratch, users.pool()), Arc::clone(&cas)).expect("runtime");
    let home = home_of(&user);
    plant_stuck(&user);
    std::os::unix::fs::chown(&home, Some(0), Some(0)).expect("chown");
    support::pool::run("/usr/bin/chflags", &["uchg", &home.to_string_lossy()]);
    let act = scratch.act();
    let first = support::run(&rt, &cas, 1, &Spec::argv(&[&act, "id"])).await;
    let reported = slots_reported(&rt);
    let second = tokio::time::timeout(
        Duration::from_secs(3),
        support::run(&rt, &cas, 2, &Spec::argv(&[&act, "id"])),
    )
    .await;
    support::pool::run(
        "/usr/bin/chflags",
        &["-R", "nouchg", &home.to_string_lossy()],
    );
    remove_aside(&user);
    let first = first.expect("the first lease ran");
    assert_eq!(first.exit_code, 0, "{}", stdout(&cas, &first));
    assert_eq!(
        reported.as_deref(),
        Some("0"),
        "a slot whose user could not be made again is still reported"
    );
    assert!(
        second.is_err(),
        "a lease ran on the quarantined slot: {second:?}"
    );
}

/// The start. Catches a start that ends a killed daemon's processes but leaves its
/// leases' state (the next lease would find it): what a lease left as the slot user (a
/// file in the home, in `T/` and in `/private/tmp`, and a crontab line) is gone once the
/// runtime has started, and the slot is reported.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[test]
fn the_start_cleans_what_a_killed_daemon_left_in_every_slot() {
    assert_root();
    let scratch = Scratch::new("start");
    let users = Users::new("start", 1);
    let user = &users.0[0];
    let mark = format!("kbf-left-{}", std::process::id());
    let plant = r#"m=$1
printf %s "$m" > "$HOME/kbf-left" &&
printf %s "$m" > "$(getconf DARWIN_USER_TEMP_DIR)kbf-left" &&
printf %s "$m" > "/private/tmp/$m" &&
echo "0 0 1 1 * /usr/bin/true # $m" | crontab -
"#;
    let check = r#"m=$1
[ -e "$HOME/kbf-left" ] && echo home
[ -e "$(getconf DARWIN_USER_TEMP_DIR)kbf-left" ] && echo T
[ -e "/private/tmp/$m" ] && echo tmp
crontab -l 2>/dev/null | grep -q "$m" && echo crontab
exit 0
"#;
    let planted = as_user(user, "/bin/sh", &["-c", plant, "sh", &mark]);
    let rt = NativeRuntime::new(
        config(&scratch, users.pool()),
        Arc::new(MemoryCas::default()),
    )
    .expect("runtime");
    let reported = slots_reported(&rt);
    drop(rt);
    let left = as_user(user, "/bin/sh", &["-c", check, "sh", &mark]);
    let _ = std::fs::remove_file(format!("/private/tmp/{mark}"));
    assert!(
        planted.status.success(),
        "planting failed: {}",
        String::from_utf8_lossy(&planted.stderr)
    );
    let left = String::from_utf8_lossy(&left.stdout).into_owned();
    assert_eq!(
        left, "",
        "the start left what the killed daemon's lease left"
    );
    assert_eq!(reported.as_deref(), Some("1"));
}
