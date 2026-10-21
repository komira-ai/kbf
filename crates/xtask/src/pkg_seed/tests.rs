use std::os::unix::fs::symlink;

use flate2::{Compression, GzBuilder};
use kbf_testkit::dir::TestDir;
use serde_json::json;

use super::*;

const A: &str = "0123456789abcdef0123456789abcdef01234567";
const B: &str = "fedcba9876543210fedcba9876543210fedcba98";
const X86: Platform = Platform::LinuxX86_64;
const ARM: Platform = Platform::LinuxArm64;
/// The binary in the fixture's tarball: not a real `kbf`.
const BINARY: &[u8] = b"\x7fELF the kbf of the fixture";
/// The fixture's bundle: obviously not a sigstore bundle.
const BUNDLE: &str = "{\"fake\":\"a test fixture, not a sigstore bundle\"}";
/// The `seq` the fixture's facts declare.
const SEQ: u64 = 801;
/// What `check` says of a path that is not a regular file of the seed.
const NOT_REGULAR: &str = "is missing or is not a regular file";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-pkg-seed-unit"), name)
}

/// What `kbf --version --json` printed for the release build of `commit` on `platform`.
fn facts(commit: &str, platform: Platform) -> Value {
    let short = &commit[..12];
    json!({
        "admin_protocol": 1,
        "commit": short,
        "meta_log": 4,
        "min_protocol": 3,
        "platform": platform.name(),
        "profile": "release",
        "protocol": 4,
        "relay": 1,
        "seq": SEQ,
        "state_schema": 7,
        "version": "0.1.0",
    })
}

fn json_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("json")
}

fn put(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).expect(name);
}

/// Writes `value` over the file `path`.
fn rewrite(path: &Path, value: &Value) {
    std::fs::write(path, json_bytes(value)).expect("write");
}

fn without(value: &mut Value, key: &str) {
    value.as_object_mut().expect("an object").remove(key);
}

/// Writes the manifest of the seed directory `dir` again, after `change`.
fn edit_manifest(dir: &Path, change: impl FnOnce(&mut Value)) {
    let held = std::fs::read(dir.join(MANIFEST)).expect("manifest");
    let mut value: Value = serde_json::from_slice(&held).expect("json");
    change(&mut value);
    let bytes = serde_json::to_vec_pretty(&value).expect("json");
    put(dir, MANIFEST, &bytes);
}

/// Changes the first byte of the file `path`.
fn flip(path: &Path) {
    let mut bytes = std::fs::read(path).expect("read");
    bytes[0] ^= 0xff;
    std::fs::write(path, bytes).expect("write");
}

/// The names in `dir`, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The problems of the refusal `got` is: its message, a line each.
fn problems_of<T: std::fmt::Debug>(got: Result<T, Error>) -> Vec<String> {
    let message = got.expect_err("a refusal").to_string();
    message.lines().map(str::to_owned).collect()
}

/// The one problem in `problems`.
fn only(mut problems: Vec<String>) -> String {
    assert_eq!(problems.len(), 1, "{problems:?}");
    problems.remove(0)
}

/// What `check` says of `path`, a file of the seed that is not a regular file.
fn not_regular(path: &Path) -> String {
    format!("{} {NOT_REGULAR}", path.display())
}

/// A gzip tar of the regular files `names`, each holding one byte.
fn tarball_of(names: &[&str]) -> Vec<u8> {
    let gzip = GzBuilder::new().write(Vec::new(), Compression::fast());
    let mut tar = tar::Builder::new(gzip);
    for name in names {
        let mut header = tar::Header::new_ustar();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        let added = tar.append_data(&mut header, name, &b"x"[..]);
        added.expect("tar into memory");
    }
    tar.into_inner().expect("tar").finish().expect("gzip")
}

/// A release in `dist`, a trust directory and their digests, for one commit and platform.
struct Fixture {
    dir: TestDir,
    dist: PathBuf,
    commit: &'static str,
    platform: Platform,
    trust: TrustDigests,
}

