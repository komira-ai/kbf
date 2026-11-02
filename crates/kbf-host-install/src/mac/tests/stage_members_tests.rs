//! The members of a release tarball on the Mac: the binary and, when the release carries
//! one, its updater declaration `updater.d/kbf.toml` are staged; a member no one asked for
//! is passed over; an unsafe member refuses the release by name.

use std::fs;
use std::path::Path;

use tar::EntryType::{self, Block, Char, Continuous, Directory, Fifo, Link, Regular, Symlink};

use super::fx::{Fx, NEW, NEW_VERSION, Sim, gzip, mkdir, mode, raw_tarball, sha256, tarball};
use crate::mac::channel::{MacChannel, Source};
use crate::mac::release::Origin;
use crate::mac::stage::{Staged, stage};
use crate::mac::unpack::{DECLARATION_CAP, FRAMING, declaration_member};
use crate::outcome::{FailureCode, Outcome};
use crate::request::Version;

const DECLARATION: &str = "updater.d/kbf.toml";
const TOML: &[u8] = b"name = \"kbf\"\n";

fn run(fx: &Fx, sim: &Sim) -> Result<Staged, Outcome> {
    mkdir(&fx.host.work, 0o711);
    let channel = MacChannel {
        source: Source::Dir(fx.channel_dir.clone()),
        sums: Some(fx.sums.clone()),
        source_ref: "refs/heads/main".into(),
        origin: Origin::kbf(),
    };
    stage(&fx.host, sim, &channel, &Version::new(NEW_VERSION).unwrap())
}

/// The detail of a stage that failed at the unpack; the stage is gone, and nothing that runs
/// after the unpack (`xattr`, `codesign`, the new binary's check) was run.
fn verify_failure(fx: &Fx, sim: &Sim) -> String {
    let before = sim.calls.borrow().len();
    let result = run(fx, sim);
    let after: Vec<String> = sim.calls.borrow()[before..]
        .iter()
        .map(|c| c.program.clone())
        .filter(|p| p.ends_with("/kbf") || p.ends_with("xattr") || p.ends_with("codesign"))
        .collect();
    assert!(after.is_empty(), "run after a refused unpack: {after:?}");
    match result {
        Err(Outcome::Failed(f)) => {
            assert_eq!(f.code, FailureCode::VerifyFailed, "{}", f.detail);
            assert!(!fx.host.stage().exists(), "a failed check leaves no stage");
            f.detail
        }
        other => panic!("not a failure: {other:?}"),
    }
}

/// Everything under `dir`, every level, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for item in fs::read_dir(dir).unwrap() {
        let path = item.unwrap().path();
        let name = path.strip_prefix(dir).unwrap().display().to_string();
        if path.is_dir() {
            found.extend(listing(&path).into_iter().map(|n| format!("{name}/{n}")));
        }
        found.push(name);
    }
    found.sort();
    found
}

/// What the stage holds beside the files it fetched and `gh`'s home.
fn unpacked(fx: &Fx) -> Vec<String> {
    listing(&fx.host.stage())
        .into_iter()
        .filter(|n| {
            !n.ends_with(".tar.gz") && !n.ends_with(".sigstore.json") && !n.starts_with("gh-home")
        })
        .collect()
}

/// Catches the declaration a release carries beside its binary not reaching the stage, in
/// either order in the tarball: the binary (0755) and `updater.d/kbf.toml` (0644, in a
/// 0755 `updater.d/`) are both there with their bytes, and the answer is a release's as
/// before.
#[test]
fn a_release_with_a_declaration_stages_the_binary_and_the_declaration() {
    let fx = Fx::new("mac-members-two");
    let bin = fx.new_binary.clone();
    for (why, members) in [
        (
            "binary-first",
            [("kbf", Regular, &bin[..]), (DECLARATION, Regular, TOML)],
        ),
        (
            "declaration-first",
            [(DECLARATION, Regular, TOML), ("kbf", Regular, &bin[..])],
        ),
    ] {
        fx.offer(&tarball(&members));
        let staged = run(&fx, &fx.sim()).expect(why);
        assert_eq!(
            staged,
            Staged {
                commit: NEW.into(),
                binary_sha256: sha256(&bin)
            },
            "{why}"
        );
        let stage = fx.host.stage();
        assert_eq!(unpacked(&fx), ["kbf", "updater.d", DECLARATION], "{why}");
        assert_eq!(fs::read(stage.join("kbf")).unwrap(), bin, "{why}");
        assert_eq!(fs::read(stage.join(DECLARATION)).unwrap(), TOML, "{why}");
        assert_eq!(mode(&stage.join("kbf")), 0o755, "{why}");
        assert_eq!(mode(&stage.join("updater.d")), 0o755, "{why}");
        assert_eq!(mode(&stage.join(DECLARATION)), 0o644, "{why}");
        fs::remove_dir_all(&stage).unwrap();
    }
}

