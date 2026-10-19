//! The form a description travels in from the fetch child to the root parent, and what the
//! parent holds a description to before it names a file to fetch. (The child reading a real
//! server is in `tests/fetch_e2e.rs`; the fetcher that uses the result is in
//! `tests/fetcher_real.rs`.)

use super::*;
use crate::testkit::{A, B};

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const BUNDLE: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const MAX: u64 = 1 << 20;

fn commit() -> CommitId {
    CommitId::new(A).expect("commit")
}

/// The tarball an honest source lists for `name` on `platform` at commit `A`.
fn entry(name: &str, platform: &str) -> Described {
    Described {
        name: name.to_owned(),
        platform: platform.to_owned(),
        tarball: format!("{name}-{A}-{platform}.tar.gz"),
        sha256: SHA.to_owned(),
        size: 1000,
        bundle_sha256: BUNDLE.to_owned(),
        bundle_size: 500,
    }
}

fn description(components: Vec<Described>) -> Description {
    Description {
        commit: A.to_owned(),
        components,
    }
}

fn fault_text(got: Result<Vec<Described>, DescribeFault>) -> String {
    match got {
        Err(DescribeFault::Unusable(why)) => why,
        other => panic!("not refused as unusable: {other:?}"),
    }
}

/// The form is these lines and no others: a person reading the child's output, and the
/// parent reading it, agree on one spelling.
///
/// Catches a field moved, a word renamed or the schema dropped from the head line.
#[test]
fn the_form_is_the_documented_lines() {
    let said = Answer::Described(description(vec![entry("kbf", "linux-x86_64")]));
    assert_eq!(
        said.render(),
        format!(
            "description 1 {A}\ncomponent kbf linux-x86_64 kbf-{A}-linux-x86_64.tar.gz {SHA} 1000 \
             {BUNDLE} 500\n"
        )
    );
    assert_eq!(
        Answer::Refused("NotFound: no tarball".to_owned()).render(),
        "refused NotFound: no tarball\n"
    );
}

/// What the child writes, the parent reads back whole, for none, one and several entries, and a
/// size of zero (a count, though a release is never one).
///
/// Catches a field read into another's place, a count that does not survive, and an entry
/// lost or added.
#[test]
fn a_description_survives_the_form() {
    let mut zero = entry("kbf-daemon", "darwin-arm64");
    zero.size = 0;
    for components in [
        Vec::new(),
        vec![entry("kbf", "linux-x86_64")],
        vec![
            entry("kbf", "linux-x86_64"),
            entry("kbf", "linux-arm64"),
            zero,
        ],
    ] {
        let said = Answer::Described(description(components));
        assert_eq!(Answer::parse(said.render().as_bytes()), Ok(said));
    }
}

/// A source's words are shown to a person and written to a log: one line, printable ASCII, at
/// most 300 characters, whatever the source put in them; and the parent cleans them again, since
/// the child's output is only the child's word.
///
/// Catches a newline in a refusal making a second line (or a forged component line), a control
/// character reaching a log, and an unbounded message.
#[test]
fn a_refusal_is_one_plain_line() {
    let rude = format!("a\nb\u{1b}[31m\u{e9}{}", "x".repeat(1000));
    let line = Answer::Refused(rude.clone()).render();
    assert_eq!(line.matches('\n').count(), 1, "{line:?}");
    assert!(line.ends_with('\n') && line.starts_with("refused a?b?[31m?x"));
    assert!(line.bytes().all(|b| b == b'\n' || (b' '..=b'~').contains(&b)));
    assert_eq!(line.len(), "refused ".len() + 300 + 1);
    // A child that writes the raw text is read the same way.
    let raw = format!("refused {}\n", "y".repeat(1000));
    let Ok(Answer::Refused(text)) = Answer::parse(raw.as_bytes()) else {
        panic!("a refusal");
    };
    assert_eq!(text, "y".repeat(300));
    let Ok(Answer::Refused(text)) = Answer::parse(b"refused tab\there\n") else {
        panic!("a refusal");
    };
    assert_eq!(text, "tab?here");
}

