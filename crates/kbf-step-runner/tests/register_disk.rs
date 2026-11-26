//! The registry's writes to its disk: a write held by the test holds up no read and no close,
//! and what was acknowledged is what a reload finds. No socket is served here, so these run on
//! every system (the served tests are in `register.rs`).

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use kbf_step_runner::register::{
    Declared, Grant, REGISTRATIONS_FILE, Registration, Registry, RegistryError, Reported, Why,
};
use support::{FakeClock, Scratch};

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

struct Table(Mutex<BTreeMap<String, Grant>>);

impl Declared for Table {
    fn grant(&self, name: &str) -> Option<Grant> {
        self.0.lock().unwrap().get(name).cloned()
    }
}

fn grant(uid: u32) -> Grant {
    Grant {
        uid,
        health_ports: vec![9301, 9302],
        registration_ttl_secs: 300,
    }
}

/// `svc` at `version`, with no health address.
fn reg(version: &str) -> Registration {
    Registration {
        name: "svc".to_owned(),
        version: version.to_owned(),
        health: None,
    }
}

/// `svc` and `other`, both declared for the test's user.
fn two_declared() -> Table {
    Table(Mutex::new(BTreeMap::from([
        ("svc".to_owned(), grant(me())),
        ("other".to_owned(), grant(me())),
    ])))
}

/// A registry whose disk is held by the test: each write sends its bytes on `writes` and waits
/// for its outcome on `outcome`; an `Ok` is then written to disk.
struct HeldDisk {
    registry: Arc<Registry>,
    writes: mpsc::Receiver<Vec<u8>>,
    outcome: mpsc::Sender<std::io::Result<()>>,
}

fn held_registry(dir: &Path) -> HeldDisk {
    let (entered, writes) = mpsc::channel::<Vec<u8>>();
    let (outcome, outcomes) = mpsc::channel::<std::io::Result<()>>();
    let outcomes = Mutex::new(outcomes);
    let writer = move |dir: &Path, bytes: &[u8]| -> std::io::Result<()> {
        entered.send(bytes.to_vec()).unwrap();
        outcomes.lock().unwrap().recv().unwrap()?;
        fs::write(dir.join(REGISTRATIONS_FILE), bytes)
    };
    let registry = Registry::load(dir, FakeClock::new(), &two_declared())
        .unwrap()
        .with_writer(Box::new(writer));
    HeldDisk {
        registry: Arc::new(registry),
        writes,
        outcome,
    }
}

/// Registers `name` at `version` on a thread of its own.
fn register_in_thread(
    registry: &Arc<Registry>,
    name: &str,
    version: &str,
) -> JoinHandle<Result<u64, RegistryError>> {
    let registry = registry.clone();
    let mut registration = reg(version);
    registration.name = name.to_owned();
    std::thread::spawn(move || registry.register(me(), &registration, &grant(me())))
}

/// The names and versions written in `bytes`, with whether each is written stopped.
fn written(bytes: &[u8]) -> Vec<(String, String, bool)> {
    let saved: Vec<serde_json::Value> = serde_json::from_slice(bytes).unwrap();
    saved
        .iter()
        .map(|s| {
            let text = |key: &str| s[key].as_str().unwrap().to_owned();
            (text("name"), text("version"), s["stopped"] == true)
        })
        .collect()
}

