//! The root side of a fetch: what the installer does with a [`ReleaseLocator`], in the order
//! that matters (design 7.1 and 7.2). Each step happens only if every one before it passed,
//! and before the last, the rename into `releases/<commit>/`, nothing a service reads has
//! changed.
//!
//! 1. The locator is checked against the host and the configuration ([`RootFetcher::check`]);
//!    a commit that is already installed **and verified** (the `VERIFIED` marker, the
//!    binary's hash, its own `--version --json` again) is returned and nothing is fetched.
//! 2. The room: the tarball, its extraction (at most 1 GiB, or ten times the tarball if
//!    smaller) and the release being replaced, free under the install root.
//! 3. The child downloads; the parent writes the bytes to `work/<commit>.partial/dl/` (0700,
//!    root), hashing as they arrive ([`crate::fetch::pipe`]), at the caps.
//! 4. The count and the sha256 are the locator's.
//! 5. `gh attestation verify`, offline, for the tarball and the locator's bundle, for the
//!    order's commit on an allowed ref. (`dl/` is opened to 0711 for it, and the tarball and
//!    bundle are 0644 root-owned files: `gh` runs as the unprivileged user of the settings and
//!    must reach them; it cannot change them.)
//! 6. The tarball holds exactly one regular file, named for the component, and it is
//!    extracted by the parent, within a wall-clock bound, to `work/<commit>.partial/release/`.
//! 7. It is the kind of binary this host runs, and for the platform the order names; on macOS
//!    it has no quarantine attribute and a valid ad hoc signature.
//! 8. `<name> --version --json` runs as the fetch user (no network, its rlimits, an empty
//!    environment and directory, ten seconds), its output is at most 4 KiB and parses
//!    strictly, and it says the order's commit and the order's protocol, min_protocol,
//!    state_schema and profile; the protocol is not below the operator floor.
//! 9. `VERIFIED` is written, and `release/` is renamed to `releases/<commit>/` and the
//!    directory synced.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kbf_host::exec::Exec;
use kbf_host::path::HostPath;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::CommitId;
use crate::archive::unpack_one_until;
use crate::attest::{Tools, Verifier};
use crate::binary::{Arch, BinaryKind, Version};
use crate::darwin;
use crate::fetch::config::{Source, UpdateConfig};
use crate::fetch::pipe::{Caps, ChildSpec, receive};
use crate::fetch::sweep::{self, marked, referenced};
use crate::fetch::{
    FetchError, Fetcher, INSTALL_ROOT, InstalledRelease, MARKER, MAX_ATTESTATION_BYTES,
    ReleaseFacts, ReleaseLocator,
};
use crate::layout::{Layout, Platform, RELEASE_BINARY, RELEASE_DAEMON};
use crate::refusal::{ArchiveFault, Refusal, VersionFault};
use crate::run::{Confine, Runner, RunAs, Spawn};
use crate::settings::Settings;

/// The wall-clock bound of each step root runs.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub gh: Duration,
    pub extract: Duration,
    pub probe: Duration,
}

impl Bounds {
    pub const DEFAULT: Bounds = Bounds {
        gh: Duration::from_secs(120),
        extract: Duration::from_secs(120),
        probe: Duration::from_secs(10),
    };
}

/// The most the probe may print.
pub const PROBE_OUTPUT_MAX: usize = 4096;
/// The most a tarball may decompress to.
pub const EXTRACT_MAX: u64 = 1 << 30;
/// How long the child may go without a byte.
pub const NO_BYTE: Duration = Duration::from_secs(60);

/// The room a fetch needs: the tarball, its extraction (the decompressed cap, or ten times
/// the tarball if that is smaller) and `replaced`, the bytes of a release it replaces.
pub fn room_needed(size: u64, replaced: u64) -> u64 {
    size.saturating_add(extract_cap(size))
        .saturating_add(replaced)
}

/// The most the tarball of `size` bytes may decompress to.
pub fn extract_cap(size: u64) -> u64 {
    EXTRACT_MAX.min(size.saturating_mul(10))
}

