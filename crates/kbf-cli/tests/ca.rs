//! `kbf ca init` and `kbf ca import`: the CA directory they leave, the pin they print and
//! what they refuse. In-process through `kbf_cli::ca::execute` (clock and randomness are
//! parameters), and once through the `kbf` binary to show the word is registered.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use kbf_ca::{Authority, Entropy, FileSigner, Policy, SystemEntropy};
use kbf_cli::ca::{self, Ca, Verb};

const NOW: i64 = 1_800_000_000;

struct Fixed;

impl Entropy for Fixed {
    fn fill(&mut self, dest: &mut [u8]) {
        dest.fill(0x5a);
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-cli-ca-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch");
    fs::canonicalize(&dir).expect("canonical")
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o7777
}

fn init(dir: &Path) -> Result<String, String> {
    let mut out = Vec::new();
    let ca = Ca {
        verb: Verb::Init {
            dir: dir.to_owned(),
        },
    };
    ca::execute(&ca, NOW, &mut Fixed, &mut out).map_err(|e| e.to_string())?;
    Ok(String::from_utf8(out).expect("utf8"))
}

fn import(dir: &Path, cert: &Path, key: &Path) -> Result<String, String> {
    let mut out = Vec::new();
    let ca = Ca {
        verb: Verb::Import {
            dir: dir.to_owned(),
            cert: cert.to_owned(),
            key: key.to_owned(),
        },
    };
    ca::execute(&ca, NOW, &mut Fixed, &mut out).map_err(|e| e.to_string())?;
    Ok(String::from_utf8(out).expect("utf8"))
}

fn pin_line(out: &str) -> &str {
    out.lines()
        .find_map(|l| l.strip_prefix("pin: "))
        .expect("a pin line")
}

// Catches: init that leaves the key or its directory open to others, a certificate that is
// not there, a printed pin that is not the root's pin (a token minted from it would never
// verify), and a printed pin that comes from anywhere but the files on disk.
#[test]
fn init_makes_the_directory_and_prints_the_pin_of_the_root() {
    let dir = scratch("init").join("ca");
    let out = init(&dir).expect("init");
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("ca.key")), 0o600);
    assert_eq!(mode(&dir.join("ca.pem")), 0o644);
    let loaded = Authority::load_dir(&dir, Policy::default()).expect("the directory loads");
    assert_eq!(pin_line(&out), loaded.pin().to_string());
    assert!(out.contains(&format!("ca: {}", dir.display())));
    assert!(!out.contains("PRIVATE KEY"));
}

// Catches: init over an existing CA (the farm's key replaced, every issued certificate
// orphaned): it must refuse and leave the key byte for byte what it was, whichever of the
// two files is there.
#[test]
fn init_refuses_an_existing_key_or_certificate_and_changes_nothing() {
    let dir = scratch("init_twice").join("ca");
    init(&dir).expect("first");
    let key = fs::read(dir.join("ca.key")).expect("key");
    let pem = fs::read(dir.join("ca.pem")).expect("pem");
    let err = init(&dir).expect_err("second init");
    assert!(
        err.contains("ca.key") && err.contains("never overwritten"),
        "{err}"
    );
    assert_eq!(fs::read(dir.join("ca.key")).expect("key"), key);
    assert_eq!(fs::read(dir.join("ca.pem")).expect("pem"), pem);

    let dir = scratch("init_pem").join("ca");
    fs::create_dir(&dir).expect("dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod");
    fs::write(dir.join("ca.pem"), b"mine").expect("write");
    let err = init(&dir).expect_err("a certificate is there");
    assert!(err.contains("ca.pem"), "{err}");
    assert!(
        !dir.join("ca.key").exists(),
        "no key made next to a certificate that was there"
    );
}

/// A CA made elsewhere: its root certificate and key as two files.
fn foreign_ca(name: &str) -> (PathBuf, PathBuf, kbf_types::CaPin) {
    let dir = scratch(name);
    let signer = Arc::new(FileSigner::generate().expect("key"));
    let ca =
        Authority::create(signer.clone(), Policy::default(), NOW, &mut SystemEntropy).expect("ca");
    let cert = dir.join("root.pem");
    let key = dir.join("root.key");
    fs::write(&cert, ca.bundle().to_pem()).expect("cert");
    fs::write(&key, signer.to_pem()).expect("key");
    (cert, key, ca.pin())
}

