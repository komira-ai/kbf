//! The Envoy verbs' work: fetch and pin a build, verify the pinned one, name it, list them
//! (see [`super`] for the order of the checks).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::fetch::Fetcher;
use super::manifest::EnvoyPin;
use super::{EnvoyError, EnvoyId, VersionFault};
use crate::binary::{Arch, BinaryKind};
use crate::inbox::{self, Entry};
use crate::layout::{self, Layout, Platform};
use crate::refusal::Refusal;
use crate::run::{RunAs, Runner, Spawn};
use crate::state::{self, EnvoyPins, Locked, State};

/// The most bytes of an Envoy binary taken (a stripped release build is about a tenth of
/// this).
pub const MAX_BYTES: u64 = 512 * 1024 * 1024;

/// What the verbs need from the host.
pub struct Env<'a> {
    pub host: &'a Layout,
    /// The CPU this host runs: which manifest entry applies, and which ELF is accepted.
    pub arch: Arch,
    /// Runs `envoy --version`.
    pub runner: &'a dyn Runner,
    pub fetcher: &'a dyn Fetcher,
    /// The user `envoy --version` runs as: `kbf-verify`, never root.
    pub verify_as: RunAs,
    /// The user whose download is accepted: `kbf-fetch`.
    pub fetch_as: RunAs,
    /// Who must own what is installed (0 on a host).
    pub owner_uid: u32,
    /// How long `envoy --version` may take.
    pub timeout: Duration,
    /// The most bytes of a download taken.
    pub cap: u64,
}

/// What the install of one build did to the directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Done {
    /// `envoy/<id>` was made.
    Installed,
    /// `envoy/<id>` was there with the same bytes and is untouched.
    Unchanged,
}

/// What a fetch would do, without doing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub id: EnvoyId,
    pub dir: PathBuf,
    pub pin: EnvoyPin,
    /// `envoy/<id>` is already there with the pinned bytes (nothing is downloaded then).
    pub installed: bool,
}

/// A fetch that was done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    pub id: EnvoyId,
    pub dir: PathBuf,
    pub done: Done,
    /// What `state.json` pins now.
    pub pins: EnvoyPins,
}

/// A build `verify` accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub id: EnvoyId,
    pub sha256: String,
    pub binary: PathBuf,
}

/// A build in `envoy/`, and whether `state.json` pins it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub id: EnvoyId,
    pub current: bool,
    pub previous: bool,
}

fn linux_only(env: &Env) -> Result<(), EnvoyError> {
    match env.host.platform() {
        Platform::Linux => Ok(()),
        other => Err(EnvoyError::Platform(other)),
    }
}

fn work_err(path: &Path) -> impl FnOnce(io::Error) -> EnvoyError {
    let path = path.to_owned();
    move |source| EnvoyError::Work { path, source }
}

fn received(refusal: Refusal) -> EnvoyError {
    EnvoyError::Received(Box::new(refusal))
}

fn envoy_root(env: &Env) -> PathBuf {
    env.host.join(&layout::path(layout::ENVOY_DIR))
}

fn dir_of(env: &Env, id: &EnvoyId) -> PathBuf {
    env.host.join(&layout::envoy_dir(id))
}

fn binary_of(env: &Env, id: &EnvoyId) -> PathBuf {
    env.host.join(&layout::envoy_file(id, layout::ENVOY_BINARY))
}

fn sha_file_of(env: &Env, id: &EnvoyId) -> PathBuf {
    env.host.join(&layout::envoy_file(id, layout::ENVOY_SHA256))
}

/// What fetching `pin` would do. Refuses what a fetch would: a directory for the build
/// that holds other bytes.
pub fn plan(env: &Env, pin: &EnvoyPin) -> Result<Plan, EnvoyError> {
    linux_only(env)?;
    let id = id_of(pin)?;
    let dir = dir_of(env, &id);
    let installed = already_there(env, &id, pin)?;
    Ok(Plan {
        id,
        dir,
        pin: pin.clone(),
        installed,
    })
}

fn id_of(pin: &EnvoyPin) -> Result<EnvoyId, EnvoyError> {
    // The manifest was checked for the form of both, so this holds.
    EnvoyId::new(&pin.version, &pin.sha256)
        .map_err(|e| EnvoyError::Manifest(super::manifest::ManifestFault::Shape(e.to_string())))
}