impl Fixture {
    fn new(name: &str, commit: &'static str, platform: Platform) -> Fixture {
        let dir = scratch(name);
        let (dist, trust_dir) = (dir.join("dist"), dir.join("trust"));
        std::fs::create_dir_all(&dist).expect("dist");
        std::fs::create_dir_all(&trust_dir).expect("trust");
        let trust_files = [
            ("gh", "the pinned gh"),
            ("installer.conf", "SOURCE_REFS=refs/heads/main\n"),
            ("trusted_root.jsonl", "{\"mediaType\":\"root\"}\n"),
        ];
        for (file, text) in trust_files {
            put(&trust_dir, file, text.as_bytes());
        }
        let names = Names::of(commit, platform);
        let tarball = package::tarball("kbf", BINARY).expect("tarball");
        put(&dist, &names.tarball, &tarball);
        put(&dist, &names.bundle, BUNDLE.as_bytes());
        put(&dist, &names.facts, &json_bytes(&facts(commit, platform)));
        let trust = TrustDigests::of_dir(&trust_dir).expect("digests");
        Fixture {
            dir,
            dist,
            commit,
            platform,
            trust,
        }
    }

    fn names(&self) -> Names {
        Names::of(self.commit, self.platform)
    }

    fn inputs(&self) -> Inputs<'_> {
        Inputs {
            dist: &self.dist,
            commit: self.commit,
            platform: self.platform,
            trust: self.trust,
        }
    }

    fn expect(&self) -> Expect<'_> {
        Expect {
            commit: self.commit,
            platform: self.platform,
            trust: self.trust,
        }
    }

    /// Where `written` puts the seed.
    fn seed_dir(&self) -> PathBuf {
        self.dir.join("seed")
    }

    /// The seed of this fixture, written.
    fn written(&self) -> PathBuf {
        let out = self.seed_dir();
        write(&self.inputs(), &out).expect("write");
        out
    }

    /// The problems `check` finds in the seed directory `dir` against this fixture.
    fn problems_in(&self, dir: &Path) -> Vec<String> {
        check(dir, &self.expect()).expect("check")
    }
}

/// A good seed is four files, named for the commit and the platform and nothing else, that
/// `check` accepts: the tarball, the bundle and the facts byte for byte as the release has
/// them, and a manifest the installer's own reader parses to the values of the inputs, whose
/// file names (as the reader derives them) are the names written. Two builds are the same
/// bytes, and `write` makes the directory of exactly those files.
///
/// Catches: a file name taken from an input (any name in `dist`), a file changed on its way
/// in (a re-compressed tarball no attestation covers), a manifest with a time or an order
/// that varies, a manifest field read from the wrong input, a builder and a reader that
/// name the files differently, and a directory with a file more or less than the four.
///
/// Mutants it catches: the facts file copied with a changed byte; `seq` or `protocol`
/// read from the wrong field; the bundle digest taken of the tarball; the `.version.json`
/// suffix changed; the manifest not first; a trust digest swapped for another's.
#[test]
fn a_good_seed_is_four_named_files_that_check_accepts_and_build_the_same_bytes() {
    let f = Fixture::new("good", A, X86);
    let seed = build(&f.inputs()).expect("build");
    let again = build(&f.inputs()).expect("again");
    assert_eq!(again, seed, "two builds differ");
    assert_eq!(seed.commit12, &A[..12]);
    let names = f.names();
    assert_eq!(names.tarball, format!("kbf-{A}-linux-x86_64.tar.gz"));
    let bundle = format!("kbf-{A}-linux-x86_64.tar.gz.sigstore.json");
    assert_eq!(names.bundle, bundle);
    assert_eq!(names.facts, format!("kbf-{A}-linux-x86_64.version.json"));
    let listed: Vec<&str> = seed.files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(listed, names.all());
    for (name, bytes) in &seed.files[1..] {
        let given = std::fs::read(f.dist.join(name)).expect("input");
        assert_eq!(bytes, &given, "{name} is not the input's bytes");
    }

    let (_, json) = &seed.files[0];
    let manifest = Manifest::parse(json).expect("parses");
    let (m, t) = (&manifest, f.trust);
    assert_eq!(m.tarball_name(), names.tarball);
    assert_eq!(m.bundle_name(), names.bundle);
    assert_eq!(m.commit.as_str(), A);
    assert_eq!(m.seq, SEQ);
    assert_eq!(m.platform, "linux-x86_64");
    assert_eq!(m.protocol, 4);
    assert_eq!(m.min_protocol, 3);
    assert_eq!(m.state_schema, 7);
    assert_eq!(m.profile, "release");
    let (tarball, bundle) = (&seed.files[1].1, &seed.files[2].1);
    assert_eq!(m.tarball_size, tarball.len() as u64);
    let digest = |bytes: &[u8]| Sha256::of(bytes).to_string();
    assert_eq!(m.tarball_sha256.as_str(), digest(tarball));
    assert_eq!(m.bundle_sha256.as_str(), digest(bundle));
    let trust = [
        (m.gh_sha256.as_str(), t.gh),
        (m.trusted_root_sha256.as_str(), t.trusted_root),
        (m.installer_conf_sha256.as_str(), t.installer_conf),
    ];
    for (found, held) in trust {
        assert_eq!(found, held.to_string());
    }
    let text = String::from_utf8(json.clone()).expect("UTF-8");
    let head = format!("{{\n  \"commit\": \"{A}\",\n");
    assert!(text.starts_with(&head), "{text}");
    assert!(text.ends_with("}\n"), "{text}");

    let out = f.written();
    let mut want = names.all().to_vec();
    want.sort_unstable();
    assert_eq!(listing(&out), want);
    assert_eq!(f.problems_in(&out), Vec::<String>::new());
    for (name, bytes) in &seed.files {
        let kept = std::fs::read(out.join(name)).expect("written");
        assert_eq!(&kept, bytes, "{name}");
    }
    let lines = write(&f.inputs(), &f.dir.join("again")).expect("write");
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert!(lines[0].starts_with("wrote "), "{lines:?}");
    let first = Sha256::of(&seed.files[0].1);
    assert_eq!(lines[1], format!("{first}  {MANIFEST}"));
}

