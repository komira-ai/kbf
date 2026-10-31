use std::cell::RefCell;
use std::io::Write;

use super::*;
use kbf_testkit::dir::TestDir;

/// Records each verification, and answers with `accept`; reads the file it is asked about.
struct Fake {
    accept: bool,
    seen: RefCell<Vec<(Vec<u8>, Vec<String>)>>,
}

impl Fake {
    fn new(accept: bool) -> Fake {
        Fake {
            accept,
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl Verifier for Fake {
    fn verify(&self, file: &Path, args: &[String]) -> Result<(bool, String), Error> {
        let bytes = std::fs::read(file).expect("the archive is on disk when gh is asked");
        self.seen.borrow_mut().push((bytes, args.to_vec()));
        Ok((self.accept, "no matching attestation".to_owned()))
    }
}

fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-fetch-attested-unit"), name)
}

const PROGRAM: &[u8] = b"the program's bytes";

/// A gzip tar with the regular file `member` holding `PROGRAM`.
fn archive(member: &str) -> Vec<u8> {
    let mut h = tar::Header::new_ustar();
    h.set_path(member).expect("path");
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(PROGRAM.len() as u64);
    h.set_mode(0o755);
    h.set_cksum();
    let mut b = tar::Builder::new(Vec::new());
    b.append(&h, PROGRAM).expect("append");
    let tar = b.into_inner().expect("tar");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).expect("gzip");
    gz.finish().expect("gzip")
}

/// A request for `bytes` (pinned by their own digest) taking `member`, with the files under `dir`.
fn request(dir: &Path, bytes: &[u8], member: &str) -> Request<'static> {
    Request {
        url: "https://example.invalid/tool.tar.gz",
        sha256: Sha256::of(bytes),
        archive: dir.join("release/tool.tar.gz"),
        member: PathBuf::from(member),
        out: dir.join("tools/tool"),
        repo: "owner/repo",
        signer_workflow: "owner/repo/.github/workflows/release.yml",
        source_ref: "refs/heads/main",
        source_digest: "1f304ae75dbef20c9b01231b666b1e91e1f21743",
    }
}

#[test]
fn a_verified_archive_gives_its_member_and_gh_is_asked_about_the_file_the_member_came_from() {
    // Asserts: gh is run once, on a file holding exactly the downloaded bytes, with the repository, the
    // signer workflow, the issuer, the source ref, the source digest and --deny-self-hosted-runners; and
    // the program written to --out is the member, mode 0755.
    let dir = scratch("ok");
    let bytes = archive("tool-1/tool");
    let req = request(&dir, &bytes, "tool-1/tool");
    let gh = Fake::new(true);
    install_verified(&req, &bytes, &gh).expect("installs");
    let seen = gh.seen.borrow();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, bytes);
    assert_eq!(
        seen[0].1.join(" "),
        "--repo owner/repo --signer-workflow owner/repo/.github/workflows/release.yml \
         --cert-oidc-issuer https://token.actions.githubusercontent.com --source-ref refs/heads/main \
         --source-digest 1f304ae75dbef20c9b01231b666b1e91e1f21743 --deny-self-hosted-runners"
    );
    assert_eq!(std::fs::read(&req.out).expect("out"), PROGRAM);
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&req.out).expect("meta").permissions(),
    );
    assert_eq!(mode & 0o777, 0o755);
}

#[test]
fn a_download_that_is_not_the_pinned_digest_is_refused_before_anything_is_written() {
    // Asserts: the mismatch error names both digests, gh is not asked, and neither the archive nor the
    // program exists afterwards, nor the directory the archive would go in (it is made only after
    // the digest is checked).
    let dir = scratch("sha");
    let bytes = archive("tool-1/tool");
    let mut req = request(&dir, &bytes, "tool-1/tool");
    req.sha256 = Sha256::of(b"another archive");
    let gh = Fake::new(true);
    let err = install_verified(&req, &bytes, &gh).expect_err("wrong sha");
    assert!(err.to_string().contains("not the pinned"), "{err}");
    assert!(gh.seen.borrow().is_empty());
    assert!(!req.archive.exists() && !req.out.exists());
    assert!(
        !req.archive
            .parent()
            .expect("the archive has a directory")
            .exists()
    );
}

