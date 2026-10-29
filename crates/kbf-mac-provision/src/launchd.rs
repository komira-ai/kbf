//! The LaunchDaemon that runs `kbf --mode=daemon` (docs/design/mac-node-provisioning.md,
//! section 5.3): the node's `kbf.toml`
//! (every setting of the daemon) and its property list (which names that file and nothing
//! else), both rendered from the profile; the check that the
//! installed one is that file, root's, with a safe error log and the job loaded; the
//! fix that makes it so; and the restart.
//!
//! `daemon_user` chooses the account the job runs as. `role`, the default, is the role
//! account, as before. `root` (docs/design/macos-end-state.md, step 2) drops `UserName`
//! and `GroupName`, makes the error log root's, and holds the node key and the scratch
//! and cache roots to root's (the `daemon_user` item). A profile sets `root` only once
//! kbf-daemon runs every action as a pool user and refuses uid 0 (step 3 of that
//! document); until then a root daemon would run actions as root.

use std::io;

use crate::items::{Ctx, Lines, Outcome, fail, pass};
use crate::pool::{self, ROOT_DIR};
use crate::profile::{DaemonUser, Profile};

const LAUNCHCTL: &str = "/bin/launchctl";
pub const KBF_BIN: &str = "/usr/local/kbf-installer/releases/current/kbf";
/// The node's config file, where the macOS package puts it (the default `--config` of
/// a `kbf` start on macOS). This tool writes it from the profile ([`render_config`]): it
/// is the one source of the daemon's settings, and the job's arguments name only it.
pub const NODE_CONFIG: &str = "/usr/local/etc/kbf/kbf.toml";
/// The directory of [`NODE_CONFIG`]: root's, so the daemon account cannot replace it.
const CONFIG_DIR: &str = "/usr/local/etc/kbf";
/// The two `[daemon]` keys earlier versions of this tool wrote after `data-dir`. The node
/// reads neither any more, so [`render_config`] omits them; a file that still has them is
/// accepted as it is ([`config_is_current`]), not reported as drift and not rewritten.
const LEGACY_UPDATE_KEYS: &str =
    "update-dir = \"/var/db/kbf-installer\"\norders-dir = \"/var/db/kbf-installer/orders\"\n";
pub const LOG_DIR: &str = "/Library/Logs/kbf";
pub const ERR_LOG: &str = "/Library/Logs/kbf/kbf-daemon.err.log";
const ROOT_FILE: &str = "-rw-r--r-- 0 0";
/// What `ls -ldn` shows for the node key of a root daemon.
const ROOT_KEY: &str = "-rw------- 0 0";
const LAUNCH_DAEMONS: &str = "/Library/LaunchDaemons";

/// The property list's path on the Mac.
pub fn plist_path(profile: &Profile) -> String {
    format!("{LAUNCH_DAEMONS}/{}.plist", profile.get("launchd_label"))
}