/// The platform is part of every name: an x86_64 seed checked as arm64 is refused for its
/// platform, and an arm64 release builds, is named for arm64 and checks as arm64.
///
/// Catches: a name or a manifest platform fixed to one architecture, and a check that does
/// not compare the platform.
///
/// Mutants it catches: `want.platform` compared with a constant; the platform left out of
/// `Names::of`.
#[test]
fn a_seed_is_for_its_platform_and_checks_only_as_that_platform() {
    let f = Fixture::new("platform", A, X86);
    let out = f.written();
    let as_arm = Expect {
        platform: ARM,
        ..f.expect()
    };
    let problems = check(&out, &as_arm).expect("check");
    let said = "manifest platform is linux-x86_64, not linux-arm64";
    assert!(problems.contains(&said.to_owned()), "{problems:?}");

    let arm = Fixture::new("platform-arm", A, ARM);
    let out = arm.written();
    assert_eq!(arm.problems_in(&out), Vec::<String>::new());
    let name = format!("kbf-{A}-linux-arm64.tar.gz");
    assert!(listing(&out).contains(&name), "{:?}", listing(&out));
}

/// `build` and `check` take only a full lowercase commit and a Linux platform, and say so
/// as a usage error (exit 2), before they read a file.
///
/// Catches: a short or upper-case commit making short names, and a Mac release made into a
/// Linux seed.
///
/// Mutants it catches: the commit check dropped; the platform check dropped.
#[test]
fn a_commit_that_is_not_full_and_a_mac_platform_are_usage_errors() {
    let f = Fixture::new("usage", A, X86);
    let out = f.written();
    let upper = A.to_uppercase();
    for commit in ["abc", upper.as_str(), ""] {
        let inputs = Inputs {
            commit,
            ..f.inputs()
        };
        let want = build_commit::not_full_commit("--commit", commit);
        assert_eq!(build(&inputs).expect_err("build"), want);
        let expect = Expect {
            commit,
            ..f.expect()
        };
        assert_eq!(check(&out, &expect).expect_err("check"), want);
    }
    let why = "--platform darwin-arm64: only Linux seeds are made";
    let want = Error::Usage(why.to_owned());
    let inputs = Inputs {
        platform: Platform::DarwinArm64,
        ..f.inputs()
    };
    assert_eq!(build(&inputs).expect_err("build"), want);
    let expect = Expect {
        platform: Platform::DarwinArm64,
        ..f.expect()
    };
    assert_eq!(check(&out, &expect).expect_err("check"), want);
}

