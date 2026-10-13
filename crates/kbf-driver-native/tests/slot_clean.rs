//! The clean of a slot between two leases, for real (docs/design/macos-end-state.md,
//! step 4): after every lease the daemon cleans its slot's user, checks the clean, makes
//! the user again when the check fails, and quarantines the slot when that fails too.
//!
//! The gate is [`a_lease_finds_nothing_the_lease_before_it_left_on_its_slot`]: with
//! one slot, lease A leaves a marker in each place a lease can leave state, and lease B,
//! on the same user, looks for each through the tool that would use it and finds none.
//! The other tests: a process the uid cannot kill fails the clean within its deadline
//! and quarantines the slot; a home the wipe cannot empty gets the user made again; a
//! user that cannot be made again quarantines the slot; the start cleans what a
//! killed daemon left; and a lease's job labelled like launchd's own is still counted.
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

/// The rows of the freshness test: each place lease A leaves a marker, and what looks
/// for it after the clean. Lease B looks for most from inside its sandbox; the sandbox
/// refuses an action the setuid tools (`crontab`, `at`, `ps`), so the test runs those
/// as the slot user outside it, and reads the process table and the disk images as
/// root.
const ROWS: [(&str, &str); 18] = [
    (
        "lease-dir",
        "a file in lease A's directory (lease B: test -e)",
    ),
    (
        "home",
        "a file in the slot user's real home (lease B: test -e)",
    ),
    ("defaults", "a preference (lease B: defaults read)"),
    (
        "keychain",
        "a keychain with a generic password (lease B: security)",
    ),
    ("T", "a file in DARWIN_USER_TEMP_DIR (lease B: test -e)"),
    ("C", "a file in DARWIN_USER_CACHE_DIR (lease B: test -e)"),
    ("tmp", "a file in /private/tmp (lease B: test -e)"),
    ("var-tmp", "a file in /private/var/tmp (lease B: test -e)"),
    (
        "shared",
        "a file in the Shared folder under /Users (lease B: test -e)",
    ),
    (
        "agent",
        "a KeepAlive LaunchAgent in user/<uid> (lease B: launchctl print)",
    ),
    (
        "apple-agent",
        "a KeepAlive LaunchAgent labelled com.apple.* in user/<uid>, its plist the lease's (lease B: launchctl print)",
    ),
    ("crontab", "a crontab line (the user: crontab -l)"),
    ("at", "an at job (the user: atq)"),
    (
        "sleeper",
        "a sleep in a session of its own whose parent exited (root: libproc)",
    ),
    (
        "setuid",
        "a setuid-root sleep the slot user started (root: libproc)",
    ),
    ("image", "an attached disk image (root: hdiutil info)"),
    ("sem", "a System V semaphore (lease B: ipcs)"),
    (
        "processes",
        "a process of the uid, by effective or real uid, but launchd's own agents (root: libproc)",
    ),
];

/// Lease A's part inside the sandbox, as the slot user: `$1` the marker. Each place
/// written prints `planted <row>`; then it waits for the test's `go`.
const PLANT_INSIDE: &str = r#"m=$1
eval "h=~$(id -un)"
t=$(getconf DARWIN_USER_TEMP_DIR)
c=$(getconf DARWIN_USER_CACHE_DIR)
ok() { echo "planted $*"; }
printf %s "$m" > kbf-fresh && ok lease-dir
printf %s "$m" > "$h/kbf-fresh" && ok home
defaults write kbf.ci.fresh v "$m" && [ "$(defaults read kbf.ci.fresh v)" = "$m" ] && ok defaults
kc="$h/Library/Keychains/kbf-fresh.keychain-db"
mkdir -p "$h/Library/Keychains" && security create-keychain -p "" "$kc" &&
  security add-generic-password -a kbf-ci -s kbf-ci-fresh -w "$m" "$kc" &&
  [ "$(security find-generic-password -s kbf-ci-fresh -w "$kc")" = "$m" ] && ok keychain
printf %s "$m" > "${t}kbf-fresh" && ok T
printf %s "$m" > "${c}kbf-fresh" && ok C
hdiutil create -quiet -size 1m -fs HFS+ -volname kbffresh "$m.dmg" &&
  hdiutil attach -quiet -nomount "$m.dmg" && ok image
sem=$(/usr/bin/perl -e 'my $id = semget(0, 1, 0600 | 01000); defined $id or die "semget: $!\n"; print $id') &&
  ipcs -s | awk -v id="$sem" '$1 == "s" && $2 == id' | grep -q . && ok sem "$sem"
