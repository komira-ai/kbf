//! The fetcher of the release the package carries (see [`crate::seed`]): it brings in no
//! bytes from anywhere, it copies the seed's tarball into the scratch directory a fetch would
//! have filled and hands it to the verify pipeline, which checks it exactly as it checks a
//! fetched one. Nothing here decides anything the pipeline decides: the digest and size the
//! pipeline holds the tarball to are the manifest's, the attestation is the bundle in the seed
//! directory, and the facts the binary must declare are the manifest's.
//!
//! What this fetcher adds to the pipeline's checks, before the pipeline runs, is consistency
//! of the package: the seed directory is root's and so are the files in it
//! ([`trusted_chain`]), the manifest names the commit this installer was built from, the order
//! names the manifest's commit (all 40 digits), the bundle is the one the manifest's digest
//! names, and the facts an order expects are the manifest's. After it, the `seq` the binary
//! declares must be the manifest's. A package that fails any of them is a package that is
//! missing a file, was built from two builds, or was swapped; none of it can make a release
//! pass the pipeline that would not.
//!
//! Whether a host may use the seed at all (the floor on `seq`, the order's source) is not this
//! fetcher's: the cycle checks the first before it asks, and
//! [`HostFetcher`](super::fetcher_host::HostFetcher) decides the second.
//!
//! Between orders the run loop also asks for a look ([`SeedFetcher::scan`]): the same files,
//! checked as far as they can be without an order, to tell the frontend the seed is there. It
//! checks what an order would (the files are root's, the manifest is this build's, the bundle is
//! the one named) and the room, the tarball's size and digest, the three trust files the
//! manifest was made beside, and the attestation. It copies, unpacks, installs and runs
//! nothing but `gh`. And an installed release can be asked for its version ([`Fetcher::probe`]),
//! which the floor on `seq` is made from.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::slice;

use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;
use kbf_host::step::Owner;
use kbf_update::{Facts, Locator, Source};
use sha2::{Digest, Sha256};

use crate::CommitId;
use crate::binary::Version;
use crate::fetch;
use crate::helper::fetcher::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::fetcher_real::{Parts, fetch_error, refused, replaced_bytes};
use crate::helper::leases::LeaseSource;
use crate::layout::{GH, Layout, RELEASE_BINARY, SETTINGS, TRUSTED_ROOT};
use crate::refusal::{Refusal, TrustFault};
use crate::run::Runner;
use crate::seed::{COMPONENT, Looked, MANIFEST, Manifest, Noticed, SeedFacts};
use crate::settings::Settings;
use crate::stage::{Fetched, Order, OrderKind, Partial, Pipeline, Policy, Received, Verifier};
use crate::trust::trusted_chain;

/// Verifies and installs the release the package carries, as of the moment it is asked.
pub struct SeedFetcher {
    pub host: Layout,
    pub settings: Settings,
    pub policy: Policy,
    /// Says who owns the seed and the trust list and reads the small files; the host's own on
    /// a host.
    pub trust: Box<dyn Exec>,
    pub runner: Box<dyn Runner>,
    /// Observed at the pipeline's lease step, for an activation.
    pub leases: Box<dyn LeaseSource>,
    /// Who owns the scratch directory and the release: root on a host.
    pub owner: Owner,
    /// The first 12 hex digits of the commit this installer was built from, which name its
    /// seed directory.
    pub build: String,
}

/// A seed read and found consistent with itself.
struct Open {
    manifest: Manifest,
    /// The sha256 of the manifest's bytes, lowercase hex: what a look is remembered by.
    digest: String,
    /// The tarball, which is copied from the disk and not read through the trust executor.
    tarball: HostPath,
    /// The attestation bundle's text.
    bundle: String,
}

impl SeedFetcher {
    /// A fetcher for the host `host` with `settings` and `policy`, taking from `parts` what
    /// the host supplies (the fetch child, which a seed never starts, is dropped), reading the
    /// seed directory of the installer built from `build`.
    pub fn new(
        host: Layout,
        settings: Settings,
        policy: Policy,
        parts: Parts,
        build: &str,
    ) -> SeedFetcher {
        SeedFetcher {
            host,
            settings,
            policy,
            trust: parts.trust,
            runner: parts.runner,
            leases: parts.leases,
            owner: parts.owner,
            build: build.to_owned(),
        }
    }