/// `kbf --mode=daemon`'s arguments: the binary, the mode and the config file, and nothing
/// else. Every setting is a key of that file ([`render_config`]), so the job has one
/// source of truth and a flag after `--config` could not be told from the file's.
pub fn daemon_args(_profile: &Profile) -> Vec<String> {
    [KBF_BIN, "--mode=daemon", "--config", NODE_CONFIG]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// The node's `kbf.toml`, which `apply` writes at [`NODE_CONFIG`] and `kbf --mode=daemon
/// --config` reads: the mode, and the flags the profile carries (the native driver, the
/// server and the CAS URL, the CA, certificate and key paths, the node id and the data
/// directory), one TOML table per flag group, a key per flag long name. The
/// tables are the ones of `GROUPS` in kbf-cli's `config.rs` (`identity` and `daemon`);
/// `tests/node_config.rs` loads the file with that crate's loader. A flag that may repeat
/// (`driver`) is an array. The file has no `label` key: node labels are removed, and the
/// next daemon release removes the key. No value needs TOML escaping: the profile's patterns
/// admit no `"`, `\` or control character.
pub fn render_config(profile: &Profile) -> String {
    let strings = |items: &[&str]| {
        let quoted: Vec<String> = items.iter().map(|i| format!("\"{i}\"")).collect();
        format!("[{}]", quoted.join(", "))
    };
    let mut out = String::from(concat!(
        "# Written by kbf-mac-provision from its profile; `apply` puts it back if it is edited.\n",
        "# One table per flag group, one key per flag long name (kbf-cli, config.rs).\n",
        "mode = \"daemon\"\n",
        "\n[identity]\n",
    ));
    for (key, value) in [
        ("server", "kbf_server"),
        ("cas", "kbf_cas"),
        ("ca-cert", "kbf_ca_cert"),
        ("cert", "kbf_cert"),
        ("key", "kbf_key"),
        ("node-id", "hostname"),
    ] {
        out.push_str(&format!("{key} = \"{}\"\n", profile.get(value)));
    }
    out.push_str("\n[daemon]\n");
    out.push_str(&format!("driver = {}\n", strings(&["native"])));
    out.push_str(&format!("data-dir = \"{}\"\n", profile.get("kbf_data_dir")));
    out
}

/// The account the job runs as, and its uid (also its gid).
fn daemon_account(profile: &Profile) -> (&str, u32) {
    match profile.daemon_user {
        DaemonUser::Role => (profile.get("role_user"), profile.role_id),
        DaemonUser::Root => ("root", 0),
    }
}

/// `ExitTimeOut`, the seconds launchd gives the daemon between SIGTERM and SIGKILL. The
/// daemon drains on SIGTERM until every lease has ended and launchd has no "infinity", so
/// this is the largest value its 32-bit integer holds (about 68 years).
const EXIT_TIME_OUT: u32 = i32::MAX as u32;

/// The property list: the job runs as the role account (no `UserName` or `GroupName`
/// for `daemon_user=root`: launchd runs it as root), `Interactive` (not throttled),
/// kept alive, with no practical limit to exit in (a drain waits for its leases), its stderr in [`ERR_LOG`] and 65536 open files. No
/// value needs XML escaping: the profile's values hold none of `<`, `>`, `&`.
pub fn render_plist(profile: &Profile) -> String {
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
    if profile.daemon_user == DaemonUser::Role {
        let role = profile.get("role_user");
        out.push_str(&format!(
            "\t<key>UserName</key>\n\t<string>{role}</string>\n\t<key>GroupName</key>\n\t<string>{role}</string>\n"
        ));
    }
    out.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n");
    out.push_str("\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n");
    out.push_str(&format!(
        "\t<key>ExitTimeOut</key>\n\t<integer>{EXIT_TIME_OUT}</integer>\n"
    ));
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

/// Whether `bytes` is the `kbf.toml` the profile renders, or that file with the two keys
/// earlier versions wrote ([`LEGACY_UPDATE_KEYS`]): the node ignores them, so a Mac
/// provisioned before they were dropped is not drift.
fn config_is_current(profile: &Profile, bytes: Option<&[u8]>) -> bool {
    let want = render_config(profile);
    bytes == Some(want.as_bytes())
        || bytes == Some(format!("{want}{LEGACY_UPDATE_KEYS}").as_bytes())
}

/// Whether the error log is a regular file with one link, the daemon account's: the
/// only file `restart` may empty as root. Its directory is root's, so no link replaces
/// it.
fn err_log_safe(c: &Ctx<'_>) -> bool {
    let (_, id) = daemon_account(c.profile);
    c.mac
        .lstat(ERR_LOG)
        .is_some_and(|s| s.kind == '-' && s.links == 1 && s.uid == id)
}

/// The installed property list is the one the profile renders, root's, mode 644; the
/// log directory root's, mode 755; the error log safe and the daemon account's, mode
/// 644; and the job loaded.
pub(crate) fn launchd_label(c: &Ctx<'_>) -> Outcome {
    let label = c.value();
    let plist = plist_path(c.profile);
    let (account, id) = daemon_account(c.profile);
    let role_file = format!("-rw-r--r-- {id} {id}");
    let plist_owner = c.mac.ls(&plist);
    let dir_owner = c.mac.ls(LOG_DIR);
    let log_owner = c.mac.ls(ERR_LOG);
    let config_owner = c.mac.ls(NODE_CONFIG);
    let config_dir_owner = c.mac.ls(CONFIG_DIR);
    if !config_is_current(c.profile, c.mac.read(NODE_CONFIG).as_deref()) {
        fail(format!(
            "{NODE_CONFIG} is missing or differs from the profile"
        ))
    } else if config_owner != ROOT_FILE {
        fail(format!("{NODE_CONFIG} is {config_owner}, want {ROOT_FILE}"))
    } else if config_dir_owner != ROOT_DIR {
        fail(format!(
            "{CONFIG_DIR} is {config_dir_owner}, want {ROOT_DIR}"
        ))
    } else if c.mac.read(&plist).as_deref() != Some(render_plist(c.profile).as_bytes()) {
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
            "system/{label} loaded, as {account}, ProcessType Interactive"
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
    UnsafeLog {
        /// The account the job runs as.
        role: String,
    },
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
            role: daemon_account(c.profile).0.to_owned(),
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
/// that is not safe to empty (unlinked, never followed) with a new file of the daemon
/// account, mode 644; writes the property list, root's, mode 644; and restarts the job.
pub(crate) fn fix_launchd_label(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let (_, id) = daemon_account(c.profile);
    c.mac.dir(LOG_DIR, 0, 0, 0o755)?;
    if !err_log_safe(c) {
        c.mac.new_file(ERR_LOG, id, id, 0o644)?;
    }
    c.mac.chmod(ERR_LOG, 0o644)?;
    c.mac.dir(CONFIG_DIR, 0, 0, 0o755)?;
    if !config_is_current(c.profile, c.mac.read(NODE_CONFIG).as_deref())
        || c.mac.ls(NODE_CONFIG) != ROOT_FILE
    {
        c.mac.replace(
            NODE_CONFIG,
            render_config(c.profile).as_bytes(),
            0,
            0,
            0o644,
        )?;
        out.change(c.key, format!("wrote {NODE_CONFIG}, root's, mode 644"));
    }
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

/// The roots a root daemon owns: the scratch root, and the cache when `kbf_cache` is
/// set.
fn roots(profile: &Profile) -> Vec<&str> {
    ["kbf_data_dir", "kbf_cache"]
        .into_iter()
        .map(|key| profile.get(key))
        .filter(|path| !path.is_empty())
        .collect()
}

/// Whether the node key is a regular file with one link, root's, mode 600.
fn key_safe(c: &Ctx<'_>) -> bool {
    let key = c.profile.get("kbf_key");
    c.mac
        .lstat(key)
        .is_some_and(|s| s.kind == '-' && s.links == 1 && s.ls() == ROOT_KEY)
}

/// For `daemon_user=root`: the node key is a regular file with one link, root's, mode
/// 600, and the scratch and cache roots are directories of root's, mode 755. For
/// `role` nothing is checked: the role account's daemon reads the key and writes the
/// scratch root.
pub(crate) fn daemon_user(c: &Ctx<'_>) -> Outcome {
    if c.profile.daemon_user == DaemonUser::Role {
        return pass(format!(
            "role: kbf-daemon runs as {}; the node key and the scratch and cache roots are checked once daemon_user=root",
            c.profile.get("role_user")
        ));
    }
    let key = c.profile.get("kbf_key");
    let mut wrong = Vec::new();
    let mut found = vec![format!("{key} {ROOT_KEY}")];
    if !key_safe(c) {
        wrong.push(format!(
            "{key} is {}, want {ROOT_KEY} with one link",
            c.mac.ls(key)
        ));
    }
    for root in roots(c.profile) {
        let have = c.mac.ls(root);
        if have != ROOT_DIR {
            wrong.push(format!("{root} is {have}, want {ROOT_DIR}"));
        }
        found.push(format!("{root} {ROOT_DIR}"));
    }
    if wrong.is_empty() {
        pass(format!("root: {}", found.join("; ")))
    } else {
        fail(wrong.join("; "))
    }
}

/// Makes the node key root's, mode 600, when it is a regular file with one link (any
/// other key is the operator's to install), and the scratch and cache roots
/// directories of root's, mode 755, only on the mounted volume.
pub(crate) fn fix_daemon_user(c: &Ctx<'_>, out: &mut Lines) -> io::Result<()> {
    let key = c.profile.get("kbf_key");
    match c.mac.lstat(key) {
        Some(s) if s.kind == '-' && s.links == 1 => {
            if !key_safe(c) {
                c.mac.chown(key, 0, 0)?;
                c.mac.chmod(key, 0o600)?;
                out.change(c.key, format!("{key} root's, mode 600"));
            }
        }
        _ => out.note(
            c.key,
            format!(
                "{key} is {}: apply sets the owner and mode of a regular file with one link only; install the key",
                c.mac.ls(key)
            ),
        ),
    }
    if !pool::volume_mounted(c) {
        return Err(pool::unmounted(c, "no root made"));
    }
    for root in roots(c.profile) {
        if c.mac.ls(root) != ROOT_DIR {
            pool::dir_of(
                c,
                out,
                root,
                (0, 0, 0o755),
                format!("{root} root's, mode 755"),
            )?;
        }
    }
    Ok(())
}
