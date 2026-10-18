//! Stage a release from an inbox entry: check everything, install it into
//! `releases/<commit>/`, activate nothing.
//!
//! The entry is the unprivileged `kbf` process's directory, so nothing in it is believed
//! until the attestation holds. The order is the safety property; each step happens only
//! if every one before it passed:
//!
//! 1. The trust list (`gh`, the trusted root, the settings) is root's alone
//!    ([`crate::trust`]).
//! 2. The entry is opened without following a link, and `offer.json` is read from it
//!    ([`crate::inbox`], [`crate::offer`]). The entry holds the offer and the files it
//!    names, nothing else.
//! 3. On macOS, none of the offered files carries the quarantine attribute.
//! 4. Each offered file is opened beneath the entry (no link followed; a FIFO, device,
//!    hard-linked, empty or oversize file is refused) and copied by its open descriptor
//!    into a scratch directory root owns. From here on the original is never read.
//! 5. Each **copy** is hashed, and its sha256 must be the one the offer names.
//! 6. The attestation gate ([`crate::attest`]) accepts every component's tarball copy
//!    with the bundle copy, for the offered commit, on an allowed ref. **Nothing has
//!    been unpacked or run yet.**
//! 7. Each tarball is unpacked ([`crate::archive`]): one regular file, named for the
//!    component, no path.
//! 8. The unpacked file is the kind of binary this host runs, by its header
//!    ([`crate::binary`]): an ELF on Linux, a Mach-O on macOS, for this CPU. An attested
//!    file for another platform is refused here, unrun.
//! 9. On macOS, its code signature is valid and ad hoc with the hardened runtime
//!    ([`crate::darwin`]).
//! 10. It is run as the unprivileged user (`--version --json`, an empty environment, an
//!     empty directory, a timeout) and must say it is the offered commit's release
//!     build.
//! 11. The release is installed through [`release::stage`](crate::release::stage): a
//!     root-owned `releases/<commit>/`, the state's `staged` set, no link moved. A
//!     release already there with other bytes is refused, never replaced.
//!
//! The scratch directory (`<state dir>/work/stage.*`) is root's and is removed whatever
//! the outcome. It is world-readable and not world-writable, so the unprivileged user
//! can read the tarball `gh` verifies and run the staged binary, and cannot change them.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kbf_host::exec::Exec;
use kbf_host::plan::Mode;
use kbf_host::step::Owner;
use sha2::{Digest, Sha256};

use crate::CommitId;
use crate::archive::unpack_one;
use crate::attest::{SourceRef, Tools, Verifier};
use crate::binary::{BinaryKind, Version};
use crate::darwin;
use crate::inbox::{self, Entry};
use crate::layout::{Layout, Platform};
use crate::offer::{OFFER_FILE, Offer, OfferedFile};
use crate::refusal::{ArchiveFault, InboxFault, Refusal, VersionFault};
use crate::release::{self, Install};
use crate::run::{Runner, Spawn};
use crate::settings::Settings;

/// The most of each kind of file a stage accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub offer: u64,
    pub bundle: u64,
    pub tarball: u64,
    /// The unpacked binary.
    pub binary: u64,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        offer: 64 * 1024,
        bundle: 8 * 1024 * 1024,
        tarball: 150 * 1024 * 1024,
        binary: 512 * 1024 * 1024,
    };
}

/// A staged release: what the stage verified and installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub commit: CommitId,
    /// The binaries installed: `kbf` (all a Mac has), `kbf-daemon` or both.
    pub components: Vec<&'static str>,
    /// The branch the attestation named.
    pub source_ref: SourceRef,
    /// From the staged binary's own `--version --json`, for the rollout planner.
    pub protocol: u64,
    pub min_protocol: u64,
    pub state_schema: u64,
    pub version: String,
    /// What the install printed: the plan and the state change.
    pub log: String,
}

/// Everything a stage needs from the host.
pub struct Context<'a, E: Exec> {
    pub host: &'a Layout,
    pub settings: &'a Settings,
    /// Changes the host: the install into `releases/`.
    pub exec: &'a mut E,
    /// Says who owns the trust list and reads the settings; the host's own on a host.
    pub trust: &'a dyn Exec,
    pub runner: &'a dyn Runner,
    /// Who owns the release: [`release::ROOT`] on a host.
    pub owner: Owner,
    pub limits: Limits,
}

/// A hook given the entry directory and the name of the file about to be opened.
pub type BeforeOpen<'a> = &'a dyn Fn(&Path, &str);

/// Points where a test acts between two steps; a stage in production has none.
#[derive(Default)]
pub struct Hooks<'a> {
    /// After the entry was listed and before the file `name` in it is opened.
    pub before_open: Option<BeforeOpen<'a>>,
    /// After every offered file was copied, with the directory holding the copies.
    pub after_copy: Option<&'a dyn Fn(&Path)>,
}