    /// The seed directory, its manifest, tarball and bundle, found and consistent: every path
    /// is root's and not writable by others, the manifest is for this installer's build, and
    /// the bundle is the one the manifest names. The tarball's bytes are left to the pipeline,
    /// which holds them to the manifest's digest and size.
    fn open(&self) -> Result<Open, FetchError> {
        let build = &self.build;
        if !is_build_commit(build) {
            let why = format!(
                "seed_missing: this installer does not know its build commit ({build:?}), so it knows no seed directory"
            );
            return Err(FetchError::Unavailable(why));
        }
        self.trusted(&self.host.seed_dir(build), Kind::Dir)?;
        let text = self.read(&self.host.seed_file(build, MANIFEST))?;
        let manifest = Manifest::parse(&text).map_err(|why| mismatch(&why))?;
        if !manifest.commit.as_str().starts_with(build.as_str()) {
            return Err(mismatch(&format!(
                "the manifest is for commit {}, not for this installer's build {build}",
                manifest.commit
            )));
        }
        let tarball = self.host.seed_file(build, &manifest.tarball_name());
        self.trusted(&tarball, Kind::File)?;
        let bundle = self.read(&self.host.seed_file(build, &manifest.bundle_name()))?;
        if hex::encode(Sha256::digest(&bundle)) != manifest.bundle_sha256.as_str() {
            return Err(mismatch("the bundle is not the one the manifest names"));
        }
        // A bundle that is not text is changed here, and `gh` then refuses it.
        let bundle = String::from_utf8_lossy(&bundle).into_owned();
        Ok(Open {
            manifest,
            digest: hex::encode(Sha256::digest(&text)),
            tarball,
            bundle,
        })
    }

    /// `path` and every directory above it are root's and not writable by others. A path that
    /// is not there is a missing seed, which is the package's fault and not a verdict on a
    /// release.
    fn trusted(&self, path: &HostPath, kind: Kind) -> Result<(), FetchError> {
        trusted_chain(&*self.trust, path, kind).map_err(|r| {
            if matches!(
                &r,
                Refusal::Untrusted {
                    fault: TrustFault::Missing,
                    ..
                }
            ) {
                FetchError::Unavailable(format!("seed_missing: {path} is missing"))
            } else {
                refused(r)
            }
        })
    }

    /// A small file of the seed, through the trust executor.
    fn read(&self, path: &HostPath) -> Result<Vec<u8>, FetchError> {
        let Ok(Some(Entry::File { bytes, .. })) = self.trust.inspect(path) else {
            let why = format!("seed_missing: {path} is missing, or is not a file");
            return Err(FetchError::Unavailable(why));
        };
        self.trusted(path, Kind::File)?;
        Ok(bytes)
    }

    /// The package's seed as the run loop sees it, between orders: `None` when the package
    /// carries none (an installer that can have no seed directory, or a package with no seed
    /// directory for its build: the normal state of a host whose package has none, and no
    /// trouble), the seed found whole otherwise, or why it is not. `known` is the seed found
    /// whole by an earlier look: while the manifest has the digest it was found by, the answer
    /// is `known` and nothing is checked again, so the package is read in full once per
    /// manifest and not at every pass. Nothing is copied, unpacked, installed or run beyond
    /// `gh`; the order, later, does all of that again on the files it is given.
    pub fn scan(&self, known: Option<&Looked>) -> Result<Option<Looked>, FetchError> {
        if !is_build_commit(&self.build) {
            return Ok(None);
        }
        let dir = self.host.seed_dir(&self.build);
        if matches!(self.trust.stat(&dir), Ok(None)) {
            return Ok(None);
        }
        let seed = self.open()?;
        if let Some(known) = known.filter(|known| known.digest == seed.digest) {
            return Ok(Some(known.clone()));
        }
        self.whole(&seed)?;
        Ok(Some(Looked {
            seed: seed.manifest.seed_facts(),
            digest: seed.digest,
        }))
    }

