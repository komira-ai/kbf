//! A package's seed on a scratch host, for the tests of the seed fetcher, the host fetcher
//! and the cycle: the files the package builder leaves (a manifest, a tarball of an
//! honest x86-64 Linux `kbf`, its attestation bundle), the trust view that makes them root's,
//! and a fake `gh` that accepts the attestation of exactly the bundle the signer made, the way
//! the real one accepts only the bundle that attests the tarball's digest. A test changes a
//! field to plant a defect (a flipped byte, a swapped bundle, a manifest for another commit)
//! and writes the files again.

use std::fs;
use std::path::PathBuf;

use kbf_host::exec::Entry;
use kbf_host::path::HostPath;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;
use kbf_update::{Facts, Source};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::CommitId;
use crate::binary::Arch;
use crate::fetch::ChildProgram;
use crate::fixtures::{FakeRunner, elf, file, me, ran, tar_gz, trusted_host_on};
use crate::helper::fetcher::ReleaseLocator;
use crate::helper::fetcher_host::HostFetcher;
use crate::helper::fetcher_real::Parts;
use crate::helper::fetcher_seed::SeedFetcher;
use crate::helper::leases::fake::FakeLeases;
use crate::layout::{Layout, path};
use crate::seed::MANIFEST;
use crate::settings::Settings;
use crate::stage::{Policy, Supported, Timeouts};
use crate::testkit::{A, B, c, host_root};

/// The `seq` the seed's manifest and its binary declare.
pub const SEQ: u64 = 500;

/// The `seq` the release that runs before the seed (A, in the cycle tests) declares: older than
/// the seed's.
pub const EARLIER_SEQ: u64 = SEQ - 100;

/// The attestation bundle the signer made for the honest tarball.
const BUNDLE: &[u8] = b"{\"mediaType\":\"application/vnd.dev.sigstore.bundle+json\"}\n";

/// The sha256 of `bytes`, as the manifest spells it.
pub fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// What the package's `installer.conf` holds in the tests. A test that is not root names the
/// ids it runs `gh` and the probe as (the settings refuse root); a root test run has no ids to
/// name, so a comment stands in their place. (An index and not an `if`: CI runs as one kind of
/// user, so a branch here would always leave a side untaken.)
fn conf_text() -> String {
    let ids = format!("RUN_AS_UID={}\nRUN_AS_GID={}\n", me().uid, me().gid);
    let comment = "# a root test run names no unprivileged ids\n";
    [ids.as_str(), comment][usize::from(me().uid == 0)].to_owned()
}

/// What `kbf --version --json` prints for a release of the layout's other commit, A, built
/// before the seed: the same shape as the seed's report, with `seq` as given.
fn earlier_probe(seq: u64) -> String {
    format!(
        "{{\"admin_protocol\":1,\"commit\":\"{}\",\"meta_log\":4,\"min_protocol\":3,\"profile\":\"release\",\"protocol\":4,\"relay\":1,\"seq\":{seq},\"state_schema\":7,\"version\":\"0.1.0\"}}\n",
        &A[..12]
    )
}

/// The seed of one commit on one host.
#[derive(Clone)]
pub struct Seeded {
    pub layout: Layout,
    pub commit: CommitId,
    /// The first 12 digits of the commit: the seed directory's name.
    pub build: String,
    pub tarball: Vec<u8>,
    pub bundle: Vec<u8>,
    /// What the signer attested: the fake `gh` accepts the bundle it is given only if it is
    /// these bytes.
    pub attested: Vec<u8>,
    /// The manifest as the package builder wrote it. A test that changes `tarball` or
    /// `bundle` and wants the manifest to agree calls [`Seeded::reseal`].
    pub manifest: Value,
    /// What the seed's binary prints for `--version --json`.
    pub probe: String,
    /// What any other release's binary prints (the cycle tests' A, which runs before the seed
    /// is installed): an older `seq`, and a commit of its own.
    pub earlier: String,
    /// The host's lease source, shared with the fetcher this makes.
    pub leases: FakeLeases,
}

impl Seeded {
    /// The seed of commit `B` on a host of its own.
    pub fn scratch(name: &str) -> Seeded {
        let (layout, _) = host_root(name);
        Seeded::on(layout, B)
    }