/// Stage the release in `entry_dir` (see the module docs for what is checked, in order).
pub fn stage_from_inbox<E: Exec>(
    ctx: &mut Context<E>,
    entry_dir: &Path,
) -> Result<Staged, Refusal> {
    stage_with(ctx, entry_dir, &Hooks::default())
}

/// [`stage_from_inbox`] with `hooks`.
pub fn stage_with<E: Exec>(
    ctx: &mut Context<E>,
    entry_dir: &Path,
    hooks: &Hooks,
) -> Result<Staged, Refusal> {
    let env = Env {
        host: ctx.host,
        settings: ctx.settings,
        trust: ctx.trust,
        runner: ctx.runner,
        limits: ctx.limits,
    };
    let work = Work::make(ctx.host, ctx.settings)?;
    let checked = env.check(entry_dir, &work, hooks)?;
    let install = Install {
        commit: checked.commit.clone(),
        from: work.out(),
    };
    let mut log = Vec::new();
    release::stage(
        &mut *ctx.exec,
        ctx.host,
        ctx.owner,
        &install,
        Mode::Apply,
        &mut log,
    )?;
    Ok(Staged {
        commit: checked.commit,
        components: checked.components,
        source_ref: checked.source_ref,
        protocol: checked.version.protocol,
        min_protocol: checked.version.min_protocol,
        state_schema: checked.version.state_schema,
        version: checked.version.version,
        log: String::from_utf8_lossy(&log).into_owned(),
    })
}

/// What a stage asks of the host before it installs, not generic over the host so one
/// copy of the checks is measured for coverage.
struct Env<'a> {
    host: &'a Layout,
    settings: &'a Settings,
    trust: &'a dyn Exec,
    runner: &'a dyn Runner,
    limits: Limits,
}

/// A release that passed every check, unpacked in the scratch directory.
struct Checked {
    commit: CommitId,
    components: Vec<&'static str>,
    source_ref: SourceRef,
    version: Version,
}

impl Env<'_> {
    fn check(&self, entry_dir: &Path, work: &Work, hooks: &Hooks) -> Result<Checked, Refusal> {
        let verifier = Verifier::pinned(self.host, self.settings);
        verifier.check_trust(self.trust)?;

        let entry = Entry::open(entry_dir)?;
        let offer = self.read_offer(&entry)?;
        let names = offer.names();
        for name in entry.names()? {
            if !names.contains(&name) {
                return Err(Refusal::Inbox {
                    name,
                    fault: InboxFault::Unexpected,
                });
            }
        }
        let offered = self.offered_files(&offer);
        if self.host.platform() == Platform::MacOs {
            for (file, _) in &offered {
                let path = entry_dir.join(&file.file);
                if darwin::quarantined(self.runner, &path, self.settings.timeout)? {
                    return Err(Refusal::Quarantined {
                        name: file.file.clone(),
                    });
                }
            }
        }

        for (file, cap) in &offered {
            if let Some(hook) = hooks.before_open {
                hook(entry_dir, &file.file);
            }
            let mut open = entry.open_file(&file.file, *cap)?;
            inbox::copy_capped(&mut open, &work.input().join(&file.file), *cap, &file.file)?;
        }
        if let Some(hook) = hooks.after_copy {
            hook(&work.input());
        }
        for (file, _) in &offered {
            let copy = work.input().join(&file.file);
            let got = sha256_of(&copy).map_err(work_err(&copy))?;
            if got != file.sha256 {
                return Err(Refusal::HashMismatch {
                    name: file.file.clone(),
                });
            }
        }

        let tools = Tools {
            trust: self.trust,
            runner: self.runner,
        };
        let bundle = work.input().join(&offer.bundle.file);
        let mut source_ref = None;
        for c in &offer.components {
            let tarball = work.input().join(&c.tarball.file);
            let verified = verifier.verify(&tools, &work.gh(), &tarball, &bundle, &offer.commit)?;
            source_ref.get_or_insert(verified.source_ref);
        }

        let want = BinaryKind::wanted(self.host.platform(), self.settings.arch);
        let mut version = None;
        for c in &offer.components {
            let tarball = work.input().join(&c.tarball.file);
            let binary = work.out().join(c.name);
            let archive = |fault: ArchiveFault| Refusal::Archive {
                component: c.name.to_owned(),
                fault,
            };
            unpack_one(&tarball, c.name, &binary, self.limits.binary).map_err(archive)?;
            let found = header_of(&binary).map_err(work_err(&binary))?;
            let found = BinaryKind::of(&found);
            if found != want {
                return Err(Refusal::WrongPlatform {
                    component: c.name.to_owned(),
                    found,
                    want,
                });
            }
            if self.host.platform() == Platform::MacOs {
                darwin::check_signature(self.runner, c.name, &binary, self.settings.timeout)?;
            }
            let said = self.version_of(c.name, &binary, work)?;
            said.check(&offer.commit)
                .map_err(|fault| Refusal::Version {
                    component: c.name.to_owned(),
                    fault,
                })?;
            version.get_or_insert(said);
        }
        Ok(Checked {
            commit: offer.commit,
            components: offer.components.iter().map(|c| c.name).collect(),
            source_ref: source_ref.expect("an offer names a component"),
            version: version.expect("an offer names a component"),
        })
    }

    /// `offer.json`, read from the entry and parsed.
    fn read_offer(&self, entry: &Entry) -> Result<Offer, Refusal> {
        let mut file = entry.open_file(OFFER_FILE, self.limits.offer)?;
        let bytes = inbox::read_capped(&mut file, self.limits.offer, OFFER_FILE)?;
        let offer = Offer::parse(&bytes).map_err(Refusal::Offer)?;
        offer.fits(self.host.platform()).map_err(Refusal::Offer)?;
        Ok(offer)
    }

    /// Every file the offer names with the most of it a stage takes: the tarballs, then
    /// the bundle.
    fn offered_files(&self, offer: &Offer) -> Vec<(OfferedFile, u64)> {
        let mut files: Vec<_> = offer
            .components
            .iter()
            .map(|c| (c.tarball.clone(), self.limits.tarball))
            .collect();
        files.push((offer.bundle.clone(), self.limits.bundle));
        files
    }

    /// `<binary> --version --json`, run as the unprivileged user.
    fn version_of(&self, component: &str, binary: &Path, work: &Work) -> Result<Version, Refusal> {
        let spawn = Spawn {
            program: binary.to_owned(),
            args: vec!["--version".into(), "--json".into()],
            env: Vec::new(),
            cwd: work.empty(),
            run_as: Some(self.settings.run_as),
            timeout: self.settings.timeout,
        };
        let ran = self.runner.run(&spawn).map_err(|source| Refusal::Tool {
            what: "the staged binary's --version --json",
            source,
        })?;
        if ran.timed_out {
            return Err(Refusal::TimedOut {
                what: "the staged binary's --version --json",
            });
        }
        let version = |fault| Refusal::Version {
            component: component.to_owned(),
            fault,
        };
        if ran.code != Some(0) {
            return Err(version(VersionFault::Failed(ran.code)));
        }
        Version::parse(&ran.stdout).map_err(version)
    }
}