: > planted
i=0
while [ ! -e go ]; do i=$((i + 1)); [ "$i" -lt 600 ] || exit 9; sleep 0.1; done
exit 0
"#;

/// Lease A's part outside the sandbox, which denies an action these writes and the
/// setuid tools: run by the test as the slot user (as a job that left the sandbox
/// would be), `$1` the marker.
const PLANT_OUTSIDE: &str = r#"m=$1
ok() { echo "planted $*"; }
printf %s "$m" > "/private/tmp/$m" && ok tmp
printf %s "$m" > "/private/var/tmp/$m" && ok var-tmp
printf %s "$m" > "/Users"/Shared/"$m" && ok shared
echo "0 0 1 1 * /usr/bin/true # $m" | crontab - && crontab -l | grep -q "$m" && ok crontab
echo /usr/bin/true | at now + 2 hours && [ -n "$(atq)" ] && ok at
exit 0
"#;

/// Lease B, in its sandbox, as the same slot user: `$1` the marker, `$2` lease A's
/// semaphore id, `$3` lease A's directory. Prints `found <row> ...` for each marker it
/// finds, then `checked`.
const CHECK: &str = r#"m=$1
uid=$(id -u)
eval "h=~$(id -un)"
t=$(getconf DARWIN_USER_TEMP_DIR)
c=$(getconf DARWIN_USER_CACHE_DIR)
found() { echo "found $*"; }
kc="$h/Library/Keychains/kbf-fresh.keychain-db"
[ -e "$3/kbf-fresh" ] && found lease-dir
[ -e "$h/kbf-fresh" ] && found home
v=$(defaults read kbf.ci.fresh v 2>/dev/null) && found defaults "$v"
v=$(security find-generic-password -s kbf-ci-fresh -w "$kc" 2>/dev/null) && found keychain "$v"
[ -e "$kc" ] && found keychain file
[ -e "${t}kbf-fresh" ] && found T
[ -e "${c}kbf-fresh" ] && found C
[ -e "/private/tmp/$m" ] && found tmp
[ -e "/private/var/tmp/$m" ] && found var-tmp
[ -e "/Users"/Shared/"$m" ] && found shared
launchctl print "user/$uid/kbf.ci.fresh" >/dev/null 2>&1 && found agent in user/$uid
[ -e "$h/Library/LaunchAgents/kbf.ci.fresh.plist" ] && found agent plist
launchctl print "user/$uid/com.apple.kbf-ci-fresh" >/dev/null 2>&1 && found apple-agent in user/$uid
hdiutil info 2>/dev/null | grep -q "$m" && found image
[ -n "$2" ] && ipcs -s | awk -v id="$2" '$1 == "s" && $2 == id' | grep -q . && found sem
echo checked
"#;

/// What the slot user, outside the sandbox, finds of lease A's schedules: `$1` the
/// marker.
const CHECK_OUTSIDE: &str = r#"m=$1
crontab -l 2>/dev/null | grep -q "$m" && echo "found crontab"
q=$(atq 2>/dev/null) && [ -n "$q" ] && echo "found at $q"
exit 0
"#;

/// A KeepAlive LaunchAgent labelled `label` that runs `sleep <secs>`, for the Background
/// session a pool user's domain is (an agent without the key is for Aqua, and its
/// bootstrap fails with EIO there).
fn agent(label: &str, secs: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>/bin/sleep</string><string>{secs}</string></array>
  <key>KeepAlive</key><true/>
  <key>RunAtLoad</key><true/>
  <key>LimitLoadToSessionType</key><string>Background</string>
</dict>
</plist>
"#
    )
}

