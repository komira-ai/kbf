//! The per-binary entitlement allow list (`tools/ci/darwin-entitlements.txt`) and the
//! parts of `cargo xtask sign-darwin` and `darwin-asset` that run on any host
//! (`xtask::entitlements`): the list's lines, the plist it renders for a name, the
//! refusals of a missing or malformed list, and the comparison of a signature's
//! entitlements, as `codesign` prints them, with the list.
//!
//! No shipped binary gets an entitlement, so the committed list names none; the parser and the
//! comparison still run against lists of their own here, so a binary that is given one later
//! meets the same checks.

use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;
use xtask::entitlements::{ALLOWLIST, AllowList, Defect, Error, compare, render, signed_keys};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

const VZ: &str = "com.apple.security.virtualization";
/// A list that gives one binary an entitlement, for the cases that need a listed binary.
const SAMPLE_LIST: &str = "kbf-sample com.apple.security.virtualization";
const GTA: &str = "com.apple.security.get-task-allow";

/// The plist [`SAMPLE_LIST`] gives `kbf-sample`, byte for byte.
const SAMPLE_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.security.virtualization</key>
	<true/>
</dict>
</plist>
"#;

/// What `codesign --display --entitlements - --xml` prints for a binary signed with
/// [`SAMPLE_PLIST`] on the hosted macOS 15 runners: one line, an `https` doctype.
const SAMPLE_SIGNED: &str = r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>com.apple.security.virtualization</key><true/></dict></plist>"#;

fn committed() -> AllowList {
    AllowList::read(&repo_root().join(ALLOWLIST)).expect("the committed allow list")
}

fn sample() -> AllowList {
    AllowList::parse(SAMPLE_LIST, Path::new("x")).expect("parse")
}

/// The lines of an allow list's text, its comments and blanks aside.
fn lines_of(text: &str) -> Vec<&str> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .filter(|l| !l.is_empty())
        .collect()
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

/// Catches an entitlement given to any binary without this test changing in the same review:
/// the committed list, its comments and blanks aside, is empty, and parses to a list that gives
/// no name an entitlement. The negative case: the same check on a list with one line sees it.
#[test]
fn the_committed_list_gives_no_binary_an_entitlement() {
    let text = std::fs::read_to_string(repo_root().join(ALLOWLIST)).expect(ALLOWLIST);
    assert_eq!(lines_of(&text), Vec::<&str>::new());
    assert_eq!(
        committed(),
        AllowList::parse("", Path::new("x")).expect("parse")
    );
    assert_eq!(lines_of(SAMPLE_LIST), [SAMPLE_LIST]);
    assert_eq!(sample().entitlements("kbf-sample"), [VZ]);
    assert_ne!(sample(), committed());
}

/// Catches a renderer whose plist is not the one the signer applies (wrong key, a value
/// other than true, a malformed document) for a listed binary.
#[test]
fn a_listed_binary_gets_the_virtualization_plist() {
    assert_eq!(sample().plist("kbf-sample").as_deref(), Some(SAMPLE_PLIST));
}

/// Catches a lookup that ignores the binary's name, or matches a prefix or a part of
/// it: every other name, the shipped binaries and near-misses of `kbf-sample` included,
/// gets no plist at all (so `sign-darwin` signs it with no entitlements), and so does every
/// name in the committed list.
#[test]
fn every_other_binary_gets_none() {
    for name in [
        "kbf",
        "kbf-server",
        "kbf-task",
        "kbf-installer",
        "kbf-sample2",
        "sample",
        "KBF-SAMPLE",
    ] {
        for list in [sample(), committed()] {
            assert_eq!(list.plist(name), None, "{name}");
            assert!(list.entitlements(name).is_empty(), "{name}");
        }
    }
    assert_eq!(committed().plist("kbf-sample"), None);
}

/// Catches a list that fails open: with the file missing, or a directory in its place,
/// reading it is an error, rather than "no list" read as "no entitlements" (which the
/// check would accept for an unentitled binary).
#[test]
fn a_missing_or_unreadable_list_is_refused() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "darwin-entitlements");
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
        matches!(AllowList::read(&dir), Err(Error::Unreadable { ref path, .. }) if path.as_path() == dir.path()),
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
            "kbf-sample a.b\nkbf-sample c.d\n",
            4,
            Defect::ListedTwice(owned("kbf-sample")),
            "kbf-sample is listed twice",
        ),
        (
            "kbf-sample a.b c.d a.b\n",
            3,
            Defect::Repeats(owned("kbf-sample")),
            "kbf-sample repeats an entitlement",
        ),
        (
            "kbf-sample\n",
            3,
            Defect::NoEntitlement(owned("kbf-sample")),
            "kbf-sample lists no entitlement (leave a binary that gets none out)",
        ),
        (
            "kbf-sample # a.b\n",
            3,
            Defect::NoEntitlement(owned("kbf-sample")),
            "kbf-sample lists no entitlement (leave a binary that gets none out)",
        ),
        (
            "kbf-sample a<b\n",
            3,
            Defect::NotAnEntitlement(owned("a<b")),
            "'a<b' is not an entitlement name",
        ),
        (
            "kbf-sample a.b .c\n",
            3,
            Defect::NotAnEntitlement(owned(".c")),
            "'.c' is not an entitlement name",
        ),
        (
            "-kbf-sample a.b\n",
            3,
            Defect::NotABinaryName(owned("-kbf-sample")),
            "'-kbf-sample' is not a binary name",
        ),
        (
            "kbf/sample a.b\n",
            3,
            Defect::NotABinaryName(owned("kbf/sample")),
            "'kbf/sample' is not a binary name",
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
        SAMPLE_PLIST.replace(
            "\t<key>com.apple.security.virtualization</key>\n\t<true/>\n",
            "\t<key>a.a</key>\n\t<true/>\n\t<key>z.z</key>\n\t<true/>\n"
        )
    );
}

