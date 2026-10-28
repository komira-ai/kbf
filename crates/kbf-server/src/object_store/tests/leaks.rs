//! Where a secret may appear: in its file, and nowhere else. Each scan first checks the
//! sentinel IS in the file, so a scan of the wrong place cannot pass by finding nothing.

use std::os::unix::fs::PermissionsExt as _;

use kbf_types::object_store_api::BindingRole;
use sha2::{Digest as _, Sha256};

use super::*;

const FIRST: &str = "FIRST-sentinel-aaaaaaaaaaaaaaaaaaaa";
const SECOND: &str = "SECOND-sentinel-bbbbbbbbbbbbbbbbbbb";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Everything under `dir`, file by file.
fn bytes_under(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

// Catches a secret, or a digest of it, written into any state row: backups of the
// state would carry it. The scan covers every row, and first checks it is non-empty.
#[test]
fn the_secret_is_only_ever_in_its_file_never_in_the_state_rows() {
    let root = scratch("leak");
    let store = as_store(&fake());
    let secrets = root.join("secrets");
    let reg = open(&store, &secrets);

    let mut body = put(FIRST);
    body.verify = Some(true);
    reg.put_connection(&name("cold"), &body).unwrap();
    // The scan means something only if the sentinel is where it should be.
    let file = std::fs::read(file_of(&secrets, "cold", 1)).unwrap();
    assert!(
        contains(&file, FIRST.as_bytes()),
        "the secret is in its file"
    );

    reg.put_connection(&name("cold"), &put(SECOND)).unwrap();
    reg.put_connection(&name("cold"), &put(SECOND)).unwrap();
    reg.put_binding(BindingRole::Cold, &bind("cold", "bucket"))
        .unwrap();
    let file = std::fs::read(file_of(&secrets, "cold", 2)).unwrap();
    assert!(contains(&file, SECOND.as_bytes()));
    assert!(!file.is_empty());
    assert_eq!(
        bytes_under(&secrets).len(),
        1,
        "the first version's file is gone"
    );

    let needles: Vec<Vec<u8>> = [FIRST, SECOND]
        .into_iter()
        .flat_map(|s| {
            let digest = Sha256::digest(s.as_bytes());
            [
                s.as_bytes().to_vec(),
                digest.to_vec(),
                hex(&digest).into_bytes(),
                hex(&digest).to_uppercase().into_bytes(),
            ]
        })
        .collect();
    // The same through the store's own reads.
    let rows = store.scan_prefix("").unwrap();
    assert!(
        rows.len() >= 6,
        "conn, bind, gen and audit rows exist to be scanned"
    );
    for (key, row) in rows {
        for needle in &needles {
            assert!(!contains(&row.value, needle), "{key}");
        }
    }
}

// Catches a Debug, Display or JSON of anything the registry hands out that carries the
// secret: errors, listings, audit rows, the registry itself, the PUT body.
#[test]
fn nothing_the_registry_returns_or_formats_holds_the_secret() {
    let store = as_store(&fake());
    let root = scratch("leak-fmt");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    let body = put(FIRST);
    let mut shown = vec![format!("{body:?}"), format!("{reg:?}")];
    let file = std::fs::read(dir.join("nothing"));
    assert!(file.is_err());
    reg.put_connection(&name("cold"), &body).unwrap();
    assert!(contains(
        &std::fs::read(file_of(&dir, "cold", 1)).unwrap(),
        FIRST.as_bytes()
    ));
    reg.put_binding(BindingRole::Cold, &bind("cold", "b"))
        .unwrap();
    shown.push(serde_json::to_string(&reg.connections().unwrap()).unwrap());
    shown.push(serde_json::to_string(&reg.bindings().unwrap()).unwrap());
    shown.push(format!("{:?}", reg.connections().unwrap()));
    shown.push(
        serde_json::to_string(&reg.audit(std::num::NonZeroUsize::new(9).unwrap()).unwrap())
            .unwrap(),
    );
    // Errors: a stale generation, a conflict, a wide file, a bad file.
    let mut stale = put(FIRST);
    stale.expect_generation = Some(99);
    shown.push(
        reg.put_connection(&name("cold"), &stale)
            .unwrap_err()
            .to_string(),
    );
    shown.push(format!(
        "{:?}",
        reg.put_connection(&name("cold"), &stale).unwrap_err()
    ));
    std::fs::write(
        file_of(&dir, "cold", 1),
        format!("{{\"secret_access_key\":\"{FIRST}\""),
    )
    .unwrap();
    let err = reg.secret(&name("cold")).unwrap_err();
    shown.push(format!("{err} {err:?}"));
    std::fs::set_permissions(
        file_of(&dir, "cold", 1),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let err = reg.secret(&name("cold")).unwrap_err();
    shown.push(format!("{err} {err:?}"));
    for text in shown {
        assert!(!text.contains(FIRST), "{text}");
    }
}

// Catches a log line (at TRACE, the most a subscriber can ask for) that carries the
// secret or the request, on the paths that log: the removal of a file that stays behind.
#[test]
fn the_logs_never_hold_the_secret() {
    let lines = crate::test_log::capture_at(tracing::Level::TRACE);

    let store = as_store(&fake());
    let root = scratch("leak-log");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    reg.put_connection(&name("cold"), &put(FIRST)).unwrap();
    // The old version's file becomes a non-empty directory: its removal fails and logs.
    let old = file_of(&dir, "cold", 1);
    std::fs::remove_file(&old).unwrap();
    std::fs::create_dir(&old).unwrap();
    std::fs::write(old.join("x"), "x").unwrap();
    reg.put_connection(&name("cold"), &put(SECOND)).unwrap();
    reg.delete_connection(&name("cold")).unwrap();

    let text = lines.text();
    assert!(
        text.contains("unreferenced secret file"),
        "the path that logs ran: {text}"
    );
    assert!(text.contains("cold"));
    assert!(!text.contains(FIRST) && !text.contains(SECOND), "{text}");
}