    /// The seed of `commit` on the host `layout` is the layout of, with its honest tarball
    /// written.
    pub fn on(layout: Layout, commit: &str) -> Seeded {
        let tarball = tar_gz(&[file("kbf", &elf(0x3e, "payload"))]);
        let build = commit[..12].to_owned();
        let manifest = json!({
            "commit": commit,
            "seq": SEQ,
            "platform": "linux-x86_64",
            "tarball_sha256": sha(&tarball),
            "tarball_size": tarball.len(),
            "bundle_sha256": sha(BUNDLE),
            "protocol": 4,
            "min_protocol": 3,
            "state_schema": 7,
            "profile": "release",
            "gh_sha256": sha(b"gh"),
            "trusted_root_sha256": sha(b"root"),
            "installer_conf_sha256": sha(conf_text().as_bytes()),
        });
        let probe = format!(
            "{{\"admin_protocol\":1,\"commit\":\"{build}\",\"meta_log\":4,\"min_protocol\":3,\"profile\":\"release\",\"protocol\":4,\"relay\":1,\"seq\":{SEQ},\"state_schema\":7,\"version\":\"0.1.0\"}}\n"
        );
        let seeded = Seeded {
            layout,
            commit: c(commit),
            build,
            tarball,
            bundle: BUNDLE.to_vec(),
            attested: BUNDLE.to_vec(),
            manifest,
            probe,
            earlier: earlier_probe(EARLIER_SEQ),
            leases: FakeLeases::default(),
        };
        seeded.write_tarball();
        seeded
    }

    /// Where the tarball, the bundle and the manifest are.
    pub fn tarball_path(&self) -> HostPath {
        let name = format!("kbf-{}-linux-x86_64.tar.gz", self.commit);
        self.layout.seed_file(&self.build, &name)
    }

    pub fn bundle_path(&self) -> HostPath {
        let name = format!("kbf-{}-linux-x86_64.tar.gz.sigstore.json", self.commit);
        self.layout.seed_file(&self.build, &name)
    }

    pub fn manifest_path(&self) -> HostPath {
        self.layout.seed_file(&self.build, MANIFEST)
    }

    /// The tarball on the disk, in the seed directory under the host's root.
    pub fn write_tarball(&self) {
        let path = self.layout.join(&self.tarball_path());
        fs::create_dir_all(path.parent().expect("a directory")).expect("the seed directory");
        fs::write(path, &self.tarball).expect("the tarball");
    }

    /// The manifest's digests and size made to agree with the tarball and the bundle as they
    /// are now.
    pub fn reseal(&mut self) {
        self.manifest["tarball_sha256"] = json!(sha(&self.tarball));
        self.manifest["tarball_size"] = json!(self.tarball.len());
        self.manifest["bundle_sha256"] = json!(sha(&self.bundle));
    }

    /// A host whose release that runs before the seed was built as the `seq` given.
    pub fn set_earlier_seq(&mut self, seq: u64) {
        self.earlier = earlier_probe(seq);
    }

    /// The seed directory and the files the trust executor sees, all root's, in `exec`: the
    /// manifest and the bundle with their bytes, the tarball by its name (its bytes are on the
    /// disk), and the three trust files holding the bytes the manifest's digests are of.
    pub fn add_to(&self, exec: &mut FakeExec) {
        let root = Owner { uid: 0, gid: 0 };
        let trust = [
            (self.layout.gh(), 0o755, b"gh".to_vec()),
            (self.layout.trusted_root(), 0o644, b"root".to_vec()),
            (self.layout.settings(), 0o644, conf_text().into_bytes()),
        ];
        for (path, mode, bytes) in trust {
            let entry = Entry::File {
                perms: Perms::new(mode),
                owner: root,
                bytes,
            };
            exec.entries.insert(path, entry);
        }
        let dirs = [
            "/usr/share/kbf-installer/seed".to_owned(),
            self.layout.seed_dir(&self.build).to_string(),
        ];
        for dir in dirs {
            let entry = Entry::Dir {
                perms: Perms::new(0o755),
                owner: root,
            };
            let at = HostPath::new(&dir).expect("a path");
            exec.entries.insert(at, entry);
        }
        let manifest = serde_json::to_vec(&self.manifest).expect("json");
        let files = [
            (self.manifest_path(), manifest),
            (self.tarball_path(), Vec::new()),
            (self.bundle_path(), self.bundle.clone()),
        ];
        for (path, bytes) in files {
            let entry = Entry::File {
                perms: Perms::new(0o644),
                owner: root,
                bytes,
            };
            exec.entries.insert(path, entry);
        }
    }

    /// The trust view of a host that has the package: the trust list, and the seed.
    pub fn exec(&self) -> FakeExec {
        let mut exec = trusted_host_on(self.layout.platform());
        self.add_to(&mut exec);
        exec
    }

