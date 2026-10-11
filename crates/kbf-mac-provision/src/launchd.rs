//! The LaunchDaemon that runs kbf-daemon (docs/design/mac-node-provisioning.md,
//! section 5.3): its property list, rendered from the profile, and the check that the
//! installed one is that file, root's, with a safe error log and the job loaded.

use crate::items::{Ctx, Outcome, fail, pass};
use crate::profile::Profile;

const LAUNCHCTL: &str = "/bin/launchctl";
pub const DAEMON_BIN: &str = "/usr/local/kbf/current/kbf-daemon";
pub const LOG_DIR: &str = "/Library/Logs/kbf";
pub const ERR_LOG: &str = "/Library/Logs/kbf/kbf-daemon.err.log";
const ROOT_FILE: &str = "-rw-r--r-- 0 0";
const ROOT_DIR: &str = "drwxr-xr-x 0 0";

/// The property list's path on the Mac.
pub fn plist_path(profile: &Profile) -> String {
    format!(
        "/Library/LaunchDaemons/{}.plist",
        profile.get("launchd_label")
    )
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
