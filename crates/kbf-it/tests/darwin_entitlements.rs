//! The per-binary entitlement allow list (`tools/ci/darwin-entitlements.txt`) and the
//! parts of `cargo xtask sign-darwin` and `darwin-asset` that run on any host
//! (`xtask::entitlements`): the list's lines, the plist it renders for a name, the
//! refusals of a missing or malformed list, and the comparison of a signature's
//! entitlements, as `codesign` prints them, with the list.
//!
//! The signing itself needs `codesign` and runs on the hosted macOS job
//! (`crates/kbf-vmm/tests/signing.rs`); docs/design/macos-vm-guests.md section 7.

use std::path::{Path, PathBuf};

use xtask::entitlements::{ALLOWLIST, AllowList, Defect, Error, compare, render, signed_keys};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

const VZ: &str = "com.apple.security.virtualization";
const GTA: &str = "com.apple.security.get-task-allow";

/// The plist the committed list gives `kbf-vmm`, byte for byte.
const VMM_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.security.virtualization</key>
	<true/>
</dict>
</plist>
"#;

/// What `codesign --display --entitlements - --xml` prints for a binary signed with
/// [`VMM_PLIST`] on the hosted macOS 15 runners: one line, an `https` doctype.
const VMM_SIGNED: &str = r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>com.apple.security.virtualization</key><true/></dict></plist>"#;

fn committed() -> AllowList {
    AllowList::read(&repo_root().join(ALLOWLIST)).expect("the committed allow list")
}

fn keys(names: &[&str]) -> Vec<String> {
    names.iter().map(|&k| k.to_owned()).collect()
}

/// `codesign`'s one-line form of a dictionary of `body`.
fn signed(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict>{body}</dict></plist>"#
    )
}

fn entry(key: &str) -> String {
    format!("<key>{key}</key><true/>")
}

/// Catches an entitlement given to any binary, or kbf-vmm's set changed, without this
/// test changing in the same review: the list's lines, comments and blanks aside, are
/// exactly kbf-vmm with `com.apple.security.virtualization`, and the list parses to
/// that alone.
#[test]
fn the_committed_list_gives_kbf_vmm_the_virtualization_entitlement_only() {
    let text = std::fs::read_to_string(repo_root().join(ALLOWLIST)).expect(ALLOWLIST);
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines, ["kbf-vmm com.apple.security.virtualization"]);
    assert_eq!(
        committed(),
        AllowList::parse("kbf-vmm com.apple.security.virtualization", Path::new("x"))
            .expect("parse")
    );
    assert_eq!(committed().entitlements("kbf-vmm"), [VZ]);
}

/// Catches a renderer whose plist is not the one the signer applies (wrong key, a value
/// other than true, a malformed document) for the one listed binary.
#[test]
fn kbf_vmm_gets_the_virtualization_plist() {
    assert_eq!(committed().plist("kbf-vmm").as_deref(), Some(VMM_PLIST));
}

/// Catches a lookup that ignores the binary's name, or matches a prefix or a part of
/// it: every other name, the shipped binaries and near-misses of `kbf-vmm` included,
/// gets no plist at all (so `sign-darwin` signs it with no entitlements).
#[test]
fn every_other_binary_gets_none() {
    let list = committed();
    for name in [
        "kbf-daemon",
        "kbf-mac-session",
        "kbf-guest",
        "kbf-server",
        "kbf-vm",
        "kbf-vmm2",
        "vmm",
        "KBF-VMM",
    ] {
        assert_eq!(list.plist(name), None, "{name}");
        assert!(list.entitlements(name).is_empty(), "{name}");
    }
}

/// Catches a list that fails open: with the file missing, or a directory in its place,
/// reading it is an error, rather than "no list" read as "no entitlements" (which the
/// check would accept for an unentitled binary).
#[test]
fn a_missing_or_unreadable_list_is_refused() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("darwin-entitlements");
    std::fs::create_dir_all(&dir).expect("create the test directory");
    let missing = dir.join("no-such-list.txt");
    let err = AllowList::read(&missing).expect_err("a missing list");
    assert_eq!(err, Error::Missing(missing.clone()));
    assert_eq!(
        err.to_string(),
        format!(
            "the entitlement allow list {} is missing",
            missing.display()
        )
    );
    assert!(
        matches!(AllowList::read(&dir), Err(Error::Unreadable { ref path, .. }) if *path == dir),
        "a directory as the list"
    );
}

