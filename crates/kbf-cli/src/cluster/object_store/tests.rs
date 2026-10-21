use std::io::Cursor;

use kbf_types::object_store_api::ConnectionPut;

use super::*;
use crate::cluster::Verb;

const SENTINEL: &str = "SENTINEL-unit-4d1e9b02c7";

fn dir(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(std::env::temp_dir(), &format!("kbf-cluster-unit-{name}"))
}

fn source(file: Option<PathBuf>) -> SecretSource {
    SecretSource {
        secret_stdin: file.is_none(),
        secret_file: file,
    }
}

fn secret_of(text: &str) -> Result<String, String> {
    read_secret(&source(None), &mut Cursor::new(text.as_bytes().to_vec()))
        .map(|s| s.to_string())
        .map_err(|e| e.to_string())
}

/// Catches only the final line ending being cut: both spellings, one of them, and
/// nothing else (an inner or a leading newline, a lone carriage return, spaces).
#[test]
fn only_the_final_line_ending_is_cut() {
    for (text, want) in [
        ("k\n", "k"),
        ("k\r\n", "k"),
        ("k", "k"),
        ("k\n\n", "k\n"),
        ("\nk", "\nk"),
        ("k\r", "k\r"),
        (" k \n", " k "),
        ("a\nb\n", "a\nb"),
    ] {
        assert_eq!(secret_of(text).as_deref(), Ok(want), "{text:?}");
    }
}

/// Catches an empty key being sent, a key too long to be one being read to its end, text
/// that is not UTF-8 being lossily turned into another key, and an error that holds a byte
/// of what was read.
#[test]
fn a_source_that_is_no_key_is_refused_without_quoting_it() {
    for text in ["", "\n", "\r\n"] {
        let err = secret_of(text).expect_err("empty");
        assert_eq!(err, "standard input: it holds no key", "{text:?}");
    }
    let long = format!("{SENTINEL}{}", "x".repeat(SECRET_READ_MAX as usize));
    let err = secret_of(&long).expect_err("long");
    assert!(err.contains("longer"), "{err}");
    assert!(!err.contains(SENTINEL), "{err}");
    let at_limit = "k".repeat(SECRET_READ_MAX as usize);
    assert_eq!(secret_of(&at_limit).as_deref(), Ok(at_limit.as_str()));
    let too_much = "k".repeat(SECRET_READ_MAX as usize + 1);
    assert!(secret_of(&too_much).is_err());
    let err = read_secret(&source(None), &mut Cursor::new(vec![b'k', 0xff, 0xfe]))
        .expect_err("not UTF-8")
        .to_string();
    assert_eq!(err, "standard input: it is not UTF-8");
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("the pipe broke"))
        }
    }
    let err = read_secret(&source(None), &mut Broken)
        .expect_err("broken")
        .to_string();
    assert_eq!(err, "standard input: the pipe broke");
}

/// Catches a secret file read whatever its mode (the mode check on the opened file), a
/// refusal that names no file or no mode, a directory read as a key, and a symlink to a
/// wide file passing for a private one.
#[test]
fn a_secret_file_is_a_private_regular_file() {
    let dir = dir("files");
    let write = |name: &str, mode: u32| {
        let path = dir.join(name);
        std::fs::write(&path, format!("{SENTINEL}\n")).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        path
    };
    let read = |path: &PathBuf| {
        read_secret(&source(Some(path.clone())), &mut std::io::empty()).map(|s| s.to_string())
    };
    for mode in [0o600, 0o400, 0o700] {
        let path = write(&format!("ok-{mode:o}"), mode);
        assert_eq!(read(&path).expect("private").as_str(), SENTINEL, "{mode:o}");
    }
    for mode in [0o640, 0o604, 0o660, 0o644, 0o666, 0o601, 0o610, 0o404] {
        let path = write(&format!("wide-{mode:o}"), mode);
        let err = read(&path).expect_err("wide").to_string();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains(&format!("{mode:04o}")), "{err}");
        assert!(!err.contains(SENTINEL), "{err}");
    }
    let wide = write("target", 0o644);
    let link = dir.join("link");
    std::os::unix::fs::symlink(&wide, &link).expect("symlink");
    assert!(read(&link).is_err(), "the mode is the target's");
    let err = read(&dir.path().to_path_buf())
        .expect_err("directory")
        .to_string();
    assert!(err.ends_with("it is not a regular file"), "{err}");
    let err = read(&dir.join("absent")).expect_err("absent").to_string();
    assert!(
        err.starts_with("secret file ") && err.contains("absent"),
        "{err}"
    );
}

