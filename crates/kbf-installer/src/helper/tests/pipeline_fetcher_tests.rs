//! A fetcher that runs the real verify pipeline ([`Pipeline`]) on a scratch host, for the
//! tests of what a fetch leaves in the state. The real fetcher (the fetch child feeding the
//! pipeline) is not on this branch; this one hands the pipeline a tarball it makes, with
//! `gh` and the release's `--version --json` played by a [`FakeRunner`]. Every check, the
//! install and anything the install writes are the pipeline's own: unlike
//! [`FakeFetcher`](crate::helper::fetcher::fake::FakeFetcher), whose release is a bare
//! directory, nothing here is faked at the state.

use std::fs;
use std::sync::{Arc, Mutex};

use kbf_host::step::Owner;
use kbf_update::{Facts, Locator};
use sha2::{Digest, Sha256};

use crate::binary::Arch;
use crate::fixtures::{FakeRunner, elf, file, me, ran, tar_gz, trusted_host_on};
use crate::helper::fetcher::fake::Tamper;
use crate::helper::fetcher::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::leases::fake::FakeLeases;
use crate::layout::Layout;
use crate::settings::Settings;
use crate::stage::{
    Fetched, Order, OrderKind, Partial, Pipeline, Policy, Received, Supported, Timeouts, Verifier,
};

/// What every release this fetcher makes declares about itself (the numbers of
/// [`FakeFetcher`](crate::helper::fetcher::fake::FakeFetcher)'s honest release, so that the
/// orders of the helper's tests expect them).
fn declared(rec: &ReleaseLocator) -> String {
    format!(
        "{{\"admin_protocol\":1,\"commit\":\"{}\",\"meta_log\":4,\"min_protocol\":6,\
         \"profile\":\"release\",\"protocol\":7,\"state_schema\":3,\"version\":\"0.1.0\"}}\n",
        &rec.commit.as_str()[..12]
    )
}

/// Pulls nothing and verifies everything: the pipeline over a tarball of an honest x86-64
/// Linux `kbf` for the commit asked for.
#[derive(Clone)]
pub(super) struct PipelineFetcher {
    layout: Layout,
    /// Changes the report once the release is installed: a fetcher that says other things
    /// than the release it left on the host.
    pub tamper: Arc<Mutex<Option<Tamper>>>,
}

impl PipelineFetcher {
    pub fn new(layout: &Layout) -> PipelineFetcher {
        PipelineFetcher {
            layout: layout.clone(),
            tamper: Arc::default(),
        }
    }
}

impl Fetcher for PipelineFetcher {
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
        let rejected = |why: &dyn std::fmt::Display| FetchError::Rejected(why.to_string());
        let owner = Owner {
            uid: me().uid,
            gid: me().gid,
        };
        let bytes = tar_gz(&[file("kbf", &elf(0x3e, "payload"))]);
        let sha256 = hex::encode(Sha256::digest(&bytes));
        let partial =
            Partial::create(&self.layout, &rec.commit, owner).map_err(|e| rejected(&e))?;
        let tarball = partial.dl().join("kbf.tar.gz");
        fs::write(&tarball, &bytes).expect("the download");
        let locator = Locator {
            name: "kbf".to_owned(),
            sha256: sha256.clone(),
            size: bytes.len() as u64,
            attestation: "{\"mediaType\":\"bundle\"}\n".to_owned(),
        };
        let received = Received {
            name: "kbf".to_owned(),
            tarball,
            bytes: bytes.len() as u64,
            sha256,
        };

        let mut settings = Settings::defaults(&self.layout);
        settings.run_as = me();
        settings.arch = Arch::X86_64;
        let trust = trusted_host_on(self.layout.platform());
        let probe = declared(rec);
        let runner = FakeRunner::new(move |spawn| {
            if spawn.program.file_name().is_some_and(|n| n == "gh") {
                Ok(ran(0, "", ""))
            } else {
                Ok(ran(0, &probe, ""))
            }
        });
        let leases = FakeLeases::default();
        let pipeline = Pipeline {
            host: &self.layout,
            settings: &settings,
            policy: Policy {
                max_component_bytes: 1 << 20,
                max_binary_bytes: 1 << 20,
                min_protocol: 0,
                supported: Supported {
                    protocol: 10,
                    min_protocol: 10,
                    state_schema: 10,
                },
                timeouts: Timeouts::DEFAULT,
            },
            trust: &trust,
            runner: &runner,
            leases: &leases,
            owner,
        };
        // An activation names its component and expects the facts; a frontend update is a
        // locator, and the pipeline then checks only the commit and the profile.
        let activation = !rec.components.is_empty();
        let expected = Facts {
            platform: "linux-x86_64".to_owned(),
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
            profile: "release".to_owned(),
        };
        let order = Order {
            kind: if activation {
                OrderKind::Activate
            } else {
                OrderKind::FrontendUpdate
            },
            commit: &rec.commit,
            components: std::slice::from_ref(&locator),
            expected: activation.then_some(&expected),
        };
        let staged = pipeline
            .verify(
                &order,
                Fetched {
                    partial,
                    files: vec![received],
                },
            )
            .map_err(|e| rejected(&e))?;
        let mut report = staged.installed();
        if let Some(change) = *self.tamper.lock().expect("lock") {
            change(&mut report);
        }
        Ok(report)
    }
}