/// The signature cases of the hosted macOS test, on the text `codesign` prints: each
/// row is a binary name, what its signature carries, and the refusal it must get (or
/// none). Catches a check that lets any entitlement through for an unlisted binary
/// (a lookup that ignores the name finds kbf-sample's set for kbf), a subset
/// compare that accepts one more key (get-task-allow among them), a check that treats
/// "no entitlements" as always fine, a compare of the count only, and one that takes
/// an empty dictionary in an unlisted binary's signature for no entitlements (the
/// signature must carry none at all).
#[test]
fn a_signature_is_compared_with_the_list() {
    let list = sample();
    let vz = entry(VZ);
    let gta = entry(GTA);
    let refused_other = |signed: &[&str]| {
        Err(Error::Other {
            name: "kbf-sample".to_owned(),
            want: keys(&[VZ]),
            signed: keys(signed),
        })
    };
    let empty_dict = signed("").replace("<dict></dict>", "<dict/>");
    let cases: [(&str, String, Result<String, Error>); 8] = [
        (
            "kbf-sample",
            SAMPLE_SIGNED.to_owned(),
            Ok(format!(
                "entitlements of kbf-sample: exactly {VZ}, as the allow list says"
            )),
        ),
        (
            "kbf",
            String::new(),
            Ok("entitlements of kbf: none, as the allow list says".to_owned()),
        ),
        (
            "kbf",
            signed(&vz),
            Err(Error::UnlistedCarries {
                name: "kbf".to_owned(),
                signed: signed(&vz),
            }),
        ),
        (
            "kbf",
            empty_dict.clone(),
            Err(Error::UnlistedCarries {
                name: "kbf".to_owned(),
                signed: empty_dict,
            }),
        ),
        (
            "kbf-task",
            signed(&gta),
            Err(Error::UnlistedCarries {
                name: "kbf-task".to_owned(),
                signed: signed(&gta),
            }),
        ),
        (
            "kbf-sample",
            signed(&format!("{vz}{gta}")),
            refused_other(&[GTA, VZ]),
        ),
        (
            "kbf-sample",
            "\n".to_owned(),
            Err(Error::ListedCarriesNone {
                name: "kbf-sample".to_owned(),
                want: keys(&[VZ]),
            }),
        ),
        ("kbf-sample", signed(&gta), refused_other(&[GTA])),
    ];
    for (name, text, want) in cases {
        assert_eq!(compare(&list, name, &text), want, "{name}: {text:?}");
    }
    let messages = [
        (
            compare(&list, "kbf", &signed(&vz)),
            "gives kbf no entitlements",
        ),
        (
            compare(&list, "kbf-sample", &signed(&gta)),
            "other entitlements than the allow list gives it",
        ),
        (
            compare(&list, "kbf-sample", ""),
            "its signature carries none",
        ),
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
    let list = sample();
    let not_true = signed(&format!("<key>{VZ}</key><false/>"));
    assert_eq!(
        compare(&list, "kbf-sample", &not_true),
        Err(Error::NotTrueValues {
            name: "kbf-sample".to_owned(),
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
    assert_eq!(signed_keys(SAMPLE_SIGNED), Some(keys(&[VZ])));
    assert_eq!(signed_keys(SAMPLE_PLIST), Some(keys(&[VZ])));
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
        format!("{SAMPLE_SIGNED}<plist/>"),
    ];
    for xml in broken {
        assert_eq!(signed_keys(&xml), None, "{xml:?}");
    }
}

/// Whether a manifest's text names the Virtualization.framework bindings, which a binary
/// reaches only with the virtualization entitlement.
fn names_virtualization(manifest: &str) -> bool {
    manifest.contains("objc2-virtualization")
}

/// Catches Virtualization.framework reaching a crate: no binary is signed with the
/// virtualization entitlement, so one that called the framework would fail at its first call on a
/// Mac. No crate manifest depends on the bindings; the negative case is a manifest that does.
#[test]
fn no_crate_depends_on_the_virtualization_bindings() {
    assert!(names_virtualization(
        "[dependencies]\nobjc2-virtualization.workspace = true\n"
    ));
    assert!(!names_virtualization(
        "[dependencies]\nlibc.workspace = true\n"
    ));
    let crates = repo_root().join("crates");
    let mut users = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates") {
        let dir = entry.expect("entry").path();
        let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
            continue;
        };
        if names_virtualization(&text) {
            users.push(
                dir.file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    assert_eq!(users, Vec::<String>::new());
    let root = std::fs::read_to_string(repo_root().join("Cargo.toml")).expect("root manifest");
    assert!(!names_virtualization(&root));
}