/// Catches a close that waits for the disk before it is believed: while the write of another
/// service's registration is held, the close of a registered service is reported `stopped` at
/// once, and both changes are on disk once the writes have ended, the close's last. Nothing
/// here waits for a time: the test waits for the report to change, which, with the close
/// waiting behind the held write, it never would.
#[test]
fn a_close_is_believed_while_another_write_is_held() {
    let scratch = Scratch::new("reg-close-held");
    let HeldDisk {
        registry,
        writes,
        outcome,
    } = held_registry(scratch.path());
    let first = register_in_thread(&registry, "svc", "2.0.0");
    writes.recv().unwrap();
    outcome.send(Ok(())).unwrap();
    let conn = first.join().unwrap().unwrap();
    assert_eq!(registry.report("svc", Some("2.0.0")), Reported::Healthy);

    let other = register_in_thread(&registry, "other", "1.0.0");
    let held = writes.recv().unwrap();
    assert_eq!(written(&held).len(), 2);
    let closing = {
        let registry = registry.clone();
        std::thread::spawn(move || registry.closed("svc", conn))
    };
    while registry.report("svc", Some("2.0.0")) != Reported::Unhealthy(Why::Stopped) {
        std::thread::yield_now();
    }
    assert_eq!(registry.report("other", None), Reported::Absent);
    outcome.send(Ok(())).unwrap();
    other.join().unwrap().unwrap();
    let last = writes.recv().unwrap();
    outcome.send(Ok(())).unwrap();
    closing.join().unwrap().unwrap();
    assert_eq!(
        written(&last),
        [
            ("other".to_owned(), "1.0.0".to_owned(), false),
            ("svc".to_owned(), "2.0.0".to_owned(), true),
        ]
    );
    let after = Registry::load(scratch.path(), FakeClock::new(), &two_declared()).unwrap();
    assert_eq!(after.report("svc", None), Reported::Unhealthy(Why::Stopped));
    assert_eq!(after.report("other", Some("1.0.0")), Reported::Healthy);
}

/// Catches writes that are not one at a time, a failed write that is believed, and a close of
/// an older connection that stops its replacement: a replacement of `svc` whose write fails
/// leaves the registration before it, on disk and in reports; a registration of `other` that
/// waited behind it writes `svc` as it stood; a later replacement of `svc` is not stopped by the
/// close of the connection it replaced; and what was acknowledged is what a reload finds.
#[test]
fn competing_changes_keep_what_was_acknowledged() {
    let scratch = Scratch::new("reg-compete");
    let HeldDisk {
        registry,
        writes,
        outcome,
    } = held_registry(scratch.path());
    let first = register_in_thread(&registry, "svc", "1.0.0");
    writes.recv().unwrap();
    outcome.send(Ok(())).unwrap();
    let old_conn = first.join().unwrap().unwrap();

    let replacing = register_in_thread(&registry, "svc", "2.0.0");
    let held = writes.recv().unwrap();
    assert_eq!(
        written(&held),
        [("svc".to_owned(), "2.0.0".to_owned(), false)]
    );
    let other = register_in_thread(&registry, "other", "1.0.0");
    outcome
        .send(Err(std::io::Error::other("the disk is full")))
        .unwrap();
    assert!(matches!(
        replacing.join().unwrap(),
        Err(RegistryError::Io(_))
    ));
    assert_eq!(registry.report("svc", Some("1.0.0")), Reported::Healthy);
    let next = writes.recv().unwrap();
    assert_eq!(
        written(&next),
        [
            ("other".to_owned(), "1.0.0".to_owned(), false),
            ("svc".to_owned(), "1.0.0".to_owned(), false),
        ]
    );
    outcome.send(Ok(())).unwrap();
    other.join().unwrap().unwrap();

    let again = register_in_thread(&registry, "svc", "3.0.0");
    writes.recv().unwrap();
    outcome.send(Ok(())).unwrap();
    let new_conn = again.join().unwrap().unwrap();
    assert_ne!(new_conn, old_conn);
    // The close of the replaced connection changes nothing and writes nothing.
    registry.closed("svc", old_conn).unwrap();
    assert_eq!(registry.report("svc", Some("3.0.0")), Reported::Healthy);
    assert!(writes.try_recv().is_err());

    let after = Registry::load(scratch.path(), FakeClock::new(), &two_declared()).unwrap();
    assert_eq!(after.report("svc", Some("3.0.0")), Reported::Healthy);
    assert_eq!(after.report("other", Some("1.0.0")), Reported::Healthy);
}
