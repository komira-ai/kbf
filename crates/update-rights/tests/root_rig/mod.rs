//! The rig of the root tests: a directory the hook may be given, accounts to run it as, the
//! probe hook, and the real `systemd-run`. Nothing here runs unless the test is run as root on a
//! host with systemd (`cargo xtask sudo-test -p update-rights --test rights_root`).
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, chown};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use update_hook::Step;
use update_rights::{
    Accounts, Declaration, EtcAccounts, Finished, Launch, Spawner, StepRun, Systemd, command,
    run_step,
};

/// The probe hook the tests run.
pub const PROBE: &str = env!("CARGO_BIN_EXE_update-rights-probe");
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

static GENERATION: AtomicU64 = AtomicU64::new(1);

/// Where the raw outputs go (the workflow uploads it).
pub fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/update-rights")
}

/// Keeps a result: printed to the log and written to a file of the artifact.
pub fn record(name: &str, text: &str) {
    println!("{text}");
    let dir = out_dir();
    fs::create_dir_all(&dir).expect("create the output directory");
    fs::write(dir.join(format!("{name}.txt")), format!("{text}\n")).expect("write the output");
}

/// What a probe the image cannot run says, loudly, and then fails the test: a probe that cannot
/// run is a finding about the image, not a pass.
pub fn refused(probe: u8, why: &str) -> ! {
    let text = format!("PROBE {probe} REFUSED BY THIS IMAGE: {why}");
    record(&format!("probe-{probe}"), &text);
    panic!("{text}");
}

/// Runs a program as the harness (root) and returns its trimmed standard output; fails the test
/// with everything it printed if it does not succeed.
pub fn ok(program: &str, args: &[&str]) -> String {
    let done = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    assert!(
        done.status.success(),
        "{program} {args:?}: {}\n{}\n{}",
        done.status,
        String::from_utf8_lossy(&done.stdout),
        String::from_utf8_lossy(&done.stderr)
    );
    String::from_utf8_lossy(&done.stdout).trim().to_owned()
}

/// The standard output of a program whatever its status (`systemctl is-active` fails when the
/// answer is not `active`).
pub fn said(program: &str, args: &[&str]) -> String {
    let done = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    String::from_utf8_lossy(&done.stdout).trim().to_owned()
}

/// `nobody` and `nogroup` of this host.
pub fn nobody() -> (u32, u32) {
    let accounts = EtcAccounts::system().expect("read the account files");
    let user = accounts.user("nobody").expect("an account nobody");
    (user.uid, accounts.group("nogroup").unwrap_or(user.gid))
}

pub fn make_dir(path: &Path, mode: u32) {
    fs::create_dir_all(path).expect("create the directory");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set the mode");
}

pub fn give_to_nobody(path: &Path) {
    let (uid, gid) = nobody();
    chown(path, Some(uid), Some(gid)).expect("chown");
}

/// An account for the length of a test: `useradd --system`, with a group of its own and these
/// supplementary groups; `userdel` when dropped.
pub struct Account {
    pub name: String,
}

impl Account {
    pub fn new(name: &str, supplementary: &[&str]) -> Account {
        let groups = supplementary.join(",");
        let mut args = vec![
            "--system",
            "--user-group",
            "--no-create-home",
            "--shell",
            "/usr/sbin/nologin",
        ];
        if !supplementary.is_empty() {
            args.extend(["--groups", &groups]);
        }
        args.push(name);
        ok("useradd", &args);
        Account {
            name: name.to_owned(),
        }
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        let _ = Command::new("userdel").arg(&self.name).output();
    }
}

/// A group for the length of a test.
pub struct Group {
    pub name: String,
}

impl Group {
    pub fn new(name: &str) -> Group {
        ok("groupadd", &["--system", name]);
        Group {
            name: name.to_owned(),
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        let _ = Command::new("groupdel").arg(&self.name).output();
    }
}

/// What the hook of a test is declared with. The defaults are a hook as `nobody` that holds no
/// capability, may open a unix socket and write in `w` (and its record directory).
#[derive(Clone)]
pub struct Spec {
    pub user: String,
    pub group: String,
    pub caps: Vec<String>,
    pub steps: Vec<(Step, Vec<String>)>,
    pub families: Vec<String>,
    pub write: Vec<String>,
    pub read: Vec<String>,
    pub env: Vec<(String, String)>,
    pub units: Vec<String>,
    /// The release the hook is run from; the rig's own by default.
    pub release: Option<PathBuf>,
    pub binary: String,
    /// The bound of every step, in seconds.
    pub timeout_secs: u64,
}

fn list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("\"{i}\"")).collect();
    format!("[{}]", quoted.join(", "))
}

/// A directory of the test, with the probe in `release/probe`.
pub struct Rig {
    pub dir: PathBuf,
    pub tag: String,
}

