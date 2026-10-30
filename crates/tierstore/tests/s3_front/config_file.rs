//! The `tierstore` binary started from a configuration file (`--config`), and `--print-config`:
//! it serves what the same settings given as flags serve, and refuses what the file must not say.

use std::path::Path;

use bytes::Bytes;
use kbf_objstore::s3::S3Store;
use kbf_objstore::{ByteRange, KeyPrefix, ObjectKey, ObjectStore, PageSize};
use rustix::process::Signal;

use crate::binary::{run, start};
use crate::files::{BIN_SECRET, Files};

/// The calls of one session, and what each answered: two puts, a put of a key that is there,
/// a ranged read of an object and of one that is not there, a listing, a delete and a listing
/// again. The transcript has no address and no time in it, so two servers over the same
/// settings give the same one.
async fn session(store: &S3Store) -> Vec<String> {
    let key = |name: &str| ObjectKey::new(name).unwrap();
    let mut said = Vec::new();
    let (a, b) = (key("cas/a"), key("cas/b"));
    let alpha = store.put_new(&a, Bytes::from_static(b"alpha"), None).await;
    said.push(format!("put a: {alpha:?}"));
    let bravo = store.put_new(&b, Bytes::from_static(b"bravo!"), None).await;
    said.push(format!("put b: {bravo:?}"));
    let again = store.put_new(&a, Bytes::from_static(b"other"), None).await;
    said.push(format!("put a again: {again:?}"));
    let part = ByteRange::new(1, 3).unwrap();
    said.push(format!(
        "get a 1..4: {:?}",
        store.get_range(&key("cas/a"), part).await
    ));
    let first = ByteRange::new(0, 1).unwrap();
    said.push(format!(
        "get absent: {:?}",
        store.get_range(&key("cas/none"), first).await
    ));
    let prefix = KeyPrefix::new("cas/").unwrap();
    for step in ["list", "list after delete"] {
        let page = store.list(&prefix, None, PageSize::MAX).await.unwrap();
        let names: Vec<String> = page
            .objects
            .iter()
            .map(|object| format!("{} {}", object.key.as_str(), object.size))
            .collect();
        said.push(format!("{step}: {names:?}"));
        said.push(format!("delete b: {:?}", store.delete(&key("cas/b")).await));
    }
    said
}

/// Starts the binary over these arguments, runs the session against it, drains it with SIGTERM
/// and checks that it exits with status 0.
async fn served(args: &[String]) -> Vec<String> {
    let mut running = start(args);
    let said = session(&running.store()).await;
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
    said
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// The text of a file for two directory tiers under `root`, written by hand, with the budget as
/// a string with a unit.
fn directory_file(key: &Path, root: &Path) -> String {
    format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nproxy_id = \"by-hand\"\nbucket = \"tier-bucket\"\n\
         access_key_file = \"{}\"\n\n[hot_store]\nkind = \"dir\"\npath = \"{}\"\nbudget = \"1MiB\"\n\n\
         [cold_store]\nkind = \"dir\"\npath = \"{}\"\n",
        key.display(),
        root.join("hot").display(),
        root.join("cold").display(),
    )
}

/// Asserts that `--print-config` over a set of flags writes the file and exits with status 0 and
/// no start (it has no address bound and says nothing on stderr), that the file names the key
/// file by its path and not its contents, and that the binary started from that file answers the
/// session exactly as the binary started from the flags: the same puts, the refusal of the
/// second put of a key, the ranged reads, the listings and the delete.
#[tokio::test]
async fn a_server_started_from_the_printed_file_serves_as_one_started_from_flags() {
    let files = Files::new("cfg-serve");
    let flags = files.flags(&["--drain-lb-wait", "50ms", "--copy-parallel", "3"]);
    let from_flags = served(&flags).await;

    let mut printing = flags.clone();
    printing.push("--print-config".to_owned());
    let (code, printed, err) = run(&printing);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(
        printed.contains(&files.key.display().to_string()),
        "{printed}"
    );
    assert!(!printed.contains(BIN_SECRET), "{printed}");
    assert!(printed.contains("copy_parallel = 3"), "{printed}");
    assert!(!printed.contains("drain_"), "{printed}");
    let file = files.secret("tierstore.toml", &printed);

    let args = strings(&[
        "--config",
        &file.display().to_string(),
        "--drain-lb-wait",
        "50ms",
    ]);
    let from_file = served(&args).await;
    assert_eq!(from_file, from_flags);
    assert!(from_file[0].contains("Ok"), "{from_file:?}");
    assert!(from_file[2].contains("AlreadyExists"), "{from_file:?}");
    assert!(from_file[4].contains("NotFound"), "{from_file:?}");
    assert_eq!(from_file[5], "list: [\"cas/a 5\", \"cas/b 6\"]");
    assert_eq!(from_file[7], "list after delete: [\"cas/a 5\"]");
}