/// `write` makes a new directory and only when the seed builds: an existing `out` is
/// refused whatever it holds, a refused release leaves no directory, parents are made, and a
/// parent that is a file is an error naming what could not be made.
///
/// Catches: a seed written into a directory that holds other files, a half-made directory
/// after a refusal, and a write failure reported as success.
///
/// Mutants it catches: the existence check dropped (the second write succeeds); `build`
/// called after the directory is made (a refusal leaves it).
#[test]
fn write_makes_a_new_directory_and_only_for_a_seed_that_builds() {
    let f = Fixture::new("write", A, X86);
    let taken = f.dir.join("taken");
    std::fs::create_dir(&taken).expect("dir");
    let refused = write(&f.inputs(), &taken).expect_err("exists");
    let said = format!("{} already exists", taken.display());
    assert_eq!(refused.to_string(), said);
    assert_eq!(listing(&taken), Vec::<String>::new());

    let deep = f.dir.join("a/b/seed");
    write(&f.inputs(), &deep).expect("parents are made");
    assert_eq!(f.problems_in(&deep), Vec::<String>::new());
    let again = write(&f.inputs(), &deep).expect_err("a second write");
    assert!(again.to_string().ends_with("already exists"), "{again}");

    let tarball = f.dist.join(f.names().tarball);
    std::fs::remove_file(tarball).expect("remove");
    let none = f.dir.join("none");
    only(problems_of(write(&f.inputs(), &none)));
    assert!(!none.exists(), "a refused release left a directory");

    put(&f.dir, "plain", b"a file");
    let under_a_file = f.dir.join("plain/seed");
    let good = Fixture::new("write-good", A, X86);
    let inputs = good.inputs();
    let failed = write(&inputs, &under_a_file).expect_err("no dir");
    assert!(failed.to_string().starts_with("create "), "{failed}");
}

/// A seed whose tarball or bundle is not the bytes the manifest names is refused with the
/// digest that differs: one flipped byte in the tarball, one in the bundle, and (a digest
/// cannot differ without a size doing so, unless the manifest says it) a wrong size.
///
/// Catches: a tarball changed after the manifest was made (an attestation that no longer
/// covers it), a bundle swapped, and a size that is not compared.
///
/// Mutants it catches: the tarball digest comparison skipped; the bundle digest comparison
/// skipped; the size comparison skipped; the two digests compared with each other's.
#[test]
fn a_flipped_byte_in_the_tarball_or_the_bundle_or_a_wrong_size_is_refused() {
    let f = Fixture::new("flipped", A, X86);
    let names = f.names();
    let out = f.written();
    flip(&out.join(&names.tarball));
    let problem = only(f.problems_in(&out));
    assert!(problem.starts_with("tarball sha256 is "), "{problem}");
    flip(&out.join(&names.tarball));
    assert_eq!(f.problems_in(&out), Vec::<String>::new());

    flip(&out.join(&names.bundle));
    let problem = only(f.problems_in(&out));
    assert!(problem.starts_with("bundle sha256 is "), "{problem}");
    flip(&out.join(&names.bundle));

    let meta = std::fs::metadata(out.join(&names.tarball)).expect("stat");
    let size = meta.len();
    edit_manifest(&out, |m| m["tarball_size"] = json!(size + 1));
    let want = format!("tarball size is {size}, not {}", size + 1);
    assert_eq!(f.problems_in(&out), vec![want]);
}

/// A release of another commit does not make this commit's seed: a dist of B's files asked
/// for A's seed lacks A's names; B's seed checked as A's is refused for its manifest's
/// commit; B's facts under A's file name are refused for the commit they state.
///
/// Catches: files taken by what is in the directory instead of by the commit's names, a
/// check that does not compare the manifest's commit, and facts of another build accepted.
///
/// Mutants it catches: names derived from a file found in `dist`; the commit comparison in
/// `manifest_unequal` dropped; `Version::check` dropped from `judge_facts`.
#[test]
fn another_commits_files_do_not_make_this_commits_seed() {
    let other = Fixture::new("other-commit", B, X86);
    let for_a = Inputs {
        commit: A,
        ..other.inputs()
    };
    let problems = problems_of(build(&for_a));
    let wanted = Names::of(A, X86);
    let all = wanted.all();
    assert_eq!(problems.len(), 3, "{problems:?}");
    for (problem, name) in problems.iter().zip(&all[1..]) {
        assert!(problem.contains(name), "{problem}");
        assert!(problem.ends_with(NOT_REGULAR), "{problem}");
    }

    let out = other.written();
    let as_a = Expect {
        commit: A,
        ..other.expect()
    };
    let problems = check(&out, &as_a).expect("check");
    let said = format!("manifest commit is {B}, not {A}");
    assert!(problems.contains(&said), "{problems:?}");
    let extra = format!("unexpected file kbf-{B}-linux-x86_64.tar.gz");
    assert!(problems.contains(&extra), "{problems:?}");

    let f = Fixture::new("facts-of-b", A, X86);
    put(&f.dist, &f.names().facts, &json_bytes(&facts(B, X86)));
    let said = "the version facts: it says commit fedcba987654, and the release is 0123456789ab";
    assert_eq!(only(problems_of(build(&f.inputs()))), said);
}

