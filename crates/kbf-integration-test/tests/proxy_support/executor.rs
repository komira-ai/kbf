//! What a `kbf frontend` child needs to hold an `Execute` open: an executor over a job service
//! that never ends a job. The job service ([`FakeJobServer`], with one Linux host) and the pack
//! store ([`FakeS3`]) run in this process, so the child reaches them on loopback; the kbf base
//! tree's bundle, the task secret and the tokens are files under the scratch directory. An
//! `Execute` through such a frontend is submitted as a job at once and its stream stays open,
//! as a build's does while its action runs, until the client ends it.
//!
//! The pack store needs a pack index, which lives in the frontend's Postgres state store: a
//! frontend started with these flags is started over a Postgres database.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use js_proto::v1::{ExecMode, Shape};
use kbf_job_client::fake::{FakeJobServer, Machine, TOKEN};
use kbf_objstore::s3::fake::FakeS3;
use kbf_state::postgres::{PostgresConfig, TestDatabase};
use kbf_tree_bundle::{Dir, Facts, Node, Writer, file_name};
use sha2::Digest as _;

/// The base environment the executor serves, pinned by digest as its flag wants.
const BASE_IMAGE: &str = "docker://registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The kbf image the jobs launch from.
const KBF_IMAGE: &str =
    "registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// A job service with one host and a pack store, for the executors of the frontend children.
pub struct Executor {
    fake: FakeJobServer,
    packs: FakeS3,
    dir: PathBuf,
}

/// A database of its own on the test server with the schema the installer makes, the pack
/// index's tables included: what a frontend whose executor writes packs is started over. Call it
/// off any runtime (making a database blocks).
pub fn pack_database() -> TestDatabase {
    let database = TestDatabase::empty();
    let config = PostgresConfig::parse(&database.url()).expect("the URL parses");
    let set = kbf_pack_index::schema::embedded();
    kbf_state::postgres::schema::apply(&config, &set, kbf_pack_index::schema::latest())
        .expect("the installer's step");
    database
}

/// A file of mode 0600 holding `text`.
fn secret(path: &Path, text: &str) -> PathBuf {
    std::fs::write(path, format!("{text}\n")).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    path.to_path_buf()
}

/// Writes into `dir` the bundle of this build's release for `linux-x86_64` and the `SHA256SUMS`
/// that lists it: what `--executor-base-tree` names, and the frontend publishes at its start.
fn base_tree(dir: &Path) {
    std::fs::create_dir_all(dir).expect("a directory");
    let mut passwd = Dir::new();
    passwd.insert(
        "passwd",
        Node::file(b"root:x:0:0::/root:/bin/nologin\n", false),
    );
    let mut tools = Dir::new();
    tools.insert("kbf-task", Node::file(b"kbf-task", true));
    let mut root = Dir::new();
    root.insert("etc", Node::Dir(passwd));
    root.insert(".kbf-tools", Node::Dir(tools));
    let built = root.build().expect("a tree");
    let commit = kbf_server::BUILD_COMMIT_FULL;
    let platform = "linux-x86_64";
    let facts = Facts {
        commit: commit.to_owned(),
        platform: platform.to_owned(),
        root: built.root.clone(),
        entries: built.entries,
        depth: built.depth as u64,
        normalised: Vec::new(),
    };
    let mut writer = Writer::new(Vec::new(), &facts).expect("a bundle");
    writer
        .object(b"root:x:0:0::/root:/bin/nologin\n")
        .expect("written");
    writer.object(b"kbf-task").expect("written");
    for (_, manifest) in &built.manifests {
        writer.object(manifest).expect("written");
    }
    let bytes = writer.finish().expect("finished");
    let name = file_name(commit, platform);
    std::fs::write(dir.join(&name), &bytes).expect("a bundle");
    let sum: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(dir.join("SHA256SUMS"), format!("{sum}  {name}\n")).expect("the sums");
}

impl Executor {
    /// A job service with one Linux host of 16 cores and a pack store, their files under `dir`.
    /// Call it on the runtime that stays up while the children run: the services are its tasks.
    pub async fn start(dir: &Path) -> Self {
        let fake = FakeJobServer::start().await.expect("a loopback port");
        fake.add_host(
            "host-1",
            Machine {
                shape: Shape {
                    cpu_millis: 16_000,
                    memory_bytes: 64 << 30,
                    gpus: 0,
                    gpu_class: String::new(),
                    disk_bytes: Some(1 << 40),
                },
                os: "linux".to_owned(),
                arch: "x86_64".to_owned(),
                exec: ExecMode::Container,
            },
        );
        let packs = FakeS3::start().await;
        let dir = dir.join("executor");
        base_tree(&dir.join("base-tree"));
        Self { fake, packs, dir }
    }

    /// The flags of a frontend whose executor is named `name` (each frontend its own name, so
    /// their job ids never meet) over these services.
    pub fn args(&self, name: &str) -> Vec<String> {
        let dir = &self.dir;
        let keys = dir.join("pack-keys.json");
        std::fs::write(&keys, r#"{"access_key_id": "k", "secret_access_key": "s"}"#)
            .expect("written");
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let job_token = secret(&dir.join("job-token"), TOKEN);
        let task_secret = secret(&dir.join("task-secret"), &"s".repeat(40));
        vec![
            "--pack-index".to_owned(),
            format!("--pack-store-endpoint={}", self.packs.endpoint()),
            "--pack-store-bucket=kbf-fake".to_owned(),
            format!("--pack-store-credentials-file={}", keys.display()),
            format!("--task-secret-file={}", task_secret.display()),
            "--task-out-root=/mnt/ts/put".to_owned(),
            "--executor=js-agent".to_owned(),
            format!("--executor-job-url={}", self.fake.origin()),
            format!("--executor-job-token-file={}", job_token.display()),
            format!("--executor-frontend-id={name}"),
            format!("--executor-base-image={BASE_IMAGE}"),
            format!("--executor-kbf-image={KBF_IMAGE}"),
            format!("--executor-base-tree={}", dir.join("base-tree").display()),
            "--executor-base-tree-url=".to_owned(),
        ]
    }

    /// How many jobs the frontends submitted.
    pub fn jobs(&self) -> usize {
        self.fake.jobs().len()
    }
}
