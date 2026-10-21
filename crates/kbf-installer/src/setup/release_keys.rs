//! The flags of `setup` that write the keys a release source has of its own in `installer.toml`
//! (docs/design/kbf-installer.md section 3): `--release-ca` for the `url` source, and the
//! snapshot of the frontend's worker listener (`--frontend-worker-url`, `--frontend-ca`,
//! `--frontend-ca-sha256`) for the `frontend` source.
//!
//! - Setup checks the shape of each value and writes it; it reads no file. The CA files are the
//!   operator's to put where root owns them, and are looked at when they are used.
//! - The three snapshot flags come together. Each flag belongs to one source and is refused
//!   beside another (`bundled` fetches nothing and takes none of them).
//! - The flags decide the keys: a run without them leaves none in the file, so a file never
//!   holds a key of a source it no longer has.
//! - No message repeats a value.

use std::path::PathBuf;

use crate::installer_toml::{FrontendPull, Source, absolute_path};

/// The names the snapshot flags are called in a message, in the order of `FrontendPull`.
const FLAGS: [&str; 3] = [
    "--frontend-worker-url",
    "--frontend-ca",
    "--frontend-ca-sha256",
];

/// The flags as the command line gave them.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct ReleaseFlags {
    /// A PEM file of the CAs the file server's certificate is checked against instead of the
    /// system's (an absolute path, a copy owned by root): written to installer.toml as
    /// `release_ca`. For --release-source url. Not used by a fetch yet.
    #[arg(long)]
    pub release_ca: Option<String>,
    /// The frontend's worker listener, where releases are served, as https://host[:port]:
    /// written to installer.toml as `frontend_url`. For --release-source frontend, with
    /// --frontend-ca and --frontend-ca-sha256. Not used by a fetch yet.
    #[arg(long)]
    pub frontend_worker_url: Option<String>,
    /// A PEM file of the CA the worker listener's certificate chains to (an absolute path, a
    /// copy owned by root): written to installer.toml as `frontend_ca`. For --release-source
    /// frontend, with --frontend-worker-url and --frontend-ca-sha256. Not used by a fetch yet.
    #[arg(long)]
    pub frontend_ca: Option<String>,
    /// The sha256 of --frontend-ca, 64 lowercase hex characters: written to installer.toml as
    /// `frontend_ca_sha256`. For --release-source frontend, with --frontend-worker-url and
    /// --frontend-ca. Not used by a fetch yet.
    #[arg(long)]
    pub frontend_ca_sha256: Option<String>,
}

impl ReleaseFlags {
    /// Check the flags against the `source` the other flags chose (`None` when those were
    /// refused), pushing one line per problem to `problems`: the CA of a `url` source and the
    /// snapshot of a `frontend` one, each `None` when its flags were not given or were refused.
    pub fn validate(
        &self,
        source: Option<&Source>,
        problems: &mut Vec<String>,
    ) -> (Option<PathBuf>, Option<FrontendPull>) {
        (
            self.server_ca(source, problems),
            self.snapshot(source, problems),
        )
    }

    fn server_ca(&self, source: Option<&Source>, problems: &mut Vec<String>) -> Option<PathBuf> {
        let given = self.release_ca.as_deref()?;
        if matches!(source, Some(Source::Frontend | Source::Bundled)) {
            problems.push("--release-ca is for --release-source url".to_owned());
            return None;
        }
        absolute_path("--release-ca", given)
            .map_err(|e| problems.push(e))
            .ok()
    }

    fn snapshot(
        &self,
        source: Option<&Source>,
        problems: &mut Vec<String>,
    ) -> Option<FrontendPull> {
        let flags = (
            &self.frontend_worker_url,
            &self.frontend_ca,
            &self.frontend_ca_sha256,
        );
        match flags {
            (None, None, None) => None,
            _ if matches!(source, Some(Source::Url(_) | Source::Bundled)) => {
                problems.push(
                    "--frontend-worker-url, --frontend-ca and --frontend-ca-sha256 are for --release-source frontend".to_owned(),
                );
                None
            }
            (Some(url), Some(ca), Some(sha256)) => {
                FrontendPull::checked(FLAGS, url, ca, sha256, problems)
            }
            _ => {
                problems.push(
                    "--frontend-worker-url, --frontend-ca and --frontend-ca-sha256 come together"
                        .to_owned(),
                );
                None
            }
        }
    }
}
