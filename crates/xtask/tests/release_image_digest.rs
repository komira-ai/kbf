//! `cargo xtask release image-digest`: a deploy of the published frontend image is given the
//! manifest digest its `index.json` names and the release's addresses of the image and its
//! bundle, read from the release (fetched with `gh release download`, here a fake `gh` that
//! copies a directory). An image that is not the file `SHA256SUMS` lists, is no image layout
//! with one manifest, or has no bundle beside it, and a repository or tag that would not make
//! a plain address, end the verb with no line printed and nothing written to `--output`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kbf_testkit::dir::TestDir;
use sha2::Digest;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "3aa7c93274c5f00dfeed0123456789abcdef0123";
const TAG: &str = "0.0.13";
const IMAGE: &str =
    "kbf-frontend-image-3aa7c93274c5f00dfeed0123456789abcdef0123-linux-x86_64.oci.tar";
const BUNDLE: &str = "kbf-frontend-image-3aa7c93274c5f00dfeed0123456789abcdef0123-linux-x86_64.oci.tar.sigstore.json";

fn sha_of(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// A plain tar of `files` (path, bytes).
fn tar_of(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut h = tar::Header::new_ustar();
        h.set_path(path).expect("path");
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append(&h, bytes.as_slice()).expect("append");
    }
    tar.into_inner().expect("tar")
}

fn blob(bytes: &[u8]) -> (String, Vec<u8>) {
    (format!("blobs/sha256/{}", sha_of(bytes)), bytes.to_vec())
}

/// The manifest of an image with one config and one layer, and the layout's blobs.
fn manifest_and_blobs(seed: &str) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    let config = format!(r#"{{"os":"linux","architecture":"amd64","seed":"{seed}"}}"#).into_bytes();
    let layer = tar_of(&[("usr/bin/kbf".to_owned(), seed.as_bytes().to_vec())]);
    let manifest = format!(
        r#"{{"schemaVersion":2,"config":{{"digest":"sha256:{}"}},"layers":[{{"digest":"sha256:{}"}}]}}"#,
        sha_of(&config),
        sha_of(&layer)
    )
    .into_bytes();
    let blobs = vec![blob(&manifest), blob(&config), blob(&layer)];
    (manifest, blobs)
}

/// An OCI image layout tar whose `index.json` names the manifests `digests`.
fn layout(digests: &[String], blobs: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let entries: Vec<String> = digests
        .iter()
        .map(|d| {
            format!(
                r#"{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{d}"}}"#
            )
        })
        .collect();
    let index = format!(
        r#"{{"schemaVersion":2,"manifests":[{}]}}"#,
        entries.join(",")
    );
    let mut files = vec![("index.json".to_owned(), index.into_bytes())];
    files.extend(blobs);
    tar_of(&files)
}

/// A good image and the digest of its manifest.
fn image() -> (Vec<u8>, String) {
    let (manifest, blobs) = manifest_and_blobs("one");
    let digest = format!("sha256:{}", sha_of(&manifest));
    (layout(std::slice::from_ref(&digest), blobs), digest)
}

/// A scratch directory with `gh/gh` (the fake, whose `release download` copies
/// `gh/releases/<TAG>/` into `--dir` and records its arguments in `gh/gh-args`) and `out`.
struct Fx {
    root: TestDir,
}