/// The name of a platform in an order: `linux-x86_64`, `linux-arm64`, `darwin-arm64`.
pub fn platform_name(kind: BinaryKind) -> Option<&'static str> {
    match kind {
        BinaryKind::Elf(Arch::X86_64) => Some("linux-x86_64"),
        BinaryKind::Elf(Arch::Aarch64) => Some("linux-arm64"),
        BinaryKind::MachO(Arch::Aarch64) => Some("darwin-arm64"),
        BinaryKind::MachO(Arch::X86_64) | BinaryKind::Other => None,
    }
}

/// The free bytes on the file system holding `path`, for a non-root user.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let stat = rustix::fs::statvfs(path)?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// The user and group `kbf-fetch` (`_kbf-fetch` on macOS) as the host's user database has
/// them. A host without it does not fetch: there is no fallback to another user.
pub fn fetch_user(exec: &kbf_host::HostExec, platform: Platform) -> Result<RunAs, FetchError> {
    let name = match platform {
        Platform::Linux => "kbf-fetch",
        Platform::MacOs => "_kbf-fetch",
    };
    match kbf_host::user::lookup(exec, name) {
        Ok(Some(user)) => Ok(RunAs {
            uid: user.uid,
            gid: user.gid,
        }),
        Ok(None) => Err(FetchError::NoFetchUser(name.to_owned())),
        Err(e) => Err(FetchError::Locator(format!("cannot look up {name}: {e}"))),
    }
}

impl ChildSpec {
    /// The child of a host: this very program, as `fetch-child`, as `run_as`.
    pub fn installed(run_as: RunAs) -> io::Result<ChildSpec> {
        Ok(ChildSpec {
            program: std::env::current_exe()?,
            args: vec!["fetch-child".into()],
            run_as: Some(run_as),
            confine: Confine::FETCH,
        })
    }
}

/// What `VERIFIED` holds: what the installer established, written by root after 7.2 passed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    schema: u64,
    commit: String,
    name: String,
    tarball_sha256: String,
    binary_sha256: String,
    facts: ReleaseFacts,
}

const MARKER_SCHEMA: u64 = 1;
const MARKER_MAX: u64 = 64 * 1024;

/// The real [`Fetcher`].
pub struct RootFetcher<'a> {
    pub host: &'a Layout,
    /// The root-owned tree (`/var/lib/kbf-installer`): `releases/` and `work/` are in it.
    pub install_root: HostPath,
    pub settings: &'a Settings,
    pub config: &'a UpdateConfig,
    /// Says who owns the trust list.
    pub trust: &'a dyn Exec,
    /// Runs `gh`, the macOS tools and the probe.
    pub runner: &'a dyn Runner,
    pub child: ChildSpec,
    /// Who the probe runs as: the fetch user.
    pub probe_as: Option<RunAs>,
    pub probe_confine: Confine,
    pub bounds: Bounds,
    pub no_byte: Duration,
    /// Free bytes under a path; the real call by default.
    pub free: &'a dyn Fn(&Path) -> io::Result<u64>,
}

