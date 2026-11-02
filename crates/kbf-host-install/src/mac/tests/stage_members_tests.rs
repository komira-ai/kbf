//! The members of a release tarball on the Mac: the binary and, when the release carries
//! one, its updater declaration `updater.d/kbf.toml` are staged; a member no one asked for
//! is passed over; an unsafe member refuses the release by name.

use std::fs;
use std::path::Path;

use tar::EntryType::{self, Block, Char, Continuous, Directory, Fifo, Link, Regular, Symlink};

use super::fx::{Fx, NEW, NEW_VERSION, Sim, mkdir, mode, raw_tarball, sha256, tarball, write};
use crate::mac::channel::{MacChannel, Source};
use crate::mac::release::Origin;
use crate::mac::stage::{DECLARATION_CAP, Staged, Wanted, declaration_member, stage, unpack};
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

/// The detail of a stage that failed its checks; the stage is gone.
fn verify_failure(fx: &Fx, sim: &Sim) -> String {
    match run(fx, sim) {
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
/// device, a FIFO and a member of another kind each refuse the release whether alone or
/// beside a good binary, and the refusal names the member.
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
            ("after", vec![(&b"kbf"[..], Regular, &bin[..]), bad]),
        ] {
            fx.offer(&raw_tarball(&members));
            assert_eq!(verify_failure(&fx, &sim), want, "{place}");
        }
    }
}

/// Catches a release that lacks the binary being staged, or refused without its name: the
/// declaration alone, another name, a name with a path.
#[test]
fn a_missing_binary_is_refused_by_name() {
    let fx = Fx::new("mac-members-missing");
    let sim = fx.sim();
    for members in [
        vec![(DECLARATION, Regular, TOML)],
        vec![("kbf2", Regular, &fx.new_binary[..])],
        vec![("bin/kbf", Regular, &fx.new_binary[..])],
    ] {
        fx.offer(&tarball(&members));
        assert_eq!(
            verify_failure(&fx, &sim),
            "tarball: holds no member named kbf"
        );
    }
}

/// Catches a wanted member that is not one plain file within its cap being staged: the
/// declaration as a directory, twice, or over its cap.
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
            vec![("kbf", Regular, &bin[..]), (DECLARATION, Regular, &big[..])],
            format!("tarball: the member {DECLARATION:?} is larger than {DECLARATION_CAP} bytes"),
        ),
    ] {
        fx.offer(&tarball(&members));
        assert_eq!(verify_failure(&fx, &sim), want);
    }
}

/// The stage's ask, `kbf` (at most `cap`) and its declaration, of the tarball `bytes`
/// unpacked into `dir`.
fn unpack_into(dir: &Path, bytes: &[u8], cap: u64) -> Result<Vec<String>, String> {
    let source = dir.join("kbf.tar.gz");
    write(&source, 0o644, bytes);
    let declaration = declaration_member("kbf");
    let wanted = [
        Wanted {
            name: "kbf",
            dest: &dir.join("kbf"),
            mode: 0o755,
            cap,
            required: true,
        },
        Wanted {
            name: &declaration,
            dest: &dir.join(&declaration),
            mode: 0o644,
            cap: DECLARATION_CAP,
            required: false,
        },
    ];
    unpack(&source, &wanted)
}

/// Catches the members passed over not being the ones told, and a gzip bomb in a member no
/// one asked for being read to its end: the names are given in the archive's order, and a
/// member that decompresses past the caps and framing stops the unpack.
#[test]
fn the_members_passed_over_are_named_and_bounded() {
    let fx = Fx::new("mac-members-bound");
    let dir = fx.dir.join("out");
    mkdir(&dir, 0o755);
    let ignored = unpack_into(
        &dir,
        &tarball(&[
            ("a", Regular, b"1"),
            ("kbf", Regular, b"bin"),
            ("b/", Directory, b""),
        ]),
        1024,
    )
    .unwrap();
    assert_eq!(ignored, ["a", "b/"]);
    assert_eq!(listing(&dir), ["kbf", "kbf.tar.gz"]);

    let dir = fx.dir.join("bomb");
    mkdir(&dir, 0o755);
    let bomb = tarball(&[
        ("kbf", Regular, b"x"),
        ("big", Regular, &vec![0u8; 8 << 20]),
    ]);
    assert_eq!(
        unpack_into(&dir, &bomb, 1024),
        Err(format!(
            "it decompresses to more than {} bytes",
            1024 + DECLARATION_CAP
        ))
    );
}

/// Catches the declaration's directory not being made, or a file or a link in its place
/// being replaced or followed, a missing parent being made, and a file already at a
/// member's destination being replaced.
#[test]
fn a_destination_that_cannot_be_made_is_refused() {
    let fx = Fx::new("mac-members-dest");
    let dir = fx.dir.join("blocked");
    mkdir(&dir, 0o755);
    write(&dir.join("updater.d"), 0o644, b"a file");
    let both = tarball(&[(DECLARATION, Regular, TOML), ("kbf", Regular, b"bin")]);
    assert!(unpack_into(&dir, &both, 1024).is_err());
    assert_eq!(fs::read(dir.join("updater.d")).unwrap(), b"a file");

    let dir = fx.dir.join("exists");
    mkdir(&dir, 0o755);
    write(&dir.join("kbf"), 0o755, b"old");
    assert!(unpack_into(&dir, &both, 1024).is_err());
    assert_eq!(fs::read(dir.join("kbf")).unwrap(), b"old");

    // A link in the place of `updater.d` is not followed, and a missing parent is not made.
    let dir = fx.dir.join("linked");
    mkdir(&dir, 0o755);
    let elsewhere = fx.dir.join("elsewhere");
    mkdir(&elsewhere, 0o755);
    std::os::unix::fs::symlink(&elsewhere, dir.join("updater.d")).unwrap();
    assert!(unpack_into(&dir, &both, 1024).is_err());
    assert!(listing(&elsewhere).is_empty(), "written through the link");
    let deep = [Wanted {
        name: "kbf",
        dest: &fx.dir.join("missing/deeper/kbf"),
        mode: 0o755,
        cap: 1024,
        required: true,
    }];
    assert!(unpack(&dir.join("kbf.tar.gz"), &deep).is_err());
    assert!(!fx.dir.join("missing").exists());

    // And a tarball that is not there.
    let wanted = [Wanted {
        name: "kbf",
        dest: &dir.join("new"),
        mode: 0o755,
        cap: 1,
        required: true,
    }];
    assert!(unpack(&dir.join("none.tar.gz"), &wanted).is_err());
}

/// Catches the Mac and the Linux unpack naming a release's declaration differently, or
/// either naming its directory other than the host's declaration directory
/// (`/etc/updater.d`, where the updater reads declarations).
#[test]
fn the_declaration_member_is_the_same_on_both_platforms_and_named_for_updater_d() {
    use crate::mac::stage::DECLARATIONS;
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