impl Fx {
    fn new(name: &str) -> Self {
        let root = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-image-digest"),
            name,
        );
        std::fs::create_dir_all(root.path().join("gh/releases").join(TAG)).expect("release");
        std::fs::hard_link(FAKE, root.path().join("gh/gh")).expect("link the fake as gh");
        Self { root }
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("gh/releases").join(TAG)
    }

    /// The release holds `image`, a bundle, and `SHA256SUMS` exactly `sums`.
    fn release_with_sums(&self, image: &[u8], sums: &[u8]) {
        std::fs::write(self.dir().join(IMAGE), image).expect("image");
        std::fs::write(self.dir().join(BUNDLE), b"{\"mediaType\":\"bundle\"}").expect("bundle");
        std::fs::write(self.dir().join("SHA256SUMS"), sums).expect("sums");
    }

    /// The release holds `image`, a bundle, and a `SHA256SUMS` that lists `listed` as the image.
    fn release(&self, image: &[u8], listed: &[u8]) {
        let sums = format!(
            "{}  {IMAGE}\n{}  other.tar.gz\n",
            sha_of(listed),
            "0".repeat(64)
        );
        self.release_with_sums(image, sums.as_bytes());
    }

    fn out(&self) -> PathBuf {
        self.root.path().join("out")
    }

    fn run(&self, extra: &[&str], env: &[(&str, &str)]) -> (Output, String) {
        self.run_as("o/r", TAG, COMMIT, extra, env)
    }

    fn run_as(
        &self,
        repo: &str,
        tag: &str,
        commit: &str,
        extra: &[&str],
        env: &[(&str, &str)],
    ) -> (Output, String) {
        let ran = Command::new(XTASK)
            .args(["release", "image-digest", "--repo", repo, "--tag", tag])
            .args(["--commit", commit, "--gh"])
            .arg(self.root.path().join("gh/gh"))
            .args(extra)
            .envs(env.iter().copied())
            .output()
            .expect("run");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let says = format!("{}{}", text(&ran.stdout), text(&ran.stderr));
        (ran, says)
    }

    fn with_output(&self) -> [String; 2] {
        ["--output".to_owned(), self.out().display().to_string()]
    }

    fn refused(&self, ran: &Output, says: &str, why: &str) {
        assert_eq!(ran.status.code(), Some(1), "{says}");
        assert!(says.contains(why), "wanted {why:?} in {says}");
        assert!(!says.contains("digest="), "{says}");
        assert!(!says.contains("-url="), "{says}");
        assert!(!self.out().exists(), "wrote the output");
    }
}

fn args(a: &[String; 2]) -> [&str; 2] {
    [a[0].as_str(), a[1].as_str()]
}

#[test]
fn the_digest_and_both_addresses_of_the_listed_image_are_printed_and_appended() {
    // Proves: for an image SHA256SUMS lists, the verb prints the manifest digest index.json
    // names and the release's download addresses of the image and its bundle (by repository
    // and tag), appends exactly those three lines after what `--output` already holds, and
    // asks `gh` for the release by tag and repository with exactly the three files. It would
    // catch a digest of the tar itself, an address of another platform's image or a missing
    // bundle pattern.
    let fx = Fx::new("ok");
    let (bytes, digest) = image();
    fx.release(&bytes, &bytes);
    std::fs::write(fx.out(), "tag=0.0.13\n").expect("write");
    let out = fx.with_output();
    let (ran, says) = fx.run(&args(&out), &[]);
    assert_eq!(ran.status.code(), Some(0), "{says}");
    let base = "https://github.com/o/r/releases/download/0.0.13";
    let want = format!("digest={digest}\nimage-url={base}/{IMAGE}\nbundle-url={base}/{BUNDLE}\n");
    assert!(says.contains(&want), "{says}");
    assert_ne!(digest, format!("sha256:{}", sha_of(&bytes)));
    assert_eq!(
        std::fs::read_to_string(fx.out()).expect("read"),
        format!("tag=0.0.13\n{want}")
    );
    let argv = std::fs::read_to_string(fx.root.path().join("gh/gh-args")).expect("argv");
    assert!(
        argv.starts_with("release download 0.0.13 --repo o/r --dir "),
        "{argv}"
    );
    assert!(
        argv.contains(&format!(
            "--pattern SHA256SUMS --pattern {IMAGE} --pattern {BUNDLE}"
        )),
        "{argv}"
    );
    // Without `--output` the lines are printed and nothing is written.
    std::fs::remove_file(fx.out()).expect("remove");
    let (ran, says) = fx.run(&[], &[]);
    assert_eq!(ran.status.code(), Some(0), "{says}");
    assert!(says.contains(&want), "{says}");
    assert!(!fx.out().exists());
}

