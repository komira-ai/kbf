//! The rig of the tests of a storage node that applies the cold-store registration: a
//! worker listener over mutual TLS with a registration a test sets, an S3 service that
//! takes one key pair, and the flags of a node of that farm.

#![allow(dead_code)]

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use kbf_map::MemberState;
use kbf_objstore::s3::fake::FakeS3;
use kbf_proto::kbf::enroll::v1::StoreConfig;
use kbf_server::StartFailure;
use kbf_server::storage_half::{StorageHalf, start};
use kbf_server::storage_node::{StorageFlags, command, parse};
use kbf_server::store_config::acks::Convergence;

use kbf_state::Store as _;

use crate::support::{Farm, Source, farm_beating};

pub const BEAT: Duration = Duration::from_millis(90);

/// The key pair the S3 service accepts.
pub const ID: &str = "AKIAOK";
pub const SECRET: &str = "right-secret-4f1c";

pub static NODES: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

pub fn own_keys(name: &str) -> Option<String> {
    match name {
        "AWS_ACCESS_KEY_ID" => Some(ID.to_owned()),
        "AWS_SECRET_ACCESS_KEY" => Some(SECRET.to_owned()),
        _ => None,
    }
}

pub fn no_keys(_: &str) -> Option<String> {
    None
}

pub fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

/// Where [`dir`] puts `name`.
pub fn dir_of(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("cold-apply")
        .join(name)
}