/// Catches a release of the binary alone (every release today) staging differently: the
/// binary and nothing else is unpacked, no `updater.d/` is made.
#[test]
fn a_release_of_the_binary_alone_stages_as_before() {
    let fx = Fx::new("mac-members-one");
    let staged = run(&fx, &fx.sim()).unwrap();
    assert_eq!(staged.binary_sha256, sha256(&fx.new_binary));
    assert_eq!(unpacked(&fx), ["kbf"]);
}

/// Catches a member no one asked for being refused or written: a README, a directory entry,
/// a file under it and another service's declaration are passed over, and only the binary
/// is staged.
#[test]
fn a_member_no_one_asked_for_is_ignored() {
    let fx = Fx::new("mac-members-extra");
    let bin = fx.new_binary.clone();
    fx.offer(&tarball(&[
        ("README", Regular, b"read me"),
        ("docs/", Directory, b""),
        ("docs/install.md", Regular, b"how"),
        ("kbf", Regular, &bin),
        (
            "updater.d/tierstore.toml",
            Regular,
            b"name = \"tierstore\"\n",
        ),
    ]));
    run(&fx, &fx.sim()).unwrap();
    assert_eq!(unpacked(&fx), ["kbf"]);
}

/// Catches an unsafe member let through because it is or is not one the stage wants: an
/// absolute name, one with a `..` component, an empty one, a symbolic link, a hard link, a
/// device, a FIFO and a member of another kind each refuse the release whether alone, before
/// or after a good binary, and the refusal names the member.
#[test]
fn an_unsafe_member_is_refused_by_name() {
    let fx = Fx::new("mac-members-unsafe");
    let sim = fx.sim();
    let bin = fx.new_binary.clone();
    let leaves =
        |n: &str| format!("tarball: the member {n:?} leaves the directory it is unpacked into");
    let kind = |n: &str, k: &str| format!("tarball: the member {n:?} is a {k}, not a regular file");
    let cases: [(&[u8], EntryType, String); 10] = [
        (
            b"/etc/updater.d/kbf.toml",
            Regular,
            leaves("/etc/updater.d/kbf.toml"),
        ),
        (b"../kbf", Regular, leaves("../kbf")),
        (
            b"updater.d/../../x.toml",
            Regular,
            leaves("updater.d/../../x.toml"),
        ),
        (b"", Regular, leaves("")),
        (b"evil", Symlink, kind("evil", "symbolic link")),
        (
            DECLARATION.as_bytes(),
            Symlink,
            kind(DECLARATION, "symbolic link"),
        ),
        (b"evil", Link, kind("evil", "hard link")),
        (b"null", Char, kind("null", "device")),
        (b"sda", Block, kind("sda", "device")),
        (b"pipe", Fifo, kind("pipe", "FIFO")),
    ];
    let other = (
        &b"odd"[..],
        Continuous,
        kind("odd", "member of another kind"),
    );
    for (name, entry, want) in cases.into_iter().chain([other]) {
        let bad = (name, entry, &b""[..]);
        for (place, members) in [
            ("alone", vec![bad]),
            ("before", vec![bad, (&b"kbf"[..], Regular, &bin[..])]),
            ("after", vec![(&b"kbf"[..], Regular, &bin[..]), bad]),
        ] {
            fx.offer(&raw_tarball(&members));
            assert_eq!(verify_failure(&fx, &sim), want, "{place}");
        }
    }
}

/// Catches a release that lacks the binary being staged, or refused without its name: the
/// declaration alone, another name, a name with a path, `./kbf`, and no entries at all.
#[test]
fn a_missing_binary_is_refused_by_name() {
    let fx = Fx::new("mac-members-missing");
    let sim = fx.sim();
    for members in [
        vec![(DECLARATION, Regular, TOML)],
        vec![("kbf2", Regular, &fx.new_binary[..])],
        vec![("bin/kbf", Regular, &fx.new_binary[..])],
        vec![],
    ] {
        fx.offer(&tarball(&members));
        assert_eq!(
            verify_failure(&fx, &sim),
            "tarball: holds no member named kbf"
        );
    }
    fx.offer(&raw_tarball(&[(b"./kbf", Regular, &fx.new_binary)]));
    assert_eq!(
        verify_failure(&fx, &sim),
        "tarball: holds no member named kbf"
    );
}

/// Catches a name that only looks like a wanted one being taken: `./updater.d/kbf.toml` and
/// `updater.d//kbf.toml` beside the binary are passed over, the binary is staged, and no
/// declaration is.
#[test]
fn a_name_that_only_looks_like_the_declaration_is_passed_over() {
    let fx = Fx::new("mac-members-alias");
    let bin = fx.new_binary.clone();
    fx.offer(&raw_tarball(&[
        (b"kbf", Regular, &bin),
        (b"./updater.d/kbf.toml", Regular, TOML),
        (b"updater.d//kbf.toml", Regular, TOML),
    ]));
    run(&fx, &fx.sim()).unwrap();
    assert_eq!(unpacked(&fx), ["kbf"]);
}

