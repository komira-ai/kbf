//! Fixtures the registry tests share: stores on a manual clock, a signer that issues
//! predictable certificates, request builders and a store wrapper that interferes.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_enroll_registry::{
    EnrollRequest, Enrolled, IssuedRole, TokenSpec, create_token, enroll_txn,
};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{
    Changes, Clock, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{NodeId, RefusalReason, Role, RoleSet, TokenId, TokenSecret};

pub const DAY: u64 = 24 * 3600;

pub struct Env {
    pub store: Arc<dyn Store>,
    pub clock: Arc<ManualClock>,
}

impl Env {
    pub fn advance(&self, secs: u64) {
        self.clock.advance(std::time::Duration::from_secs(secs));
    }

    pub fn now(&self) -> u64 {
        self.store.now().as_duration().as_secs()
    }
}

/// The in-memory store. The clock starts at ten days so "seven days ago" exists.
pub fn fake() -> Env {
    let clock = Arc::new(ManualClock::default());
    clock.advance(std::time::Duration::from_secs(10 * DAY));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    Env { store, clock }
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let id = std::process::id();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("registry-{name}-{id}-{n}"));
    std::fs::create_dir(&dir).expect("create a scratch directory of its own");
    dir
}

/// The SQLite store in a directory of its own.
pub fn sqlite(name: &str) -> Env {
    let clock = Arc::new(ManualClock::default());
    clock.advance(std::time::Duration::from_secs(10 * DAY));
    let as_clock: Arc<dyn Clock> = clock.clone();
    let path = scratch(name).join("state.db");
    let store = SqliteStore::open(&path, as_clock, Options::default()).expect("open");
    Env {
        store: Arc::new(store),
        clock,
    }
}

pub fn tid(n: u64) -> TokenId {
    TokenId::from_bits(n)
}

pub fn secret_bytes(n: u8) -> [u8; 32] {
    [n; 32]
}

pub fn node(text: &str) -> NodeId {
    text.parse().expect("a node id")
}

pub fn roles(of: &[Role]) -> RoleSet {
    RoleSet::new(of.iter().copied()).expect("roles")
}

pub fn spec(of: &[Role], max_uses: u32) -> TokenSpec {
    TokenSpec {
        roles: roles(of),
        max_uses,
        ttl: None,
        node_id: None,
        node_prefix: None,
        names: Vec::new(),
        replace: false,
        label: "lab".to_owned(),
        created_by: "tester".to_owned(),
    }
}

/// Creates token `n` whose secret is thirty-two bytes of `n`.
pub fn make_token(env: &Env, n: u8, spec: &TokenSpec) {
    create_token(
        &*env.store,
        &tid(u64::from(n)),
        &TokenSecret::from_bytes(secret_bytes(n)),
        spec,
    )
    .expect("create token");
}

/// One enroll request's owned parts.
#[derive(Clone)]
pub struct Req {
    pub token: TokenId,
    pub secret: Vec<u8>,
    pub node: NodeId,
    pub csrs: Vec<(Role, Vec<u8>)>,
    pub advertise: String,
    pub zone: String,
}

impl Req {
    /// A request with token `n` for node `name`, a CSR for each of `of`.
    pub fn new(n: u8, name: &str, of: &[Role]) -> Req {
        let storage = of.contains(&Role::Storage);
        Req {
            token: tid(u64::from(n)),
            secret: secret_bytes(n).to_vec(),
            node: node(name),
            csrs: of
                .iter()
                .map(|r| (*r, format!("csr-{name}-{r}").into_bytes()))
                .collect(),
            advertise: if storage {
                format!("{name}.example:7000")
            } else {
                String::new()
            },
            zone: "z1".to_owned(),
        }
    }

    pub fn roles(&self) -> Vec<Role> {
        self.csrs.iter().map(|(r, _)| *r).collect()
    }

    pub fn run(
        &self,
        store: &dyn Store,
        sign: &mut dyn FnMut() -> Result<Vec<IssuedRole>, RefusalReason>,
    ) -> Result<Enrolled, RefusalReason> {
        let csrs: Vec<(Role, &[u8])> = self.csrs.iter().map(|(r, d)| (*r, d.as_slice())).collect();
        enroll_txn(
            store,
            &EnrollRequest {
                token_id: &self.token,
                secret: &self.secret,
                node_id: &self.node,
                csrs: &csrs,
                advertise: &self.advertise,
                zone: &self.zone,
            },
            sign,
        )
    }

    /// Runs with a signer that issues `seed`-numbered certificates for the requested roles
    /// and counts its calls.
    pub fn enroll(&self, env: &Env, seed: u8) -> Result<Enrolled, RefusalReason> {
        let now = env.now();
        let roles = self.roles();
        self.run(&*env.store, &mut || Ok(issue(&roles, seed, now)))
    }
}

/// Certificates for `of`: the key of role `r` under `seed` is `[seed, r, 0..]`, its serial
/// is `[0, seed, r]` (a leading zero byte on purpose).
pub fn issue(of: &[Role], seed: u8, now: u64) -> Vec<IssuedRole> {
    of.iter()
        .map(|role| {
            let index = if *role == Role::Storage { 1 } else { 2 };
            let mut spki = [0u8; 32];
            spki[0] = seed;
            spki[1] = index;
            IssuedRole {
                role: *role,
                spki_sha256: spki,
                serial: vec![0, seed, index],
                not_before: now - 300,
                not_after: now + 7 * DAY,
                cert_der: vec![seed, index],
            }
        })
        .collect()
}

pub fn serial_of(seed: u8, role: Role) -> String {
    let index = if role == Role::Storage { 1 } else { 2 };
    format!("{seed:x}0{index}")
}

/// How the wrapper treats transactions and reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Pass,
    /// Every transaction conflicts.
    Conflict,
    /// Every transaction fails in the backend.
    FailTxn,
    /// Every read fails in the backend.
    FailGet,
}

/// A store that delegates, records the transactions it applies, can run a hook just before
/// the first one (a rival writer), and can fail.
pub struct Wrap {
    pub inner: Arc<dyn Store>,
    pub mode: Mode,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    pub txns: Mutex<Vec<Txn>>,
}

impl Wrap {
    pub fn new(inner: Arc<dyn Store>, mode: Mode) -> Wrap {
        Wrap {
            inner,
            mode,
            hook: Mutex::new(None),
            txns: Mutex::new(Vec::new()),
        }
    }

    pub fn before_first_txn(&self, hook: impl FnOnce() + Send + 'static) {
        *self.hook.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    pub fn recorded(&self) -> Vec<Txn> {
        self.txns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Store for Wrap {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let hook = self
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
        self.txns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(batch.iter().cloned());
        match self.mode {
            Mode::Conflict => Ok(batch
                .iter()
                .map(|_| {
                    Err(Conflict {
                        index: 0,
                        key: "x".to_owned(),
                    })
                })
                .collect()),
            Mode::FailTxn => Err(StoreError::Failed("down".to_owned())),
            Mode::Pass | Mode::FailGet => self.inner.apply_batch(batch),
        }
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if self.mode == Mode::FailGet {
            return Err(StoreError::Failed("down".to_owned()));
        }
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}