/// An empty directory for `name`. Emptied first: each test's frontend is new, so its
/// state store hands out boot 1 again, which a metadata directory a previous run left has
/// set already (a registered node refuses to start over it).
pub fn dir(name: &str) -> PathBuf {
    let dir = dir_of(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("directory");
    dir
}

/// What a registration says: the store of `bucket` on `s3`, with this key pair.
pub fn registered(s3: &FakeS3, bucket: &str, id: &str, secret: &str) -> StoreConfig {
    StoreConfig {
        connection: "main".to_owned(),
        role: "cold".to_owned(),
        endpoint: s3.endpoint().to_owned(),
        region: "us-east-1".to_owned(),
        bucket: bucket.to_owned(),
        prefix: "kbf/".to_owned(),
        conditional_put: true,
        min_put_rate: 0,
        access_key_id: id.to_owned(),
        secret_access_key: secret.to_owned(),
        secret_version: 1,
    }
}

/// A frontend at `generation` registering `config`, an S3 service that takes one key pair.
pub struct Rig {
    pub farm: Farm,
    pub s3: FakeS3,
    pub source: std::sync::Arc<Source>,
}

pub async fn rig(generation: u64, config: impl FnOnce(&FakeS3) -> StoreConfig) -> Rig {
    let s3 = FakeS3::start().await;
    s3.require_credentials(ID, SECRET);
    let source = Source::new(generation);
    source.serve(config(&s3));
    let farm = farm_beating(source.clone(), false, None, BEAT * 3).await;
    Rig { farm, s3, source }
}

/// The flags of a node of `rig` named `name`, with a cold store `cold` and the key pair
/// `env` gives.
pub fn flags(rig: &Rig, name: &str, cold: &str, env: fn(&str) -> Option<String>) -> StorageFlags {
    flags_at(rig, (dir(name), next_port()), cold, env)
}

/// The flags of a node whose directory is `d` (as it is: not emptied) and whose address is
/// `localhost:port`: the same node again, for a restart over the metadata log it left.
pub fn flags_at(
    rig: &Rig,
    at: (PathBuf, u16),
    cold: &str,
    env: fn(&str) -> Option<String>,
) -> StorageFlags {
    flags_as(rig, at, "storage", cold, env)
}

/// [`flags_at`] for a node whose certificate holds `role`: any but `storage` is a node the
/// frontend's role table denies the cold-store service.
pub fn flags_as(
    rig: &Rig,
    (d, port): (PathBuf, u16),
    role: &str,
    cold: &str,
    env: fn(&str) -> Option<String>,
) -> StorageFlags {
    let write = |file: &str, text: String| {
        let path = d.join(file);
        std::fs::write(&path, text).expect("write");
        path.display().to_string()
    };
    let (cert, key) = rig.farm.pki.client_pem(Some(role), &["localhost"]);
    let argv = [
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        // Its own address per node: the address is the member's identity.
        format!("--advertise-addr=localhost:{port}"),
        format!("--cold-store={cold}"),
        format!("--meta-dir={}", d.join("meta").display()),
        format!("--frontend=https://localhost:{}", rig.farm.addr.port()),
        format!("--storage-tls-cert={}", write("node.pem", cert)),
        format!("--storage-tls-key={}", write("node.key", key)),
        format!(
            "--storage-client-ca={}",
            write("ca.pem", rig.farm.pki.ca_pem())
        ),
    ];
    let mut flags = parse(&command().try_get_matches_from(argv).expect("flags")).expect("taken");
    flags.env = env;
    flags
}

/// The next address port a node of this binary gets.
pub fn next_port() -> u16 {
    9000 + NODES.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// `s3://bkt/kbf/` on the rig's service.
pub fn bucket(rig: &Rig) -> String {
    format!("s3://bkt/kbf/?endpoint={}", rig.s3.endpoint())
}

pub async fn started(flags: &StorageFlags) -> Result<StorageHalf, Box<dyn Error>> {
    start(flags, never).await
}

/// Yields until `probe` finds something: no sleep and no limit, so a wait that never ends is
/// the test run's to cut, and a slow machine is never a failure.
pub async fn spin_until<T>(mut probe: impl FnMut() -> Option<T>) -> T {
    loop {
        if let Some(found) = probe() {
            return found;
        }
        tokio::task::yield_now().await;
    }
}

pub async fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    for _ in 0..1000 {
        if let Some(found) = probe() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The map, with every member `Active`: the nodes here rebuild their index from an S3
/// service that does not list, so none would ever say it is whole, and a member that is not
/// `Active` is not counted by the farm's convergence.
pub fn promoted(rig: &Rig) -> Option<kbf_map::Map> {
    let map = rig.farm.members.refresh().ok().flatten()?;
    for member in map.members() {
        rig.farm
            .members
            .set_state(member.id, MemberState::Active)
            .ok()?;
    }
    rig.farm.members.refresh().ok().flatten()
}

/// The status number the node's last report to the frontend carried, as the frontend kept
/// it (`objstore/ack/<member>`: boot, generation, status, time; 1 ok, 2 pending): the
/// node's own word, before the frontend judges it against its generation.
pub fn reported_status(rig: &Rig, member: u64) -> Option<u64> {
    let row = rig
        .farm
        .ack_store
        .fake
        .get(&format!("objstore/ack/{member}"))
        .ok()??;
    let word = row.value.chunks_exact(8).nth(2)?;
    Some(u64::from_be_bytes(word.try_into().ok()?))
}

/// What the frontend's acks say about `generation`, once the first node has reported.
pub async fn reported(rig: &Rig, generation: u64) -> Convergence {
    until("the node's report", || {
        let map = promoted(rig)?;
        let got = rig.farm.acks.convergence(&map, generation).ok()?;
        let seen = got
            .nodes
            .first()
            .is_some_and(|node| node.last_seen.is_some());
        seen.then_some(got)
    })
    .await
}

pub fn class(error: &(dyn Error + 'static)) -> StartFailure {
    StartFailure::of(error)
}

/// Writes a blob through the node's storage and reads it back; the cache is open on
/// whatever store the node chose, and a write that store refuses fails here.
pub async fn round_trip(half: &StorageHalf, text: &str) {
    let storage = half.storage();
    let digest = storage
        .put(Bytes::copy_from_slice(text.as_bytes()))
        .await
        .expect("the blob is stored");
    assert_eq!(
        storage.read_blob(&digest).await.expect("read back"),
        text.as_bytes()
    );
}
