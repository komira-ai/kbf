//! The LaunchDaemon that runs kbf-daemon (docs/design/mac-node-provisioning.md,
//! section 5.3): its property list, rendered from the profile; the check that the
//! installed one is that file, root's, with a safe error log and the job loaded; the
//! fix that makes it so; and the restart.

use std::io;

use crate::items::{Ctx, Lines, Outcome, fail, pass};
use crate::profile::Profile;

const LAUNCHCTL: &str = "/bin/launchctl";
pub const DAEMON_BIN: &str = "/usr/local/kbf/current/kbf-daemon";
pub const LOG_DIR: &str = "/Library/Logs/kbf";
pub const ERR_LOG: &str = "/Library/Logs/kbf/kbf-daemon.err.log";
const ROOT_FILE: &str = "-rw-r--r-- 0 0";
const ROOT_DIR: &str = "drwxr-xr-x 0 0";
const LAUNCH_DAEMONS: &str = "/Library/LaunchDaemons";

/// The property list's path on the Mac.
pub fn plist_path(profile: &Profile) -> String {
    format!("{LAUNCH_DAEMONS}/{}.plist", profile.get("launchd_label"))
}

/// kbf-daemon's arguments: the native driver, the server, its certificates, the node
/// id (the host name), the scratch directory, the node label and the profile's labels.
pub fn daemon_args(profile: &Profile) -> Vec<String> {
    let mut args: Vec<String> = [
        DAEMON_BIN,
        "--driver",
        "native",
        "--server",
        profile.get("kbf_server"),
        "--cas",
        profile.get("kbf_cas"),
        "--ca-cert",
        profile.get("kbf_ca_cert"),
        "--cert",
        profile.get("kbf_cert"),
        "--key",
        profile.get("kbf_key"),
        "--node-id",
        profile.get("hostname"),
        "--scratch",
        profile.get("kbf_scratch"),
        "--label",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(profile.node_label());
    for label in &profile.labels {
        args.push("--label".into());
        args.push(label.clone());
    }
    args
}

/// The property list: the job runs as the role account, `Interactive` (not throttled),
/// kept alive, with 30 s to exit, its stderr in [`ERR_LOG`] and 65536 open files. No
/// value needs XML escaping: the profile's values hold none of `<`, `>`, `&`.
pub fn render_plist(profile: &Profile) -> String {
    let role = profile.get("role_user");
    let mut out = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">\n",
        "<dict>\n",
    ));
    out.push_str(&format!(
        "\t<key>Label</key>\n\t<string>{}</string>\n",
        profile.get("launchd_label")
    ));
    out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for arg in daemon_args(profile) {
        out.push_str(&format!("\t\t<string>{arg}</string>\n"));
    }
    out.push_str("\t</array>\n");
    out.push_str(&format!(
        "\t<key>UserName</key>\n\t<string>{role}</string>\n\t<key>GroupName</key>\n\t<string>{role}</string>\n"
    ));
    out.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n");
    out.push_str("\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n");
    out.push_str("\t<key>ExitTimeOut</key>\n\t<integer>30</integer>\n");
    out.push_str("\t<key>ProcessType</key>\n\t<string>Interactive</string>\n");
    out.push_str(&format!(
        "\t<key>StandardErrorPath</key>\n\t<string>{ERR_LOG}</string>\n"
    ));
    for limits in ["SoftResourceLimits", "HardResourceLimits"] {
        out.push_str(&format!(
            "\t<key>{limits}</key>\n\t<dict>\n\t\t<key>NumberOfFiles</key>\n\t\t<integer>65536</integer>\n\t</dict>\n"
        ));
    }
    out.push_str("</dict>\n</plist>\n");
    out
}

/// Whether the error log is a regular file with one link, the role account's: the only
/// file `restart` may empty as root. Its directory is root's, so no link replaces it.
fn err_log_safe(c: &Ctx<'_>) -> bool {
    c.mac
        .lstat(ERR_LOG)
        .is_some_and(|s| s.kind == '-' && s.links == 1 && s.uid == c.profile.role_id)
}

