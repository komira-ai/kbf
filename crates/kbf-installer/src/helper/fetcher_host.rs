//! The release fetcher of the binary: a [`RealFetcher`] made anew for each fetch, from what the
//! host says at that moment.
//!
//! Nothing is read when the fetcher is built, and nothing is kept between two fetches. That is
//! deliberate, for two reasons:
//!
//! - `setup` builds its fetcher before it has written `installer.toml` or made the `kbf-fetch`
//!   user, and both must be there when the first release is fetched;
//! - `run` is a long-lived service: a second run of `setup` (a new source, a new floor) must
//!   take effect at the next order without a restart, and so must a changed `installer.conf`.
//!
//! What a fetch uses, and where each part comes from:
//!
//! - `installer.toml` ([`InstallerToml::read`]): the source, the component cap and the fetch
//!   timeout, the operator's `min_protocol` floor ([`Policy::of`], [`Limits::of`]), and the
//!   refs it names, which can only narrow `installer.conf`'s list
//!   ([`Settings::narrowed_to`]). The file is held to the trust files' rule: root's alone.
//! - `installer.conf` ([`Settings::load`]), for the refs a release may be attested for and the
//!   unprivileged user `gh` and the probe run as.
//! - the host ([`Parts`]): the executor that answers for ownership, the runner, the lease
//!   source, the owner of the scratch directory and the fetch child's program.
//!
//! Any of them that cannot be had makes the fetch `Unavailable` with the reason, so the cycle
//! refuses the order (`fetch`) and goes on, and `setup` says why it stopped.

use kbf_host::{HostExec, HostPath, Owner};
use kbf_update::Facts;

use crate::fetch::{ChildProgram, Limits, ProgramFault, Source};
use crate::helper::fetcher::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::fetcher_real::{Parts, RealFetcher};
use crate::helper::leases::LeaseSource;
use crate::installer_toml::{InstallerToml, ReadError, Source as Configured};
use crate::layout::{Layout, Platform};
use crate::refusal::Refusal;
use crate::run::SystemRunner;
use crate::settings::{NoCommonRef, Settings};
use crate::stage::Policy;

/// Pulls and installs a release the way the host is configured to, as of the moment it is
/// asked. See the module documentation.
pub struct HostFetcher {
    host: Layout,
    /// `installer.toml`, as a path under the host's root.
    installer_toml: HostPath,
    /// What the host supplies, made for each fetch.
    parts: Box<dyn Fn() -> Result<Parts, String>>,
}

impl HostFetcher {
    /// A fetcher on `host` that reads `installer_toml` and takes its [`Parts`] from `parts`
    /// (made for each fetch; its error is why this host cannot fetch). [`HostFetcher::on_host`]
    /// is the host's own; a test plays the host with this.
    pub fn new(
        host: Layout,
        installer_toml: HostPath,
        parts: impl Fn() -> Result<Parts, String> + 'static,
    ) -> HostFetcher {
        HostFetcher {
            host,
            installer_toml,
            parts: Box::new(parts),
        }
    }

    /// The fetcher of the host `host` itself: the real executor and runner, the owner of this
    /// process, the fetch child that is this program run as the fetch user, and the lease
    /// source `leases` makes.
    pub fn on_host(
        host: Layout,
        installer_toml: HostPath,
        leases: impl Fn() -> Box<dyn LeaseSource> + 'static,
    ) -> HostFetcher {
        HostFetcher::on_host_with(host, installer_toml, leases, ChildProgram::of_this_program)
    }

    /// [`HostFetcher::on_host`] with the fetch child's program made by `child` (a test cannot
    /// make the fetch user).
    fn on_host_with(
        host: Layout,
        installer_toml: HostPath,
        leases: impl Fn() -> Box<dyn LeaseSource> + 'static,
        child: fn(Platform) -> Result<ChildProgram, ProgramFault>,
    ) -> HostFetcher {
        let own = host.clone();
        HostFetcher::new(host, installer_toml, move || {
            let child = child(own.platform()).map_err(|e| e.to_string())?;
            Ok(host_parts(&own, leases(), child))
        })
    }

