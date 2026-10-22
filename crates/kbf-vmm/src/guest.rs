//! Running a guest that carries `kbf-guest`: the shared folder it finds, the files the
//! guest writes back, and the launch daemons that start the guest side offline.
//!
//! The base image has no user, no agent and no network. The only way into it without a
//! person at a console is a file written while the guest is off (probe P0), so the
//! daemons below are the offline payload; without them the guest runs nothing of ours and
//! every probe that needs a guest answer says `needs-manual`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::probe::{Ctx, Vm};
use crate::system::OfflineFile;

/// Where the guest finds the automounted share.
pub const GUEST_SHARE: &str = "/Volumes/My Shared Files/kbf";
/// The custom virtiofs tag of the second share.
pub const TAG: &str = "kbfprobe";
/// The vsock port the host listens on and the guest tries.
pub const VSOCK_PORT: u32 = 5000;
/// The console port name the host offers and the guest tries.
pub const CONSOLE_NAME: &str = "kbf.probe";

pub const RESULTS: &str = "results";
pub const GUEST_BIN: &str = "kbf-guest";
/// Results written by the daemon that runs as root (the control arm).
pub const ROOT_RESULT: &str = "guest-probe-root.json";
/// Results written by the daemon that runs as the unprivileged user `nobody`.
pub const USER_RESULT: &str = "guest-probe-user.json";
/// Markers: a daemon ran (`system`), the login window exists (`login`), and, when the
/// image has an auto-login user with the agent installed, the Aqua session (`aqua`).
pub const READY_SYSTEM: &str = "ready-system";
pub const READY_LOGIN: &str = "ready-login";
pub const READY_AQUA: &str = "ready-aqua";

/// The host side of the share for one VM.
#[derive(Debug, Clone)]
pub struct Share {
    pub dir: PathBuf,
    pub tagged: PathBuf,
}

impl Share {
    #[must_use]
    pub fn results(&self) -> PathBuf {
        self.dir.join(RESULTS)
    }
}

/// Makes the share directories for the VM `name`, with the results directory and, when
/// given, the guest binary.
///
/// # Errors
/// A directory or the copy could not be made.
pub fn prepare_share(ctx: &Ctx<'_>, name: &str) -> Result<Share, String> {
    let dir = ctx.work.join(format!("{name}-share"));
    let tagged = ctx.work.join(format!("{name}-tagged"));
    for d in [&dir.join(RESULTS), &tagged] {
        std::fs::create_dir_all(d).map_err(|e| format!("create {}: {e}", d.display()))?;
    }
    if let Some(bin) = &ctx.guest_binary {
        std::fs::copy(bin, dir.join(GUEST_BIN))
            .map_err(|e| format!("copy {} into the share: {e}", bin.display()))?;
    }
    Ok(Share { dir, tagged })
}

/// What a guest run found.
#[derive(Debug)]
pub struct GuestRun {
    /// Result files that came back, by file name.
    pub results: BTreeMap<String, Value>,
    /// Result files that did not come back within the time.
    pub missing: Vec<String>,
    /// Vsock connections and console writes the host saw.
    pub traffic: Value,
}

/// Waits for each of `wanted` (file names under the results directory) and reads them.
pub fn collect(ctx: &Ctx<'_>, vm: &mut Vm, share: &Share, wanted: &[&str]) -> GuestRun {
    let mut results = BTreeMap::new();
    let mut missing = Vec::new();
    for name in wanted {
        let path = share.results().join(name);
        if vm
            .wait_file(&path, ctx.timing.guest_timeout, ctx.timing.tick)
            .is_none()
        {
            missing.push((*name).to_owned());
            continue;
        }
        let value = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str(&t).map_err(|e| format!("not JSON ({e}): {t}")))
            .unwrap_or_else(|e| serde_json::json!({"unreadable": e}));
        results.insert((*name).to_owned(), value);
    }
    // Let a late connection or console write arrive before the traffic is read.
    vm.watch(ctx.timing.tick, ctx.timing.tick, false);
    GuestRun {
        results,
        missing,
        traffic: vm.guest_traffic(),
    }
}

