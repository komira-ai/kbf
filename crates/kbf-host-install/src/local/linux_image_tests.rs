//! The frontend image of a release on a Linux host: fetched as the unprivileged user, copied
//! once, checked against the release's sums and its attestation, and handed over only when
//! all of it holds.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use super::{COMMIT, Rig, arch_name, put, refused, stable, version};
use crate::local::answer::Code;
use crate::local::frontend::ImageSource;

const IMAGE: &[u8] = b"an oci layout, in one tar";

fn image_name(commit: &str) -> String {
    format!("kbf-frontend-image-{commit}-linux-{}.oci.tar", arch_name())
}

fn bundle_name(commit: &str) -> String {
    format!("{}.sigstore.json", image_name(commit))
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sums_for(commit: &str, bytes: &[u8]) -> String {
    format!(
        "{}  kbf-{commit}-linux-{}.tar.gz\n{}  {}\n",
        "0".repeat(64),
        arch_name(),
        sha(bytes),
        image_name(commit)
    )
}

impl Rig {
    /// The channel's GitHub offers the image of `commit`, with `sums` as its `SHA256SUMS` and
    /// `listed` as the size the listing gives the image.
    fn offer_image(&self, commit: &str, bytes: &[u8], sums: &str, listed: usize) {
        let tag = format!("v-{}", &commit[..8]);
        let assets = format!(
            "{{\"name\":\"{}\",\"size\":{listed}}},{{\"name\":\"{}\",\"size\":30}},{{\"name\":\"SHA256SUMS\",\"size\":{}}}",
            image_name(commit),
            bundle_name(commit),
            sums.len()
        );
        let release = format!("{{\"tag_name\":\"{tag}\",\"draft\":false,\"assets\":[{assets}]}}");
        let mut net = self.net.borrow_mut();
        net.pages = vec![format!("[{release}]")];
        let files = net.assets.entry(tag).or_default();
        files.insert(image_name(commit), bytes.to_vec());
        files.insert(
            bundle_name(commit),
            b"{\"mediaType\":\"bundle\"}\n".to_vec(),
        );
        files.insert("SHA256SUMS".to_owned(), sums.as_bytes().to_vec());
    }

    fn offer_good_image(&self) {
        self.offer_image(COMMIT, IMAGE, &sums_for(COMMIT, IMAGE), IMAGE.len());
    }

    fn work(&self) -> PathBuf {
        self.root().join("var/lib/kbf-installer/work")
    }

    fn work_entries(&self) -> Vec<String> {
        fs::read_dir(self.work())
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Nothing is left of the fetch, and the installer's attestation check never ran.
    fn assert_nothing_kept(&self) {
        assert!(self.work_entries().is_empty(), "{:?}", self.work_entries());
    }
}

/// The image is fetched as the unprivileged user with its bundle and the release's sums,
/// copied into a directory of root's, attested offline for the full commit from that copy, and
/// handed over; the directory goes with the handle.
#[test]
fn an_image_is_fetched_checked_and_handed_over() {
    let rig = Rig::new("image-good");
    rig.offer_good_image();
    let fetched = rig.backend.fetch(&version(), &stable()).unwrap();
    assert_eq!(fetched.commit, COMMIT);
    assert_eq!(fs::read(&fetched.file).unwrap(), IMAGE);
    assert_eq!(
        fetched.file.file_name().unwrap().to_string_lossy(),
        image_name(COMMIT)
    );
    assert_eq!(
        fs::metadata(&fetched.file).unwrap().permissions().mode() & 0o7777,
        0o644
    );
    assert!(
        fetched.file.starts_with(rig.work()),
        "the copy is under the installer's work"
    );

    // The download asked for the three files, and as the unprivileged user.
    let download = rig
        .calls
        .borrow()
        .iter()
        .find(|c| c.args.first().map(String::as_str) == Some("release"))
        .cloned()
        .unwrap();
    for name in [
        image_name(COMMIT),
        bundle_name(COMMIT),
        "SHA256SUMS".to_owned(),
    ] {
        assert!(download.args.contains(&name), "{name}");
    }
    assert!(
        !download
            .args
            .contains(&format!("kbf-{COMMIT}-linux-{}.tar.gz", arch_name()))
    );
    assert_eq!(download.run_as.map(|i| i.uid), Some(super::me().uid));

    // `gh attestation verify` ran once, on the copy, for the full commit; no probe of a binary.
    let spawns = rig.installer.borrow().clone();
    assert_eq!(spawns.len(), 1);
    let args: Vec<String> = spawns[0]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args[2], fetched.file.display().to_string());
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--source-digest" && w[1] == COMMIT)
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--source-ref" && w[1] == "refs/heads/main")
    );
    assert!(args.contains(&"--deny-self-hosted-runners".to_owned()));

    // Only the handle's directory is left, and it goes with it.
    assert_eq!(rig.work_entries(), [format!("{COMMIT}.check")]);
    drop(fetched);
    rig.assert_nothing_kept();
}