/// Whether `envoy/<id>` exists with the bytes `pin` names. `Ok(false)` when it does not
/// exist; a directory that does, with anything else in it, is [`EnvoyError::Differs`].
fn already_there(env: &Env, id: &EnvoyId, pin: &EnvoyPin) -> Result<bool, EnvoyError> {
    let dir = dir_of(env, id);
    match fs::symlink_metadata(&dir) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(work_err(&dir)(source)),
    }
    if same_bytes(env, id, &pin.sha256) {
        Ok(true)
    } else {
        Err(EnvoyError::Differs { path: dir })
    }
}

/// Whether the build's directory is a plain directory holding a binary and a recorded
/// sha256 that both say `sha256`. Anything that cannot be read says no.
fn same_bytes(env: &Env, id: &EnvoyId, sha256: &str) -> bool {
    let plain = |path: &Path, dir: bool| {
        fs::symlink_metadata(path)
            .map(|m| if dir { m.is_dir() } else { m.is_file() })
            .unwrap_or(false)
    };
    let (dir, binary, recorded) = (dir_of(env, id), binary_of(env, id), sha_file_of(env, id));
    if !(plain(&dir, true) && plain(&binary, false) && plain(&recorded, false)) {
        return false;
    }
    let recorded_ok = fs::read(&recorded).is_ok_and(|b| b == recorded_bytes(sha256));
    let binary_ok = File::open(&binary)
        .and_then(|mut f| digest(&mut f))
        .is_ok_and(|(got, _)| got == sha256);
    recorded_ok && binary_ok
}

/// What `envoy.sha256` holds: the sha256 and a newline.
fn recorded_bytes(sha256: &str) -> Vec<u8> {
    format!("{sha256}\n").into_bytes()
}

/// The lowercase hex sha256 of what is left to read of `file`, and the first 32 bytes.
fn digest(file: &mut File) -> io::Result<(String, Vec<u8>)> {
    let mut hash = Sha256::new();
    let mut head = Vec::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok((hex::encode(hash.finalize()), head));
        }
        if head.len() < 32 {
            let room = 32 - head.len();
            head.extend_from_slice(&buf[..n.min(room)]);
        }
        hash.update(&buf[..n]);
    }
}

/// Fetch the build `pin` names, check it, install it and pin it in `state.json`.
pub fn fetch(env: &Env, pin: &EnvoyPin) -> Result<Fetched, EnvoyError> {
    let plan = plan(env, pin)?;
    if plan.installed {
        // "Already installed" never skips the checks (design S20): the owner, the mode,
        // the hash, the ELF machine and the version are looked at again.
        verify_installed(env, &plan.id)?;
        let pins = pin_in_state(env, &plan.id)?;
        return Ok(Fetched {
            id: plan.id,
            dir: plan.dir,
            done: Done::Unchanged,
            pins,
        });
    }
    let scratch = Scratch::make(env)?;
    let copy = download(env, pin, &scratch)?;
    // Under the lock from here: the install and the state change are one step to every
    // other run.
    let lock = Locked::acquire(env.host)?;
    let done = install_dir(env, &plan.id, &copy, pin)?;
    let pins = pin_in(&lock, &plan.id)?;
    Ok(Fetched {
        id: plan.id,
        dir: plan.dir,
        done,
        pins,
    })
}

/// Download `pin`'s file and take it through the checks that need no installed copy:
/// opened beneath the download directory, copied, hashed, an ELF for this CPU, and
/// `--version` naming the pin's version. The root-owned copy, ready to install.
fn download(env: &Env, pin: &EnvoyPin, scratch: &Scratch) -> Result<PathBuf, EnvoyError> {
    let target = scratch.download().join(DOWNLOAD);
    env.fetcher.fetch(&pin.url, &target, env.cap)?;
    let entry = Entry::open(&scratch.download()).map_err(received)?;
    let mut file = entry.open_file(DOWNLOAD, env.cap).map_err(received)?;
    check_received(&file, env.fetch_as.uid)?;
    let copy = scratch.input().join(DOWNLOAD);
    inbox::copy_capped(&mut file, &copy, env.cap, DOWNLOAD).map_err(received)?;
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)).map_err(work_err(&copy))?;

    let (got, head) = File::open(&copy)
        .and_then(|mut f| digest(&mut f))
        .map_err(work_err(&copy))?;
    if got != pin.sha256 {
        return Err(EnvoyError::Hash {
            want: pin.sha256.clone(),
            got,
        });
    }
    check_kind(env, &head)?;
    let said = version_of(env, &copy, &scratch.empty())?;
    if said != pin.version {
        return Err(EnvoyError::Version(VersionFault::Mismatch {
            want: pin.version.clone(),
            got: said,
        }));
    }
    Ok(copy)
}

/// The name the downloaded file has in the download directory and the copy directory.
const DOWNLOAD: &str = "envoy";

