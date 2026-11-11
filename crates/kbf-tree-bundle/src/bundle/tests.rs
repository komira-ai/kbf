//! The bundle written and read back, and each bundle the reader refuses.

use std::io;

use super::{BundleError, FACTS, Facts, Item, Normalised, ReadError, Writer, file_name, read};

fn facts() -> Facts {
    Facts {
        commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        platform: "linux-x86_64".to_owned(),
        root: "ab".repeat(32),
        entries: 7,
        depth: 3,
        normalised: vec![Normalised {
            path: "/etc/shadow".to_owned(),
            from: "0640".to_owned(),
            to: "0644".to_owned(),
        }],
    }
}

/// Every item of `bundle`, or why it was refused.
fn items(bundle: &[u8]) -> Result<Vec<Item>, ReadError<String>> {
    let mut out = Vec::new();
    read(bundle, |item| {
        out.push(item);
        Ok::<(), String>(())
    })?;
    Ok(out)
}

/// A tar of `entries` (name, bytes), each a regular file, or a directory when `bytes` is None.
fn tar_of(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (name, bytes) in entries {
        let mut header = tar::Header::new_ustar();
        match bytes {
            Some(bytes) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(bytes.len() as u64);
                tar.append_data(&mut header, name, *bytes).expect("append");
            }
            None => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                tar.append_data(&mut header, name, io::empty())
                    .expect("append");
            }
        }
    }
    tar.into_inner().expect("tar")
}

fn sha(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Asserts a bundle reads back as written: the facts first, then each object once, in the order
/// added, each under its SHA-256; and that the same input gives the same bytes. Catches: a
/// bundle the frontend cannot read, an object written twice, and bytes that differ from one
/// build to the next (which would differ from the release's `SHA256SUMS`).
#[test]
fn a_bundle_reads_back_as_written_and_is_the_same_bytes_each_time() {
    let write = || {
        let mut writer = Writer::new(Vec::new(), &facts()).expect("writer");
        assert_eq!(writer.object(b"one").expect("one"), sha(b"one"));
        writer.object(b"two").expect("two");
        writer.object(b"one").expect("again");
        writer.finish().expect("finish")
    };
    let bundle = write();
    assert_eq!(bundle, write());
    assert_eq!(
        items(&bundle).expect("read"),
        vec![
            Item::Facts(facts()),
            Item::Object {
                sha256: sha(b"one"),
                bytes: b"one".to_vec()
            },
            Item::Object {
                sha256: sha(b"two"),
                bytes: b"two".to_vec()
            },
        ]
    );
    assert_eq!(
        file_name("abc", "linux-arm64"),
        "kbf-base-tree-abc-linux-arm64.tree.tar"
    );
}

/// Asserts the facts' JSON exactly, and that a reader ignores a member it does not know.
/// Catches: a change of the facts' text the release attests, and a reader that would refuse the
/// facts of a later release that added a member (B8).
#[test]
fn the_facts_are_this_json_and_an_unknown_member_is_ignored() {
    let json = serde_json::to_string(&facts()).expect("json");
    assert_eq!(
        json,
        format!(
            concat!(
                r#"{{"commit":"0123456789abcdef0123456789abcdef01234567","#,
                r#""platform":"linux-x86_64","root":"{}","entries":7,"depth":3,"#,
                r#""normalised":[{{"path":"/etc/shadow","from":"0640","to":"0644"}}]}}"#
            ),
            "ab".repeat(32)
        )
    );
    let later = json.replace(r#""entries""#, r#""later":{"x":1},"entries""#);
    let bundle = tar_of(&[(FACTS, Some(later.as_bytes()))]);
    assert_eq!(items(&bundle).expect("read"), vec![Item::Facts(facts())]);
}

/// Asserts each malformed bundle is refused by name: empty, facts not first, facts that are
/// not JSON, a stray entry, an object name that is not 64 lowercase hex digits, a directory
/// entry, and an object whose bytes are not its name's. Catches: a frontend that would publish
/// an object under a key that is not its content's SHA-256.
#[test]
fn each_malformed_bundle_is_refused_by_name() {
    let good = serde_json::to_vec(&facts()).expect("json");
    let good = good.as_slice();
    let refusal = |bundle: Vec<u8>| match items(&bundle) {
        Err(ReadError::Bundle(e)) => e.to_string(),
        other => panic!("not refused: {other:?}"),
    };
    assert_eq!(
        refusal(tar_of(&[])),
        "the bundle is empty: its first entry must be facts.json"
    );
    let one = format!("objects/{}", sha(b"one"));
    assert_eq!(
        refusal(tar_of(&[(&one, Some(b"one")), (FACTS, Some(good))])),
        format!("the bundle's first entry is {one:?}, not facts.json")
    );
    assert!(refusal(tar_of(&[(FACTS, Some(b"{"))])).starts_with("the bundle's facts.json: "));
    for stray in [
        "README".to_owned(),
        FACTS.to_owned(),
        format!("objects/{}", sha(b"one").to_uppercase()),
        format!("objects/{}", &sha(b"one")[1..]),
        format!("other/{}", sha(b"one")),
    ] {
        assert_eq!(
            refusal(tar_of(&[(FACTS, Some(good)), (&stray, Some(b"one"))])),
            format!(
                "the bundle holds {stray:?}, which is neither facts.json first nor objects/<sha256>"
            )
        );
    }
    assert_eq!(
        refusal(tar_of(&[(FACTS, Some(good)), ("objects/", None)])),
        "the bundle's entry \"objects/\" is not a regular file"
    );
    let named = format!("objects/{}", sha(b"one"));
    assert_eq!(
        refusal(tar_of(&[(FACTS, Some(good)), (&named, Some(b"two"))])),
        format!(
            "the bundle's object {named} holds bytes whose SHA-256 is {}",
            sha(b"two")
        )
    );
    let truncated = {
        let mut whole = tar_of(&[(FACTS, Some(good)), (&named, Some(b"one"))]);
        whole.truncate(1024 + 100);
        whole
    };
    assert!(refusal(truncated).starts_with("the bundle: "));
}

/// Asserts reading stops at the first item the caller refuses, with the caller's reason, and
/// reads no further. Catches: a publish that goes on writing objects after one failed.
#[test]
fn reading_stops_where_the_caller_refuses() {
    let mut writer = Writer::new(Vec::new(), &facts()).expect("writer");
    writer.object(b"one").expect("one");
    writer.object(b"two").expect("two");
    let bundle = writer.finish().expect("finish");
    let mut seen = 0;
    let result = read(bundle.as_slice(), |item| {
        seen += 1;
        match item {
            Item::Object { .. } => Err("no".to_owned()),
            Item::Facts(_) => Ok(()),
        }
    });
    assert!(matches!(result, Err(ReadError::Stopped(ref why)) if why == "no"));
    assert_eq!(result.err().map(|e| e.to_string()).as_deref(), Some("no"));
    assert_eq!(seen, 2);
}

/// A writer whose output always fails.
struct Broken;

impl io::Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("disk full"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Asserts a failed output is the writer's error, named. Catches: a release step that would
/// list a bundle it could not write.
#[test]
fn a_failed_output_is_an_error() {
    let refused = Writer::new(Broken, &facts()).err().expect("refused");
    assert_eq!(refused.to_string(), "the bundle: disk full");
    assert!(matches!(refused, BundleError::Io(_)));
}