/// A description with a field that is not one word (a space would make two fields, a newline a
/// second line) cannot be written in the form; it is written as the refusal that says so, and
/// that reads back as a refusal.
///
/// Catches a hostile name or digest from the source being relayed as extra fields or lines.
#[test]
fn a_field_that_is_not_one_word_is_written_as_a_refusal() {
    let spoilers: [fn(&mut Described); 5] = [
        |d| d.name = "kbf evil".to_owned(),
        |d| d.platform = "linux\nx".to_owned(),
        |d| d.tarball = String::new(),
        |d| d.sha256 = "caf\u{e9}".to_owned(),
        |d| d.bundle_sha256 = "a\tb".to_owned(),
    ];
    for spoil in spoilers {
        let mut bad = entry("kbf", "linux-x86_64");
        spoil(&mut bad);
        let line = Answer::Described(description(vec![bad])).render();
        assert!(line.starts_with("refused the description is not usable"), "{line}");
        assert_eq!(line.matches('\n').count(), 1);
        assert!(matches!(
            Answer::parse(line.as_bytes()),
            Ok(Answer::Refused(_))
        ));
    }
    let mut head = description(Vec::new());
    head.commit = "a b".to_owned();
    assert!(Answer::Described(head).render().starts_with("refused "));
}

/// The parent reads exactly the form and refuses whatever else the child wrote, whole.
///
/// Catches a lenient reader: a line it skips, a field it ignores, a count in another spelling
/// (`+5`, `05`, `1e3`) or too big, a schema other than 1, a text that is not UTF-8 or has no
/// final newline.
#[test]
fn the_parent_refuses_anything_but_the_form() {
    let head = format!("description 1 {A}");
    let comp = format!(
        "component kbf linux-x86_64 kbf-{A}-linux-x86_64.tar.gz {SHA} 1000 {BUNDLE} 500"
    );
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (Vec::new(), "newline"),
        (head.clone().into_bytes(), "newline"),
        (format!("{head}\n{comp}").into_bytes(), "newline"),
        (vec![0xff, b'\n'], "UTF-8"),
        (b"\n".to_vec(), "first line"),
        (format!("description 2 {A}\n").into_bytes(), "first line"),
        (format!("description 01 {A}\n").into_bytes(), "not a count"),
        (format!("description x {A}\n").into_bytes(), "not a count"),
        (format!("description 1 {A} more\n").into_bytes(), "first line"),
        (b"description 1\n".to_vec(), "first line"),
        (format!("descriptions 1 {A}\n").into_bytes(), "first line"),
        (format!("description  1 {A}\n").into_bytes(), "first line"),
        (format!("{head}\n\n").into_bytes(), "seven fields"),
        (format!("{head}\n{comp} extra\n").into_bytes(), "seven fields"),
        (
            format!("{head}\n{}\n", comp.replace("component ", "")).into_bytes(),
            "seven fields",
        ),
        (
            format!("{head}\n{}\n", comp.replacen("component", "components", 1)).into_bytes(),
            "not a component",
        ),
        (format!("{head}\n{}\n", comp.replace(" 1000 ", " +1000 ")).into_bytes(), "not a count"),
        (format!("{head}\n{}\n", comp.replace(" 1000 ", " 01000 ")).into_bytes(), "not a count"),
        (format!("{head}\n{}\n", comp.replace(" 1000 ", " 1e3 ")).into_bytes(), "not a count"),
        (format!("{head}\n{}\n", comp.replace(" 500", " -5")).into_bytes(), "not a count"),
        (
            format!("{head}\n{}\n", comp.replace(" 500", " 99999999999999999999999")).into_bytes(),
            "not a count",
        ),
        (b"refused why\nmore\n".to_vec(), "one line"),
    ];
    for (bytes, said) in cases {
        match Answer::parse(&bytes) {
            Err(DescribeFault::Unusable(why)) => {
                assert!(why.contains(said), "{:?}: {why}", String::from_utf8_lossy(&bytes));
            }
            other => panic!("{:?}: {other:?}", String::from_utf8_lossy(&bytes)),
        }
    }
}