/// An image whose bytes are not the ones the release's sums list for it is refused before
/// the attestation is asked, and nothing is left.
#[test]
fn an_image_that_the_sums_do_not_list_is_refused() {
    let rig = Rig::new("image-sums");
    let other = sums_for(COMMIT, b"another file");
    rig.offer_image(COMMIT, IMAGE, &other, IMAGE.len());
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(
        fault.refused && fault.code == Code::VerifyFailed,
        "{fault:?}"
    );
    assert!(
        fault
            .detail
            .contains("not the one the release's SHA256SUMS lists"),
        "{}",
        fault.detail
    );
    assert!(rig.installer.borrow().is_empty());
    rig.assert_nothing_kept();

    // Sums that do not list the image at all, and sums that are not sums.
    for (sums, why) in [
        (
            format!("{}  something-else\n", "0".repeat(64)),
            "does not list",
        ),
        ("garbage".to_owned(), "malformed"),
        (String::new(), "does not list"),
    ] {
        rig.offer_image(COMMIT, IMAGE, &sums, IMAGE.len());
        let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
        assert!(
            fault.refused && fault.code == Code::VerifyFailed,
            "{fault:?}"
        );
        assert!(fault.detail.contains(why), "{}", fault.detail);
        rig.assert_nothing_kept();
    }
}

/// A channel that pins its sums takes the image only if the pin lists it as well.
#[test]
fn a_channels_pinned_sums_must_list_the_image_too() {
    let rig = Rig::new("image-pin");
    rig.offer_good_image();
    let pin = rig.dir.join("stable.SHA256SUMS");
    let channels = |pin: &std::path::Path| {
        format!(
            "[channels.stable]\nsource = \"github\"\nsums = \"{}\"\n",
            pin.display()
        )
    };
    put(&rig.dir.join("releases.toml"), channels(&pin));

    // The pin is missing (the channel is not usable), not root's alone to write, a link, or
    // not text.
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(
        fault.refused && fault.code == Code::ConfigInvalid,
        "{fault:?}"
    );
    put(&pin, sums_for(COMMIT, IMAGE));
    fs::set_permissions(&pin, fs::Permissions::from_mode(0o666)).unwrap();
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(
        fault.refused && fault.code == Code::ConfigUnsafe,
        "{fault:?}"
    );
    let target = rig.dir.join("target.SHA256SUMS");
    put(&target, sums_for(COMMIT, IMAGE));
    fs::remove_file(&pin).unwrap();
    std::os::unix::fs::symlink(&target, &pin).unwrap();
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(
        fault.refused && fault.code == Code::ConfigUnsafe,
        "{fault:?}"
    );
    fs::remove_file(&pin).unwrap();
    put(&pin, "");
    fs::write(&pin, [0xff_u8, 0xfe]).unwrap();
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(
        fault.refused && fault.code == Code::ConfigUnsafe,
        "{fault:?}"
    );
    rig.assert_nothing_kept();

    put(&pin, sums_for(COMMIT, b"another file"));
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(
        fault.refused && fault.code == Code::VerifyFailed,
        "{fault:?}"
    );
    assert!(
        fault.detail.contains("the channel's sums"),
        "{}",
        fault.detail
    );
    assert!(rig.installer.borrow().is_empty());
    rig.assert_nothing_kept();

    put(&pin, sums_for(COMMIT, IMAGE));
    assert_eq!(
        rig.backend.fetch(&version(), &stable()).unwrap().commit,
        COMMIT
    );
}