/// Catches a tarball whose stream does not end where its tar does being staged: a header or
/// a member cut short, a damaged gzip checksum or length, bytes after the gzip member, a
/// second gzip member, a non-zero byte after the tar's end, and zero padding past the bound.
/// Each refuses the release, with the stage gone and nothing run.
#[test]
fn the_stream_ends_where_the_tar_does() {
    let fx = Fx::new("mac-members-stream");
    let sim = fx.sim();
    let tar = raw_tar_of(&fx.new_binary);
    let clean = gzip(&tar);
    let cut = |at: usize| gzip(&tar[..at]);
    let mut crc = clean.clone();
    let n = crc.len();
    crc[n - 8] ^= 0xff;
    let mut length = clean.clone();
    length[n - 2] ^= 0xff;
    let mut trailing = clean.clone();
    trailing.extend_from_slice(b"\0\0\0\0");
    let mut second = clean.clone();
    second.extend_from_slice(&clean);
    let mut tail = tar.clone();
    tail.extend_from_slice(b"hidden");
    let bound = fx.host.tuning.binary_max + DECLARATION_CAP + FRAMING;
    let mut padded = tar.clone();
    padded.resize(usize::try_from(bound).unwrap() + 1, 0);
    let follows = "tarball: data follows the end of the archive".to_owned();
    for (why, bytes, want) in [
        ("cut-header", cut(300), None),
        ("cut-member", cut(700), None),
        ("crc", crc, None),
        ("length", length, None),
        ("trailing", trailing, Some(follows.clone())),
        ("second-member", second, Some(follows.clone())),
        ("tail", gzip(&tail), Some(follows.clone())),
        (
            "padding",
            gzip(&padded),
            Some(format!(
                "tarball: it decompresses to more than {bound} bytes"
            )),
        ),
    ] {
        fx.offer(&bytes);
        let detail = verify_failure(&fx, &sim);
        match want {
            Some(want) => assert_eq!(detail, want, "{why}"),
            None => assert!(detail.starts_with("tarball: "), "{why}: {detail}"),
        }
    }
    // Padded with zeros up to the bound exactly, it is a release.
    let mut at_bound = tar;
    at_bound.resize(usize::try_from(bound).unwrap(), 0);
    fx.offer(&gzip(&at_bound));
    run(&fx, &sim).unwrap();
}

/// The uncompressed tar of the one member `kbf`.
fn raw_tar_of(binary: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(Regular);
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder.append_data(&mut header, "kbf", binary).unwrap();
    builder.into_inner().unwrap()
}

/// Catches a wanted member that is not one plain file within its cap being staged: the
/// declaration as a directory, either member twice, or the declaration over its cap.
#[test]
fn a_wanted_member_must_be_one_regular_file_within_its_cap() {
    let fx = Fx::new("mac-members-wanted");
    let sim = fx.sim();
    let bin = fx.new_binary.clone();
    let big = vec![b'#'; usize::try_from(DECLARATION_CAP).unwrap() + 1];
    for (members, want) in [
        (
            vec![
                ("kbf", Regular, &bin[..]),
                (DECLARATION, Directory, &b""[..]),
            ],
            format!("tarball: the member {DECLARATION:?} is a directory, not a regular file"),
        ),
        (
            vec![
                (DECLARATION, Regular, TOML),
                ("kbf", Regular, &bin[..]),
                (DECLARATION, Regular, TOML),
            ],
            format!("tarball: the member {DECLARATION:?} is in it twice"),
        ),
        (
            vec![("kbf", Regular, &bin[..]), ("kbf", Regular, &bin[..])],
            "tarball: the member \"kbf\" is in it twice".to_owned(),
        ),
        (
            vec![("kbf", Regular, &bin[..]), (DECLARATION, Regular, &big[..])],
            format!("tarball: the member {DECLARATION:?} is larger than {DECLARATION_CAP} bytes"),
        ),
    ] {
        fx.offer(&tarball(&members));
        assert_eq!(verify_failure(&fx, &sim), want);
    }
}

/// Catches the Mac and the Linux unpack naming a release's declaration differently, or
/// either naming its directory other than the host's declaration directory
/// (`/etc/updater.d`, where the updater reads declarations).
#[test]
fn the_declaration_member_is_the_same_on_both_platforms_and_named_for_updater_d() {
    use crate::mac::unpack::DECLARATIONS;
    assert_eq!(declaration_member("tierstore"), "updater.d/tierstore.toml");
    assert_eq!(
        Path::new(update_rights::load::DECLARATION_DIR)
            .file_name()
            .and_then(|n| n.to_str()),
        Some(DECLARATIONS)
    );
    #[cfg(target_os = "linux")]
    {
        assert_eq!(kbf_installer::archive::DECLARATIONS, DECLARATIONS);
        assert_eq!(kbf_installer::archive::DECLARATION_CAP, DECLARATION_CAP);
        for name in ["kbf", "kbf-daemon", "tierstore"] {
            assert_eq!(
                kbf_installer::archive::declaration_member(name),
                declaration_member(name)
            );
        }
    }
}