    /// The real fetcher for this fetch.
    fn build(&self) -> Result<RealFetcher, FetchError> {
        self.assemble()
            .map_err(|e| FetchError::Unavailable(e.to_string()))
    }

    fn assemble(&self) -> Result<RealFetcher, NotBuilt> {
        let parts = (self.parts)().map_err(NotBuilt::Host)?;
        let toml = InstallerToml::read(&*parts.trust, &self.installer_toml)?;
        let settings = Settings::load(&*parts.trust, &self.host)
            .map_err(NotBuilt::Settings)?
            .narrowed_to(&toml.source_refs)?;
        let source = source_of(&toml)?;
        Ok(RealFetcher::new(
            self.host.clone(),
            settings,
            Policy::of(&toml),
            source,
            Limits::of(&toml),
            parts,
        ))
    }
}

impl Fetcher for HostFetcher {
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
        self.build()?.fetch(rec)
    }

    fn fetch_activation(
        &self,
        rec: &ReleaseLocator,
        expected: &Facts,
    ) -> Result<InstalledRelease, FetchError> {
        self.build()?.fetch_activation(rec, expected)
    }
}

/// Why no fetcher could be made for this fetch.
#[derive(Debug, thiserror::Error)]
enum NotBuilt {
    /// The host could not supply its parts (no fetch user, for one).
    #[error("{0}")]
    Host(String),
    #[error("{0}")]
    Config(#[from] ReadError),
    #[error("{0}")]
    Settings(Refusal),
    #[error("{0}")]
    Refs(#[from] NoCommonRef),
    #[error("{0}")]
    Source(&'static str),
}

/// The parts of the host `host` itself, with the fetch child `child` and the lease source
/// `leases`: the real executor under the host's root, the real runner, and this process's
/// effective ids as the owner of what a fetch makes (root on a host).
pub fn host_parts(host: &Layout, leases: Box<dyn LeaseSource>, child: ChildProgram) -> Parts {
    Parts {
        trust: Box::new(HostExec::new(host.root().clone())),
        runner: Box::new(SystemRunner),
        leases,
        owner: Owner {
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
        },
        child,
    }
}

/// Where `installer.toml` says the bytes come from.
///
/// The `url` source is a base the file validated, with the system's roots. `installer.toml`
/// can now hold the keys of the other sources (`release_ca`, the frontend snapshot, `bundled`),
/// and this build reads them without using them, so a source that needs one is refused, with
/// the reason, rather than built without it: a `url` host with a `release_ca` would otherwise
/// trust the system's roots where the operator named a CA, the `frontend` has no fetch of its
/// own yet, and a `bundled` host has no fetcher for the release the package carries.
fn source_of(toml: &InstallerToml) -> Result<Source, NotBuilt> {
    match (&toml.source, &toml.release_ca) {
        (Configured::Url(base), None) => Ok(Source::Url {
            base: base.clone(),
            ca_file: None,
        }),
        (Configured::Url(_), Some(_)) => Err(NotBuilt::Source(
            "release_ca is set and this build does not use it yet: a fetch would trust the system's roots instead of the CA named; remove release_ca",
        )),
        (Configured::Frontend, _) => Err(NotBuilt::Source(
            "release_source = \"frontend\": this build cannot fetch from the frontend yet, even with frontend_url, frontend_ca and frontend_ca_sha256 set; use release_source = \"url\"",
        )),
        (Configured::Bundled, _) => Err(NotBuilt::Source(
            "release_source = \"bundled\": this build has no bundled fetcher yet, and nothing else is fetched under bundled",
        )),
    }
}

#[cfg(test)]
#[path = "fetcher_host_tests.rs"]
mod tests;