/// An image the attestation does not vouch for is refused, and nothing is left.
#[test]
fn an_image_that_is_not_attested_is_refused() {
    let rig = Rig::new("image-unattested");
    rig.offer_good_image();
    *rig.gh_exit.borrow_mut() = 1;
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");
    assert_eq!(
        rig.installer.borrow().len(),
        2,
        "both allowed signers were tried"
    );
    rig.assert_nothing_kept();
}

/// A release without an image for this platform is unknown, a failed download and a file that
/// is not a plain file are fetch failures, and an image over the limit is refused; none leaves
/// anything.
#[test]
fn what_cannot_be_fetched_is_refused_and_leaves_nothing() {
    let rig = Rig::new("image-fetch");
    // The channel offers the binary's tarball only.
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(refused(Code::UnknownRelease)(&fault), "{fault:?}");
    rig.assert_nothing_kept();

    rig.offer_good_image();
    rig.net.borrow_mut().broken.insert("download-fails");
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(refused(Code::FetchFailed)(&fault), "{fault:?}");
    rig.net.borrow_mut().broken.clear();
    rig.assert_nothing_kept();

    for broken in [
        "tarball-is-a-link",
        "tarball-is-a-directory",
        "bundle-is-a-link",
        "sums-is-a-directory",
        "sums-is-a-link",
    ] {
        rig.net.borrow_mut().broken.insert(broken);
        let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
        assert!(refused(Code::FetchFailed)(&fault), "{broken}: {fault:?}");
        rig.net.borrow_mut().broken.clear();
        rig.assert_nothing_kept();
    }

    // The listing says the image is small and it is not: the copy is cut, and refused.
    let big = vec![7_u8; 70 * 1024];
    rig.offer_image(COMMIT, &big, &sums_for(COMMIT, &big), 100);
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(refused(Code::FetchFailed)(&fault), "{fault:?}");
    rig.assert_nothing_kept();

    // The listing says so itself.
    rig.offer_image(COMMIT, &big, &sums_for(COMMIT, &big), 70 * 1024);
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(
        fault.detail.contains("larger than the limit"),
        "{}",
        fault.detail
    );
}

/// A scratch directory that cannot be made is a failure of the stage, not a half-checked image.
#[test]
fn a_directory_in_the_way_of_the_checks_fails_the_stage() {
    let rig = Rig::new("image-scratch");
    rig.offer_good_image();
    fs::create_dir_all(rig.work()).unwrap();
    fs::write(rig.work().join(format!("{COMMIT}.check")), "in the way").unwrap();
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(
        !fault.refused && fault.code == Code::StageFailed,
        "{fault:?}"
    );
    assert!(rig.installer.borrow().is_empty());

    // A check directory a crash left behind is replaced.
    fs::remove_file(rig.work().join(format!("{COMMIT}.check"))).unwrap();
    fs::create_dir_all(rig.work().join(format!("{COMMIT}.check/stale"))).unwrap();
    let fetched = rig.backend.fetch(&version(), &stable()).unwrap();
    assert!(!fetched.file.parent().unwrap().join("stale").exists());
}

/// The image's size limit is the host's, and the production tools carry it.
#[test]
fn the_production_limit_for_an_image_is_a_gibibyte() {
    assert_eq!(super::Tools::production().image_max, 1 << 30);
}

