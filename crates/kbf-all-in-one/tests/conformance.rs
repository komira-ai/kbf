//! The data plane's conformance checks (`kbf-cas-conformance`) against
//! [`LocalDataPlane`]: each check gets a server started as `kbf-server` starts one
//! (`kbf_server::start`, an in-memory store) and the plane over that server's
//! `LocalStore`, so the plane calls the cache and the `Segments` handlers a remote
//! daemon's calls reach. They run twice: over a server with a write tier, where `Commit`
//! trusts the daemon's digests and the flusher checks every record before its PUT, and
//! over one without, where each record is hashed as its ranges arrive. The transfer
//! engine, the remote plane, runs the same checks in `kbf-cas-conformance`'s own tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use clap::Parser as _;
use kbf_all_in_one::LocalDataPlane;
use kbf_cas_conformance::Subject;

/// A server per [`Subject::fresh`], with a write tier of its own or none.
struct Local {
    scratch: PathBuf,
    tier: bool,
}

/// Servers started by this process so far: each tier directory is one server's alone,
/// as the checks run at once.
static STARTED: AtomicU64 = AtomicU64::new(0);

impl Local {
    fn new(tier: bool) -> Self {
        let name = if tier { "local-tier" } else { "local-no-tier" };
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-all-in-one-conformance")
            .join(format!("{name}-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).expect("scratch directory");
        Self { scratch, tier }
    }
}

impl Subject for Local {
    type Cas = LocalDataPlane;

    async fn fresh(&self) -> Arc<LocalDataPlane> {
        let n = STARTED.fetch_add(1, Ordering::SeqCst);
        let mut argv = vec![
            "kbf-server".to_owned(),
            "--listen=127.0.0.1:0".to_owned(),
            "--worker-listen=127.0.0.1:0".to_owned(),
            format!(
                "--cold-store=file://{}",
                self.scratch.join(format!("cold-{n}")).display()
            ),
        ];
        // The hot store holds the metadata log too; without one it is a directory of
        // its own.
        if self.tier {
            let dir = self.scratch.join(format!("hot-{n}"));
            argv.push(format!("--hot-store=file://{}", dir.display()));
        } else {
            let dir = self.scratch.join(format!("meta-{n}"));
            argv.push(format!("--meta-dir={}", dir.display()));
        }
        let args = kbf_server::Args::try_parse_from(argv).expect("flags");
        let never = || Ok(std::future::pending::<()>());
        let server = kbf_server::start(&args, never).await.expect("start");
        let plane = LocalDataPlane::new(server.local(caller()), tokio::runtime::Handle::current());
        // Serves (and flushes its tier) for as long as the test runs.
        tokio::spawn(server.serve());
        Arc::new(plane)
    }

    fn scratch(&self) -> &Path {
        &self.scratch
    }
}

/// The daemon the plane's calls are admitted as: a certificate naming one node. The
/// server here serves plain text and has no deny list, so it is admitted.
fn caller() -> kbf_server::Caller {
    let made = rcgen::generate_simple_self_signed(vec!["node-1".to_owned()]).expect("cert");
    kbf_server::Caller::from_pem(made.cert.pem().as_bytes()).expect("caller")
}

mod tier {
    use super::Local;

    kbf_cas_conformance::tests!(Local::new(true));
}

mod no_tier {
    use super::Local;

    kbf_cas_conformance::tests!(Local::new(false));
}