impl<'a> RootFetcher<'a> {
    /// A fetcher with the production bounds, root and confinement.
    pub fn new(
        host: &'a Layout,
        settings: &'a Settings,
        config: &'a UpdateConfig,
        trust: &'a dyn Exec,
        runner: &'a dyn Runner,
        child: ChildSpec,
    ) -> RootFetcher<'a> {
        RootFetcher {
            host,
            install_root: HostPath::new(INSTALL_ROOT).expect("a constant path"),
            settings,
            config,
            trust,
            runner,
            probe_as: child.run_as,
            child,
            probe_confine: Confine::PROBE,
            bounds: Bounds::DEFAULT,
            no_byte: NO_BYTE,
            free: &free_bytes,
        }
    }

    fn root(&self) -> PathBuf {
        self.host.join(&self.install_root)
    }

    fn releases(&self) -> PathBuf {
        self.root().join("releases")
    }

    fn work(&self) -> PathBuf {
        self.root().join("work")
    }

    /// Remove what a crash left (see [`sweep`]). Run at the start of every pass.
    pub fn sweep(&self) -> io::Result<sweep::Swept> {
        sweep::sweep(&self.releases(), &self.work())
    }

    /// The component this host needs, and its platform name.
    fn wanted(&self) -> (BinaryKind, &'static str) {
        let kind = BinaryKind::wanted(self.host.platform(), self.settings.arch);
        let name = match self.host.platform() {
            Platform::Linux => RELEASE_BINARY,
            Platform::MacOs => RELEASE_DAEMON,
        };
        (kind, name)
    }

    /// The locator against the host and the configuration.
    pub fn check(&self, rec: &ReleaseLocator) -> Result<(), FetchError> {
        let bad = |why: String| Err(FetchError::Locator(why));
        let (kind, name) = self.wanted();
        if rec.name != name {
            return bad(format!("this host runs {name}, not {:?}", rec.name));
        }
        if platform_name(kind) != Some(rec.platform.as_str()) {
            return bad(format!(
                "this host is {:?}, not {:?}",
                platform_name(kind).unwrap_or("unknown"),
                rec.platform
            ));
        }
        if rec.sha256.len() != 64
            || !rec
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return bad("the sha256 is not 64 lowercase hex digits".to_owned());
        }
        if rec.size == 0 || rec.size > self.config.max_component_bytes {
            return bad(format!(
                "the size {} is not 1 to {}",
                rec.size, self.config.max_component_bytes
            ));
        }
        if rec.attestation.is_empty() || rec.attestation.len() > MAX_ATTESTATION_BYTES {
            return bad(format!(
                "the attestation is {} bytes, not 1 to {MAX_ATTESTATION_BYTES}",
                rec.attestation.len()
            ));
        }
        let plain = |c: char| c.is_ascii_alphanumeric() || c == '-';
        if rec.profile.is_empty() || rec.profile.len() > 32 || !rec.profile.chars().all(plain) {
            return bad("the expected profile is empty or not plain".to_owned());
        }
        Ok(())
    }

    fn io(path: &Path) -> impl FnOnce(io::Error) -> FetchError + '_ {
        move |source| FetchError::Io {
            path: path.to_owned(),
            source,
        }
    }

    /// The release, if `releases/<commit>/` holds one this installer verified. Everything the
    /// marker says is checked again against the directory: it is the root's own record, but it
    /// sits beside a binary other things have touched. The probe is run again (design 7.2:
    /// a rollback re-runs step 5 and the floor).
    fn installed(&self, rec: &ReleaseLocator) -> Result<Option<InstalledRelease>, FetchError> {
        let dir = self.releases().join(rec.commit.as_str());
        let Some(marker) = read_marker(&dir.join(MARKER)) else {
            return Ok(None);
        };
        let binary = dir.join(&rec.name);
        let held = marker.commit == rec.commit.as_str()
            && marker.name == rec.name
            && marker.tarball_sha256 == rec.sha256
            && owned_plain(&binary).unwrap_or(false)
            && file_sha256(&binary).ok().as_deref() == Some(marker.binary_sha256.as_str());
        if !held {
            return Ok(None);
        }
        let facts = self.probe(rec, &binary, &self.ensure_empty()?)?;
        Ok(Some(InstalledRelease {
            commit: rec.commit.clone(),
            name: rec.name.clone(),
            dir,
            facts,
            fetched: false,
        }))
    }

    /// `work/empty`: the directory the probe runs in.
    fn ensure_empty(&self) -> Result<PathBuf, FetchError> {
        let work = self.work();
        make_dir(&work).map_err(Self::io(&work))?;
        let empty = work.join("empty");
        make_dir(&empty).map_err(Self::io(&empty))?;
        Ok(empty)
    }

    /// `<binary> --version --json` as the fetch user, and the checks of step 8.
    fn probe(
        &self,
        rec: &ReleaseLocator,
        binary: &Path,
        cwd: &Path,
    ) -> Result<ReleaseFacts, FetchError> {
        let spawn = Spawn {
            program: binary.to_owned(),
            args: vec!["--version".into(), "--json".into()],
            env: Vec::new(),
            cwd: cwd.to_owned(),
            run_as: self.probe_as,
            timeout: self.bounds.probe,
            confine: Some(self.probe_confine),
        };
        const WHAT: &str = "the downloaded binary's --version --json";
        let ran = self.runner.run(&spawn).map_err(|source| Refusal::Tool {
            what: WHAT,
            source,
        })?;
        if ran.timed_out {
            return Err(Refusal::TimedOut { what: WHAT }.into());
        }
        let version = |fault| {
            FetchError::from(Refusal::Version {
                component: rec.name.clone(),
                fault,
            })
        };
        if ran.code != Some(0) {
            return Err(version(VersionFault::Failed(ran.code)));
        }
        if ran.stdout.len() > PROBE_OUTPUT_MAX {
            return Err(version(VersionFault::Unreadable(format!(
                "more than {PROBE_OUTPUT_MAX} bytes of output"
            ))));
        }
        let said = Version::parse(&ran.stdout).map_err(version)?;
        said.check(&rec.commit).map_err(version)?;
        let expect = |what: &'static str, want: String, got: String| {
            if want == got {
                Ok(())
            } else {
                Err(FetchError::Expected { what, want, got })
            }
        };
        expect("protocol", rec.protocol.to_string(), said.protocol.to_string())?;
        expect(
            "min_protocol",
            rec.min_protocol.to_string(),
            said.min_protocol.to_string(),
        )?;
        expect(
            "state_schema",
            rec.state_schema.to_string(),
            said.state_schema.to_string(),
        )?;
        expect("profile", rec.profile.clone(), said.profile.clone())?;
        if let Some(platform) = &said.platform {
            expect("platform", rec.platform.clone(), platform.clone())?;
        }
        if said.protocol < self.config.min_protocol {
            return Err(FetchError::Floor {
                got: said.protocol,
                floor: self.config.min_protocol,
            });
        }
        Ok(ReleaseFacts {
            platform: rec.platform.clone(),
            protocol: said.protocol,
            min_protocol: said.min_protocol,
            state_schema: said.state_schema,
            profile: said.profile,
            version: said.version,
        })
    }

    fn flags(&self, rec: &ReleaseLocator, url: &str) -> Vec<std::ffi::OsString> {
        let mut flags: Vec<std::ffi::OsString> = vec![
            "--url".into(),
            url.into(),
            "--sha256".into(),
            rec.sha256.clone().into(),
            // One byte more than declared: the parent sees it and ends the fetch.
            "--max-bytes".into(),
            rec.size.saturating_add(1).to_string().into(),
        ];
        if let Some(ca) = &self.config.ca_file {
            flags.push("--ca-file".into());
            flags.push(ca.into());
        }
        flags
    }

    /// Steps 3 to 8, in the scratch directory. Returns the facts and the binary's sha256.
    fn verified(
        &self,
        rec: &ReleaseLocator,
        scratch: &Scratch,
        empty: &Path,
        url: &str,
    ) -> Result<(ReleaseFacts, String), FetchError> {
        let tarball = scratch.dl.join(format!("{}.tar.gz", rec.name));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tarball)
            .map_err(Self::io(&tarball))?;
        let caps = Caps {
            size: rec.size,
            no_byte: self.no_byte,
            total: self.config.fetch_timeout,
        };
        let got = receive(&self.child, &self.flags(rec, url), caps, &mut file)?;
        drop(file);
        if got.count != rec.size || got.sha256 != rec.sha256 {
            return Err(FetchError::Digest {
                actual: got.sha256,
                want: rec.sha256.clone(),
            });
        }

        // `gh`, which runs as the settings' user, reaches the tarball and the bundle: both are
        // root's and 0644 now, in a directory it can pass through and not list.
        let bundle = scratch.dl.join(format!("{}.attestation", rec.name));
        write_file(&bundle, 0o644, &rec.attestation).map_err(Self::io(&bundle))?;
        fs::set_permissions(&tarball, fs::Permissions::from_mode(0o644))
            .map_err(Self::io(&tarball))?;
        fs::set_permissions(&scratch.dl, fs::Permissions::from_mode(0o711))
            .map_err(Self::io(&scratch.dl))?;
        let mut verifier = Verifier::pinned(self.host, self.settings);
        verifier.timeout = self.bounds.gh;
        let tools = Tools {
            trust: self.trust,
            runner: self.runner,
        };
        verifier.verify(&tools, &scratch.gh, &tarball, &bundle, &rec.commit)?;

        let binary = scratch.release.join(&rec.name);
        let deadline = Instant::now() + self.bounds.extract;
        unpack_one_until(
            &tarball,
            &rec.name,
            &binary,
            extract_cap(rec.size),
            Some(deadline),
        )
        .map_err(|fault: ArchiveFault| Refusal::Archive {
            component: rec.name.clone(),
            fault,
        })?;

        let (want, _) = self.wanted();
        let found = BinaryKind::of(&header(&binary).map_err(Self::io(&binary))?);
        if found != want {
            return Err(Refusal::WrongPlatform {
                component: rec.name.clone(),
                found,
                want,
            }
            .into());
        }
        if self.host.platform() == Platform::MacOs {
            let timeout = self.settings.timeout;
            if darwin::quarantined(self.runner, &binary, timeout)? {
                return Err(Refusal::Quarantined {
                    name: rec.name.clone(),
                }
                .into());
            }
            darwin::check_signature(self.runner, &rec.name, &binary, timeout)?;
        }
        let facts = self.probe(rec, &binary, empty)?;
        let digest = file_sha256(&binary).map_err(Self::io(&binary))?;
        Ok((facts, digest))
    }

    /// Step 9: the marker, then the rename.
    fn place(
        &self,
        rec: &ReleaseLocator,
        scratch: &Scratch,
        facts: &ReleaseFacts,
        binary_sha256: &str,
    ) -> Result<PathBuf, FetchError> {
        let marker = Marker {
            schema: MARKER_SCHEMA,
            commit: rec.commit.to_string(),
            name: rec.name.clone(),
            tarball_sha256: rec.sha256.clone(),
            binary_sha256: binary_sha256.to_owned(),
            facts: facts.clone(),
        };
        let text = serde_json::to_vec(&marker).expect("plain fields serialize");
        let releases = self.releases();
        let dest = releases.join(rec.commit.as_str());
        match fs::symlink_metadata(&dest) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(Self::io(&dest)(e)),
            Ok(_) => {
                // A directory is there: one nobody verified, or one whose marker no longer
                // matches. Verified bytes are never replaced; unverified ones the host runs
                // (`current`, `previous`) are adopted only if they are the bytes just verified.
                let held = dest.join(&rec.name);
                let same = file_sha256(&held).ok().as_deref() == Some(binary_sha256);
                if marked(&dest) && !same {
                    return Err(FetchError::Differs { path: held });
                }
                if referenced(&releases, &rec.commit) {
                    if !same {
                        return Err(FetchError::Differs { path: held });
                    }
                    let tmp = dest.join(format!("{MARKER}.new"));
                    write_file(&tmp, 0o644, &text).map_err(Self::io(&tmp))?;
                    fs::rename(&tmp, dest.join(MARKER)).map_err(Self::io(&dest))?;
                    sync_dir(&dest).map_err(Self::io(&dest))?;
                    return Ok(dest);
                }
                fs::remove_dir_all(&dest).map_err(Self::io(&dest))?;
            }
        }
        let release = &scratch.release;
        write_file(&release.join(MARKER), 0o644, &text).map_err(Self::io(release))?;
        fs::set_permissions(release.join(&rec.name), fs::Permissions::from_mode(0o755))
            .map_err(Self::io(release))?;
        sync_dir(release).map_err(Self::io(release))?;
        fs::rename(release, &dest).map_err(Self::io(&dest))?;
        sync_dir(&releases).map_err(Self::io(&releases))?;
        Ok(dest)
    }
}

