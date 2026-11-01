//! Shared by the tests of the root part: a world in a scratch directory (the declarations, the
//! staged releases with their seals, the hooks' record directories), fake accounts, a clock that
//! moves only when told to, and a `systemd-run` that runs the hook directly as this user with the
//! unit's environment, keeping every command it was given.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kbf_step_runner::Clock;
use kbf_step_runner::record::CrashGate;
use kbf_updater::root::{Config, Parts, RootPart};
use update_rights::{EtcAccounts, Finished, Spawner, SystemdRun};

/// The hook of the fake service.
pub const HOOK: &str = env!("CARGO_BIN_EXE_kbf-updater-test-hook");

pub fn my_uid() -> u32 {
    kbf_updater::secret::euid()
}

pub fn my_gid() -> u32 {
    // SAFETY: getegid has no preconditions and cannot fail.
    unsafe { libc::getegid() }
}

/// A file of this mode, whatever the umask.
pub fn put(path: &Path, text: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let _ = fs::remove_file(path);
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A directory of this mode, whatever the umask.
pub fn dir(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A clock that moves only when a test or a `sleep_secs` moves it.
pub struct FakeClock {
    mono: AtomicU64,
    boot: Mutex<String>,
}

impl FakeClock {
    pub fn new() -> Arc<FakeClock> {
        Arc::new(FakeClock {
            mono: AtomicU64::new(1000),
            boot: Mutex::new("boot-a".to_owned()),
        })
    }

    pub fn reboot(&self, id: &str) {
        *self.boot.lock().unwrap() = id.to_owned();
    }
}

impl Clock for FakeClock {
    fn mono_secs(&self) -> u64 {
        self.mono.load(Ordering::SeqCst)
    }

    fn boot_id(&self) -> String {
        self.boot.lock().unwrap().clone()
    }

    fn sleep_secs(&self, secs: u64) {
        self.mono.fetch_add(secs, Ordering::SeqCst);
    }
}

/// `systemd-run` played by this process: the hook after the lone `--` runs as this user with
/// exactly the `Environment=` properties of the unit, and the command is kept.
#[derive(Default)]
pub struct Direct {
    pub seen: Mutex<Vec<SystemdRun>>,
}

impl Spawner for Direct {
    fn run(&self, command: &SystemdRun) -> io::Result<Finished> {
        self.seen.lock().unwrap().push(command.clone());
        let args = words(command);
        let at = args.iter().position(|a| a == "--").expect("a lone --");
        let env: Vec<(String, String)> = args[..at]
            .iter()
            .filter_map(|a| a.strip_prefix("--property=Environment="))
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let done = Command::new(&args[at + 1])
            .args(&args[at + 2..])
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .output()?;
        Ok(Finished {
            code: done.status.code(),
            stdout: done.stdout,
            stderr: done.stderr,
        })
    }
}

/// `systemctl` played by this process: every call kept, every one answered with `code`.
pub struct Systemctl {
    pub seen: Mutex<Vec<Vec<String>>>,
    pub code: i32,
}

impl Systemctl {
    pub fn answering(code: i32) -> Arc<Systemctl> {
        Arc::new(Systemctl {
            seen: Mutex::default(),
            code,
        })
    }

    pub fn stopped(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap();
        seen.iter().map(|call| call[1].clone()).collect()
    }
}

impl Spawner for Systemctl {
    fn run(&self, command: &SystemdRun) -> io::Result<Finished> {
        self.seen.lock().unwrap().push(words(command));
        Ok(Finished {
            code: Some(self.code),
            stdout: Vec::new(),
            stderr: b"from systemctl\n".to_vec(),
        })
    }
}

pub fn words(command: &SystemdRun) -> Vec<String> {
    command
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// The value of the property `name` of a command.
pub fn property(command: &SystemdRun, name: &str) -> Option<String> {
    let prefix = format!("--property={name}=");
    words(command)
        .iter()
        .find_map(|a| a.strip_prefix(&prefix).map(str::to_owned))
}

/// The word the hook was run for: the step, or the boot entry's word.
pub fn word(command: &SystemdRun) -> String {
    let args = words(command);
    let at = args.iter().position(|a| a == "--").unwrap();
    let after: Vec<&String> = args[at + 2..]
        .iter()
        .filter(|a| *a != "update-hook")
        .collect();
    after[0].clone()
}

/// The accounts of the world: the fake service `svc` (990), its hook account `svc-update`, which
/// is this user so that the hook can write its record directory when it is run directly, and
/// `root`, also this user, for the declarations of hooks that say they run as root (the gate reads
/// the declaration, not the uid).
pub fn accounts() -> EtcAccounts {
    let (uid, gid) = (my_uid(), my_gid());
    EtcAccounts::new(
        &format!(
            "root:x:{uid}:{gid}::/root:/bin/sh\n\
             svc:x:990:990::/nonexistent:/usr/sbin/nologin\n\
             svc-update:x:{uid}:{gid}::/nonexistent:/usr/sbin/nologin\n\
             other:x:992:992::/nonexistent:/usr/sbin/nologin\n\
             me:x:{uid}:{gid}::/nonexistent:/usr/sbin/nologin\n\
             reg-update:x:3001:3001::/nonexistent:/usr/sbin/nologin\n"
        ),
        &format!(
            "root:x:{gid}:\n\
             svc:x:990:\n\
             svc-update:x:{gid}:\n\
             other:x:992:\n\
             me:x:{gid}:\n\
             reg-update:x:3001:\n"
        ),
    )
}

/// A scratch world, removed when it is dropped.
pub struct World {
    pub root: PathBuf,
    pub clock: Arc<FakeClock>,
    pub direct: Arc<Direct>,
    pub systemctl: Arc<Systemctl>,
}

impl World {
    pub fn new(name: &str) -> World {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("kbfu-root-{name}"));
        let _ = fs::remove_dir_all(&root);
        dir(&root, 0o755);
        for sub in ["updater.d", "releases", "hooks"] {
            dir(&root.join(sub), 0o755);
        }
        World {
            root,
            clock: FakeClock::new(),
            direct: Arc::default(),
            systemctl: Systemctl::answering(0),
        }
    }

    pub fn path(&self, sub: &str) -> PathBuf {
        self.root.join(sub)
    }

    pub fn config(&self) -> Config {
        Config {
            declarations: self.path("updater.d"),
            approvals: self.path("updater.d/approved"),
            state: self.path("state"),
            releases: self.path("releases"),
            trusted_uid: my_uid(),
        }
    }

    pub fn parts(&self, crash_gate: Option<CrashGate>) -> Parts {
        Parts {
            accounts: Arc::new(accounts()),
            clock: self.clock.clone(),
            systemd_run: self.direct.clone(),
            systemctl: self.systemctl.clone(),
            crash_gate,
            deadman_poll_secs: None,
        }
    }

    /// A root part over this world, as one started now would be.
    pub fn root_part(&self) -> RootPart {
        RootPart::new(self.config(), self.parts(None)).expect("the root part starts")
    }

    pub fn root_part_gated(&self, gate: CrashGate) -> RootPart {
        RootPart::new(self.config(), self.parts(Some(gate))).expect("the root part starts")
    }

    /// The record directory of `service`'s hook.
    pub fn hook_dir(&self, service: &str) -> PathBuf {
        self.path("hooks").join(service)
    }

    /// A declaration of `service` shaped like the design's tierstore example: `CAP_NET_ADMIN`
    /// hook-wide and no `step_capabilities`, a boot entry `restore`; `hook` is the text of the
    /// `[hook]` table's account lines and anything more.
    pub fn declaration(&self, service: &str, top: &str, hook: &str) -> String {
        format!(
            "name = \"{service}\"\n\
             binary = \"hook\"\n\
             signer = {{ repository = \"komira-ai/kbf\", workflow = \"artifacts.yml\" }}\n\
             uid = \"svc\"\n\
             verify_deadline_secs = 1600\n\
             promotion_ceiling_secs = 3600\n\
             cleanup_retries = 2\n\
             state_dir = \"{state}\"\n\
             {top}\
             [hook]\n\
             {hook}\
             address_families = [\"AF_UNIX\", \"AF_NETLINK\"]\n\
             environment = {{ HOOK_RECORD_DIR = \"{state}\" }}\n\
             [hook.timeout_secs]\n\
             prepare = 600\ncutover = 300\nverify = 1000\nrollback = 600\ncleanup = 1300\n",
            state = self.hook_dir(service).display(),
        )
    }

    /// The tierstore-shaped declaration of `svc`: a hook account of its own and `CAP_NET_ADMIN`
    /// hook-wide, with a boot entry.
    pub fn tierstore_shaped(&self) -> String {
        self.declaration(
            "svc",
            "boot = \"restore\"\n",
            "user = \"svc-update\"\ngroup = \"svc-update\"\ncapabilities = [\"CAP_NET_ADMIN\"]\n",
        )
    }

    /// Installs `text` as `service`'s declaration, root-owned and mode 0644 in this world.
    pub fn declare(&self, service: &str, text: &str) {
        put(
            &self.path("updater.d").join(format!("{service}.toml")),
            text.as_bytes(),
            0o644,
        );
    }

    /// Stages release `version` of `service` with the hook, carrying `declaration` in
    /// `updater.d/<service>.toml` when given, and seals it.
    pub fn stage(&self, service: &str, version: &str, declaration: Option<&str>) -> PathBuf {
        self.stage_bytes(service, version, declaration.map(str::as_bytes))
    }

    /// [`World::stage`] with the bytes of the declaration the release carries.
    pub fn stage_bytes(&self, service: &str, version: &str, declaration: Option<&[u8]>) -> PathBuf {
        let service_dir = self.path("releases").join(service);
        dir(&service_dir, 0o755);
        let tree = service_dir.join(version);
        let _ = fs::remove_dir_all(&tree);
        dir(&tree, 0o755);
        fs::copy(HOOK, tree.join("hook")).unwrap();
        fs::set_permissions(tree.join("hook"), fs::Permissions::from_mode(0o755)).unwrap();
        if let Some(text) = declaration {
            dir(&tree.join("updater.d"), 0o755);
            put(
                &tree.join("updater.d").join(format!("{service}.toml")),
                text,
                0o644,
            );
        }
        let digest = kbf_step_runner::tree_digest(&tree).unwrap();
        let seal = format!(
            "{{\"tree_digest\":\"{digest}\",\"artifact_digest\":\"{}\"}}",
            "ab".repeat(32)
        );
        put(
            &service_dir.join(format!("{version}.seal")),
            seal.as_bytes(),
            0o644,
        );
        tree
    }

    /// Approves the declaration `text` as an operator does (`kbf-updater approve`).
    pub fn approve(&self, service: &str, text: &[u8]) {
        let env = update_rights::cli::Env {
            approvals: self.path("updater.d/approved"),
            trusted_uid: my_uid(),
            effective_uid: my_uid(),
        };
        let digest = update_rights::Digest::of(text).to_string();
        let args: Vec<String> = ["kbf-updater", "approve", service, &digest]
            .map(str::to_owned)
            .to_vec();
        let (code, said) = kbf_updater::run_approve_with(&args, &env);
        assert_eq!(code, 0, "{said}");
    }

    /// Tells the hook of `service` to exit `code` with `line` when it runs `word`.
    pub fn tell(&self, service: &str, word: &str, told: &str) {
        let hook_dir = self.hook_dir(service);
        fs::create_dir_all(&hook_dir).unwrap();
        fs::write(hook_dir.join(format!("exit-{word}")), told).unwrap();
    }

    /// The words the hook of `service` was run for, in order.
    pub fn steps(&self, service: &str) -> Vec<String> {
        fs::read_to_string(self.hook_dir(service).join("steps.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The commands `systemd-run` was given.
    pub fn launched(&self) -> Vec<SystemdRun> {
        self.direct.seen.lock().unwrap().clone()
    }

    /// The step record of `service`, as JSON.
    pub fn record(&self, service: &str) -> serde_json::Value {
        let bytes = fs::read(self.path("state").join(service).join("step-record.json"))
            .expect("a step record");
        serde_json::from_slice(&bytes).unwrap()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
