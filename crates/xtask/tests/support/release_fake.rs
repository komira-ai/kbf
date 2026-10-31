//! The fake GitHub the process tests of the release verbs share (`release fetch-previous`
//! and the other verbs that read a release): `xtask-fake` linked as `gh`, its `gh-api.json`
//! (the API as each test writes it), its `releases/<tag>/` (the files a release holds) and its `attested` (the
//! attestations the artifacts workflow made of them).
//!
//! A test file includes it with `#[path = "support/release_fake.rs"] mod release_fake;`.
#![allow(dead_code)] // Each test binary uses its own part of this module.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::{Engine, engine::general_purpose::STANDARD};
use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};
use sha2::Digest;

pub const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
pub const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
pub const REPO: &str = "komira-ai/kbf";
pub const ARTIFACTS: &str = "komira-ai/kbf/.github/workflows/artifacts.yml";
pub const SBOM: &str = "https://cyclonedx.org/bom";

/// The exit code, stdout and stderr of a run.
pub type Ran = (Option<i32>, String, String);

pub fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

pub fn sha_of(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// The line `key=` of the verb's output.
pub fn value<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key}= in {out}"))
}

/// The names of the entries of `dir`, sorted.
pub fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the directory")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

pub fn mode_of(path: &Path) -> u32 {
    let meta = std::fs::metadata(path).expect("metadata");
    meta.permissions().mode()
}

/// One entry of a tar: its name, header fields and content.
pub struct Entry {
    pub name: String,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub bytes: Vec<u8>,
}

/// The entries of the tar `bytes`, in order.
pub fn entries_of(bytes: &[u8]) -> Vec<Entry> {
    let mut archive = tar::Archive::new(bytes);
    let mut out = Vec::new();
    for entry in archive.entries().expect("a tar") {
        let mut entry = entry.expect("an entry");
        let header = entry.header();
        let mode = header.mode().expect("the mode");
        let (uid, gid) = (header.uid().expect("uid"), header.gid().expect("gid"));
        let mtime = header.mtime().expect("the time");
        let path = entry.path().expect("a name").into_owned();
        let name = path.to_string_lossy().into_owned();
        let mut content = Vec::new();
        entry.read_to_end(&mut content).expect("the content");
        out.push(Entry {
            name,
            mode,
            uid,
            gid,
            mtime,
            bytes: content,
        });
    }
    out
}

/// The entry of `entries` named `name`.
pub fn entry<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
    let found = entries.iter().find(|e| e.name == name);
    found.unwrap_or_else(|| panic!("no entry {name}"))
}

/// What `kbf --version --json` of a build of `commit` prints: its identity, with `meta_log`.
pub fn identity(commit: &str, meta_log: u64) -> String {
    format!(
        "{{\"commit\":\"{}\",\"meta_log\":{meta_log},\"profile\":\"release\",\"protocol\":1}}\n",
        &commit[..12]
    )
}

/// The fake with the trailer it reads as `kbf`: `--version` names `stamp` and `--version
/// --json` prints `json`.
pub fn kbf_bytes(stamp: &str, json: &str) -> Vec<u8> {
    let mut bytes = std::fs::read(FAKE).expect("the fake");
    bytes.extend_from_slice(format!("\nxtask-fake-kbf\n{stamp}\n{json}").as_bytes());
    bytes
}

/// The flags a host verifies a tarball against its bundle with.
pub fn bundle_flags(commit: &str) -> String {
    format!(
        "--repo {REPO} --signer-workflow {ARTIFACTS} --cert-oidc-issuer \
         https://token.actions.githubusercontent.com --source-ref refs/heads/main \
         --source-digest {commit} --predicate-type https://slsa.dev/provenance/v1 \
         --deny-self-hosted-runners"
    )
}

/// The provenance bundle of the tarball whose digest is `sha`: the fake gh verifies it
/// under the flags `flags`.
pub fn bundle_json(sha: &str, flags: &str) -> String {
    let statement = json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": "asset", "digest": { "sha256": sha } }],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {},
    });
    let b = json!({
        "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
        "verificationMaterial": {},
        "dsseEnvelope": {
            "payloadType": "application/vnd.in-toto+json",
            "payload": STANDARD.encode(statement.to_string()),
            "signatures": [],
        },
        "fakeAttestations": [format!("{sha} {flags}")],
    });
    format!("{b}\n")
}