impl Fetcher for RootFetcher<'_> {
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
        self.check(rec)?;
        let Source::Url(url) = &self.config.source else {
            return Err(FetchError::FrontendUnsupported);
        };
        let (releases, work) = (self.releases(), self.work());
        make_dir(&releases).map_err(Self::io(&releases))?;
        make_dir(&work).map_err(Self::io(&work))?;
        self.sweep().map_err(Self::io(&work))?;
        if let Some(done) = self.installed(rec)? {
            return Ok(done);
        }
        let dest = releases.join(rec.commit.as_str());
        let replaced = fs::metadata(dest.join(&rec.name)).map_or(0, |m| m.len());
        let need = room_needed(rec.size, replaced);
        let free = (self.free)(&work).map_err(Self::io(&work))?;
        if free < need {
            return Err(FetchError::NoRoom {
                need,
                free,
                path: self.root(),
            });
        }
        let empty = self.ensure_empty()?;
        let scratch = Scratch::make(&work, &rec.commit, self.settings.run_as)
            .map_err(Self::io(&work))?;
        let (facts, digest) = self.verified(rec, &scratch, &empty, url.as_str())?;
        let dir = self.place(rec, &scratch, &facts, &digest)?;
        Ok(InstalledRelease {
            commit: rec.commit.clone(),
            name: rec.name.clone(),
            dir,
            facts,
            fetched: true,
        })
    }
}

