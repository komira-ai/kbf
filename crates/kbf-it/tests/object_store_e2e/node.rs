//! A real storage node (`kbf_server::storage_half`) of the farm: registered with the
//! frontend over mutual TLS, its `--cold-store` flag naming the farm's S3 bucket, started
//! and stopped as many times as a scenario needs, on the same directory and under the same
//! advertised address (so the frontend sees one member come back, not a new one).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use bytes::Bytes;
use kbf_server::storage_half::start;
use kbf_server::storage_node::{StorageFlags, command, parse};
use kbf_storage::Storage;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::farm::{Farm, Frontend, Keys, OLD};

/// The key pair a node holds of its own, as `AWS_*` variables would give it.
pub type Env = fn(&str) -> Option<String>;

/// A node with the old key pair in its environment: what an operator who had given nodes
/// keys before the registration existed has.
pub fn own_keys(name: &str) -> Option<String> {
    keys_of(OLD, name)
}

/// A node with nothing of its own: it runs on the registration or not at all.
pub fn no_keys(_: &str) -> Option<String> {
    None
}

fn keys_of(keys: Keys, name: &str) -> Option<String> {
    match name {
        "AWS_ACCESS_KEY_ID" => Some(keys.id.to_owned()),
        "AWS_SECRET_ACCESS_KEY" => Some(keys.secret.to_owned()),
        _ => None,
    }
}

static NEXT_PORT: AtomicU16 = AtomicU16::new(29_000);

/// The identity of a node across its restarts: the name, the advertised address and the
/// directory.
pub struct Seat {
    pub name: &'static str,
    /// What the frontend calls the node: the address it registered.
    pub addr: String,
    dir: PathBuf,
}

impl Seat {
    pub fn new(farm: &Farm, name: &'static str) -> Self {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        let dir = farm.root.join("nodes").join(name);
        std::fs::create_dir_all(&dir).expect("a node directory");
        Self {
            name,
            addr: format!("localhost:{port}"),
            dir,
        }
    }

    fn flags(&self, farm: &Farm, frontend: &Frontend, env: Env) -> StorageFlags {
        let write = |file: &str, text: String| {
            let path = self.dir.join(file);
            std::fs::write(&path, text).expect("write");
            path.display().to_string()
        };
        let (cert, key) = farm.pki.storage_pem("localhost");
        let argv = [
            "storage".to_owned(),
            "--storage-listen=127.0.0.1:0".to_owned(),
            format!("--advertise-addr={}", self.addr),
            format!("--cold-store={}", farm.cold_flag()),
            format!("--meta-dir={}", self.dir.join("meta").display()),
            format!("--frontend=https://localhost:{}", frontend.worker.port()),
            format!("--storage-tls-cert={}", write("node.pem", cert)),
            format!("--storage-tls-key={}", write("node.key", key)),
            format!("--storage-client-ca={}", write("ca.pem", farm.pki.ca_pem())),
        ];
        let mut flags =
            parse(&command().try_get_matches_from(argv).expect("flags")).expect("taken");
        flags.env = env;
        flags
    }
}

/// A running node.
pub struct Node {
    storage: Arc<dyn Storage>,
    stop: Option<oneshot::Sender<()>>,
    serving: Option<JoinHandle<()>>,
}

impl Node {
    /// Starts the node of `seat` against `frontend`, with `env` for its own keys. It has
    /// fetched, probed and opened (or kept its flag's store) when this returns, and is a
    /// member the frontend counts.
    pub async fn start(farm: &Farm, frontend: &Frontend, seat: &Seat, env: Env) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let half = start(&seat.flags(farm, frontend, env), move || {
            Ok(async move {
                let _ = stopped.await;
            })
        })
        .await
        .unwrap_or_else(|e| panic!("node {} starts: {e}", seat.name));
        let storage = half.storage();
        let serving = tokio::spawn(async move {
            half.serve().await.expect("the node serves");
        });
        frontend.promote();
        Self {
            storage,
            stop: Some(stop),
            serving: Some(serving),
        }
    }

    /// Stops the node as an operator's restart does, and waits until it has let go of its
    /// directory.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(serving) = self.serving.take() {
            serving.await.expect("the node stops");
        }
    }

    /// Writes a blob through the node and reads it back: the cache is open on whatever
    /// store the node chose, and a store that refuses its keys fails here.
    pub async fn round_trip(&self, text: &str) {
        let digest = self
            .storage
            .put(Bytes::copy_from_slice(text.as_bytes()))
            .await
            .expect("the blob is stored");
        assert_eq!(
            self.storage.read_blob(&digest).await.expect("read back"),
            text.as_bytes()
        );
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(serving) = &self.serving {
            serving.abort();
        }
    }
}