/// The manifest's digests of the three trust files are held to the package's: a changed
/// `gh`, a changed trusted root and a changed `installer.conf` are each refused, naming
/// the file and not the others.
///
/// Catches: a package assembled from two builds (a seed beside the trust files of another
/// generation), and one digest compared against another file's.
///
/// Mutants it catches: any of the three comparisons dropped; `gh` and `installer.conf`
/// compared with each other's digests.
#[test]
fn a_changed_trust_file_is_refused_by_name() {
    let f = Fixture::new("trust", A, X86);
    let out = f.written();
    let other = Sha256::of(b"a file of another build");
    let mut drifted = [f.trust; 3];
    drifted[0].gh = other;
    drifted[1].trusted_root = other;
    drifted[2].installer_conf = other;
    let names = ["gh", "trusted_root", "installer_conf"];
    let held = [f.trust.gh, f.trust.trusted_root, f.trust.installer_conf];
    for ((trust, name), real) in drifted.into_iter().zip(names).zip(held) {
        let want = Expect {
            trust,
            ..f.expect()
        };
        let problems = check(&out, &want).expect("check");
        let said = format!("manifest {name} sha256 is {real}, not {other}");
        assert_eq!(problems, vec![said]);
    }
}

/// The digests of the trust files are those of the three names `TRUST_FILES` gives, each
/// field of its own file, and a file that is not there is an error naming it.
///
/// Catches: two fields read from one file, and a missing file read as empty.
///
/// Mutants it catches: `gh` and `installer.conf` swapped in `of_dir`; the read error of any
/// of the three ignored.
#[test]
fn trust_digests_are_those_of_the_three_files_and_each_must_exist() {
    let dir = scratch("trust-digests");
    let names = ["gh", "installer.conf", "trusted_root.jsonl"];
    for (name, text) in names.into_iter().zip(["1", "2", "3"]) {
        put(&dir, name, text.as_bytes());
    }
    let got = TrustDigests::of_dir(&dir).expect("digests");
    assert_eq!(got.gh, Sha256::of(b"1"));
    assert_eq!(got.installer_conf, Sha256::of(b"2"));
    assert_eq!(got.trusted_root, Sha256::of(b"3"));
    for name in names {
        std::fs::remove_file(dir.join(name)).expect("remove");
        let err = TrustDigests::of_dir(&dir).expect_err("a missing file");
        assert!(err.to_string().contains(name), "{name}: {err}");
        put(&dir, name, b"back");
    }
}