/// Catches a parser that accepts a list two readers could read two ways: a binary
/// listed twice (which line wins?), an entitlement repeated, a binary with no
/// entitlement, and a name or entitlement holding characters the plist would have to
/// escape. Each is refused with its line number, wherever the bad line is, so no name
/// (the unlisted ones included) can be looked up in such a list.
#[test]
fn a_malformed_list_is_refused() {
    let owned = |s: &str| s.to_owned();
    let cases = [
        (
            "kbf-vmm a.b\nkbf-vmm c.d\n",
            4,
            Defect::ListedTwice(owned("kbf-vmm")),
            "kbf-vmm is listed twice",
        ),
        (
            "kbf-vmm a.b c.d a.b\n",
            3,
            Defect::Repeats(owned("kbf-vmm")),
            "kbf-vmm repeats an entitlement",
        ),
        (
            "kbf-vmm\n",
            3,
            Defect::NoEntitlement(owned("kbf-vmm")),
            "kbf-vmm lists no entitlement (leave a binary that gets none out)",
        ),
        (
            "kbf-vmm # a.b\n",
            3,
            Defect::NoEntitlement(owned("kbf-vmm")),
            "kbf-vmm lists no entitlement (leave a binary that gets none out)",
        ),
        (
            "kbf-vmm a<b\n",
            3,
            Defect::NotAnEntitlement(owned("a<b")),
            "'a<b' is not an entitlement name",
        ),
        (
            "kbf-vmm a.b .c\n",
            3,
            Defect::NotAnEntitlement(owned(".c")),
            "'.c' is not an entitlement name",
        ),
        (
            "-kbf-vmm a.b\n",
            3,
            Defect::NotABinaryName(owned("-kbf-vmm")),
            "'-kbf-vmm' is not a binary name",
        ),
        (
            "kbf/vmm a.b\n",
            3,
            Defect::NotABinaryName(owned("kbf/vmm")),
            "'kbf/vmm' is not a binary name",
        ),
    ];
    let path = Path::new("list.txt");
    for (text, line, defect, message) in cases {
        let text = format!("# a comment\n\n{text}");
        let err = AllowList::parse(&text, path).expect_err(&text);
        assert_eq!(
            err,
            Error::Malformed {
                path: path.to_owned(),
                line,
                defect
            },
            "{text:?}"
        );
        assert_eq!(err.to_string(), format!("list.txt:{line}: {message}"));
    }
}

/// Catches a renderer that keeps the list's order or drops all but the first key, and a
/// lookup that takes another line's set: the keys of the asked line come out sorted, a
/// comment after them is ignored, a last line without a newline is read, and the other
/// binary gets its own set. The word rule takes `.`, `_` and `-` after the first
/// character.
#[test]
fn a_line_renders_its_own_keys_sorted() {
    let list = AllowList::parse(
        "kbf-a z.z a_a-1 # trailing comment\n\tkbf_b m.m",
        Path::new("x"),
    )
    .expect("parse");
    assert_eq!(list.entitlements("kbf-a"), ["a_a-1", "z.z"]);
    assert_eq!(list.entitlements("kbf_b"), ["m.m"]);
    assert_eq!(
        render(&keys(&["a.a", "z.z"])),
        VMM_PLIST.replace(
            "\t<key>com.apple.security.virtualization</key>\n\t<true/>\n",
            "\t<key>a.a</key>\n\t<true/>\n\t<key>z.z</key>\n\t<true/>\n"
        )
    );
}