    /// What a seed that is consistent with itself must also be to be told available: the disk
    /// has room for an order to install it, the tarball is the size and has the digest the
    /// manifest names, the three trust files are the ones the manifest was made beside (a
    /// package half way through an upgrade is not whole), and `gh` finds the bundle a valid
    /// attestation of the tarball, offline, for the manifest's commit.
    fn whole(&self, seed: &Open) -> Result<(), FetchError> {
        let manifest = &seed.manifest;
        let size = manifest.tarball_size.min(self.policy.max_component_bytes);
        let need = fetch::room_needed(size, replaced_bytes(&self.host));
        let state = self.host.join(&self.host.state_dir());
        fetch::ensure_room(&state, need).map_err(fetch_error)?;

        let from = self.host.join(&seed.tarball);
        let missing = |e: io::Error| {
            FetchError::Unavailable(format!("seed_missing: {}: {e}", from.display()))
        };
        // Counted and hashed in one read of at most one byte more than the manifest says, as a
        // fetch counts what it brings in: a file that is longer is seen to be, not read to its end.
        let file = open_nofollow(&from).map_err(missing)?;
        let says = manifest.tarball_size;
        let mut limited = file.take(says.saturating_add(1));
        let read = copy_hashed(&mut limited, &mut io::sink());
        let (len, digest) = read.map_err(|fault| match fault {
            CopyFault::Read(e) | CopyFault::Write(e) => missing(e),
        })?;
        if len != says {
            let why = format!("the tarball is not the {says} bytes the manifest names");
            return Err(flux(&why));
        }
        if digest != manifest.tarball_sha256.as_str() {
            return Err(flux("the tarball is not the one the manifest names"));
        }

        let (root, conf) = (self.host.trusted_root(), self.host.settings());
        let trust = [
            (self.host.gh(), GH, &manifest.gh_sha256),
            (root, TRUSTED_ROOT, &manifest.trusted_root_sha256),
            (conf, SETTINGS, &manifest.installer_conf_sha256),
        ];
        for (path, name, want) in trust {
            if hex::encode(Sha256::digest(self.read(&path)?)) != want.as_str() {
                let why = format!("{name} is not the one the manifest was made for");
                return Err(flux(&why));
            }
        }

        let pipeline = self.pipeline();
        let attested = pipeline.attest_in_place(&manifest.commit, COMPONENT, &from, &seed.bundle);
        attested.map_err(refused)?;
        Ok(())
    }

    /// The verify pipeline over this host's parts.
    fn pipeline(&self) -> Pipeline<'_> {
        Pipeline {
            host: &self.host,
            settings: &self.settings,
            policy: self.policy,
            trust: &*self.trust,
            runner: &*self.runner,
            leases: &*self.leases,
            owner: self.owner,
        }
    }

    /// The tarball streamed into a new file of `partial`, where a fetch would have left it,
    /// counted and hashed as the fetch parent counts and hashes what it brings in. The room is
    /// asked for first, as a fetch asks. The copy is of at most one byte more than the manifest
    /// says (the pipeline sees that the file is bigger), a link in the tarball's place is not
    /// followed, and a failure to read it (the package's fault) is told from one to write it
    /// (the host's).
    fn copy(&self, partial: &Partial, seed: &Open) -> Result<Received, FetchError> {
        // The host's cap is at most a gigabyte (`installer.toml` is held to it), so one more
        // cannot overflow.
        let cap = self.policy.max_component_bytes;
        let size = seed.manifest.tarball_size.min(cap);
        let need = fetch::room_needed(size, replaced_bytes(&self.host));
        fetch::ensure_room(&partial.dl(), need).map_err(fetch_error)?;
        let from = self.host.join(&seed.tarball);
        let to = partial.dl().join(format!("{COMPONENT}.tar.gz"));
        let missing = |e: io::Error| {
            FetchError::Unavailable(format!("seed_missing: {}: {e}", from.display()))
        };
        let source = open_nofollow(&from).map_err(missing)?;
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&to)
            .map_err(|e| FetchError::Unavailable(format!("cannot create {}: {e}", to.display())))?;
        let copied = copy_hashed(&mut source.take(size + 1), &mut out);
        let (bytes, sha256) = match copied {
            Ok(done) => done,
            Err(CopyFault::Read(e)) => return Err(missing(e)),
            Err(CopyFault::Write(e)) => {
                let why = format!("cannot write {}: {e}", to.display());
                return Err(FetchError::Unavailable(why));
            }
        };
        Ok(Received {
            name: COMPONENT.to_owned(),
            tarball: to,
            bytes,
            sha256,
        })
    }

    /// The seed through the verify pipeline as an order of `kind`: the order names the seed's
    /// commit, `expected` (if the caller has facts of its own) are the manifest's, and the
    /// release is held to the manifest's facts by the pipeline whatever `kind` is.
    fn pull(
        &self,
        kind: OrderKind,
        rec: &ReleaseLocator,
        expected: Option<&Facts>,
    ) -> Result<InstalledRelease, FetchError> {
        // Only the package's release is brought in here: a locator to fetch is the fetching
        // fetcher's.
        if rec.source != Source::Staged {
            return Err(FetchError::Order {
                word: "denied",
                why: "source=fetch names a release to fetch, which the package does not carry"
                    .to_owned(),
            });
        }
        let seed = self.open()?;
        let facts = seed.manifest.facts();
        if seed.manifest.commit != rec.commit {
            return Err(mismatch(&format!(
                "this host carries {}, not {}",
                seed.manifest.commit, rec.commit
            )));
        }
        if expected.is_some_and(|e| *e != facts) {
            return Err(FetchError::Order {
                word: "facts",
                why: "the facts the order expects are not the ones the seed's manifest declares"
                    .to_owned(),
            });
        }
        let partial = Partial::create(&self.host, &rec.commit, self.owner).map_err(refused)?;
        let received = self.copy(&partial, &seed)?;
        let locator = Locator {
            name: COMPONENT.to_owned(),
            sha256: seed.manifest.tarball_sha256.as_str().to_owned(),
            size: seed.manifest.tarball_size,
            attestation: seed.bundle,
        };
        let order = Order {
            kind,
            commit: &rec.commit,
            components: slice::from_ref(&locator),
            expected: Some(&facts),
        };
        let fetched = Fetched {
            partial,
            files: vec![received],
        };
        let staged = self.pipeline().verify(&order, fetched).map_err(refused)?;
        let declared = staged.version.seq;
        if declared != Some(seed.manifest.seq) {
            return Err(FetchError::Order {
                word: "facts",
                why: format!(
                    "seq: the binary declares {declared:?}, the manifest says {}",
                    seed.manifest.seq
                ),
            });
        }
        Ok(staged.installed())
    }
}