/// The `url` source's file: schema 1, with keys this reader does not know ignored, is a
/// description; a file of another schema is refused for that even when its shape is different
/// (so the refusal says why); a file that is not JSON, has no schema, or is of schema 1 but
/// does not fit, is refused with what was wrong.
///
/// Catches a reader that fails on an added key (the documented way to evolve the file), one that
/// reads a schema it does not know, and a refusal that blames the shape of a newer file.
#[test]
fn the_urls_json_is_read_by_schema() {
    let json = |schema: &str, extra: &str, components: &str| {
        format!(
            "{{\"schema\":{schema},\"commit\":\"{A}\"{extra},\"components\":{components}}}"
        )
    };
    let one = format!(
        "[{{\"name\":\"kbf\",\"platform\":\"linux-x86_64\",\"tarball\":\"kbf-{A}-linux-x86_64.tar.gz\",\
         \"sha256\":\"{SHA}\",\"size\":1000,\"bundle_sha256\":\"{BUNDLE}\",\"bundle_size\":500,\
         \"signed_by\":\"someone\"}}]"
    );
    let good = json("1", ",\"generated\":\"later\"", &one);
    assert_eq!(
        Answer::from_json(good.as_bytes()),
        Answer::Described(description(vec![entry("kbf", "linux-x86_64")]))
    );
    let refused = |file: &str| match Answer::from_json(file.as_bytes()) {
        Answer::Refused(why) => why,
        other => panic!("{file}: {other:?}"),
    };
    assert!(refused(&json("2", "", &one)).contains("schema 2"));
    assert!(refused(&json("2", "", "\"a different shape\"")).contains("schema 2"));
    assert!(refused("not json").contains("no schema"));
    assert!(refused("{\"commit\":\"x\"}").contains("no schema"));
    assert!(refused(&json("1", "", "\"not a list\"")).contains("not a description"));
    let no_size = one.replace("\"size\":1000,", "");
    assert!(refused(&json("1", "", &no_size)).contains("not a description"));
}

/// The frontend's protobuf answer keeps every field in its place.
///
/// Catches two fields of the message swapped when it becomes a description.
#[test]
fn the_frontends_answer_keeps_each_field() {
    use kbf_proto::update::{ReleaseComponent, ReleaseDescription};
    let said = ReleaseDescription {
        commit: A.to_owned(),
        components: vec![ReleaseComponent {
            name: "kbf".to_owned(),
            platform: "linux-x86_64".to_owned(),
            tarball: format!("kbf-{A}-linux-x86_64.tar.gz"),
            sha256: SHA.to_owned(),
            size: 1000,
            bundle_sha256: BUNDLE.to_owned(),
            bundle_size: 500,
        }],
    };
    assert_eq!(
        Answer::from_proto(said),
        Answer::Described(description(vec![entry("kbf", "linux-x86_64")]))
    );
}

/// A good description of the host's platform gives back its components, in the order the
/// platform lists them, whatever else the description lists.
///
/// Catches the host's component not being chosen, and other platforms' entries leaking in.
#[test]
fn the_hosts_components_are_chosen() {
    let all = description(vec![
        entry("kbf", "linux-arm64"),
        entry("kbf", "linux-x86_64"),
        entry("kbf-daemon", "darwin-arm64"),
    ]);
    assert_eq!(
        all.select(&commit(), ReleasePlatform::LinuxX86_64, MAX),
        Ok(vec![entry("kbf", "linux-x86_64")])
    );
    assert_eq!(
        all.select(&commit(), ReleasePlatform::DarwinArm64, MAX),
        Ok(vec![entry("kbf-daemon", "darwin-arm64")])
    );
}

/// Entries of other platforms are not read at all: a source that lists a platform this
/// installer has never heard of, or a broken entry for another platform, still describes this
/// host's release (the `url` file lists every platform, and a newer release may add one).
///
/// Catches the platform filter missing (the strange entries would then be held to this host's
/// rules and refuse the whole description).
#[test]
fn entries_of_other_platforms_are_not_held_to_the_hosts_rules() {
    let mut broken = entry("kbf", "linux-arm64");
    broken.sha256 = "short".to_owned();
    broken.size = 0;
    let all = description(vec![
        entry("kbf", "linux-x86_64"),
        broken,
        entry("kbf", "plan9-mips"),
        entry("surprise", "darwin-x86_64"),
    ]);
    assert_eq!(
        all.select(&commit(), ReleasePlatform::LinuxX86_64, MAX),
        Ok(vec![entry("kbf", "linux-x86_64")])
    );
}

/// A description of another commit than the order's is refused, whoever is asking for it.
///
/// Catches a mirror's file for one release being taken for another (a stale or swapped file).
#[test]
fn a_description_of_another_commit_is_refused() {
    let mut other = description(vec![entry("kbf", "linux-x86_64")]);
    other.commit = B.to_owned();
    let why = fault_text(other.select(&commit(), ReleasePlatform::LinuxX86_64, MAX));
    assert!(why.contains(B) && why.contains(A), "{why}");
}

