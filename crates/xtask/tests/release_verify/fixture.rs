//! The fixtures both halves share: a git repository per test, tags made by `git tag` (so a
//! lightweight tag is a ref to a commit and an annotated one a tag object, as GitHub would hold
//! them), the record `good()`, and `run`, which starts the binary.

use kbf_testkit::dir::TestDir;
use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_gate::{CommitId, GateResult, Header, Limit, ResultDoc, Sample, SetHash, Stage, Status};

pub const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
/// The release commit of [`fx`]: fixed dates and identity make git's hash a constant,
/// so the golden record can name it.
pub const RELEASE: &str = "357494ac6b0fecfa065d6c8fa038cfca9ae9c13f";
pub const TAG: &str = "v0.10.0";
/// The record `release verdict-write` writes (its golden), for tag `v0.3.7`.
pub const WRITTEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/release-verdict-write.json"
);
/// What `SHA256SUMS` holds in every test, and its set hash.
pub const SUMS: &str = "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881  a\n";
pub const SET: &str = "4a4b874a1e9776a52e38fd82a053825f309f46187a803a0fc0c6670c6670e768";

pub struct Fx {
    pub root: PathBuf,
    /// The first commit, an ancestor of both.
    pub first: String,
    /// A commit on a side branch, not on main.
    pub side: String,
    /// Last field: the directory is removed with the fixture.
    pub _own: TestDir,
}

impl Fx {
    pub fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    pub fn dist(&self) -> PathBuf {
        self.root.join("dist")
    }

    pub fn git(&self, args: &[&str]) -> String {
        git(&self.repo(), args, None)
    }

    /// Makes annotated tag `name` at `commit` with exactly `message`.
    pub fn annotate(&self, name: &str, commit: &str, message: &[u8]) {
        let file = self.root.join("message");
        std::fs::write(&file, message).unwrap();
        let file = file.to_str().unwrap();
        self.git(&["tag", "-a", name, "-F", file, "--cleanup=verbatim", commit]);
    }
}

pub fn git(dir: &Path, args: &[&str], stdin: Option<&str>) -> String {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .env("GIT_AUTHOR_DATE", "2026-10-18T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-10-18T00:00:00Z")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    drop(
        child
            .stdin
            .take()
            .map(|mut i| i.write_all(stdin.unwrap_or("").as_bytes())),
    );
    let out = child.wait_with_output().expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

/// A repository: main holds `first` and `release commit` (hash [`RELEASE`]); the branch
/// `side` holds `side` on top of `first`. Release files: `dist/SHA256SUMS`.
pub fn fx(name: &str) -> Fx {
    let own = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("release-verify"),
        name,
    );
    let root = own.to_path_buf();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(root.join("dist")).unwrap();
    std::fs::write(root.join("dist/SHA256SUMS"), SUMS).unwrap();
    git(
        &repo,
        &["init", "-q", "-b", "main", "--object-format=sha1"],
        None,
    );
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "first"],
        None,
    );
    let first = git(&repo, &["rev-parse", "HEAD"], None);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "release commit"],
        None,
    );
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"], None),
        RELEASE,
        "git hashes changed"
    );
    git(&repo, &["switch", "-q", "-c", "side", &first], None);
    git(
        &repo,
        &["commit", "-q", "--allow-empty", "-m", "side"],
        None,
    );
    let side = git(&repo, &["rev-parse", "HEAD"], None);
    git(&repo, &["switch", "-q", "main"], None);
    Fx {
        root,
        first,
        side,
        _own: own,
    }
}
/// A Beta record for [`TAG`], [`RELEASE`] and [`SET`], as `release verdict-write` writes
/// one: one green gate, no bake, no load.
pub fn good() -> ResultDoc {
    let header = Header {
        stage: Stage::Beta,
        commit: CommitId::parse(RELEASE).unwrap(),
        set_hash: SetHash::parse(SET).unwrap(),
        run_id: "run-0001".to_owned(),
        attempt: 1,
    };
    let mut d = ResultDoc::begin(header, 1_000);
    d.status = Status::Green;
    d.finished = Some(90_000);
    d.gates = vec![GateResult::decide(
        "lost-blobs",
        Sample::Value(0.0),
        Limit::AtMost(0.0),
        2_000,
        3_000,
    )];
    d.tag = Some(TAG.to_owned());
    d
}

/// A tag message: the record `doc` as it is written.
pub fn msg(doc: &ResultDoc) -> Vec<u8> {
    doc.to_bytes().unwrap()
}

pub type Ran = (Option<i32>, String, String);

pub fn xtask(args: &[&str]) -> Ran {
    let out = Command::new(XTASK).args(args).output().expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

pub fn run(fx: &Fx, verb: &str, tag: &str, commit: &str, extra: &[&str]) -> Ran {
    let (dist, repo) = (fx.dist(), fx.repo());
    let mut args = vec![
        "release",
        verb,
        dist.to_str().unwrap(),
        "--repo-dir",
        repo.to_str().unwrap(),
        "--tag",
        tag,
        "--commit",
        commit,
        "--main-ref",
        "main",
    ];
    args.extend(extra);
    xtask(&args)
}

pub fn verify(fx: &Fx, tag: &str, commit: &str) -> Ran {
    run(fx, "verify", tag, commit, &[])
}

/// Tags [`RELEASE`] with `message` and expects `release verify` to pass and print the
/// files' set hash: the message is not consulted.
pub fn passes_with(name: &str, message: &[u8]) {
    let fx = fx(name);
    fx.annotate(TAG, RELEASE, message);
    let (code, out, err) = verify(&fx, TAG, RELEASE);
    assert_eq!(code, Some(0), "{name}: {out}{err}");
    assert!(out.ends_with(&format!("set-hash={SET}\n")), "{name}: {out}");
}