/// The signature cases of the hosted macOS test, on the text `codesign` prints: each
/// row is a binary name, what its signature carries, and the refusal it must get (or
/// none). Catches a check that lets any entitlement through for an unlisted binary
/// (a lookup that ignores the name finds kbf-vmm's set for kbf-daemon), a subset
/// compare that accepts one more key (get-task-allow among them), a check that treats
/// "no entitlements" as always fine, and a compare of the count only.
#[test]
fn a_signature_is_compared_with_the_list() {
    let list = committed();
    let vz = entry(VZ);
    let gta = entry(GTA);
    let refused_other = |signed: &[&str]| {
        Err(Error::Other {
            name: "kbf-vmm".to_owned(),
            want: keys(&[VZ]),
            signed: keys(signed),
        })
    };
    let cases: [(&str, String, Result<String, Error>); 7] = [
        (
            "kbf-vmm",
            VMM_SIGNED.to_owned(),
            Ok(format!(
                "entitlements of kbf-vmm: exactly {VZ}, as the allow list says"
            )),
        ),
        (
            "kbf-daemon",
            String::new(),
            Ok("entitlements of kbf-daemon: none, as the allow list says".to_owned()),
        ),
        (
            "kbf-daemon",
            signed(&vz),
            Err(Error::UnlistedCarries {
                name: "kbf-daemon".to_owned(),
                signed: signed(&vz),
            }),
        ),
        (
            "kbf-mac-session",
            signed(&gta),
            Err(Error::UnlistedCarries {
                name: "kbf-mac-session".to_owned(),
                signed: signed(&gta),
            }),
        ),
        (
            "kbf-vmm",
            signed(&format!("{vz}{gta}")),
            refused_other(&[GTA, VZ]),
        ),
        (
            "kbf-vmm",
            "\n".to_owned(),
            Err(Error::ListedCarriesNone {
                name: "kbf-vmm".to_owned(),
                want: keys(&[VZ]),
            }),
        ),
        ("kbf-vmm", signed(&gta), refused_other(&[GTA])),
    ];
    for (name, text, want) in cases {
        assert_eq!(compare(&list, name, &text), want, "{name}: {text:?}");
    }
    let messages = [
        (
            compare(&list, "kbf-daemon", &signed(&vz)),
            "gives kbf-daemon no entitlements",
        ),
        (
            compare(&list, "kbf-vmm", &signed(&gta)),
            "other entitlements than the allow list gives it",
        ),
        (compare(&list, "kbf-vmm", ""), "its signature carries none"),
    ];
    for (got, fragment) in messages {
        let message = got.expect_err(fragment).to_string();
        assert!(message.contains(fragment), "{message:?} lacks {fragment:?}");
    }
}

/// Catches a check that takes a value other than `<true/>` for true, or reads only the
/// keys: such a signature is refused for a listed binary, and so is anything the
/// reader does not understand.
#[test]
fn a_value_other_than_true_is_refused() {
    let list = committed();
    let not_true = signed(&format!("<key>{VZ}</key><false/>"));
    assert_eq!(
        compare(&list, "kbf-vmm", &not_true),
        Err(Error::NotTrueValues {
            name: "kbf-vmm".to_owned(),
            signed: not_true.clone(),
        })
    );
}

/// Catches a reader of the signed plist that accepts what it does not understand: each
/// row breaks one part of the one shape it reads, and must give no keys. The controls
/// read the one-line form `codesign` prints, the multi-line form the signer writes, and
/// an empty dictionary.
#[test]
fn the_signed_plist_reader_accepts_one_shape_only() {
    assert_eq!(signed_keys(VMM_SIGNED), Some(keys(&[VZ])));
    assert_eq!(signed_keys(VMM_PLIST), Some(keys(&[VZ])));
    assert_eq!(
        signed_keys(&signed(&format!("{} {}", entry("z.z"), entry("a.a")))),
        Some(keys(&["a.a", "z.z"]))
    );
    assert_eq!(
        signed_keys(r#"<plist version="1.0"><dict/></plist>"#),
        Some(vec![])
    );
    assert_eq!(
        signed_keys(r#"<plist version="1.0"><dict></dict></plist>"#),
        Some(vec![])
    );
    let vz = entry(VZ);
    let broken = [
        // An XML declaration or doctype that never ends.
        "<?xml version=\"1.0\"".to_owned(),
        "<!DOCTYPE plist".to_owned(),
        // Not a plist, or another version.
        format!("<dict>{vz}</dict>"),
        format!("<plist version=\"0.9\"><dict>{vz}</dict></plist>"),
        // Not a dictionary.
        "<plist version=\"1.0\"><array/></plist>".to_owned(),
        // A dictionary that never ends, or holds something other than a key.
        format!("<plist version=\"1.0\"><dict>{vz}"),
        "<plist version=\"1.0\"><dict><string>x</string></dict></plist>".to_owned(),
        // A key that never ends, or one with characters the list never holds.
        "<plist version=\"1.0\"><dict><key>a.b".to_owned(),
        signed("<key>a&amp;b</key><true/>"),
        signed("<key></key><true/>"),
        // A value other than true.
        signed(&format!("<key>{VZ}</key><false/>")),
        signed(&format!("<key>{VZ}</key><string>yes</string>")),
        // A key twice.
        signed(&format!("{vz}{vz}")),
        // The plist not closed, or something after it.
        format!("<plist version=\"1.0\"><dict>{vz}</dict>"),
        format!("{VMM_SIGNED}<plist/>"),
    ];
    for xml in broken {
        assert_eq!(signed_keys(&xml), None, "{xml:?}");
    }
}