/// The reason text for a probe whose guest wrote nothing.
#[must_use]
pub fn no_agent_reason(have_binary: bool, missing: &[String]) -> String {
    let binary = if have_binary {
        ""
    } else {
        "no --guest-binary was given, so the share holds no kbf-guest; "
    };
    format!(
        "{binary}the guest wrote no {}: the base image has nothing that starts kbf-guest. Install the launch \
         daemons (`kbf-vmm probe p0` does it as root, offline) or log in once and run \
         `{GUEST_SHARE}/{GUEST_BIN} probe-guest --out {GUEST_SHARE}/{RESULTS}/<file>` by hand",
        missing.join(", ")
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A launch daemon plist that runs `args` at load, as `user` when given, restarting
/// itself every few seconds until the share is mounted and the command succeeds.
#[must_use]
pub fn daemon_plist(label: &str, user: Option<&str>, args: &[String]) -> String {
    let args: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", escape(a)))
        .collect();
    let user = user
        .map(|u| format!("  <key>UserName</key>\n  <string>{}</string>\n", escape(u)))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{label}</string>\n  <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n{user}  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n  <key>ThrottleInterval</key>\n  <integer>3</integer>\n  <key>StandardErrorPath</key>\n  <string>/var/log/{label}.log</string>\n</dict>\n</plist>\n"
    )
}

fn guest_args(sub: &[&str]) -> Vec<String> {
    let mut v = vec![format!("{GUEST_SHARE}/{GUEST_BIN}")];
    v.extend(sub.iter().map(|s| (*s).to_owned()));
    v
}

/// The files the offline write puts into the guest: the Setup Assistant marker and four
/// launch daemons (a probe as root, a probe as `nobody`, the system marker, the login
/// marker).
#[must_use]
pub fn offline_payload() -> Vec<OfflineFile> {
    let out = |name: &str| format!("{GUEST_SHARE}/{RESULTS}/{name}");
    let wait = GUEST_SHARE;
    let mut files = vec![OfflineFile {
        path: "private/var/db/.AppleSetupDone".to_owned(),
        contents: Vec::new(),
        mode: 0o644,
    }];
    let daemons: [(&str, Option<&str>, Vec<String>); 4] = [
        (
            "com.kbf.probe.root",
            None,
            guest_args(&[
                "probe-guest",
                "--wait-path",
                wait,
                "--out",
                &out(ROOT_RESULT),
                "--vsock-port",
                &VSOCK_PORT.to_string(),
                "--console-name",
                CONSOLE_NAME,
                "--tag",
                TAG,
                "--mountpoint",
                "/private/tmp/kbfmnt-root",
            ]),
        ),
        (
            "com.kbf.probe.user",
            Some("nobody"),
            guest_args(&[
                "probe-guest",
                "--wait-path",
                wait,
                "--out",
                &out(USER_RESULT),
                "--vsock-port",
                &VSOCK_PORT.to_string(),
                "--console-name",
                CONSOLE_NAME,
                "--tag",
                TAG,
                "--mountpoint",
                "/private/tmp/kbfmnt-user",
            ]),
        ),
        (
            "com.kbf.probe.system",
            Some("nobody"),
            guest_args(&[
                "probe-ready",
                "--wait-path",
                wait,
                "--out",
                &out(READY_SYSTEM),
            ]),
        ),
        (
            "com.kbf.probe.login",
            Some("nobody"),
            guest_args(&[
                "probe-ready",
                "--wait-path",
                wait,
                "--wait-process",
                "loginwindow",
                "--out",
                &out(READY_LOGIN),
            ]),
        ),
    ];
    for (label, user, args) in daemons {
        files.push(OfflineFile {
            path: format!("Library/LaunchDaemons/{label}.plist"),
            contents: daemon_plist(label, user, &args).into_bytes(),
            mode: 0o644,
        });
    }
    files
}

/// True when `dir` holds the guest binary.
#[must_use]
pub fn has_guest_binary(share: &Share) -> bool {
    share.dir.join(GUEST_BIN).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeSystem, ctx, holds, work_with_base};

    /// Catches plist text that is not escaped (a path with `&` would corrupt the file
    /// launchd reads), a missing user key for the unprivileged daemons, and one present
    /// for the root control.
    #[test]
    fn a_daemon_plist_escapes_and_names_its_user() {
        let user = daemon_plist("com.kbf.x", Some("nobody"), &["/a&b".into(), "<c>".into()]);
        assert!(
            holds(&[
                user.contains("<string>/a&amp;b</string>"),
                user.contains("&lt;c&gt;")
            ]),
            "{user}"
        );
        assert!(
            user.contains("<key>UserName</key>\n  <string>nobody</string>"),
            "{user}"
        );
        assert!(user.contains("<string>/var/log/com.kbf.x.log</string>"));
        let root = daemon_plist("com.kbf.y", None, &["/bin/true".into()]);
        assert!(!root.contains("UserName"), "{root}");
    }

    /// Catches a payload that lacks the marker that skips Setup Assistant, a daemon that
    /// the guest would never start, or a root control that runs as nobody (the G3 verdict
    /// compares the two users).
    #[test]
    fn the_offline_payload_holds_the_marker_and_four_daemons() {
        let files = offline_payload();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths[0], "private/var/db/.AppleSetupDone");
        assert_eq!(paths.len(), 5);
        assert!(paths[1..].iter().all(|p| holds(&[
            p.starts_with("Library/LaunchDaemons/com.kbf.probe."),
            p.ends_with(".plist")
        ])));
        let text = |i: usize| String::from_utf8(files[i].contents.clone()).expect("utf8");
        assert!(holds(&[
            !text(1).contains("UserName"),
            text(1).contains(ROOT_RESULT)
        ]));
        assert!(holds(&[
            text(2).contains("<string>nobody</string>"),
            text(2).contains(USER_RESULT)
        ]));
        assert!(text(3).contains(READY_SYSTEM));
        assert!(holds(&[
            text(4).contains("loginwindow"),
            text(4).contains(READY_LOGIN)
        ]));
        assert!(files.iter().all(|f| f.mode == 0o644));
    }

    /// Catches a share that is not made, or a guest binary that is not copied into it.
    #[test]
    fn the_share_gets_a_results_directory_and_the_guest_binary() {
        let work = work_with_base("guest-share");
        let sys = FakeSystem::new();
        let mut c = ctx(&sys, &work);
        let share = prepare_share(&c, "vm").expect("share");
        assert!(holds(&[share.results().is_dir(), share.tagged.is_dir()]));
        assert!(!has_guest_binary(&share));
        let bin = work.join("kbf-guest-src");
        std::fs::write(&bin, "bin").expect("bin");
        c.guest_binary = Some(bin);
        let share = prepare_share(&c, "vm2").expect("share");
        assert!(has_guest_binary(&share));
        c.guest_binary = Some(work.join("missing"));
        assert!(prepare_share(&c, "vm3").unwrap_err().contains("copy"));
        // A work directory that is a file cannot hold a share.
        c.work = work.join("base").join("disk.img");
        assert!(prepare_share(&c, "vm4").unwrap_err().contains("create"));
    }

    /// Catches a collector that reports a file it never saw, parses nothing, or lets a
    /// file that is not JSON (or not a file) stop the run.
    #[test]
    fn collect_reads_the_files_that_came_back() {
        let work = work_with_base("guest-collect");
        let sys = FakeSystem::new();
        let c = ctx(&sys, &work);
        let share = prepare_share(&c, "vm").expect("share");
        std::fs::write(share.results().join("a.json"), r#"{"k":1}"#).expect("a");
        std::fs::write(share.results().join("b.json"), "not json").expect("b");
        std::fs::create_dir(share.results().join("c.json")).expect("c");
        let mut vm = crate::probe::Vm::spawn(
            &sys,
            "vm",
            &crate::spec::VmSpec::minimal(work.join("base")),
            None,
        )
        .expect("vm");
        let run = collect(
            &c,
            &mut vm,
            &share,
            &["a.json", "b.json", "c.json", "d.json"],
        );
        assert_eq!(run.results["a.json"]["k"], 1);
        assert!(
            run.results["b.json"]["unreadable"]
                .as_str()
                .expect("text")
                .contains("not JSON")
        );
        assert!(run.results["c.json"]["unreadable"].is_string());
        assert_eq!(run.missing, vec!["d.json".to_owned()]);
        assert_eq!(run.traffic["vsock_connections"], serde_json::json!([]));
    }

    /// Catches a reason that does not tell the operator what to do, or that blames the
    /// image when the binary was simply not given.
    #[test]
    fn the_no_agent_reason_names_the_fix() {
        let with = no_agent_reason(true, &["x.json".into()]);
        assert!(
            holds(&[
                with.contains("x.json"),
                with.contains("probe p0"),
                !with.contains("--guest-binary")
            ]),
            "{with}"
        );
        let without = no_agent_reason(false, &["x.json".into(), "y.json".into()]);
        assert!(
            holds(&[
                without.contains("--guest-binary"),
                without.contains("x.json, y.json")
            ]),
            "{without}"
        );
    }
}