/// Catches a body this client sends that the server would refuse or read differently:
/// every option set, then none, parsed into the server's own `ConnectionPut`
/// (`deny_unknown_fields`), so a renamed or added field there fails here.
#[test]
fn the_connection_body_is_what_the_server_parses() {
    let full = Body::json(&ConnectionBody {
        endpoint: "http://127.0.0.1:9000",
        region: Some("us-east-1"),
        access_key_id: "AKID",
        secret_access_key: SENTINEL,
        conditional_put: Some(true),
        min_put_rate: NonZeroU64::new(4096),
        expect_generation: Some(3),
        verify: Some(false),
    });
    let put: ConnectionPut = serde_json::from_str(full.as_str()).expect("parses");
    assert_eq!(put.endpoint, "http://127.0.0.1:9000");
    assert_eq!(put.region.as_deref(), Some("us-east-1"));
    assert_eq!(put.access_key_id, "AKID");
    assert_eq!(put.secret_access_key.expose(), SENTINEL);
    assert_eq!(put.conditional_put, Some(true));
    assert_eq!(put.min_put_rate, NonZeroU64::new(4096));
    assert_eq!(put.expect_generation, Some(3));
    assert_eq!(put.verify, Some(false));

    let least = Body::json(&ConnectionBody {
        endpoint: "http://h",
        region: None,
        access_key_id: "A",
        secret_access_key: "S",
        conditional_put: None,
        min_put_rate: None,
        expect_generation: None,
        verify: None,
    });
    let put: ConnectionPut = serde_json::from_str(least.as_str()).expect("parses");
    assert_eq!(
        (
            put.region,
            put.conditional_put,
            put.min_put_rate,
            put.verify
        ),
        (None, None, None, None)
    );
    assert!(!least.as_str().contains("null"), "{}", least.as_str());
}

/// Catches a printed `Call` or `Verb` holding the key: a `Body` has a `Debug` that shows
/// nothing of its text, and the verb holds the key's source, not the key.
#[test]
fn a_call_printed_for_debugging_shows_no_secret() {
    let body = Body::json(&serde_json::json!({ "secret_access_key": SENTINEL }));
    assert!(body.as_str().contains(SENTINEL));
    let call = Call::Put("/v1/object-stores/connections/main".to_owned(), body);
    let shown = format!("{call:?}");
    assert!(!shown.contains(SENTINEL), "{shown}");
    assert!(shown.contains("Body(..)"), "{shown}");
}

/// Catches `--secret-stdin` reading standard input when the verb is not a `put`: no other
/// verb takes the `stdin` it is handed.
#[test]
fn only_a_put_reads_standard_input() {
    struct Untouched;
    impl Read for Untouched {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("read standard input");
        }
    }
    let token = PathBuf::from("t");
    let name: ConnectionName = "main".parse().expect("a name");
    let verbs = [
        Verb::ObjectStore {
            verb: ObjectStore::Status {
                token: Token {
                    token_file: token.clone(),
                },
            },
        },
        Verb::ObjectStore {
            verb: ObjectStore::Connection {
                verb: Connection::Show {
                    name,
                    token: Token { token_file: token },
                },
            },
        },
    ];
    for verb in verbs {
        assert!(verb.call(&mut Untouched).is_ok());
        assert!(verb.token_file().is_some());
    }
}