#[test]
fn an_image_that_is_not_the_one_sha256sums_lists_exactly_once_is_refused() {
    // Proves: other bytes than SHA256SUMS lists, a SHA256SUMS that names other files only,
    // and one that lists the image twice (even with its digest) do not vouch for the image.
    let fx = Fx::new("listing");
    let (bytes, _) = image();
    let out = fx.with_output();
    fx.release(&bytes, b"other bytes");
    let (ran, says) = fx.run(&args(&out), &[]);
    fx.refused(
        &ran,
        &says,
        "is not the file SHA256SUMS lists (listed 1 time(s))",
    );
    let other = format!("{}  something-else.tar.gz\n", sha_of(&bytes));
    fx.release_with_sums(&bytes, other.as_bytes());
    let (ran, says) = fx.run(&args(&out), &[]);
    fx.refused(&ran, &says, "(listed 0 time(s))");
    let twice = format!("{0}  {IMAGE}\n{0}  {IMAGE}\n", sha_of(&bytes));
    fx.release_with_sums(&bytes, twice.as_bytes());
    let (ran, says) = fx.run(&args(&out), &[]);
    fx.refused(&ran, &says, "(listed 2 time(s))");
}

#[test]
fn a_listed_file_that_is_no_layout_with_one_good_manifest_is_refused_with_the_reason() {
    // Proves: each image the deployer could not pull by the digest printed is refused, with
    // the problem named: no index.json, an index naming no manifest or two, a manifest blob
    // the layout lacks, a manifest blob whose bytes do not hash to its digest, and bytes that
    // are no tar at all.
    let fx = Fx::new("layout");
    let out = fx.with_output();
    let (m1, b1) = manifest_and_blobs("one");
    let (m2, b2) = manifest_and_blobs("two");
    let d1 = format!("sha256:{}", sha_of(&m1));
    let d2 = format!("sha256:{}", sha_of(&m2));
    let mut both = b1.clone();
    both.extend(b2);
    let missing = format!("sha256:{}", "0".repeat(64));
    let forged: Vec<(String, Vec<u8>)> = b1
        .iter()
        .map(|(p, b)| {
            if p.ends_with(&sha_of(&m1)) {
                (p.clone(), b"{\"forged\":true}".to_vec())
            } else {
                (p.clone(), b.clone())
            }
        })
        .collect();
    let cases: [(Vec<u8>, &str); 6] = [
        (tar_of(&b1), "holds no index.json"),
        (
            layout(&[], b1.clone()),
            "index.json lists 0 manifests, not exactly one",
        ),
        (
            layout(&[d1.clone(), d2], both),
            "index.json lists 2 manifests, not exactly one",
        ),
        (
            layout(std::slice::from_ref(&missing), b1),
            "holds no blob for the manifest",
        ),
        (
            layout(std::slice::from_ref(&d1), forged),
            "has another digest",
        ),
        (vec![7u8; 700], "is not an OCI image tar"),
    ];
    for (bytes, why) in cases {
        fx.release(&bytes, &bytes);
        let (ran, says) = fx.run(&args(&out), &[]);
        fx.refused(&ran, &says, why);
        assert!(says.contains(IMAGE), "the image is named: {says}");
    }
}

#[test]
fn a_release_without_the_image_its_bundle_or_its_sums_is_refused() {
    // Proves: a release that lacks the linux-x86_64 frontend image, its provenance bundle (or
    // holds an empty one), or SHA256SUMS ends the verb before any line is printed: the
    // deployer would download an address that does not exist.
    let fx = Fx::new("missing");
    let out = fx.with_output();
    let (bytes, _) = image();
    for (file, why) in [
        (IMAGE, IMAGE),
        (BUNDLE, BUNDLE),
        ("SHA256SUMS", "SHA256SUMS"),
    ] {
        fx.release(&bytes, &bytes);
        std::fs::remove_file(fx.dir().join(file)).expect("remove");
        let (ran, says) = fx.run(&args(&out), &[]);
        fx.refused(&ran, &says, "read ");
        assert!(says.contains(why), "{says}");
    }
    fx.release(&bytes, &bytes);
    std::fs::write(fx.dir().join(BUNDLE), b"").expect("empty bundle");
    let (ran, says) = fx.run(&args(&out), &[]);
    fx.refused(&ran, &says, &format!("{BUNDLE} is empty"));
}