/// Facts that are not those of the release build of the commit, with a counted `seq`, for
/// this platform, are refused, each with its own reason and only that one: `seq` 0 or
/// absent, a platform absent or another, a debug build, a number the manifest cannot hold,
/// text that is not the report, and a report over the cap.
///
/// Catches: a seed whose binary cannot be put in order, one for another platform, a debug
/// build shipped, and a protocol number cut to 32 bits by a cast.
///
/// Mutants it catches: the `seq` test made `== 0` only (an absent `seq` passes); the
/// platform test dropped; `Version::check` dropped; `u32::try_from` made `as u32`.
#[test]
fn facts_that_are_not_a_counted_release_build_for_the_platform_are_refused() {
    type Change = fn(&mut Value);
    let f = Fixture::new("facts", A, X86);
    let names = f.names();
    let cases: [(&str, Change, &str); 8] = [
        ("seq 0", |v| v["seq"] = json!(0), "have no seq, or seq 0"),
        ("no seq", |v| without(v, "seq"), "have no seq, or seq 0"),
        (
            "no platform",
            |v| without(v, "platform"),
            "say no platform, not linux-x86_64",
        ),
        (
            "another platform",
            |v| v["platform"] = json!("linux-arm64"),
            "say linux-arm64, not linux-x86_64",
        ),
        (
            "a debug build",
            |v| v["profile"] = json!("debug"),
            "not \"release\"",
        ),
        (
            "protocol over 32 bits",
            |v| v["protocol"] = json!(1_u64 << 32),
            "protocol 4294967296 is over 32 bits",
        ),
        (
            "min_protocol over 32 bits",
            |v| v["min_protocol"] = json!(1_u64 << 32),
            "min_protocol 4294967296 is over 32 bits",
        ),
        (
            "state_schema over 32 bits",
            |v| v["state_schema"] = json!(1_u64 << 32),
            "state_schema 4294967296 is over 32 bits",
        ),
    ];
    for (what, change, said) in cases {
        let mut value = facts(A, X86);
        change(&mut value);
        rewrite(&f.dist.join(&names.facts), &value);
        let problem = only(problems_of(build(&f.inputs())));
        assert!(problem.contains(said), "{what}: {problem}");
    }
    let oversize = vec![b' '; 5000];
    let texts: [(&str, &[u8]); 2] = [("not JSON", b"kbf 0.1.0"), ("over the cap", &oversize)];
    for (what, bytes) in texts {
        put(&f.dist, &names.facts, bytes);
        let problem = only(problems_of(build(&f.inputs())));
        assert!(
            problem.starts_with("the version facts: it "),
            "{what}: {problem}"
        );
    }
}

/// A facts file in a seed directory that no longer agrees with the manifest is refused, field
/// by field: the `seq`, each of the three numbers, and the profile (a manifest that says
/// `debug` against facts that say `release`). Facts that are themselves refused are
/// reported, and the comparison with the manifest is not made against them.
///
/// Catches: a manifest and a facts file that drifted (one rewritten), and any one field left
/// out of the comparison.
///
/// Mutants it catches: any of the five comparisons in `facts_unequal` dropped; the numbers
/// compared as text with a different spelling.
#[test]
fn a_facts_file_that_disagrees_with_the_manifest_is_refused_field_by_field() {
    let f = Fixture::new("disagree", A, X86);
    let out = f.written();
    let facts_file = out.join(f.names().facts);
    let changes: [(&str, u64, u64); 4] = [
        ("seq", SEQ, SEQ + 1),
        ("protocol", 4, 5),
        ("min_protocol", 3, 2),
        ("state_schema", 7, 8),
    ];
    for (key, was, now) in changes {
        let mut value = facts(A, X86);
        value[key] = json!(now);
        rewrite(&facts_file, &value);
        let want = format!("version facts {key} is {now}, not {was}");
        assert_eq!(f.problems_in(&out), vec![want]);
    }
    rewrite(&facts_file, &facts(A, X86));
    edit_manifest(&out, |m| m["profile"] = json!("debug"));
    let want = "version facts profile is release, not debug".to_owned();
    assert_eq!(f.problems_in(&out), vec![want]);

    let mut value = facts(A, X86);
    value["seq"] = json!(0);
    rewrite(&facts_file, &value);
    let problem = only(f.problems_in(&out));
    assert!(problem.contains("have no seq, or seq 0"), "{problem}");
}