/// The agents [`plant_outside`] bootstraps: their row, label and sleep. The second is
/// labelled like launchd's own agents, which the clean leaves loaded: its plist (the
/// lease's, in the home) says it is not.
const AGENTS: [(&str, &str, u32); 2] = [
    ("agent", "kbf.ci.fresh", 3003),
    ("apple-agent", "com.apple.kbf-ci-fresh", 3006),
];

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
/// [`ROWS`]: from inside its sandbox, its own directory, the real home, a preference,
/// a keychain, `T/` and `C/` of its `/var/folders` entry, a setsid sleeper, an attached
/// disk image and a System V semaphore; and as the slot user outside the sandbox
/// (which denies an action these writes and the setuid tools; a job that left it is
/// what the clean must also catch), files in `/private/tmp`, `/private/var/tmp` and the
/// `Shared` folder under `/Users`, a crontab line, an `at` job, a setuid-root sleep and
/// two KeepAlive LaunchAgents bootstrapped into `user/<uid>` from the home, one of them
/// labelled `com.apple.kbf-ci-fresh` like launchd's own agents. After the
/// clean that follows lease A, none is found: lease B, on the same user, looks for each
/// through the tool that would use it, the slot user runs `crontab -l` and `atq`, and
/// root reads the process table and `hdiutil info`. Background Task Management entries
/// of the uid, if the agent made any, are gone too.
///
/// Catches every step of the clean skipped, each turning its own rows red: the
/// bootout (agent, apple-agent), the crontab and `at` sweep (crontab, at), the image detach (image),
/// the IPC removal (sem), the home wipe (home, defaults, keychain, agent), the
/// `/var/folders` wipe (T, C), the shared sweep (tmp, var-tmp, shared).
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
    let setuid = scratch.setuid_sleep();
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
    ]);
    let a_root = scratch.lease_root(1);
    let outside = {
        let (user, mark, a_root) = (user.clone(), mark.clone(), a_root.clone());
        let drop_to = scratch.as_user_binary();
        async move {
            let give_up = Instant::now() + PROMPT * 3;
            while !a_root.join("planted").exists() {
                if Instant::now() > give_up {
                    return Outside::default();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let planted = tokio::task::spawn_blocking(move || {
                plant_outside(&user, &mark, &setuid, &drop_to, &a_root)
            });
            let planted = planted.await.expect("planted");
            let _ = std::fs::write(scratch_go(&planted.a_root), b"");
            planted
        }
    };
    let (first, mut outside) = tokio::join!(support::run(&rt, &cas, 1, &a), outside);
    let first = first.expect("lease A ran");
    let a_out = stdout(&cas, &first);
    let a_err = support::stderr(&cas, &first);
    // Between the leases: the clean ran before lease A's run returned.
    let mut found: Vec<(String, String)> = Vec::new();
    let left = kbf_driver_native::slots::macos::host::leftover_processes(user.uid).expect("list");
    if !left.is_empty() {
        found.push(("processes".to_owned(), format!("{left:?}")));
    }
    for (row, pid) in [("sleeper", outside.sleeper), ("setuid", outside.setuid_pid)] {
        if pid.is_some_and(support::alive) {
            found.push((row.to_owned(), format!("pid {pid:?}")));
        }
    }
    let attached = images(user.uid, &mark);
    if !attached.is_empty() {
        found.push(("image".to_owned(), format!("{attached:?}")));
    }
    let btm_after = btm(user.uid);
    if !btm_after.is_empty() {
        found.push(("btm".to_owned(), format!("{btm_after:?}")));
    }
    let schedules = as_user(&user, "/bin/sh", &["-c", CHECK_OUTSIDE, "sh", &mark]);
    found.extend(rows(&String::from_utf8_lossy(&schedules.stdout), "found"));
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
    found.extend(rows(&b_out, "found"));
    if let Some(child) = outside.setuid.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let mut planted: BTreeSet<String> = rows(&a_out, "planted")
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    planted.extend(rows(&outside.text, "planted").into_iter().map(|(r, _)| r));
    planted.insert("processes".to_owned());
    let not_planted: Vec<&str> = ROWS
        .iter()
        .map(|(row, _)| *row)
        .filter(|row| !planted.contains(*row))
        .collect();
    let red: BTreeSet<&str> = found.iter().map(|(row, _)| row.as_str()).collect();
    let diagnosis = diagnosis(&user);
    eprintln!(
        "freshness: planted {planted:?}; red rows {red:?}; found {found:?}\n{}",
        outside.text
    );
    assert_eq!(
        first.exit_code, 0,
        "lease A: {a_out}{a_err}\n{}",
        outside.text
    );
    assert!(
        not_planted.is_empty(),
        "lease A did not plant {not_planted:?} (so those rows prove nothing):\n{a_out}{a_err}\n{}\n{diagnosis}",
        outside.text
    );
    assert_eq!(second.exit_code, 0, "lease B: {b_out}{b_err}");
    assert!(b_out.ends_with("checked\n"), "lease B: {b_out}{b_err}");
    assert!(
        found.is_empty(),
        "what lease A left was found, rows {red:?}: {found:?}\n{diagnosis}"
    );
}

/// `go` in lease A's directory: it ends lease A's wait.
fn scratch_go(a_root: &Path) -> PathBuf {
    a_root.join("go")
}

/// What [`plant_outside`] planted.
#[derive(Default)]
struct Outside {
    /// Its `planted <row>` lines, and what failed.
    text: String,
    /// Lease A's directory.
    a_root: PathBuf,
    /// Lease A's setsid sleeper, from its `sleeper` file, if it is the uid's.
    sleeper: Option<i32>,
    /// The setuid-root sleep, the slot user's by its real uid.
    setuid: Option<std::process::Child>,
    setuid_pid: Option<i32>,
}

/// The part of lease A made as the slot user outside the sandbox while lease A waits:
/// [`PLANT_OUTSIDE`], a setuid-root sleep (`setuid`), and the LaunchAgents
/// ([`AGENTS`]), which the user bootstraps into `user/<uid>` from its home through
/// `launchctl asuser` and the `as-user` drop of `drop_to` (this test binary). Also looks up lease A's setsid
/// sleeper, and notes what `hdiutil info` lists of the uid's while lease A runs.
fn plant_outside(
    user: &SlotUser,
    mark: &str,
    setuid: &Path,
    drop_to: &str,
    a_root: &Path,
) -> Outside {
    let out = as_user(user, "/bin/sh", &["-c", PLANT_OUTSIDE, "sh", mark]);
    let mut text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let listed = uid::processes(user.uid).unwrap_or_default();
    let sleeper = std::fs::read_to_string(a_root.join("sleeper"))
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok())
        .filter(|pid| listed.contains(pid));
    if sleeper.is_some() {
        text.push_str("planted sleeper\n");
    }
    let child = Command::new(setuid)
        .arg("3005")
        .stdin(Stdio::null())
        .uid(user.uid)
        .gid(user.gid)
        .spawn()
        .expect("the setuid sleep");
    let setuid_pid = i32::try_from(child.id()).expect("pid");
    let give_up = Instant::now() + PROMPT;
    while ids_of(setuid_pid) != Some((0, user.uid)) && Instant::now() < give_up {
        std::thread::sleep(Duration::from_millis(20));
    }
    if ids_of(setuid_pid) == Some((0, user.uid)) {
        text.push_str("planted setuid\n");
    }
    let agents = home_of(user).join("Library").join("LaunchAgents");
    let made = as_user(user, "/bin/mkdir", &["-p", &agents.to_string_lossy()]);
    if made.status.success() {
        for (row, label, secs) in AGENTS {
            bootstrap(user, drop_to, &agents, row, label, secs, &mut text);
        }
    } else {
        text.push_str("mkdir LaunchAgents failed\n");
    }
    text.push_str(&format!("btm after the agent: {:?}\n", btm(user.uid)));
    text.push_str(&format!(
        "images during lease A: {:?}\n",
        images(user.uid, mark)
    ));
    Outside {
        text,
        a_root: a_root.to_owned(),
        sleeper,
        setuid: Some(child),
        setuid_pid: Some(setuid_pid),
    }
}