/// A download is the fetch user's own file: what any other user could have put there is
/// not taken.
pub(super) fn check_received(file: &File, fetch_uid: u32) -> Result<(), EnvoyError> {
    let meta = file.metadata().map_err(work_err(Path::new(DOWNLOAD)))?;
    if meta.uid() != fetch_uid {
        return Err(EnvoyError::DownloadOwner {
            found: meta.uid(),
            want: fetch_uid,
        });
    }
    Ok(())
}

fn check_kind(env: &Env, head: &[u8]) -> Result<(), EnvoyError> {
    let want = BinaryKind::wanted(Platform::Linux, env.arch);
    let found = BinaryKind::of(head);
    if found == want {
        Ok(())
    } else {
        Err(EnvoyError::WrongBinary { found, want })
    }
}

/// `<binary> --version`, run as the verify user in `cwd` with an empty environment; the
/// version it reports.
fn version_of(env: &Env, binary: &Path, cwd: &Path) -> Result<String, EnvoyError> {
    let what = "envoy --version";
    let spawn = Spawn {
        program: binary.to_owned(),
        args: vec!["--version".into()],
        env: Vec::new(),
        cwd: cwd.to_owned(),
        run_as: Some(env.verify_as),
        timeout: env.timeout,
    };
    let ran = env
        .runner
        .run(&spawn)
        .map_err(|source| EnvoyError::Tool { what, source })?;
    if ran.timed_out {
        return Err(EnvoyError::TimedOut { what });
    }
    if ran.code != Some(0) {
        return Err(EnvoyError::Version(VersionFault::Failed { code: ran.code }));
    }
    parse_version(&ran.stdout).map_err(EnvoyError::Version)
}

/// The version in `envoy  version: <commit>/<version>/<status>/<build>/<ssl>`.
pub fn parse_version(stdout: &[u8]) -> Result<String, VersionFault> {
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        if let Some((_, rest)) = line.split_once("version:")
            && let Some(version) = rest.trim().split('/').nth(1).filter(|v| !v.is_empty())
        {
            return Ok(version.to_owned());
        }
    }
    Err(VersionFault::Unreadable)
}

/// Write the verified `copy` as `envoy/<id>/envoy` with `envoy.sha256` beside it, by a
/// staging directory and a rename, hashing the bytes as they are written (design S6, S19).
/// The caller holds the lock. A directory that is already there is left alone if it holds
/// the pinned bytes and refused if not.
fn install_dir(env: &Env, id: &EnvoyId, copy: &Path, pin: &EnvoyPin) -> Result<Done, EnvoyError> {
    if already_there(env, id, pin)? {
        return Ok(Done::Unchanged);
    }
    let root = envoy_root(env);
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o755);
    builder.create(&root).map_err(work_err(&root))?;

    let stage = root.join(format!(".stage-{id}.{}", std::process::id()));
    // What an interrupted run of this process id left; it was never renamed into place.
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir(&stage).map_err(work_err(&stage))?;
    let guard = Cleanup(stage.clone());
    fs::set_permissions(&stage, fs::Permissions::from_mode(0o755)).map_err(work_err(&stage))?;

    let binary = stage.join(layout::ENVOY_BINARY);
    let wrote = write_hashed(copy, &binary).map_err(work_err(&binary))?;
    if wrote != pin.sha256 {
        return Err(EnvoyError::Hash {
            want: pin.sha256.clone(),
            got: wrote,
        });
    }
    let recorded = stage.join(layout::ENVOY_SHA256);
    write_file(&recorded, &recorded_bytes(&pin.sha256), 0o644).map_err(work_err(&recorded))?;
    sync_dir(&stage).map_err(work_err(&stage))?;

    let target = dir_of(env, id);
    fs::rename(&stage, &target).map_err(work_err(&target))?;
    drop(guard);
    sync_dir(&root).map_err(work_err(&root))?;
    Ok(Done::Installed)
}