/// A directory of the wrong shape is refused: files more (named, in order), a link in place
/// of a file even when its target is the right bytes, a directory where a file belongs, a
/// file less, a manifest less (nothing else is judged against it), and a directory that
/// cannot be listed (an error, not a clean check).
///
/// Catches: a seed that carries a file the package builder did not make (a second tarball,
/// a script), a link followed out of the directory, and a missing file passed over.
///
/// Mutants it catches: the list of unexpected names dropped or unsorted; `symlink_metadata`
/// made `metadata` (the link is followed); the manifest-less check going on to compare.
#[test]
fn a_seed_directory_of_the_wrong_shape_is_refused() {
    let f = Fixture::new("shape", A, X86);
    let names = f.names();
    let out = f.written();
    put(&out, "b.txt", b"extra");
    put(&out, "a.txt", b"extra");
    std::fs::create_dir(out.join("zzz")).expect("dir");
    let extras = ["a.txt", "b.txt", "zzz"];
    let want = extras.map(|n| format!("unexpected file {n}"));
    assert_eq!(f.problems_in(&out), want);
    for extra in ["a.txt", "b.txt"] {
        std::fs::remove_file(out.join(extra)).expect("remove");
    }
    std::fs::remove_dir(out.join("zzz")).expect("remove");

    let bundle = out.join(&names.bundle);
    let target = f.dir.join("elsewhere");
    std::fs::rename(&bundle, &target).expect("move");
    symlink(&target, &bundle).expect("link");
    assert_eq!(f.problems_in(&out), vec![not_regular(&bundle)]);
    std::fs::remove_file(&bundle).expect("remove");
    std::fs::rename(&target, &bundle).expect("move back");

    let tarball = out.join(&names.tarball);
    let kept = f.dir.join("tarball-kept");
    std::fs::rename(&tarball, &kept).expect("move");
    std::fs::create_dir(&tarball).expect("a directory in its place");
    assert_eq!(f.problems_in(&out), vec![not_regular(&tarball)]);
    std::fs::remove_dir(&tarball).expect("remove");
    assert_eq!(f.problems_in(&out), vec![not_regular(&tarball)]);
    std::fs::rename(&kept, &tarball).expect("move back");

    flip(&bundle);
    let manifest = out.join(MANIFEST);
    std::fs::remove_file(&manifest).expect("remove");
    assert_eq!(f.problems_in(&out), vec![not_regular(&manifest)]);

    let nowhere = f.dir.join("not-there");
    let gone = check(&nowhere, &f.expect()).expect_err("none");
    assert!(gone.to_string().starts_with("read "), "{gone}");
}

/// A manifest the installer's reader refuses is refused with the reader's words: a key it
/// does not know, text that is not JSON; and the check goes no further against it.
///
/// Catches: a check that parses the manifest its own way and passes what the installer
/// would refuse on a host.
///
/// Mutants it catches: `Manifest::parse` replaced by a lenient `serde_json::from_slice`
/// (the unknown key passes).
#[test]
fn a_manifest_the_installer_would_refuse_is_refused() {
    let f = Fixture::new("manifest", A, X86);
    let out = f.written();
    edit_manifest(&out, |m| m["extra"] = json!(1));
    let problem = only(f.problems_in(&out));
    assert!(
        problem.starts_with("manifest.json: unknown field `extra`"),
        "{problem}"
    );
    put(&out, MANIFEST, b"not json");
    let problem = only(f.problems_in(&out));
    assert!(problem.starts_with("manifest.json: "), "{problem}");
}

/// A bundle that is not a JSON object and a tarball that does not hold the one regular
/// file `kbf` are refused, each with its reason: bundle text that is not JSON, an empty
/// bundle, JSON that is not an object, a tarball that is not gzip, one of another file's
/// name, one of `kbf` and a second file (the installer refuses it on every host), and a
/// link in the dist where a file belongs.
///
/// Catches: a seed made of a bundle that is an HTML error page or empty, a tarball of
/// something else or of more than the binary, and a link in the dist followed out of it.
///
/// Mutants it catches: `bundle_checked` accepting any JSON; `tarball_checked` accepting any
/// tar, or a tar that holds `kbf` among other files; `take` made to follow links.
#[test]
fn a_bundle_that_is_not_an_object_and_a_tarball_without_kbf_are_refused() {
    let f = Fixture::new("inputs", A, X86);
    let names = f.names();
    let bundles: [(&[u8], &str); 4] = [
        (b"not json", "the bundle is not JSON: "),
        (b"", "the bundle is not JSON: "),
        (b"[]", "the bundle is JSON, but not an object"),
        (b"\"x\"", "the bundle is JSON, but not an object"),
    ];
    for (bytes, said) in bundles {
        put(&f.dist, &names.bundle, bytes);
        let problem = only(problems_of(build(&f.inputs())));
        assert!(problem.starts_with(said), "{problem}");
    }
    put(&f.dist, &names.bundle, BUNDLE.as_bytes());

    let real = package::tarball("kbf", BINARY).expect("tarball");
    let other = package::tarball("not-kbf", BINARY).expect("tarball");
    let two = tarball_of(&["kbf", "README"]);
    let tarballs: [(&[u8], &str); 3] = [
        (&other, "the tarball holds [\"not-kbf\"]"),
        (&two, "the tarball holds [\"kbf\", \"README\"]"),
        (b"plain text", "the tarball: not a gzip stream: "),
    ];
    for (bytes, said) in tarballs {
        put(&f.dist, &names.tarball, bytes);
        let problem = only(problems_of(build(&f.inputs())));
        assert!(problem.starts_with(said), "{problem}");
    }
    put(&f.dist, &names.tarball, &real);

    let moved = f.dir.join("real-bundle");
    std::fs::rename(f.dist.join(&names.bundle), &moved).expect("move");
    symlink(&moved, f.dist.join(&names.bundle)).expect("link");
    let problem = only(problems_of(build(&f.inputs())));
    assert!(problem.ends_with(NOT_REGULAR), "{problem}");
}