impl Fetcher for SeedFetcher {
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
        self.pull(OrderKind::FrontendUpdate, rec, None)
    }

    fn fetch_activation(
        &self,
        rec: &ReleaseLocator,
        expected: &Facts,
    ) -> Result<InstalledRelease, FetchError> {
        self.pull(OrderKind::Activate, rec, Some(expected))
    }

    fn seed(&self) -> Result<SeedFacts, FetchError> {
        Ok(self.open()?.manifest.seed_facts())
    }

    fn look(&self, known: Option<&Looked>) -> Noticed {
        Noticed {
            source: None,
            seed: self.scan(known),
        }
    }

    /// The release's own binary, run from its own directory as the unprivileged user the
    /// pipeline runs a binary as; what it prints is read as the pipeline reads it.
    fn probe(&self, commit: &CommitId) -> Result<Version, FetchError> {
        let release = self.host.join(&self.host.release_dir(commit));
        let binary = release.join(RELEASE_BINARY);
        let said = self.pipeline().probe(&binary, COMPONENT, &release);
        said.map_err(refused)
    }
}

/// A package that is half way to being another (a package manager unpacks files one by one):
/// not a verdict on the release, and looked at again at the next pass.
fn flux(why: &str) -> FetchError {
    FetchError::Unavailable(format!("package_in_flux: {why}"))
}

/// A package whose parts do not agree with each other or with this installer: refused under
/// the word `commit_or_ref`, naming `seed_mismatch`.
fn mismatch(why: &str) -> FetchError {
    FetchError::Order {
        word: "commit_or_ref",
        why: format!("seed_mismatch: {why}"),
    }
}

/// The first 12 hex digits of a commit, as a seed directory is named.
fn is_build_commit(build: &str) -> bool {
    let hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
    build.len() == 12 && build.bytes().all(hex)
}

/// The file at `path` opened for reading, not if it is a link (and without waiting, so a pipe
/// in its place cannot hold root up).
fn open_nofollow(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// Why bytes did not get from the package's file to the scratch directory: the package could
/// not be read, or the host could not write.
#[derive(Debug)]
enum CopyFault {
    Read(io::Error),
    Write(io::Error),
}

/// A writer that counts and hashes what it passes on, and remembers that a failure was its
/// destination's.
struct Hashing<'a, W> {
    to: &'a mut W,
    hasher: Sha256,
    bytes: u64,
    failed: bool,
}

impl<W: Write> Write for Hashing<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.to.write(buf).inspect_err(|_| self.failed = true)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.to.flush().inspect_err(|_| self.failed = true)
    }
}

/// All of `from` written to `to`: how many bytes, and their sha256 in lowercase hex.
fn copy_hashed(from: &mut impl Read, to: &mut impl Write) -> Result<(u64, String), CopyFault> {
    let mut hashing = Hashing {
        to,
        hasher: Sha256::new(),
        bytes: 0,
        failed: false,
    };
    let done = io::copy(from, &mut hashing).and_then(|_| hashing.flush());
    match done {
        Ok(()) => Ok((hashing.bytes, hex::encode(hashing.hasher.finalize()))),
        Err(e) if hashing.failed => Err(CopyFault::Write(e)),
        Err(e) => Err(CopyFault::Read(e)),
    }
}

#[cfg(test)]
#[path = "fetcher_seed_tests.rs"]
mod tests;