#[test]
fn an_archive_whose_attestation_does_not_verify_gives_no_program() {
    // Asserts: the right digest is not enough: when gh refuses, the error names the signer, the commit and
    // what gh said, and the program is not written (the archive is: it is what was refused).
    let dir = scratch("attest");
    let bytes = archive("tool-1/tool");
    let req = request(&dir, &bytes, "tool-1/tool");
    let gh = Fake::new(false);
    let err = install_verified(&req, &bytes, &gh).expect_err("refused");
    let text = err.to_string();
    for want in [
        "owner/repo/.github/workflows/release.yml",
        "1f304ae75dbef20c9b01231b666b1e91e1f21743",
        "no matching attestation",
        "nothing was installed",
    ] {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert_eq!(gh.seen.borrow().len(), 1);
    assert!(!req.out.exists(), "a refused archive must give no program");
}

#[test]
fn a_member_the_archive_does_not_have_gives_no_program() {
    // Asserts: a verified archive without the named member is an error that names it, and writes no program.
    let dir = scratch("member");
    let bytes = archive("tool-1/other");
    let req = request(&dir, &bytes, "tool-1/tool");
    let err = install_verified(&req, &bytes, &Fake::new(true)).expect_err("no member");
    assert!(err.to_string().contains("tool-1/tool"), "{err}");
    assert!(!req.out.exists());
}

#[test]
fn a_member_that_leaves_the_archive_is_refused_before_any_download() {
    // Asserts: an absolute member, a `..` part and an empty member are usage errors from the verb itself,
    // which is run with an unusable url and gh: it must refuse before it uses either.
    let dir = scratch("traversal");
    let bytes = archive("tool-1/tool");
    for member in ["../tool", "tool-1/../../tool", "/etc/passwd", ""] {
        let req = request(&dir, &bytes, member);
        let err = fetch_attested_tool(&req, Path::new("/nonexistent/gh")).expect_err(member);
        assert!(matches!(err, Error::Usage(_)), "{member:?}: {err}");
        assert!(err.to_string().contains("plain names"), "{member:?}: {err}");
    }
    assert!(!dir.join("release").exists() && !dir.join("tools").exists());
}

#[test]
fn an_archive_directory_that_cannot_be_made_is_an_error_and_nothing_is_verified() {
    // Asserts: an --archive below a regular file, and an --archive with no parent at all, are
    // errors naming what failed; gh is not asked and no program is written.
    let dir = scratch("archive-dir");
    let bytes = archive("tool-1/tool");
    let file = dir.join("a-file");
    std::fs::write(&file, b"x").expect("file");
    let mut req = request(&dir, &bytes, "tool-1/tool");
    req.archive = file.join("release/tool.tar.gz");
    let gh = Fake::new(true);
    let err = install_verified(&req, &bytes, &gh).expect_err("parent is a file");
    assert!(
        err.to_string()
            .contains(&format!("create {}", file.join("release").display())),
        "{err}"
    );
    req.archive = PathBuf::new();
    let err = install_verified(&req, &bytes, &gh).expect_err("no parent");
    assert!(err.to_string().contains("write "), "{err}");
    assert!(gh.seen.borrow().is_empty());
    assert!(!req.out.exists());
}

#[test]
fn the_gh_program_is_run_on_the_archive_and_its_answer_and_failure_to_start_are_reported() {
    // Asserts: a program that exits 0 accepts the file, one that exits non-zero refuses it, and a
    // program that does not exist is an error naming it (not a refusal that reads as an answer).
    let file = Path::new("x.tar.gz");
    let (accepted, _) = Gh(Path::new("true")).verify(file, &[]).expect("true runs");
    assert!(accepted);
    let (accepted, said) = Gh(Path::new("false"))
        .verify(file, &[])
        .expect("false runs");
    assert!(!accepted);
    assert_eq!(said, "");
    let err = Gh(Path::new("/nonexistent/gh"))
        .verify(file, &[])
        .expect_err("no such program");
    assert!(err.to_string().contains("/nonexistent/gh"), "{err}");
}