pub fn refs_path(page: u32) -> String {
    format!("repos/{REPO}/git/matching-refs/tags/v?per_page=100&page={page}")
}

/// GitHub's list of the release tags `tags`.
pub fn refs(tags: &[&str]) -> (String, Value) {
    let list: Vec<Value> = tags
        .iter()
        .map(|t| json!({ "ref": format!("refs/tags/{t}") }))
        .collect();
    (refs_path(1), Value::Array(list))
}

/// GitHub's compare of the tag (or commit) `base` against the commit `sha`.
pub fn compare(base: &str, sha: &str, status: &str) -> (String, Value) {
    (
        format!("repos/{REPO}/compare/{base}...{sha}"),
        json!({ "status": status }),
    )
}

/// The ref of the annotated tag `tag`, and the tag object it names, which names `commit`.
pub fn tag_answers(tag: &str, commit: &str) -> Vec<(String, Value)> {
    let object = hex::encode(sha2::Sha256::digest(tag))[..40].to_owned();
    let reference = json!({ "object": { "type": "tag", "sha": object } });
    let annotation = json!({ "object": { "type": "commit", "sha": commit } });
    vec![
        (format!("repos/{REPO}/git/ref/tags/{tag}"), reference),
        (format!("repos/{REPO}/git/tags/{object}"), annotation),
    ]
}

/// [`tag_answers`], and GitHub's compare of `commit` against `sha`, which says `ahead`.
pub fn annotated(tag: &str, commit: &str, sha: &str) -> Vec<(String, Value)> {
    let mut answers = tag_answers(tag, commit);
    answers.push(compare(commit, sha, "ahead"));
    answers
}

/// GitHub's release of the tag `tag`.
pub fn release_answer(tag: &str) -> (String, Value) {
    (
        format!("repos/{REPO}/releases/tags/{tag}"),
        json!({ "tag_name": tag }),
    )
}

/// What GitHub holds of the tag that wins: its annotated tag, naming `commit` (an ancestor
/// of `sha`), and its release.
pub fn found(tag: &str, commit: &str, sha: &str) -> Vec<(String, Value)> {
    let mut entries = annotated(tag, commit, sha);
    entries.push(release_answer(tag));
    entries
}

/// One test's world: a directory holding the fake as `gh/gh` and an empty `tmp/` (the
/// verb's `TMPDIR`).
pub struct Fx {
    pub root: PathBuf,
    /// Last field: the directory is removed with the fixture.
    _own: TestDir,
}

/// A world for the test `name` under `target/tmp/<group>`.
pub fn fx(group: &str, name: &str) -> Fx {
    let beneath = Path::new(env!("CARGO_TARGET_TMPDIR")).join(group);
    let own = TestDir::under(beneath, name);
    let root = own.to_path_buf();
    std::fs::create_dir_all(root.join("gh")).expect("gh dir");
    std::fs::create_dir_all(root.join("tmp")).expect("tmp dir");
    std::fs::hard_link(FAKE, root.join("gh/gh")).expect("link the fake as gh");
    Fx { root, _own: own }
}

impl Fx {
    /// Writes GitHub's answers, `(path, JSON)`, to the fake's `gh-api.json`.
    pub fn answers(&self, answers: &[(String, Value)]) {
        let map: serde_json::Map<String, Value> = answers.iter().cloned().collect();
        std::fs::write(
            self.root.join("gh/gh-api.json"),
            Value::Object(map).to_string(),
        )
        .expect("gh-api.json");
    }

    /// GitHub with the release tags `tags`, `(tag, status of its compare against sha)`, and
    /// the answers `more`.
    pub fn github_for(&self, sha: &str, tags: &[(&str, &str)], more: Vec<(String, Value)>) {
        let names: Vec<&str> = tags.iter().map(|(t, _)| *t).collect();
        let mut all = vec![refs(&names)];
        all.extend(tags.iter().map(|(t, status)| compare(t, sha, status)));
        all.extend(more);
        self.answers(&all);
    }