/// Writes the agent `label` (row `row`, `sleep <secs>`) into the user's `agents`
/// folder and bootstraps it into `user/<uid>` as the user, noting in `text` what
/// launchctl said and `planted <row>` once `launchctl print` lists it.
fn bootstrap(
    user: &SlotUser,
    drop_to: &str,
    agents: &Path,
    row: &str,
    label: &str,
    secs: u32,
    text: &mut String,
) {
    let plist = agents.join(format!("{label}.plist"));
    std::fs::write(&plist, agent(label, secs)).expect("agent plist");
    std::os::unix::fs::chown(&plist, Some(user.uid), Some(user.gid)).expect("chown");
    std::fs::set_permissions(&plist, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let domain = format!("user/{}", user.uid);
    let path = plist.to_string_lossy().into_owned();
    let (uid, gid) = (user.uid.to_string(), user.gid.to_string());
    let service = format!("{domain}/{label}");
    let tries: [&[&str]; 2] = [
        &[
            "asuser",
            &uid,
            drop_to,
            kbf_driver_native::slots::AS_USER_ARG,
            &uid,
            &gid,
            "--",
            "/bin/launchctl",
            "bootstrap",
            &domain,
            &path,
        ],
        &[
            "asuser",
            &uid,
            "/bin/launchctl",
            "bootstrap",
            &domain,
            &path,
        ],
    ];
    for args in tries {
        let (booted, said) = root("/bin/launchctl", args);
        text.push_str(&format!("launchctl {}: {booted} {said}\n", args.join(" ")));
        if root("/bin/launchctl", &["print", &service]).0 {
            text.push_str(&format!("planted {row}\n"));
            break;
        }
    }
}

/// The images `hdiutil info -plist` lists as `uid`'s or by a path naming `mark`.
fn images(uid: u32, mark: &str) -> Vec<String> {
    let out = Command::new("/usr/bin/hdiutil")
        .args(["info", "-plist"])
        .output()
        .expect("hdiutil");
    let Ok(info) = plist::Value::from_reader_xml(out.stdout.as_slice()) else {
        return vec![format!(
            "unreadable: {}",
            String::from_utf8_lossy(&out.stdout)
        )];
    };
    let images = info
        .as_dictionary()
        .and_then(|d| d.get("images"))
        .and_then(plist::Value::as_array)
        .cloned()
        .unwrap_or_default();
    images
        .iter()
        .filter_map(plist::Value::as_dictionary)
        .filter_map(|image| {
            let path = image.get("image-path").and_then(plist::Value::as_string)?;
            let owner = image
                .get("owner-uid")
                .and_then(plist::Value::as_unsigned_integer);
            (owner == Some(u64::from(uid)) || path.contains(mark)).then(|| path.to_owned())
        })
        .collect()
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

impl Unkillable {
    /// Moves this process to a thread that kills and reaps it once told to (through the
    /// sender), or after `within`, whichever comes first.
    fn guard(self, within: Duration) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (release, told) = std::sync::mpsc::channel::<()>();
        let guard = std::thread::spawn(move || {
            let _ = told.recv_timeout(within);
            drop(self);
        });
        (release, guard)
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
    // Killed by its guard after 120 s at the latest: a kill with no deadline, which
    // waits for it forever (on the runtime's thread too, as the dropped run ends its
    // uid), then ends, and the test fails instead of hanging the job.
    let (release, guard) = stuck.guard(Duration::from_secs(120));
    let act = scratch.act();
    let action = Spec::argv(&[&act, "id"]).store(&cas);
    let started = Instant::now();
    let ended =
        tokio::time::timeout(Duration::from_secs(90), rt.run(support::work(1, action, 0))).await;
    let took = started.elapsed();
    let after = slots_reported(&rt);
    let _ = release.send(());
    guard.join().expect("the guard");
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

/// Launchd's own agents are told by their plist, not their label. Catches a check that
/// leaves a process out by its job's label, which the lease chooses (a KeepAlive agent
/// labelled `com.apple.*` that a bootout left loaded would run beside every later lease,
/// and the check would pass): the running agent `com.apple.kbf-ci-label`, from a plist
/// in the home, is among the uid's leftover processes.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[test]
fn a_job_labelled_like_launchds_own_is_still_the_leases() {
    assert_root();
    let scratch = Scratch::new("label");
    let users = Users::new("label", 1);
    let user = &users.0[0];
    let label = "com.apple.kbf-ci-label";
    let agents = home_of(user).join("Library").join("LaunchAgents");
    let made = as_user(user, "/bin/mkdir", &["-p", &agents.to_string_lossy()]);
    assert!(made.status.success(), "mkdir: {made:?}");
    let mut text = String::new();
    let drop_to = scratch.as_user_binary();
    bootstrap(user, &drop_to, &agents, "label", label, 3007, &mut text);
    let service = format!("user/{}/{label}", user.uid);
    let mut pid = None;
    let give_up = Instant::now() + PROMPT;
    while pid.is_none() && Instant::now() < give_up {
        let (_, said) = root("/bin/launchctl", &["print", &service]);
        pid = said
            .lines()
            .find_map(|line| line.strip_prefix("\tpid = "))
            .and_then(|pid| pid.trim().parse::<i32>().ok());
        if pid.is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let left = kbf_driver_native::slots::macos::host::leftover_processes(user.uid).expect("list");
    let (_, booted) = root("/bin/launchctl", &["bootout", &service]);
    assert!(text.contains("planted label"), "{text}");
    let pid = pid.unwrap_or_else(|| panic!("the agent never ran: {text}"));
    assert!(
        left.contains(&pid),
        "the lease's agent {label} (pid {pid}) is left out of uid {}'s processes {left:?} \
         (bootout: {booted})",
        user.uid
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