/// The installed property list is the one the profile renders, root's, mode 644; the
/// log directory root's, mode 755; the error log safe and the role account's, mode
/// 644; and the job loaded.
pub(crate) fn launchd_label(c: &Ctx<'_>) -> Outcome {
    let label = c.value();
    let plist = plist_path(c.profile);
    let id = c.profile.role_id;
    let role_file = format!("-rw-r--r-- {id} {id}");
    let plist_owner = c.mac.ls(&plist);
    let dir_owner = c.mac.ls(LOG_DIR);
    let log_owner = c.mac.ls(ERR_LOG);
    if c.mac.read(&plist).as_deref() != Some(render_plist(c.profile).as_bytes()) {
        fail(format!("{plist} is missing or differs from the profile"))
    } else if plist_owner != ROOT_FILE {
        fail(format!("{plist} is {plist_owner}, want {ROOT_FILE}"))
    } else if dir_owner != ROOT_DIR {
        fail(format!("{LOG_DIR} is {dir_owner}, want {ROOT_DIR}"))
    } else if !err_log_safe(c) {
        fail(format!(
            "{ERR_LOG} is {log_owner}, want a regular file with one link, {role_file}"
        ))
    } else if log_owner != role_file {
        fail(format!("{ERR_LOG} is {log_owner}, want {role_file}"))
    } else if !c
        .mac
        .succeeds(LAUNCHCTL, &["print", &format!("system/{label}")])
    {
        fail(format!("system/{label} is not loaded"))
    } else {
        pass(format!(
            "system/{label} loaded, as {}, ProcessType Interactive",
            c.profile.get("role_user")
        ))
    }
}

/// Why `restart` did not restart the job.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RestartError {
    /// The error log is not a file `restart` may empty as root; nothing was stopped.
    #[error(
        "{ERR_LOG} is not a regular file of {role} with one link; nothing restarted (apply replaces it)"
    )]
    UnsafeLog { role: String },
    /// The error log could not be emptied; the job was started again.
    #[error("cannot empty the error log ({0}); the job was started again")]
    Truncate(String),
    /// `launchctl bootstrap` failed ten times, a second apart.
    #[error("launchctl bootstrap system {plist} failed")]
    NotStarted { plist: String },
}

/// How many times `launchctl bootstrap` is tried, a second apart.
const BOOTSTRAP_TRIES: u32 = 10;

/// Stops the job, empties its error log while it is stopped (launchd keeps the
/// descriptor open while the job runs, so the file is never rotated), and starts it
/// again. Refuses, having stopped nothing, when the error log is not safe to empty.
pub(crate) fn restart_job(c: &Ctx<'_>) -> Result<(), RestartError> {
    if !err_log_safe(c) {
        return Err(RestartError::UnsafeLog {
            role: c.profile.get("role_user").to_owned(),
        });
    }
    let label = c.profile.get("launchd_label");
    let plist = plist_path(c.profile);
    let real_plist = c.mac.path(&plist);
    c.mac
        .succeeds(LAUNCHCTL, &["bootout", &format!("system/{label}")]);
    let emptied = c.mac.truncate(ERR_LOG);
    let mut started = false;
    for attempt in 1..=BOOTSTRAP_TRIES {
        if c.mac
            .succeeds(LAUNCHCTL, &["bootstrap", "system", &real_plist])
        {
            started = true;
            break;
        }
        if attempt < BOOTSTRAP_TRIES {
            c.mac.pause(1);
        }
    }
    if !started {
        return Err(RestartError::NotStarted { plist });
    }
    emptied.map_err(|e| RestartError::Truncate(e.to_string()))
}

/// Makes the log directory root's, mode 755; replaces anything at the error log's path
/// that is not safe to empty (unlinked, never followed) with a new file of the role
/// account, mode 644; writes the property list, root's, mode 644; and restarts the job.
pub(crate) fn fix_launchd_label(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let id = c.profile.role_id;
    c.mac.dir(LOG_DIR, 0, 0, 0o755)?;
    if !err_log_safe(c) {
        c.mac.new_file(ERR_LOG, id, id, 0o644)?;
    }
    c.mac.chmod(ERR_LOG, 0o644)?;
    let plist = plist_path(c.profile);
    c.mac.mkdir(LAUNCH_DAEMONS)?;
    c.mac
        .replace(&plist, render_plist(c.profile).as_bytes(), 0, 0, 0o644)?;
    match restart_job(c) {
        Ok(()) => out.change(
            c.key,
            format!("wrote {plist} and restarted the job (error log truncated)"),
        ),
        Err(e) => out.change(
            c.key,
            format!("wrote {plist}; the job did not restart: {e}"),
        ),
    }
    Ok(())
}