    /// What the fake gh was asked, one line per command.
    pub fn gh_args(&self) -> String {
        std::fs::read_to_string(self.root.join("gh/gh-args")).unwrap_or_default()
    }

    pub fn tmp_is_empty(&self) -> bool {
        let mut entries = std::fs::read_dir(self.root.join("tmp")).expect("tmp");
        entries.next().is_none()
    }

    /// The release `tag` of `commit` as the pipeline publishes it, in the fake's
    /// `releases/<tag>/`: for each of `assets`, `(stem, tarball)` with the stem
    /// `<binary>-<commit>-<platform>`, the tarball with its SBOM and bundle, and the
    /// `SHA256SUMS`; and the artifacts workflow's attestations of them, in its `attested`.
    /// For each of `facts`, `(stem, bytes)` with the stem of a `kbf` tarball of `assets`, the
    /// file `<stem>.version.json` holding `bytes`, listed in the `SHA256SUMS` and attested for
    /// build provenance alone, as `release version-facts` and the artifacts workflow leave it.
    pub fn publish_with_facts(
        &self,
        tag: &str,
        commit: &str,
        assets: &[(String, Vec<u8>)],
        facts: &[(String, Vec<u8>)],
    ) {
        let dir = self.root.join("gh/releases").join(tag);
        std::fs::create_dir_all(&dir).expect("release dir");
        let signed = format!(
            "--repo {REPO} --signer-workflow {ARTIFACTS} --source-ref refs/heads/main \
             --deny-self-hosted-runners"
        );
        let mut lines: Vec<(String, String)> = Vec::new();
        let mut attested = String::new();
        for (stem, tgz) in assets {
            let bom = json!({ "bomFormat": "CycloneDX", "stem": stem });
            let sbom = format!("{bom}\n");
            let (tgz_sha, sbom_sha) = (sha_of(tgz), sha_of(sbom.as_bytes()));
            std::fs::write(dir.join(format!("{stem}.tar.gz")), tgz).expect("tarball");
            std::fs::write(dir.join(format!("{stem}.cdx.json")), &sbom).expect("sbom");
            std::fs::write(
                dir.join(format!("{stem}.tar.gz.sigstore.json")),
                bundle_json(&tgz_sha, &bundle_flags(commit)),
            )
            .expect("bundle");
            attested.push_str(&format!("{tgz_sha} {signed}\n{sbom_sha} {signed}\n"));
            attested.push_str(&format!("{tgz_sha} {signed} --predicate-type {SBOM}\n"));
            lines.push((tgz_sha, format!("{stem}.tar.gz")));
            lines.push((sbom_sha, format!("{stem}.cdx.json")));
        }
        for (stem, bytes) in facts {
            let sha = sha_of(bytes);
            std::fs::write(dir.join(format!("{stem}.version.json")), bytes).expect("facts");
            attested.push_str(&format!("{sha} {signed}\n"));
            lines.push((sha, format!("{stem}.version.json")));
        }
        lines.sort_by(|a, b| a.1.cmp(&b.1));
        let sums: String = lines.iter().map(|(s, n)| format!("{s}  {n}\n")).collect();
        std::fs::write(dir.join("SHA256SUMS"), sums).expect("SHA256SUMS");
        let store = self.root.join("gh/attested");
        let mut text = std::fs::read_to_string(&store).unwrap_or_default();
        text.push_str(&attested);
        std::fs::write(&store, text).expect("attested");
    }

    /// `cargo xtask <args> --gh <the fake>` with `TMPDIR` at `tmp`, planting the fake's
    /// defect `defect` (or none).
    pub fn xtask(&self, tmp: &Path, args: &[&str], defect: Option<&str>) -> Ran {
        let gh = self.root.join("gh/gh");
        let mut cmd = Command::new(XTASK);
        cmd.args(args)
            .arg("--gh")
            .arg(&gh)
            .env("TMPDIR", tmp)
            .env_remove("XTASK_FAKE_DEFECT");
        if let Some(d) = defect {
            cmd.env("XTASK_FAKE_DEFECT", d);
        }
        let out = cmd.output().expect("run xtask");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (out.status.code(), text(&out.stdout), text(&out.stderr))
    }
}