/// Removes a staging directory that was not renamed away.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copy `from` to the new file `to` (0755), hashing what is written; its sha256.
fn write_hashed(from: &Path, to: &Path) -> io::Result<String> {
    let mut input = File::open(from)?;
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(to)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    // Whatever the umask was.
    out.set_permissions(fs::Permissions::from_mode(0o755))?;
    out.sync_all()?;
    Ok(hex::encode(hash.finalize()))
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    out.write_all(bytes)?;
    out.set_permissions(fs::Permissions::from_mode(mode))?;
    out.sync_all()
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Pin `id` in `state.json` under the lock the caller does not hold yet.
fn pin_in_state(env: &Env, id: &EnvoyId) -> Result<EnvoyPins, EnvoyError> {
    pin_in(&Locked::acquire(env.host)?, id)
}

/// Make `id` the current Envoy, and the one it replaces the previous. A state already
/// pinning `id` is not written again. A missing state file is a state with no pin.
fn pin_in(lock: &Locked, id: &EnvoyId) -> Result<EnvoyPins, EnvoyError> {
    let mut state = lock.read()?.unwrap_or_default();
    if state.envoy.current.as_ref() != Some(id) {
        state.envoy = EnvoyPins {
            previous: state.envoy.current.take(),
            current: Some(id.clone()),
        };
        lock.write(&state)?;
    }
    Ok(state.envoy)
}

/// Check the installed build `id` the way `verify` does: its directory and files are
/// plain, root's and closed to group and other writes; the recorded sha256 is one
/// sha256 that starts as the name does; the binary hashes to it, is an ELF for this CPU
/// and says it is the version the name has.
pub fn verify_installed(env: &Env, id: &EnvoyId) -> Result<Verified, EnvoyError> {
    let root = envoy_root(env);
    let dir = dir_of(env, id);
    trusted(env, &root, true)?;
    trusted(env, &dir, true)?;

    let sha_path = sha_file_of(env, id);
    trusted(env, &sha_path, false)?;
    let recorded = read_recorded(&sha_path)?;
    if !recorded.starts_with(id.sha_prefix()) {
        return Err(EnvoyError::Installed {
            path: sha_path,
            reason: "the recorded sha256 does not belong to this build's name".to_owned(),
        });
    }

    let binary = binary_of(env, id);
    trusted(env, &binary, false)?;
    let mut file = File::open(&binary).map_err(work_err(&binary))?;
    // The descriptor, not the path, is what is judged and hashed from here.
    judge(env, &binary, file.metadata(), false)?;
    let (got, head) = digest(&mut file).map_err(work_err(&binary))?;
    if got != recorded {
        return Err(EnvoyError::Hash {
            want: recorded,
            got,
        });
    }
    check_kind(env, &head)?;
    let said = version_of(env, &binary, &dir)?;
    if said != id.version() {
        return Err(EnvoyError::Version(VersionFault::Mismatch {
            want: id.version().to_owned(),
            got: said,
        }));
    }
    Ok(Verified {
        id: id.clone(),
        sha256: recorded,
        binary,
    })
}

/// `path` is a plain directory (`dir`) or file, owned by the host's owner and closed to
/// group and other writes: not a link, not something another user could have put there.
fn trusted(env: &Env, path: &Path, dir: bool) -> Result<(), EnvoyError> {
    judge(env, path, fs::symlink_metadata(path), dir)
}

/// [`trusted`] for what `meta` says about `path`: a path's own (not following a link), or
/// that of the descriptor it was opened as.
fn judge(
    env: &Env,
    path: &Path,
    meta: io::Result<fs::Metadata>,
    dir: bool,
) -> Result<(), EnvoyError> {
    let refuse = |reason: String| EnvoyError::Installed {
        path: path.to_owned(),
        reason,
    };
    let meta = meta.map_err(|e| {
        refuse(if e.kind() == io::ErrorKind::NotFound {
            "it is missing".to_owned()
        } else {
            e.to_string()
        })
    })?;
    check_meta(env, &meta, dir).map_err(refuse)
}

fn check_meta(env: &Env, meta: &fs::Metadata, dir: bool) -> Result<(), String> {
    let plain = if dir { meta.is_dir() } else { meta.is_file() };
    if !plain {
        let want = if dir { "directory" } else { "file" };
        return Err(format!("it is not a plain {want} (a link?)"));
    }
    if meta.uid() != env.owner_uid {
        return Err(format!(
            "it is owned by uid {}, not {}",
            meta.uid(),
            env.owner_uid
        ));
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o022 != 0 {
        return Err(format!(
            "it is writable by group or others (mode {mode:04o})"
        ));
    }
    Ok(())
}

/// The sha256 `envoy.sha256` records: 64 lowercase hex digits and a newline, nothing else.
fn read_recorded(path: &Path) -> Result<String, EnvoyError> {
    let bad = |reason: &str| EnvoyError::Installed {
        path: path.to_owned(),
        reason: reason.to_owned(),
    };
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(66).read_to_end(&mut bytes))
        .map_err(work_err(path))?;
    let text = String::from_utf8(bytes).map_err(|_| bad("it is not text"))?;
    match text.strip_suffix('\n') {
        Some(sha) if super::valid_sha256(sha) => Ok(sha.to_owned()),
        _ => Err(bad("it is not one sha256 and a newline")),
    }
}

/// The build `state.json` pins as current.
fn pinned(env: &Env) -> Result<EnvoyId, EnvoyError> {
    let state: Option<State> = state::read_unlocked(env.host)?;
    state
        .and_then(|s| s.envoy.current)
        .ok_or(EnvoyError::Unpinned)
}

/// `envoy verify`: the pinned build passes [`verify_installed`].
pub fn verify(env: &Env) -> Result<Verified, EnvoyError> {
    linux_only(env)?;
    let id = pinned(env)?;
    verify_installed(env, &id)
}

/// `envoy path`: where the pinned binary is. It is not checked beyond being a file: that
/// is `verify`'s.
pub fn path(env: &Env) -> Result<PathBuf, EnvoyError> {
    linux_only(env)?;
    let id = pinned(env)?;
    let binary = binary_of(env, &id);
    match fs::symlink_metadata(&binary) {
        Ok(m) if m.is_file() => Ok(binary),
        _ => Err(EnvoyError::Installed {
            path: binary,
            reason: "the pinned build's binary is missing".to_owned(),
        }),
    }
}

/// `envoy list`: every build in `envoy/` (names that are not builds are skipped), in the
/// order of their names, with what `state.json` pins.
pub fn list(env: &Env) -> Result<Vec<Listed>, EnvoyError> {
    let pins = state::read_unlocked(env.host)?
        .map(|s| s.envoy)
        .unwrap_or_default();
    let root = envoy_root(env);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(work_err(&root)(source)),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(work_err(&root))?;
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        let id = entry
            .file_name()
            .to_str()
            .and_then(|n| EnvoyId::parse(n).ok());
        ids.extend(id.filter(|_| is_dir));
    }
    ids.sort();
    Ok(ids
        .into_iter()
        .map(|id| Listed {
            current: pins.current.as_ref() == Some(&id),
            previous: pins.previous.as_ref() == Some(&id),
            id,
        })
        .collect())
}