// Catches: an import that does not carry the operator's CA over (so the existing
// certificates stop verifying), prints another pin than the imported root's, or leaves
// the copy open to others.
#[test]
fn import_copies_the_ca_and_prints_its_pin() {
    let (cert, key, pin) = foreign_ca("import_src");
    let dir = scratch("import").join("ca");
    let out = import(&dir, &cert, &key).expect("import");
    assert_eq!(pin_line(&out), pin.to_string());
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("ca.key")), 0o600);
    assert_eq!(mode(&dir.join("ca.pem")), 0o644);
    let loaded = Authority::load_dir(&dir, Policy::default()).expect("loads");
    assert_eq!(loaded.pin(), pin);
}

// Catches: an import over an existing CA, a certificate that is not for the key (a CA
// directory that cannot sign for its own root), files that are not a certificate and key,
// and files that cannot be read; none may leave a key behind.
#[test]
fn import_refuses_an_existing_ca_and_files_that_do_not_match() {
    let (cert, key, _) = foreign_ca("import_bad_src");
    let dir = scratch("import_twice").join("ca");
    import(&dir, &cert, &key).expect("first");
    let before = fs::read(dir.join("ca.key")).expect("key");
    let err = import(&dir, &cert, &key).expect_err("second import");
    assert!(err.contains("never overwritten"), "{err}");
    assert_eq!(fs::read(dir.join("ca.key")).expect("key"), before);

    let (other_cert, other_key, _) = foreign_ca("import_other_src");
    let dir = scratch("import_mismatch").join("ca");
    let err = import(&dir, &other_cert, &key).expect_err("a certificate for another key");
    assert!(err.contains("key"), "{err}");
    assert!(!dir.exists(), "nothing made for files that do not match");
    let err = import(&dir, &cert, &other_key).expect_err("the other way round");
    assert!(err.contains("key"), "{err}");

    let junk = scratch("import_junk");
    fs::write(junk.join("cert"), b"not pem").expect("write");
    fs::write(junk.join("key"), b"not pem").expect("write");
    assert!(import(&dir, &junk.join("cert"), &key).is_err());
    assert!(import(&dir, &cert, &junk.join("key")).is_err());
    assert!(import(&dir, &junk.join("missing"), &key).is_err());
    assert!(!dir.exists());

    // A certificate already in the directory is not overwritten either.
    let dir = scratch("import_pem").join("ca");
    fs::create_dir(&dir).expect("dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod");
    fs::write(dir.join("ca.pem"), b"mine").expect("write");
    assert!(
        import(&dir, &cert, &key)
            .expect_err("pem there")
            .contains("ca.pem")
    );
    assert!(!dir.join("ca.key").exists());
}

fn args(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

// Catches: the word not registered or its arguments misread: `kbf ca init DIR` and
// `kbf ca import DIR --cert --key` parse to the verbs, a missing flag is refused.
#[test]
fn the_command_line_parses() {
    assert_eq!(
        ca::parse(&args(&["kbf", "ca", "init", "/x"]))
            .expect("init")
            .verb,
        Verb::Init { dir: "/x".into() }
    );
    assert_eq!(
        ca::parse(&args(&[
            "kbf", "ca", "import", "/x", "--cert", "c", "--key", "k"
        ]))
        .expect("import")
        .verb,
        Verb::Import {
            dir: "/x".into(),
            cert: "c".into(),
            key: "k".into()
        }
    );
    assert!(ca::parse(&args(&["kbf", "ca", "import", "/x", "--cert", "c"])).is_err());
    assert!(ca::parse(&args(&["kbf", "ca"])).is_err());
}

// Catches: the `kbf` binary not dispatching the word (it would read `ca` as a mode and
// refuse it), the real clock and randomness path, and an exit code that hides a refusal.
#[test]
fn the_binary_runs_init_and_refuses_a_second_one() {
    let dir = scratch("binary").join("ca");
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_kbf"))
            .args(["ca", "init"])
            .arg(&dir)
            .output()
            .expect("run kbf")
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let out = String::from_utf8(first.stdout).expect("utf8");
    let loaded = Authority::load_dir(&dir, Policy::default()).expect("loads");
    assert_eq!(pin_line(&out), loaded.pin().to_string());
    let second = run();
    assert_eq!(second.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&second.stderr).starts_with("kbf ca: "));
    let usage = Command::new(env!("CARGO_BIN_EXE_kbf"))
        .args(["ca", "frob"])
        .output()
        .expect("run");
    assert_eq!(usage.status.code(), Some(2));
}