/// Two helpers (two frontend containers) that stage the same release one after another, the
/// first's download still held, never touch each other's directories: each has its own, named
/// after its container, and the default helper keeps the plain names.
#[test]
fn helpers_with_different_containers_keep_their_own_directories() {
    let mut rig = Rig::new("image-two-helpers");
    rig.offer_good_image();
    let me = super::me();
    let who = crate::runner::Identity {
        uid: me.uid,
        gid: me.gid,
    };
    // The scratch of the fetch.
    let plain = rig.backend.fetch_scratch("abcd", who).unwrap();
    rig.backend.scratch_tag = Some("kbf-fe-a".to_owned());
    let tagged = rig.backend.fetch_scratch("abcd", who).unwrap();
    assert_eq!(plain.file_name().unwrap(), "abcd.fetch");
    assert_eq!(tagged.file_name().unwrap(), "abcd.kbf-fe-a.fetch");
    assert!(plain.exists() && tagged.exists());
    // The checked copy that the handle holds.
    let first = rig.backend.fetch(&version(), &stable()).unwrap();
    rig.backend.scratch_tag = Some("kbf-fe-b".to_owned());
    let second = rig.backend.fetch(&version(), &stable()).unwrap();
    assert_ne!(first.file, second.file);
    assert!(
        first
            .file
            .starts_with(rig.work().join(format!("{COMMIT}.kbf-fe-a.check")))
    );
    assert!(
        second
            .file
            .starts_with(rig.work().join(format!("{COMMIT}.kbf-fe-b.check")))
    );
    assert_eq!(fs::read(&first.file).unwrap(), IMAGE);
    drop(second);
    // The first helper's copy is still there after the second's is gone.
    assert_eq!(fs::read(&first.file).unwrap(), IMAGE);
    assert!(plain.exists() && tagged.exists());
}

/// The frontend image of a channel for a trusted repository is listed from, downloaded from
/// and attested for that repository and its workflow; without the repository's listing in the
/// channels file the channel is refused before a fetch.
#[test]
fn an_image_of_another_repository_is_attested_by_its_own_workflow() {
    let channel = |trusted: &str| {
        format!(
            "{trusted}[channels.stable]\nsource = \"github\"\nrepository = \"owner/other\"\nsigner_workflow = \"owner/other/.github/workflows/release.yml\"\n"
        )
    };
    let rig = Rig::new("image-other-refused");
    rig.offer_good_image();
    put(&rig.dir.join("releases.toml"), channel(""));
    let fault = rig.backend.fetch(&version(), &stable()).err().unwrap();
    assert!(refused(Code::ChannelNotAllowed)(&fault), "{fault:?}");
    assert!(rig.calls.borrow().is_empty());
    rig.assert_nothing_kept();

    let rig = Rig::new("image-other");
    rig.offer_good_image();
    put(
        &rig.dir.join("releases.toml"),
        channel("trusted_workflows = [\"owner/other/.github/workflows/release.yml\"]\n"),
    );
    let fetched = rig.backend.fetch(&version(), &stable()).unwrap();
    assert_eq!(fetched.commit, COMMIT);
    let calls = rig.calls.borrow();
    let listing = calls.iter().find(|c| c.program.ends_with("curl")).unwrap();
    assert!(
        listing
            .args
            .last()
            .unwrap()
            .starts_with("https://api.github.com/repos/owner/other/releases?")
    );
    let download = calls
        .iter()
        .find(|c| c.args.first().map(String::as_str) == Some("release"))
        .unwrap();
    let at = download.args.iter().position(|a| a == "--repo").unwrap();
    assert_eq!(download.args[at + 1], "owner/other");
    assert!(download.args.contains(&image_name(COMMIT)));
    let spawns = rig.installer.borrow().clone();
    assert_eq!(spawns.len(), 1);
    let args: Vec<String> = spawns[0]
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let has = |flag: &str, value: &str| args.windows(2).any(|w| w[0] == flag && w[1] == value);
    assert!(has("--repo", "owner/other"));
    assert!(has(
        "--signer-workflow",
        "owner/other/.github/workflows/release.yml"
    ));
}