/// What a host's platform needs must all be there, once, and nothing else of that platform.
///
/// Catches an incomplete platform installed (the pipeline would have a release with a binary
/// missing), a component twice (two files under one name), and a component the platform has no
/// use for.
#[test]
fn a_platform_must_be_complete_and_exact() {
    let host = ReleasePlatform::LinuxX86_64;
    let none = description(vec![entry("kbf", "linux-arm64")]);
    assert_eq!(
        fault_text(none.select(&commit(), host, MAX)),
        "it has no kbf for linux-x86_64"
    );
    let twice = description(vec![entry("kbf", "linux-x86_64"), entry("kbf", "linux-x86_64")]);
    assert_eq!(
        fault_text(twice.select(&commit(), host, MAX)),
        "it has kbf for linux-x86_64 twice"
    );
    let extra = description(vec![
        entry("kbf", "linux-x86_64"),
        entry("kbf-daemon", "linux-x86_64"),
    ]);
    assert!(
        fault_text(extra.select(&commit(), host, MAX))
            .contains("\"kbf-daemon\" is not a component of linux-x86_64")
    );
    let empty = description(Vec::new());
    assert_eq!(
        fault_text(empty.select(&commit(), host, MAX)),
        "it has no kbf for linux-x86_64"
    );
    let mac = description(vec![entry("kbf", "darwin-arm64")]);
    assert!(
        fault_text(mac.select(&commit(), ReleasePlatform::DarwinArm64, MAX))
            .contains("\"kbf\" is not a component of darwin-arm64")
    );
}

/// Each field of the host's entry is held to its shape: the file is the tarball of this name
/// and commit and platform, the digests are 64 lowercase hex digits, a tarball is within the
/// host's cap and a bundle within 256 KiB, neither empty.
///
/// Catches a value from the source used unchecked: a name that is a path, a file name that
/// is another release's, a digest in capitals or too short (which names a second copy, or
/// a path), a size of zero or over a bound (and the boundary itself: exactly the bound is
/// accepted).
#[test]
fn every_field_of_the_hosts_entry_is_held_to_its_shape() {
    let host = ReleasePlatform::LinuxX86_64;
    let spoilt = |change: fn(&mut Described)| {
        let mut bad = entry("kbf", "linux-x86_64");
        change(&mut bad);
        fault_text(description(vec![bad]).select(&commit(), host, MAX))
    };
    assert!(spoilt(|d| d.name = "../kbf".to_owned()).contains("not a component"));
    assert!(spoilt(|d| d.tarball = format!("kbf-{B}-linux-x86_64.tar.gz")).contains("the file of kbf"));
    assert!(spoilt(|d| d.tarball = "../kbf.tar.gz".to_owned()).contains("the file of kbf"));
    assert!(spoilt(|d| d.sha256 = SHA.to_uppercase()).contains("64 lowercase hex"));
    assert!(spoilt(|d| d.sha256 = SHA[1..].to_owned()).contains("64 lowercase hex"));
    assert!(spoilt(|d| d.bundle_sha256 = BUNDLE.to_uppercase()).contains("64 lowercase hex"));
    assert!(spoilt(|d| d.bundle_sha256 = format!("{BUNDLE}0")).contains("64 lowercase hex"));
    assert!(spoilt(|d| d.size = 0).contains("0 bytes; at least 1"));
    assert!(spoilt(|d| d.size = MAX + 1).contains(&format!("at most {MAX}")));
    assert!(spoilt(|d| d.bundle_size = 0).contains("the bundle of kbf is 0 bytes"));
    assert!(spoilt(|d| d.bundle_size = 256 * 1024 + 1).contains("at most 262144"));
    // The bounds themselves are fine.
    let mut edge = entry("kbf", "linux-x86_64");
    (edge.size, edge.bundle_size) = (MAX, 256 * 1024);
    assert_eq!(
        description(vec![edge.clone()]).select(&commit(), host, MAX),
        Ok(vec![edge.clone()])
    );
    edge.size = 1;
    edge.bundle_size = 1;
    assert_eq!(
        description(vec![edge.clone()]).select(&commit(), host, MAX),
        Ok(vec![edge])
    );
}

/// What a refusal says reaches the log as plain text, however the description spelt it.
#[test]
fn a_refusal_names_the_source_and_its_words() {
    assert_eq!(
        DescribeFault::Refused("NotFound: no tarball".to_owned()).to_string(),
        "the release source will not describe it: NotFound: no tarball"
    );
    assert_eq!(
        DescribeFault::unusable("bad\nline\u{7}").to_string(),
        "the description is not usable: bad?line?"
    );
}