/// Give `dir` to `user`, when the caller is root: nobody else can, and on a host root is
/// who runs this.
pub(super) fn hand_over(dir: &Path, user: RunAs, root: bool) -> Result<(), EnvoyError> {
    if root {
        std::os::unix::fs::chown(dir, Some(user.uid), Some(user.gid)).map_err(work_err(dir))?;
    }
    Ok(())
}

/// The scratch directory of one fetch, removed when dropped: `download/` (the fetch
/// user's), `in/` (root's copy) and `empty/` (where `--version` runs).
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn make(env: &Env) -> Result<Scratch, EnvoyError> {
        let parent = env.host.join(&env.host.work_dir());
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o755);
        builder.create(&parent).map_err(work_err(&parent))?;
        let mut n = 0;
        let dir = loop {
            let dir = parent.join(format!("envoy.{}.{n}", std::process::id()));
            match fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(work_err(&dir)(e)),
            }
        };
        let made = Scratch { dir };
        fs::set_permissions(&made.dir, fs::Permissions::from_mode(0o755))
            .map_err(work_err(&made.dir))?;
        for (sub, mode) in [
            (made.download(), 0o700),
            (made.input(), 0o755),
            (made.empty(), 0o755),
        ] {
            fs::create_dir(&sub).map_err(work_err(&sub))?;
            fs::set_permissions(&sub, fs::Permissions::from_mode(mode)).map_err(work_err(&sub))?;
        }
        let (download, root) = (made.download(), rustix::process::geteuid().is_root());
        hand_over(&download, env.fetch_as, root)?;
        Ok(made)
    }

    fn download(&self) -> PathBuf {
        self.dir.join("download")
    }

    fn input(&self) -> PathBuf {
        self.dir.join("in")
    }

    fn empty(&self) -> PathBuf {
        self.dir.join("empty")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The bytes of the file at `path` (a plain file of at most `cap` bytes, owned by the
/// host's owner and closed to group and other writes), for an input the unprivileged could
/// otherwise have written. A refusal is [`EnvoyError::ManifestFile`].
pub fn read_trusted(env: &Env, path: &Path, cap: u64) -> Result<Vec<u8>, EnvoyError> {
    let refuse = |reason: String| EnvoyError::ManifestFile {
        path: path.to_owned(),
        reason,
    };
    let meta = fs::symlink_metadata(path).map_err(|e| refuse(e.to_string()))?;
    check_meta(env, &meta, false).map_err(refuse)?;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(cap + 1).read_to_end(&mut bytes))
        .map_err(|e| refuse(e.to_string()))?;
    if bytes.len() as u64 > cap {
        return Err(refuse(format!("it is over {cap} bytes")));
    }
    Ok(bytes)
}
