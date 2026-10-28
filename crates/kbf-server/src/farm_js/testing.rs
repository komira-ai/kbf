//! What the tests of this module share: a store, a handle on it with this server's codec and
//! rows, and a request.

use std::sync::Arc;

use js_store::{EpochPolicy, Rows};
use kbf_front::Described;
use kbf_sched::Request;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Cond, ManualClock, Store, Txn};
use kbf_types::sched::usage::LeaseCost;
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources};

use super::codec::KbfRowCodec;
use super::ext::KbfExt;
use crate::action_usage::Subject;
use crate::farm::durable::subject;

/// A fresh store with the marker that says several writers share it.
pub(super) fn store() -> Arc<Fake> {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    kbf_state::share(&*store).expect("the shared marker");
    store
}

/// A handle of frontend `frontend` on `store`.
pub(super) fn rows(store: &Arc<Fake>, frontend: u64) -> Rows {
    let handle: Arc<dyn Store> = store.clone();
    let codec = Arc::new(KbfRowCodec);
    let (rows, _) =
        Rows::open(handle, EpochPolicy::Keep, frontend, codec, Arc::new(KbfExt)).expect("open");
    rows
}

/// A request for the action of digest tag `tag`.
pub(super) fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

/// The value of a `subject/` row: what the `action usage:` line says of an operation that has
/// nothing to say.
pub(super) fn parcel() -> Vec<u8> {
    subject::encode(&Subject::of(Described::default()).0)
}

/// A cost with every field set.
pub(super) fn charged() -> LeaseCost {
    LeaseCost {
        cpu_millis: 4,
        memory_bytes: 5,
        limit_bytes: 6,
        cold: true,
        retry: true,
        key_hash: 7,
    }
}

/// Puts `value` at `key`, whatever is there.
pub(super) fn overwrite(store: &Fake, key: &str, value: Vec<u8>) {
    let txn = Txn::new().put(key.to_owned(), value, Cond::Any);
    store.txn(txn).expect("a store").expect("a write");
}
