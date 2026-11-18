//! What the two update rehearsals share: a Linux host laid out by hand from release N, as a
//! farm node is, with release N+1 named and checked, the helper's unit and the `kbf` unit
//! written, and the helpers that run commands and keep their output.
//!
//! N+1 is the release named in `/var/tmp/kbf-update-rehearsal.to`, or the newest published
//! release when the file is absent; N is the published release before it. The host gets:
//! release N's `kbf` laid out by N's `kbf-deploy host install` under
//! `/var/lib/kbf-installer/releases`; N's `kbf-host-install` at `/usr/libexec/kbf`; the trust
//! files of N's package (`/usr/share/kbf/trust`) and its accounts; the helper's unit and the
//! updater's account from this checkout's `packaging/host-agent/linux`, the unit's
//! `--restart-unit` pointed at the one `kbf` unit here (an operator edits that line the same
//! way); and that `kbf` unit, a storage node that runs `releases/current/kbf`.
//!
//! The installer first, as on a real host: when N+1 declares a `protocol`, `min_protocol` or
//! `state_schema` above N's, which is what N's installer understands (both read the same
//! numbers of their build), N's installer refuses N+1 (`installer_too_old`), and an operator
//! puts the newer installer on the host before the update (`docs/deploy/host-install.md`). The
//! rehearsal does the same: N+1's `kbf-host-install` replaces N's at `/usr/libexec/kbf` before
//! the helper starts ([`installer_for`]). Otherwise the helper stays N's, so a release N's
//! helper cannot take for any other reason still fails here.
//!
//! Every command's output and the units' journal go to `target/update-rehearsal` (the
//! workflow uploads it). The workflow leaves its read-only token in
//! `/var/tmp/kbf-update-rehearsal.token`; the channel's `token_file` is a root-owned copy.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::Duration;

use kbf_host_install::LONG_TOKEN_MARKER;
use serde_json::Value;

pub mod farm;
pub mod fleet;
pub mod issuer;

pub const REPO: &str = "komira-ai/kbf";
pub const TOKEN_FILE: &str = "/var/tmp/kbf-update-rehearsal.token";
pub const TO_FILE: &str = "/var/tmp/kbf-update-rehearsal.to";
/// The host's working directory: the downloads, the farm's files, and the copy of the test
/// binary the updater's account runs.
pub const WORK: &str = "/var/lib/kbf-update-rehearsal";
pub const SOCKET: &str = "/run/kbf-host-install/local.sock";
pub const RELEASES: &str = "/var/lib/kbf-installer/releases";
pub const HELPER: &str = "/usr/libexec/kbf/kbf-host-install";
pub const TRUST: &str = "/usr/share/kbf/trust";
pub const CHANNELS: &str = "/etc/kbf/host-install-releases.toml";
/// The channel's token file, root's alone.
pub const CHANNEL_TOKEN: &str = "/etc/kbf/host-install-github.token";
pub const UNIT_DIR: &str = "/etc/systemd/system";
pub const HELPER_UNIT: &str = "kbf-host-local.service";
/// The `kbf` unit the helper restarts.
pub const KBF_UNIT: &str = "kbf-rehearsal.service";
/// The `--restart-unit` of the packaged helper unit, replaced by [`KBF_UNIT`].
pub const PACKAGED_RESTART: &str = "--restart-unit kbf-storage.service";
/// The layout of the packaged helper unit (the `kbf` deb's), taken out: the rehearsal's host is
/// laid out by hand, as a hand-laid host places the unit (`packaging/host-agent/README.md`).
pub const PACKAGED_LAYOUT: &str = " --layout deb";
pub const CHANNEL: &str = "stable";
pub const UPDATER: &str = "kbf-updater";
/// How many times a unit may restart before it is ready: more means it fails at start, and the
/// rehearsal stops instead of waiting for the job's limit.
pub const MAX_RESTARTS: u32 = 3;

/// Where the raw outputs go.
pub fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/update-rehearsal")
}

pub fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Writes a command's output under `name`.
pub fn capture(name: &str, out: &Output) {
    let dir = out_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{name}.stdout")), &out.stdout).unwrap();
    fs::write(dir.join(format!("{name}.stderr")), &out.stderr).unwrap();
    fs::write(
        dir.join(format!("{name}.code")),
        format!("{:?}\n", out.status.code()),
    )
    .unwrap();
}