#[test]
fn a_sums_file_that_is_not_sums_or_over_its_cap_is_refused() {
    // Proves: a SHA256SUMS line that is not `<sha256>  <name>` is named by its line, bytes that
    // are not UTF-8 are refused as such, and a SHA256SUMS over 1 MiB is refused before it is
    // parsed.
    let fx = Fx::new("sums");
    let out = fx.with_output();
    let (bytes, _) = image();
    fx.release_with_sums(&bytes, b"garbage\n");
    let (ran, says) = fx.run(&args(&out), &[]);
    fx.refused(&ran, &says, "SHA256SUMS: line 1:");
    fx.release_with_sums(&bytes, &[0xff, 0xfe, b'\n']);
    let (ran, says) = fx.run(&args(&out), &[]);
    fx.refused(&ran, &says, "SHA256SUMS is not UTF-8");
    fx.release_with_sums(&bytes, "a".repeat((1 << 20) + 1).as_bytes());
    let (ran, says) = fx.run(&args(&out), &[]);
    fx.refused(&ran, &says, &format!("is over {} bytes", 1 << 20));
}

#[test]
fn a_repository_tag_or_commit_that_makes_no_plain_address_is_a_usage_error() {
    // Proves: a repository that is not `<owner>/<name>` of plain names, a tag with a `/`, a
    // space, a `?` or that is `..`, and a short commit each end with exit 2 before `gh` runs,
    // so no address with a path or query of someone's choosing is printed. The boundary
    // spellings `.`-, `_`- and `-`-holding names pass.
    let fx = Fx::new("names");
    let (bytes, _) = image();
    fx.release(&bytes, &bytes);
    let refused = [
        ("o", TAG, COMMIT),
        ("o/r/x", TAG, COMMIT),
        ("/r", TAG, COMMIT),
        ("o/", TAG, COMMIT),
        ("o/..", TAG, COMMIT),
        ("o/r", "v1/../x", COMMIT),
        ("o/r", "v 1", COMMIT),
        ("o/r", "v1?x=1", COMMIT),
        ("o/r", "..", COMMIT),
        ("o/r", ".", COMMIT),
        ("o/r", "", COMMIT),
        ("o/r", TAG, "3aa7c93"),
    ];
    for (repo, tag, commit) in refused {
        let (ran, says) = fx.run_as(repo, tag, commit, &[], &[]);
        assert_eq!(ran.status.code(), Some(2), "{repo} {tag} {commit}: {says}");
        assert!(!says.contains("digest="), "{says}");
    }
    assert!(!fx.root.path().join("gh/gh-args").exists(), "gh ran");
    let (ran, says) = fx.run_as("my-org.x/kbf_y", TAG, COMMIT, &[], &[]);
    assert_eq!(ran.status.code(), Some(0), "{says}");
    assert!(
        says.contains("https://github.com/my-org.x/kbf_y/releases/download/0.0.13/"),
        "{says}"
    );
}

#[test]
fn a_failing_gh_a_missing_scratch_or_an_unwritable_output_ends_the_verb() {
    // Proves: a `gh release download` that exits non-zero is reported with gh's own words; a
    // `gh` that cannot be started is reported by its path; a temporary directory that does not
    // exist is refused as such; and an `--output` that cannot be appended to fails the verb
    // instead of leaving a later step without the digest.
    let fx = Fx::new("fails");
    let (bytes, _) = image();
    fx.release(&bytes, &bytes);
    let out = fx.with_output();
    let (ran, says) = fx.run(&args(&out), &[("XTASK_FAKE_DEFECT", "release-fails")]);
    fx.refused(
        &ran,
        &says,
        "gh release download: xtask-fake: release failed",
    );
    let ran = Command::new(XTASK)
        .args(["release", "image-digest", "--repo", "o/r", "--tag", TAG])
        .args(["--commit", COMMIT, "--gh", "/nonexistent/gh"])
        .output()
        .expect("run");
    let says = String::from_utf8_lossy(&ran.stderr).into_owned();
    assert_eq!(ran.status.code(), Some(1), "{says}");
    assert!(says.contains("/nonexistent/gh"), "{says}");
    let (ran, says) = fx.run(&args(&out), &[("TMPDIR", "/nonexistent/tmp")]);
    fx.refused(&ran, &says, "create ");
    let dir = fx.root.path().join("a-directory");
    std::fs::create_dir(&dir).expect("dir");
    let (ran, says) = fx.run(&["--output", dir.to_str().expect("utf-8")], &[]);
    assert_eq!(ran.status.code(), Some(1), "{says}");
    assert!(says.contains("append to "), "{says}");
}