/// Every problem of the inputs comes back in one refusal, in the order the files are read
/// (tarball, bundle, facts): a missing tarball, a bundle that is not an object and facts of
/// seq 0 are three lines of one `Refused`, and nothing is written.
///
/// Catches: a builder that stops at the first problem (the operator fixes one, reruns, and
/// finds the next), and one that goes on to make a manifest of inputs it refused.
///
/// Mutants it catches: an early return after the first refused input.
#[test]
fn several_problems_come_back_in_one_refusal() {
    let f = Fixture::new("several", A, X86);
    let names = f.names();
    std::fs::remove_file(f.dist.join(&names.tarball)).expect("remove");
    put(&f.dist, &names.bundle, b"[]");
    let mut value = facts(A, X86);
    value["seq"] = json!(0);
    rewrite(&f.dist.join(&names.facts), &value);
    let problems = problems_of(build(&f.inputs()));
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(problems[0].contains(&names.tarball), "{problems:?}");
    assert!(problems[1].contains("not an object"), "{problems:?}");
    assert!(problems[2].contains("seq 0"), "{problems:?}");
}

/// The verb reads the trust digests from a directory and writes the seed: the directory it
/// makes checks clean, and a trust directory with a file missing is an error before
/// anything is made.
///
/// Catches: the verb not wired to the trust directory it is given (digests of nothing), and
/// a seed made when the trust files cannot be read.
///
/// Mutants it catches: `run` passing the digests of `--dist` in place of `--trust`.
#[test]
fn the_verb_makes_the_seed_from_a_trust_directory_or_fails_before_it_makes_any() {
    let f = Fixture::new("verb", A, X86);
    let args = Args {
        dist: f.dist.clone(),
        commit: A.to_owned(),
        platform: X86,
        trust: f.dir.join("trust"),
        out: f.seed_dir(),
    };
    let lines = run(&args).expect("run");
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert_eq!(f.problems_in(&f.seed_dir()), Vec::<String>::new());
    let failing = Args {
        trust: f.dir.join("no-trust"),
        out: f.dir.join("not-made"),
        ..args
    };
    let err = run(&failing).expect_err("no trust files");
    assert!(err.to_string().starts_with("read "), "{err}");
    assert!(!f.dir.join("not-made").exists());
}

/// The file names the installer derives from a manifest are held to the ones written: the
/// manifest of a seed has no problem against its own names, and against the names of another
/// commit it has a line for the tarball and a line for the bundle, each naming both.
///
/// Catches: a builder and a reader that name the files differently (a seed whose files the
/// installer would look for under other names).
///
/// Mutants it catches: `naming` returning nothing; the tarball name or the bundle name left
/// out of it; the written name and the derived one swapped. The call in `build` is not shown
/// absent by this test: the reader and the builder agree, so the call never has a line to add.
#[test]
fn the_installers_file_names_are_held_to_the_ones_written() {
    let f = Fixture::new("naming", A, X86);
    let seed = build(&f.inputs()).expect("build");
    let manifest = Manifest::parse(&seed.files[0].1).expect("manifest");
    let (right, wrong) = (Names::of(A, X86), Names::of(B, X86));
    assert_eq!(naming(&manifest, &right), Vec::<String>::new());
    let (rt, rb) = (&right.tarball, &right.bundle);
    let (wt, wb) = (&wrong.tarball, &wrong.bundle);
    let want = [
        format!("the installer's tarball name is {rt}, not {wt}"),
        format!("the installer's bundle name is {rb}, not {wb}"),
    ];
    assert_eq!(naming(&manifest, &wrong), want);
}