/// The scratch directory of one fetch, removed when it is dropped:
/// `work/<commit>.partial/` with `dl/` (0700 root: the tarball), `gh/` (the home of `gh`),
/// and `release/` (what becomes `releases/<commit>/`).
struct Scratch {
    dir: PathBuf,
    dl: PathBuf,
    gh: PathBuf,
    release: PathBuf,
}

impl Scratch {
    fn make(work: &Path, commit: &CommitId, gh_user: RunAs) -> io::Result<Scratch> {
        let dir = work.join(format!("{commit}.partial"));
        fs::create_dir(&dir)?;
        let made = Scratch {
            dl: dir.join("dl"),
            gh: dir.join("gh"),
            release: dir.join("release"),
            dir,
        };
        fs::set_permissions(&made.dir, fs::Permissions::from_mode(0o755))?;
        for (sub, mode) in [(&made.dl, 0o700), (&made.gh, 0o700), (&made.release, 0o755)] {
            fs::create_dir(sub)?;
            fs::set_permissions(sub, fs::Permissions::from_mode(mode))?;
        }
        std::os::unix::fs::chown(&made.gh, Some(gh_user.uid), Some(gh_user.gid))?;
        Ok(made)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn make_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o755).create(path)
}

fn write_file(path: &Path, mode: u32, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn header(path: &Path) -> io::Result<Vec<u8>> {
    let mut header = Vec::new();
    File::open(path)?.take(32).read_to_end(&mut header)?;
    Ok(header)
}

fn file_sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex::encode(hash.finalize()));
        }
        hash.update(&buf[..n]);
    }
}

/// Whether `path` is a regular file (not followed if a link) of this process's user that no
/// one else can write.
fn owned_plain(path: &Path) -> io::Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    Ok(meta.is_file()
        && meta.uid() == rustix::process::geteuid().as_raw()
        && meta.mode() & 0o022 == 0)
}

/// The marker at `path`, if it is a plain file of this user that others cannot write and holds
/// a marker of this schema.
fn read_marker(path: &Path) -> Option<Marker> {
    if !owned_plain(path).ok()? {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut text = Vec::new();
    file.take(MARKER_MAX).read_to_end(&mut text).ok()?;
    let marker: Marker = serde_json::from_slice(&text).ok()?;
    (marker.schema == MARKER_SCHEMA).then_some(marker)
}