    /// `gh` accepts the bundle the signer made and no other; the seed's binary (unpacked for a
    /// check, or installed) prints `probe`, any other release's prints `earlier`.
    pub fn runner(&self) -> FakeRunner {
        let (attested, probe) = (self.attested.clone(), self.probe.clone());
        let (earlier, commit) = (self.earlier.clone(), self.commit.to_string());
        FakeRunner::new(move |spawn| {
            if spawn.program.file_name().is_none_or(|n| n != "gh") {
                let ours = spawn.program.to_string_lossy().contains(commit.as_str());
                let said = if ours {
                    probe.as_str()
                } else {
                    earlier.as_str()
                };
                return Ok(ran(0, said, ""));
            }
            let at = spawn
                .args
                .iter()
                .position(|a| a == "--bundle")
                .expect("--bundle");
            let given = fs::read(&spawn.args[at + 1]).expect("the bundle gh is given");
            Ok(if given == attested {
                ran(0, "", "")
            } else {
                ran(1, "", "no attestation matches")
            })
        })
    }

    /// What the host supplies, over the trust view `exec`. The fetch child is a program that
    /// does not exist: a fetch that starts it fails for that.
    pub fn parts(&self, exec: FakeExec) -> Parts {
        Parts {
            trust: Box::new(exec),
            runner: Box::new(self.runner()),
            leases: Box::new(self.leases.clone()),
            owner: Owner {
                uid: me().uid,
                gid: me().gid,
            },
            child: ChildProgram {
                program: PathBuf::from("/nonexistent/kbf-installer"),
                run_as: None,
            },
        }
    }

    /// The seed fetcher of this host over the trust view `exec`, as a Linux x86-64 host with
    /// room for a megabyte, whatever machine the test runs on.
    pub fn fetcher_on(&self, exec: FakeExec) -> SeedFetcher {
        let mut settings = Settings::defaults(&self.layout);
        settings.run_as = me();
        settings.arch = Arch::X86_64;
        let policy = Policy {
            max_component_bytes: 1 << 20,
            max_binary_bytes: 1 << 20,
            min_protocol: 0,
            supported: Supported {
                protocol: 10,
                min_protocol: 10,
                state_schema: 10,
            },
            timeouts: Timeouts::DEFAULT,
        };
        let parts = self.parts(exec);
        SeedFetcher::new(self.layout.clone(), settings, policy, parts, &self.build)
    }

    pub fn fetcher(&self) -> SeedFetcher {
        self.fetcher_on(self.exec())
    }

    /// The host's own kind of fetcher on this host: the package carries the seed, and
    /// `installer.toml` (root's, in `/etc/kbf`) says `toml`. It is asked for its parts and for
    /// the file at each fetch, as on a host.
    pub fn host_fetcher(&self, toml: &str) -> HostFetcher {
        let (seeded, toml) = (self.clone(), toml.to_owned());
        let installer_toml = path("/etc/kbf/installer.toml");
        let at = installer_toml.clone();
        HostFetcher::new(self.layout.clone(), installer_toml, move || {
            let mut exec = seeded.exec();
            let root = Owner { uid: 0, gid: 0 };
            for dir in ["/etc", "/etc/kbf"] {
                let entry = Entry::Dir {
                    perms: Perms::new(0o755),
                    owner: root,
                };
                exec.entries.insert(path(dir), entry);
            }
            let file = Entry::File {
                perms: Perms::new(0o644),
                owner: root,
                bytes: toml.clone().into_bytes(),
            };
            exec.entries.insert(at.clone(), file);
            Ok(seeded.parts(exec))
        })
        .with_build(&self.build)
    }

    /// The facts the manifest declares.
    pub fn facts(&self) -> Facts {
        Facts {
            platform: "linux-x86_64".to_owned(),
            protocol: 4,
            min_protocol: 3,
            state_schema: 7,
            profile: "release".to_owned(),
        }
    }

    /// The locator of an order that asks for this seed.
    pub fn locator(&self) -> ReleaseLocator {
        ReleaseLocator {
            commit: self.commit.clone(),
            components: Vec::new(),
            source: Source::Staged,
        }
    }

    /// The floor the host holds: the highest `seq` that is not allowed.
    pub fn set_floor(&self, floor: u64) {
        let path = self.layout.join(&self.layout.seq_floor());
        fs::write(path, format!("{floor}\n")).expect("the floor");
    }

    /// What the floor file holds, if the host has one.
    pub fn floor_text(&self) -> Option<String> {
        fs::read_to_string(self.layout.join(&self.layout.seq_floor())).ok()
    }

    /// Whether a release of the seed's commit is installed and verified on the host.
    pub fn installed(&self) -> bool {
        let dir = self.layout.join(&self.layout.release_dir(&self.commit));
        dir.join("VERIFIED").is_file() && dir.join("kbf").is_file()
    }
}