/// Like [`run`], and when `cmd` fails while the journal of `units` shows GitHub refused the
/// helper for its quota, the failure says so ([`rate_limit_note`]).
pub fn run_explaining(name: &str, cmd: &mut Command, units: &[&str]) -> String {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{name}: did not start: {e}"));
    capture(name, &out);
    if !out.status.success()
        && let Some(note) = rate_limit_note(&journal(units))
    {
        panic!(
            "{name} failed ({:?}): {}",
            out.status.code(),
            explained(name, &note, &String::from_utf8_lossy(&out.stderr))
        );
    }
    check(name, &out)
}

/// Runs `cmd`, keeps its output as `name`, and requires it to succeed.
pub fn run(name: &str, cmd: &mut Command) -> String {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{name}: did not start: {e}"));
    capture(name, &out);
    check(name, &out)
}

/// `out`'s stdout, when the command succeeded; a panic with its output when it did not.
fn check(name: &str, out: &Output) -> String {
    assert!(
        out.status.success(),
        "{name} failed ({:?}): {}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn gh(token: &str) -> Command {
    let mut cmd = Command::new("gh");
    cmd.env("GH_TOKEN", token);
    cmd
}

/// The published releases, newest first: not drafts, not pre-releases.
pub fn release_tags(token: &str) -> Vec<String> {
    let text = run(
        "releases",
        gh(token).args([
            "api",
            &format!("repos/{REPO}/releases?per_page=30"),
            "--jq",
            ".[] | select(.draft | not) | select(.prerelease | not) | .tag_name",
        ]),
    );
    text.lines().map(str::to_owned).collect()
}

/// The commit a release's `SHA256SUMS` names for the Linux x86_64 tarball of `kbf`.
pub fn commit_of(sums: &str) -> String {
    let found: Vec<&str> = sums
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter_map(|name| name.strip_prefix("kbf-"))
        .filter_map(|rest| rest.strip_suffix("-linux-x86_64.tar.gz"))
        .filter(|c| c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect();
    assert_eq!(found.len(), 1, "one kbf linux-x86_64 tarball in SHA256SUMS");
    found[0].to_owned()
}

/// One release's files, downloaded into `dir` and checked against its `SHA256SUMS`.
pub fn download(token: &str, tag: &str, dir: &Path, names: &[String]) {
    fs::create_dir_all(dir).unwrap();
    let mut cmd = gh(token);
    cmd.args(["release", "download", tag, "-R", REPO, "-D"])
        .arg(dir)
        .args(["--clobber", "-p", "SHA256SUMS"]);
    for name in names {
        cmd.args(["-p", name]);
    }
    run(&format!("download_{tag}"), &mut cmd);
    if names.is_empty() {
        return;
    }
    let checked = run(
        &format!("sha256_{tag}"),
        Command::new("sha256sum")
            .args(["--check", "--strict", "--ignore-missing", "SHA256SUMS"])
            .current_dir(dir),
    );
    for name in names {
        assert!(
            checked.lines().any(|l| l == format!("{name}: OK")),
            "{tag}: {name} is not checked OK: {checked}"
        );
    }
}

/// What the helper's journal says when GitHub refuses its release listing.
pub const RATE_LIMITED: &str = "rate limited by GitHub";

/// How many of the last lines of a command's stderr go after the note.
const STDERR_LINES: usize = 40;

/// The plain words for a failure whose journal shows GitHub refused the helper for its quota;
/// `None` for any other journal.
///
/// The wording is of what the journal says, not of whether the channel had a token: GitHub's
/// API quota for this request, whichever address or token it was counted against.
pub fn rate_limit_note(journal: &str) -> Option<String> {
    journal.contains(RATE_LIMITED).then(|| {
        "GitHub's API quota refused the helper's release listing (counted against the runner's \
         address when the channel had no token, against the run's token when it had one). \
         This says nothing of the release. Re-run the job."
            .to_owned()
    })
}

/// The failure message of a command whose journal showed the quota refusal: the note, then
/// the last [`STDERR_LINES`] lines of the command's `stderr`, so that an earlier refusal in
/// the journal cannot hide a different cause the command itself reports.
pub fn explained(name: &str, note: &str, stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().collect();
    let tail = lines[lines.len().saturating_sub(STDERR_LINES)..].join("\n");
    format!("{name} failed: {note}\n---- last lines of {name}'s stderr\n{tail}")
}

/// The units' journal so far.
pub fn journal(units: &[&str]) -> String {
    let mut cmd = Command::new("journalctl");
    cmd.arg("--no-pager");
    for unit in units {
        cmd.args(["-u", unit]);
    }
    let out = cmd.output().expect("journalctl");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The units' journal and the log files named, written when the rehearsal ends, whichever
/// way; printed when it ends in a failure.
pub struct Journal {
    pub units: Vec<String>,
    pub logs: Vec<PathBuf>,
}

impl Journal {
    pub fn of(units: &[&str]) -> Self {
        Self {
            units: units.iter().map(|u| (*u).to_owned()).collect(),
            logs: Vec::new(),
        }
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = fs::remove_file(CHANNEL_TOKEN);
        let mut cmd = Command::new("journalctl");
        cmd.arg("--no-pager");
        for unit in &self.units {
            cmd.args(["-u", unit]);
        }
        let failing = std::thread::panicking();
        if let Ok(out) = cmd.output() {
            capture("journal", &out);
            if failing {
                eprintln!("---- journal\n{}", String::from_utf8_lossy(&out.stdout));
            }
        }
        if failing {
            for log in &self.logs {
                let text = fs::read_to_string(log).unwrap_or_default();
                eprintln!("---- {}\n{text}", log.display());
            }
        }
    }
}

/// The uid and gid of an account, from `/etc/passwd`.
pub fn ids_of(account: &str) -> (u32, u32) {
    let passwd = fs::read_to_string("/etc/passwd").unwrap();
    let fields: Vec<&str> = passwd
        .lines()
        .find(|l| l.starts_with(&format!("{account}:")))
        .unwrap_or_else(|| panic!("no account {account}"))
        .split(':')
        .collect();
    (fields[2].parse().unwrap(), fields[3].parse().unwrap())
}

pub fn systemctl(name: &str, args: &[&str]) -> String {
    run(name, Command::new("systemctl").args(args))
}

/// How many times `unit` has been restarted by systemd.
pub fn restarts(unit: &str) -> u32 {
    systemctl("restarts", &["show", "-p", "NRestarts", "--value", unit])
        .trim()
        .parse()
        .unwrap()
}

/// Waits until `ready` holds, and stops the rehearsal when `unit` restarts more than
/// [`MAX_RESTARTS`] times first: it fails at start, and would never be ready.
pub fn until(unit: &str, mut ready: impl FnMut() -> bool) {
    while !ready() {
        let n = restarts(unit);
        assert!(
            n <= MAX_RESTARTS,
            "{unit} restarted {n} times before it was ready"
        );
        sleep(Duration::from_secs(1));
    }
}

/// Whether something accepts a connection at `addr`.
pub fn answers(addr: &str) -> bool {
    std::net::TcpStream::connect(addr).is_ok()
}

pub fn install_file(text: &str, path: &str, mode: u32) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Whose `kbf-host-install` the host runs when it is asked for N+1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installer {
    /// Release N's, the one the host was laid out with.
    Previous,
    /// Release N+1's, put on the host first: N+1 declares a number N's installer does not
    /// understand.
    Next,
}

/// The numbers of `kbf --version --json` an installer must understand to stage a release, in
/// the order its refusal names them (`kbf_installer::stage::supported`).
pub const UNDERSTOOD: [&str; 3] = ["protocol", "min_protocol", "state_schema"];

/// Whose installer the host needs for N+1, from the version facts of N (`from`) and N+1
/// (`to`): N+1's when any of the [`UNDERSTOOD`] numbers N+1 declares is above N's, since an
/// installer understands the numbers its own build's `kbf` declares and no more; N's
/// otherwise, a lower number included. A fact that is not a number stops the rehearsal.
pub fn installer_for(from: &Value, to: &Value) -> Installer {
    let number = |facts: &Value, which: &str, key: &str| {
        facts[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{which}'s `{key}` is not a number: {facts}"))
    };
    let raised = UNDERSTOOD
        .iter()
        .any(|key| number(to, "N+1", key) > number(from, "N", key));
    if raised {
        Installer::Next
    } else {
        Installer::Previous
    }
}

/// One release of the rehearsal.
pub struct Release {
    pub tag: String,
    /// The full commit its `SHA256SUMS` names.
    pub commit: String,
    /// Where its files are downloaded.
    pub dir: PathBuf,
}

/// The rehearsal: the two releases, and what is known of N+1 before anything is changed.
pub struct Rehearsal {
    pub token: String,
    pub from: Release,
    pub to: Release,
    pub work: PathBuf,
    /// N+1's version as a host reports it: `<semver>+g<12 hex digits of the commit>`, from
    /// N+1's version facts.
    pub version: String,
    /// Whose `kbf-host-install` the host runs for the update.
    pub installer: Installer,
}

/// A release's version facts (`kbf --version --json` of its Linux x86_64 `kbf`), downloaded
/// into its directory.
fn facts_of(token: &str, tag: &str, dir: &Path, commit: &str) -> Value {
    let name = format!("kbf-{commit}-linux-x86_64.version.json");
    download(token, tag, dir, std::slice::from_ref(&name));
    let text = fs::read_to_string(dir.join(&name)).unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{tag}: {name}: {e}"))
}

/// The two releases, from the token the workflow left and the listing of the releases.
pub fn plan() -> Rehearsal {
    assert!(
        rustix::process::geteuid().is_root(),
        "run through `cargo xtask sudo-test`"
    );
    let token = fs::read_to_string(TOKEN_FILE)
        .unwrap_or_else(|e| panic!("{TOKEN_FILE}: {e}"))
        .trim()
        .to_owned();
    let tags = release_tags(&token);
    let to = match fs::read_to_string(TO_FILE) {
        Ok(text) => text.trim().to_owned(),
        Err(_) => tags.first().expect("a published release").clone(),
    };
    let from = tags
        .iter()
        .skip_while(|t| **t != to)
        .nth(1)
        .unwrap_or_else(|| panic!("no published release before {to} in {tags:?}"))
        .clone();
    println!("rehearsing the update from {from} to {to}");

    let work = Path::new(WORK).to_path_buf();
    fs::create_dir_all(&work).unwrap();
    fs::set_permissions(&work, fs::Permissions::from_mode(0o755)).unwrap();

    // The commits, from each release's SHA256SUMS; N+1's version and whose installer the
    // host needs, from their version facts.
    let (old_dir, new_dir) = (work.join(&from), work.join(&to));
    download(&token, &from, &old_dir, &[]);
    let old = commit_of(&fs::read_to_string(old_dir.join("SHA256SUMS")).unwrap());
    download(&token, &to, &new_dir, &[]);
    let new = commit_of(&fs::read_to_string(new_dir.join("SHA256SUMS")).unwrap());
    assert_ne!(old, new, "{from} and {to} are the same commit");
    let old_facts = facts_of(&token, &from, &old_dir, &old);
    let facts = facts_of(&token, &to, &new_dir, &new);
    let version = format!(
        "{}+g{}",
        facts["version"].as_str().expect("a version"),
        &new[..12]
    );
    let installer = installer_for(&old_facts, &facts);
    if installer == Installer::Next {
        println!(
            "{to} declares a number {from}'s installer does not understand: \
             the host gets {to}'s kbf-host-install first"
        );
    }
    Rehearsal {
        token,
        from: Release {
            tag: from,
            commit: old,
            dir: old_dir,
        },
        to: Release {
            tag: to,
            commit: new,
            dir: new_dir,
        },
        work,
        version,
        installer,
    }
}

/// The `kbf` unit: a storage node on a `file://` cold store that runs `releases/current/kbf`,
/// as `account` (a `User=` or `DynamicUser=` line), with `flags` on its command line.
pub fn kbf_unit(account: &str, flags: &str) -> String {
    format!(
        "[Unit]\n\
         Description=kbf storage node of the update rehearsal, run from the current release\n\
         \n\
         [Service]\n\
         {account}\n\
         StateDirectory=kbf-rehearsal\n\
         WorkingDirectory=/var/lib/kbf-rehearsal\n\
         ExecStart={RELEASES}/current/kbf --mode=storage \
         --hot-store=file:///var/lib/kbf-rehearsal/hot \
         --cold-store=file:///var/lib/kbf-rehearsal/cold {flags}\n\
         Restart=on-failure\n\
         RestartSec=2\n"
    )
}

/// Release N's files: its `kbf`, `kbf-host-install`, `kbf-deploy` and package, downloaded
/// and unpacked under its directory: `release/`, `helper/`, `deploy/` and `deb/`; and, when
/// the host needs N+1's installer ([`Installer::Next`]), N+1's `kbf-host-install`, unpacked
/// under N+1's directory's `helper/`.
pub fn fetch_old_release(r: &Rehearsal) {
    let (old, dir) = (&r.from.commit, &r.from.dir);
    let names = ["kbf", "kbf-host-install", "kbf-deploy"]
        .map(|c| format!("{c}-{old}-linux-x86_64.tar.gz"))
        .to_vec();
    let deb = format!("kbf-{old}-linux-x86_64.deb");
    let mut all = names.clone();
    all.push(deb.clone());
    download(&r.token, &r.from.tag, dir, &all);
    for (name, into) in names.iter().zip(["release", "helper", "deploy"]) {
        let into = dir.join(into);
        fs::create_dir_all(&into).unwrap();
        run(
            &format!("untar_{}", into.file_name().unwrap().to_string_lossy()),
            Command::new("tar")
                .arg("-xzf")
                .arg(dir.join(name))
                .arg("-C")
                .arg(&into),
        );
    }
    run(
        "dpkg_deb",
        Command::new("dpkg-deb")
            .arg("-x")
            .arg(dir.join(&deb))
            .arg(dir.join("deb")),
    );
    if r.installer == Installer::Next {
        let (new, dir) = (&r.to.commit, &r.to.dir);
        let name = format!("kbf-host-install-{new}-linux-x86_64.tar.gz");
        download(&r.token, &r.to.tag, dir, std::slice::from_ref(&name));
        let into = dir.join("helper");
        fs::create_dir_all(&into).unwrap();
        run(
            "untar_next_helper",
            Command::new("tar")
                .arg("-xzf")
                .arg(dir.join(&name))
                .arg("-C")
                .arg(&into),
        );
    }
}

/// The directory whose `kbf-host-install` the host runs: N's, or N+1's when it needs N+1's
/// installer.
fn installer_dir(r: &Rehearsal) -> PathBuf {
    match r.installer {
        Installer::Previous => r.from.dir.join("helper"),
        Installer::Next => r.to.dir.join("helper"),
    }
}

/// Lays the host out from release N: the trust files and accounts of N's package, the
/// updater's account, N's `kbf` through its own `kbf-deploy`, N's helper (then N+1's in its
/// place, installer first, when the host needs it: [`Installer::Next`]), the channel, and the
/// units (`kbf_unit` for the `kbf` unit, the packaged helper unit pointed at it). Nothing is
/// started. Returns the checkout's `packaging/host-agent/linux`.
pub fn lay_out(r: &Rehearsal, kbf_unit: &str) -> PathBuf {
    let (old, dir) = (&r.from.commit, &r.from.dir);
    let unpacked = dir.join("deb");
    // The hosted runner's image leaves `/usr/share` writable by everyone (0777), which the
    // helper refuses above its trust files, as it should; a host's is 0755.
    fs::set_permissions("/usr/share", fs::Permissions::from_mode(0o755)).unwrap();
    // The trust files and accounts of N's package, then the updater's account of this checkout.
    run(
        "trust_dir",
        Command::new("install").args(["-d", "-o", "root", "-g", "root", "-m", "0755", TRUST]),
    );
    for entry in fs::read_dir(unpacked.join(TRUST.trim_start_matches('/'))).unwrap() {
        let path = entry.unwrap().path();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        run(
            "trust_file",
            Command::new("install")
                .args(["-o", "root", "-g", "root", "-m", &format!("{mode:o}")])
                .arg(&path)
                .arg(Path::new(TRUST).join(path.file_name().unwrap())),
        );
    }
    let sysusers = unpacked.join("usr/lib/sysusers.d");
    if sysusers.is_dir() {
        for entry in fs::read_dir(&sysusers).unwrap() {
            run(
                "sysusers_package",
                Command::new("systemd-sysusers").arg(entry.unwrap().path()),
            );
        }
    }
    let host_agent = checkout().join("packaging/host-agent/linux");
    run(
        "sysusers_updater",
        Command::new("systemd-sysusers").arg(host_agent.join("kbf-updater.conf")),
    );

    // Release N laid out by its own kbf-deploy, and its helper.
    run(
        "host_install",
        Command::new(dir.join("deploy/kbf-deploy"))
            .args(["host", "install", "--commit", old, "--from"])
            .arg(dir.join("release"))
            .arg("--apply"),
    );
    run(
        "helper_binary",
        Command::new("install")
            .args(["-D", "-o", "root", "-g", "root", "-m", "0755"])
            .arg(dir.join("helper/kbf-host-install"))
            .arg(HELPER),
    );
    // The installer first: N+1's helper over N's, before the helper starts, as an operator
    // puts the newer installer on a host before an update its own installer refuses.
    let installer = installer_dir(r).join("kbf-host-install");
    if r.installer == Installer::Next {
        run(
            "helper_binary_next",
            Command::new("install")
                .args(["-D", "-o", "root", "-g", "root", "-m", "0755"])
                .arg(&installer)
                .arg(HELPER),
        );
    }
    fs::create_dir_all("/etc/kbf").unwrap();
    // The channel as the farm's nodes name theirs, with a token file when the host's helper
    // takes GitHub's Actions token: a hosted runner's address shares GitHub's anonymous quota
    // with other jobs, so the helper lists the releases under the run's own read-only token.
    // The helper reads a token file that is root's alone, so the workflow's file (the runner
    // user's) is copied to one root owns, and removed when the rehearsal ends.
    let _ = fs::remove_file(CHANNEL_TOKEN);
    let channel = if takes_actions_token(&installer) {
        let token = fs::read_to_string(TOKEN_FILE).unwrap_or_else(|e| panic!("{TOKEN_FILE}: {e}"));
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(CHANNEL_TOKEN)
                .unwrap_or_else(|e| panic!("{CHANNEL_TOKEN}: {e}"));
            file.write_all(token.trim().as_bytes()).unwrap();
        }
        format!("[channels.{CHANNEL}]\nsource = \"github\"\ntoken_file = \"{CHANNEL_TOKEN}\"\n")
    } else {
        // Only the first release after the helper learned the token can be here: its
        // predecessor's helper refuses a token of this length (`config_invalid`).
        eprintln!(
            "the host's helper ({}) takes no Actions token: its channel names none",
            installer.display()
        );
        format!("[channels.{CHANNEL}]\nsource = \"github\"\n")
    };
    install_file(&channel, CHANNELS, 0o644);

    // The units: the kbf unit, and the helper's unit of this checkout restarting it.
    install_file(kbf_unit, &format!("{UNIT_DIR}/{KBF_UNIT}"), 0o644);
    let packaged = fs::read_to_string(host_agent.join(HELPER_UNIT)).unwrap();
    install_file(
        &hand_laid_helper(&packaged),
        &format!("{UNIT_DIR}/{HELPER_UNIT}"),
        0o644,
    );
    systemctl("daemon_reload", &["daemon-reload"]);
    host_agent
}

/// The packaged helper unit as the rehearsal's hand-laid host places it: its one `ExecStart` line
/// restarts [`KBF_UNIT`] and has no `--layout deb`; every other line (the comments, which name the
/// flag too) is the package's.
pub fn hand_laid_helper(packaged: &str) -> String {
    let execs = packaged
        .lines()
        .filter(|l| l.starts_with("ExecStart="))
        .count();
    assert_eq!(execs, 1, "one ExecStart in the packaged unit: {packaged}");
    packaged
        .lines()
        .map(|line| {
            if !line.starts_with("ExecStart=") {
                return line.to_owned();
            }
            assert_eq!(line.matches(PACKAGED_RESTART).count(), 1, "{line}");
            assert_eq!(line.matches(PACKAGED_LAYOUT).count(), 1, "{line}");
            line.replace(PACKAGED_RESTART, &format!("--restart-unit {KBF_UNIT}"))
                .replace(PACKAGED_LAYOUT, "")
        })
        .map(|line| line + "\n")
        .collect()
}

/// Whether the helper binary at `path` carries [`LONG_TOKEN_MARKER`].
fn takes_actions_token(path: &Path) -> bool {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    bytes
        .windows(LONG_TOKEN_MARKER.len())
        .any(|w| w == LONG_TOKEN_MARKER)
}

/// Starts the helper and waits for its socket.
pub fn start_helper() {
    systemctl("start_helper", &["start", HELPER_UNIT]);
    until(HELPER_UNIT, || Path::new(SOCKET).exists());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped helper unit, as the rehearsal places it on its hand-laid host: the command
    /// line lays releases out by hand (no `--layout`) and restarts the rehearsal's unit, and the
    /// rest of the file, comments that name `--layout deb` among them, is the package's.
    ///
    /// Catches: a rehearsal that refuses the shipped unit because its comments name the flag too,
    /// or that leaves the package layout on a host laid out by hand.
    #[test]
    fn the_hand_laid_host_gets_the_packaged_helper_without_the_package_layout() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/host-agent/linux")
            .join(HELPER_UNIT);
        let packaged = fs::read_to_string(&path).unwrap();
        assert!(
            packaged.matches(PACKAGED_LAYOUT).count() > 1,
            "the comments name it"
        );
        let placed = hand_laid_helper(&packaged);
        let exec: Vec<&str> = placed
            .lines()
            .filter(|l| l.starts_with("ExecStart="))
            .collect();
        assert_eq!(
            exec,
            [format!(
                "ExecStart=/usr/libexec/kbf/kbf-host-install --serve --socket {SOCKET} \
                 --socket-user {UPDATER} --window-secs 600 --restart-unit {KBF_UNIT}"
            )]
        );
        let others = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|l| !l.starts_with("ExecStart="))
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(others(&placed), others(&packaged));
    }

    /// Version facts in the form a release publishes them (`kbf --version --json`): protocol
    /// 1, min_protocol 1, state_schema 3.
    const BEFORE: &str = r#"{"admin_protocol":1,"commit":"aaaaaaaaaaaa","commit_full":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","meta_log":5,"min_protocol":1,"profile":"release","protocol":1,"relay":1,"seq":7629,"state_schema":3,"version":"0.1.0"}"#;

    /// [`BEFORE`] with `key` set to `value`.
    fn with(key: &str, value: Value) -> Value {
        let mut facts: Value = serde_json::from_str(BEFORE).unwrap();
        facts[key] = value;
        facts
    }

    /// The case of a release that adds a state schema: N declares `state_schema` 3, N+1
    /// declares 4, as N's installer refuses ("the release needs a newer installer:
    /// state_schema 4; this installer understands up to 3"). The host gets N+1's installer,
    /// from N+1's own directory.
    ///
    /// Catches the rehearsal staging N+1 with N's installer whatever N+1 declares.
    #[test]
    fn a_release_one_state_schema_above_gets_its_own_installer_first() {
        let from: Value = serde_json::from_str(BEFORE).unwrap();
        let to = with("state_schema", 4.into());
        assert_eq!(installer_for(&from, &to), Installer::Next);
        let r = Rehearsal {
            token: String::new(),
            from: Release {
                tag: "v0.0.1".to_owned(),
                commit: "a".repeat(40),
                dir: PathBuf::from("work/v0.0.1"),
            },
            to: Release {
                tag: "v0.0.2".to_owned(),
                commit: "b".repeat(40),
                dir: PathBuf::from("work/v0.0.2"),
            },
            work: PathBuf::from("work"),
            version: String::new(),
            installer: installer_for(&from, &to),
        };
        assert_eq!(installer_dir(&r), Path::new("work/v0.0.2/helper"));
        let r = Rehearsal {
            installer: Installer::Previous,
            ..r
        };
        assert_eq!(installer_dir(&r), Path::new("work/v0.0.1/helper"));
    }

    /// Each number the installer checks, raised by one alone, needs N+1's installer; the same
    /// numbers, a lower state schema (N's installer understands it), and a change in the facts
    /// the installer does not check (the version, `seq`, `meta_log`, `admin_protocol`,
    /// `relay`) keep N's, so the rehearsal still proves N's helper takes N+1.
    ///
    /// Catches a number left out of the rule (a raised protocol staged with N's installer) and
    /// a rule that swaps the installer for any difference at all.
    #[test]
    fn only_a_raised_installer_number_swaps_the_installer() {
        let from: Value = serde_json::from_str(BEFORE).unwrap();
        for key in UNDERSTOOD {
            let raised = from[key].as_u64().unwrap() + 1;
            assert_eq!(
                installer_for(&from, &with(key, raised.into())),
                Installer::Next,
                "{key} raised"
            );
        }
        assert_eq!(installer_for(&from, &from), Installer::Previous);
        // A lower number, of each the installer checks, against N's facts with every number at 2.
        let mut high = from.clone();
        for key in UNDERSTOOD {
            high[key] = 2.into();
        }
        for key in UNDERSTOOD {
            let mut lower = high.clone();
            lower[key] = 1.into();
            assert_eq!(
                installer_for(&high, &lower),
                Installer::Previous,
                "{key} lower"
            );
        }
        // One number raised and another lowered: the raised one decides.
        let mut mixed = with("protocol", 2.into());
        mixed["state_schema"] = 2.into();
        assert_eq!(installer_for(&from, &mixed), Installer::Next);
        for (key, value) in [
            ("version", Value::from("0.2.0")),
            ("seq", 9999.into()),
            ("meta_log", 6.into()),
            ("admin_protocol", 2.into()),
            ("relay", 2.into()),
        ] {
            assert_eq!(
                installer_for(&from, &with(key, value)),
                Installer::Previous,
                "{key} changed"
            );
        }
    }

    /// Facts that lack a number the rule reads stop the rehearsal, naming the release and the
    /// key, rather than guessing whose installer to run.
    ///
    /// Catches a missing `state_schema` read as 0, which would keep N's installer.
    #[test]
    #[should_panic(expected = "N+1's `state_schema` is not a number")]
    fn facts_without_a_number_stop_the_rehearsal() {
        let from: Value = serde_json::from_str(BEFORE).unwrap();
        let mut to = from.clone();
        to.as_object_mut().unwrap().remove("state_schema");
        installer_for(&from, &to);
    }

    /// The same, for N's facts.
    #[test]
    #[should_panic(expected = "N's `protocol` is not a number")]
    fn previous_facts_without_a_number_stop_the_rehearsal() {
        let from = with("protocol", Value::from("1"));
        let to: Value = serde_json::from_str(BEFORE).unwrap();
        installer_for(&from, &to);
    }

    /// Catches a rate-limited helper reported as a bare "deploy_farm failed", and a journal
    /// that mentions nothing of the quota being blamed on it.
    #[test]
    fn a_refused_listing_is_explained_and_other_journals_are_not() {
        let refused = "kbf-host-install: stage: fetch_failed: rate limited by GitHub, retry after \
                       2026-11-01T05:45:24Z or set token_file\n";
        let note = rate_limit_note(refused).expect("a note for a rate-limited helper");
        assert!(!note.contains("takes no"), "{note}");
        assert!(note.contains("Re-run the job"), "{note}");
        assert_eq!(
            rate_limit_note("kbf-host-install: stage: verify_failed\n"),
            None
        );
        assert_eq!(rate_limit_note(""), None);
    }

    /// Catches the note replacing the command's own report: a different real cause must stay
    /// in the message, and a long stderr is cut to its last lines.
    #[test]
    fn the_explanation_keeps_the_end_of_the_commands_stderr() {
        let stderr: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let text = explained("deploy_farm", "NOTE", &stderr);
        assert!(text.contains("NOTE"), "{text}");
        assert!(text.contains("line 99"), "{text}");
        assert!(text.contains("line 60"), "{text}");
        assert!(
            !text.contains("line 59\n") && !text.ends_with("line 59"),
            "{text}"
        );
        assert!(!text.contains("line 0\n"), "{text}");
        assert!(explained("x", "N", "").contains("N"));
    }
}