/// Asserts that a file written by hand, with directory tiers and a budget written `1MiB`, starts
/// the binary, which keeps what it is given in the hot tier's directory, as the same tiers given
/// as flags do.
#[tokio::test]
async fn a_file_written_by_hand_with_directory_tiers_keeps_objects_on_disk() {
    let files = Files::new("cfg-dirs");
    let root = files.key.parent().unwrap().to_path_buf();
    let file = files.secret("by-hand.toml", &directory_file(&files.key, &root));
    let args = strings(&[
        "--config",
        &file.display().to_string(),
        "--drain-lb-wait",
        "50ms",
    ]);
    let said = served(&args).await;
    assert!(said[0].contains("Ok"), "{said:?}");
    assert!(root.join("hot/cas~a").is_file());
}

/// Asserts that `--print-config` over flags that no file can say exits with status 2 and writes
/// nothing to stdout, and that flags are still accepted without `--config` as they were.
#[test]
fn print_config_of_flags_that_no_file_can_say_is_a_refusal() {
    let files = Files::new("cfg-print-refused");
    let mut flags = files.flags(&["--print-config"]);
    flags = crate::files::with(&flags, "--hot", "tape");
    let (code, out, err) = run(&flags);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("--hot") && err.contains("tape"), "{err}");
}

/// Asserts the refusals of a start with a file, each with status 2, nothing on stdout and a
/// message that names what is wrong: a data path flag given with the file (all such flags named,
/// and `--drain-lb-wait` not), `--print-config` with the file, a file that is not there, an
/// unknown key, an inline secret, and `[local_cache]` along with `[hot_store]`.
#[test]
fn a_start_with_a_file_that_must_be_refused_says_why() {
    let files = Files::new("cfg-refused");
    let good = directory_file(&files.key, files.key.parent().unwrap());
    let file = files.secret("good.toml", &good);
    let path = file.display().to_string();

    let args = strings(&[
        "--config",
        &path,
        "--listen",
        "127.0.0.1:0",
        "--hot",
        "memory",
    ]);
    let (code, out, err) = run(&args);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.starts_with("tierstore: "), "{err}");
    assert!(err.contains("--listen, --hot"), "{err}");
    let (code, _, err) = run(&strings(&["--config", &path, "--print-config"]));
    assert_eq!(code, 2);
    assert!(err.contains("--print-config"), "{err}");
    let (code, _, err) = run(&strings(&["--config", "/no/such/tierstore.toml"]));
    assert_eq!(code, 2);
    assert!(err.contains("/no/such/tierstore.toml"), "{err}");

    let refused = [
        ("[server]\n", "[server]\nbogus = 1\n", "bogus"),
        (
            "[hot_store]\n",
            "[hot_store]\nsecret_access_key = \"x\"\n",
            "hot_store.secret_access_key",
        ),
        (
            "[cold_store]\n",
            "[local_cache]\npath = \"/c\"\n\n[cold_store]\n",
            "is refused with [hot_store]",
        ),
    ];
    for (from, to, want) in refused {
        let bad = files.secret("bad.toml", &good.replacen(from, to, 1));
        let (code, out, err) = run(&strings(&["--config", &bad.display().to_string()]));
        assert_eq!((code, out.as_str()), (2, ""), "{want}");
        assert!(err.contains(want) && err.contains("bad.toml"), "{err}");
    }
}

/// Asserts that a start from a file whose settings the checks of the start refuse (water marks
/// out of order) exits with status 1 and says so in the keys of the file, not in flags.
#[test]
fn a_refusal_of_the_start_speaks_of_the_keys_of_the_file() {
    let files = Files::new("cfg-keys");
    let text = directory_file(&files.key, files.key.parent().unwrap());
    let text = text.replacen("budget = \"1MiB\"", "budget = \"1MiB\"\nlow_water = 90", 1);
    let file = files.secret("marks.toml", &text);
    let (code, out, err) = run(&strings(&["--config", &file.display().to_string()]));
    assert_eq!((code, out.as_str()), (1, ""));
    assert!(err.contains("hot_store.low_water"), "{err}");
    assert!(err.contains("hot_store.high_water"), "{err}");
    assert!(!err.contains("--low-water"), "{err}");
}