impl Rig {
    /// `/var/lib/probe-<tag>-<pid>`, named for the service so a root hook may write in it: not under `/tmp` (the hook has a private one) and not
    /// under a home (it has none).
    pub fn new(tag: &str) -> Rig {
        let dir = PathBuf::from(format!("/var/lib/probe-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        make_dir(&dir, 0o755);
        for sub in ["release", "w", "ro", "state"] {
            make_dir(&dir.join(sub), 0o755);
        }
        fs::copy(PROBE, dir.join("release/probe")).expect("copy the probe");
        fs::set_permissions(dir.join("release/probe"), fs::Permissions::from_mode(0o755))
            .expect("set the mode");
        for sub in ["w", "ro", "state"] {
            give_to_nobody(&dir.join(sub));
        }
        Rig {
            dir,
            tag: tag.to_owned(),
        }
    }

    pub fn path(&self, sub: &str) -> String {
        self.dir.join(sub).display().to_string()
    }

    /// The service the declarations are for.
    pub fn service(&self) -> String {
        format!("probe-{}", self.tag)
    }

    pub fn spec(&self) -> Spec {
        let (_, _) = nobody();
        Spec {
            user: "nobody".to_owned(),
            group: "nogroup".to_owned(),
            caps: Vec::new(),
            steps: Vec::new(),
            families: vec!["AF_UNIX".to_owned()],
            write: vec![self.path("w")],
            read: Vec::new(),
            env: Vec::new(),
            units: Vec::new(),
            release: None,
            binary: "probe".to_owned(),
            timeout_secs: 60,
        }
    }

    /// The text of the declaration of `spec` for the probe operation in `hook_args`.
    pub fn toml(&self, name: &str, spec: &Spec, hook_args: &[&str]) -> String {
        let args: Vec<String> = hook_args.iter().map(|a| (*a).to_owned()).collect();
        let steps: String = spec
            .steps
            .iter()
            .map(|(step, caps)| format!("{step} = {}\n", list(caps)))
            .collect();
        let env: Vec<String> = spec
            .env
            .iter()
            .map(|(k, v)| format!("{k} = \"{v}\""))
            .collect();
        format!(
            "name = \"{name}\"\nbinary = \"{binary}\"\nhook_args = {args}\n\
             signer = {{ repository = \"komira-ai/kbf\", workflow = \"artifacts.yml\" }}\n\
             uid = \"daemon\"\nverify_deadline_secs = 120\nstate_dir = \"{state}\"\n\
             [hook]\nuser = \"{user}\"\ngroup = \"{group}\"\ncapabilities = {caps}\n\
             read_paths = {read}\nwrite_paths = {write}\nunits = {units}\n\
             address_families = {families}\nenvironment = {{ {env} }}\n\
             [hook.step_capabilities]\n{steps}\
             [hook.timeout_secs]\nprepare = {t}\ncutover = {t}\nverify = {t}\nrollback = {t}\ncleanup = {t}\n",
            binary = spec.binary,
            t = spec.timeout_secs,
            args = list(&args),
            state = self.path("state"),
            user = spec.user,
            group = spec.group,
            caps = list(&spec.caps),
            read = list(&spec.read),
            write = list(&spec.write),
            units = list(&spec.units),
            families = list(&spec.families),
            env = env.join(", "),
        )
    }

    pub fn declaration(&self, spec: &Spec, hook_args: &[&str]) -> Declaration {
        let text = self.toml(&self.service(), spec, hook_args);
        Declaration::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    fn release(&self, spec: &Spec) -> PathBuf {
        spec.release
            .clone()
            .unwrap_or_else(|| self.dir.join("release"))
    }

    /// The declaration of `spec` as a loaded one: the uid of the account the hook runs as, and the
    /// service of the rig as the only declared name.
    pub fn declared(&self, spec: &Spec, hook_args: &[&str]) -> update_rights::Declared {
        update_rights::Declared {
            declaration: self.declaration(spec, hook_args),
            digest: update_rights::Digest::of(b"by hand"),
            file: PathBuf::from("by-hand.toml"),
            service: update_rights::UserIds { uid: 1, gid: 1 },
            hook: update_rights::UserIds {
                uid: hook_uid(spec),
                gid: hook_uid(spec),
            },
            service_gids: vec![1],
            names: [self.service()].into_iter().collect(),
        }
    }

    /// Runs one step of the hook through the real `systemd-run`.
    pub fn run(&self, spec: &Spec, step: Step, hook_args: &[&str]) -> StepRun {
        let declared = self.declared(spec, hook_args);
        let release = self.release(spec);
        let launch = Launch::of(
            &declared,
            step,
            GENERATION.fetch_add(1, Ordering::Relaxed),
            &release,
            None,
        );
        run_step(&Systemd::system(), &launch)
            .unwrap_or_else(|e| panic!("the step did not run: {e}"))
    }

    /// The command line of a step, for a test that runs `systemd-run` itself.
    pub fn command(
        &self,
        spec: &Spec,
        step: Step,
        hook_args: &[&str],
    ) -> update_rights::SystemdRun {
        let declared = self.declared(spec, hook_args);
        let release = self.release(spec);
        let launch = Launch::of(
            &declared,
            step,
            GENERATION.fetch_add(1, Ordering::Relaxed),
            &release,
            None,
        );
        command(&launch).expect("the command builds")
    }

    /// The same, with everything `systemd-run` returned.
    pub fn raw(
        &self,
        spec: &Spec,
        step: Step,
        hook_args: &[&str],
        params: Option<&Path>,
    ) -> Finished {
        let declared = self.declared(spec, hook_args);
        let release = self.release(spec);
        let launch = Launch::of(
            &declared,
            step,
            GENERATION.fetch_add(1, Ordering::Relaxed),
            &release,
            params,
        );
        let command = command(&launch).expect("the command builds");
        Systemd::system().run(&command).expect("systemd-run starts")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// What the hook said in its report, for a message.
pub fn said_by(run: &StepRun) -> String {
    format!(
        "outcome {:?}, detail {:?}, stderr {:?}",
        run.outcome, run.report.detail, run.stderr
    )
}

/// The uid of the account a spec runs the hook as: 0 for root, `nobody`'s otherwise.
pub fn hook_uid(spec: &Spec) -> u32 {
    if spec.user == "root" { 0 } else { nobody().0 }
}