/// The refusal for an `io::Error` from using `path` in the scratch directory.
fn work_err(path: &Path) -> impl FnOnce(io::Error) -> Refusal {
    let path = path.to_owned();
    move |source| Refusal::Work { path, source }
}

/// The lowercase hex sha256 of the file at `path`, read in full.
fn sha256_of(path: &Path) -> io::Result<String> {
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

/// The first 32 bytes of the file at `path` (fewer if it is shorter).
fn header_of(path: &Path) -> io::Result<Vec<u8>> {
    let mut header = Vec::new();
    File::open(path)?.take(32).read_to_end(&mut header)?;
    Ok(header)
}

/// The scratch directory of one stage, removed when it is dropped.
struct Work {
    dir: PathBuf,
}

impl Work {
    /// Make `<state dir>/work/stage.<pid>.<n>`: root's, 0755, with `in/` (the copies),
    /// `out/` (the unpacked binaries), `empty/` (a directory with nothing in it) and
    /// `gh/` (the home of `gh`: the unprivileged user's alone, 0700).
    fn make(host: &Layout, settings: &Settings) -> Result<Work, Refusal> {
        let parent = host.join(&host.work_dir());
        // Each directory above is made 0755 when it is missing.
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o755);
        builder.create(&parent).map_err(work_err(&parent))?;
        // The first free name: a directory a crashed stage left is not touched.
        let mut n = 0;
        let dir = loop {
            let dir = parent.join(format!("stage.{}.{n}", std::process::id()));
            match fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(work_err(&dir)(e)),
            }
        };
        let made = Work { dir };
        let dirs = [
            (made.input(), 0o755),
            (made.out(), 0o755),
            (made.empty(), 0o755),
            (made.gh(), 0o700),
        ];
        // Whatever the umask, the stage's directory and the three the unprivileged user
        // reads (`in/`, `out/`, `empty/`) are open to read; `gh/` is its alone.
        fs::set_permissions(&made.dir, fs::Permissions::from_mode(0o755))
            .map_err(work_err(&made.dir))?;
        for (sub, mode) in dirs {
            fs::create_dir(&sub).map_err(work_err(&sub))?;
            fs::set_permissions(&sub, fs::Permissions::from_mode(mode)).map_err(work_err(&sub))?;
        }
        let gh = made.gh();
        std::os::unix::fs::chown(&gh, Some(settings.run_as.uid), Some(settings.run_as.gid))
            .map_err(work_err(&gh))?;
        Ok(made)
    }

    fn input(&self) -> PathBuf {
        self.dir.join("in")
    }

    fn out(&self) -> PathBuf {
        self.dir.join("out")
    }

    fn empty(&self) -> PathBuf {
        self.dir.join("empty")
    }

    fn gh(&self) -> PathBuf {
        self.dir.join("gh")
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
#[path = "stage_tests.rs"]
mod tests;
