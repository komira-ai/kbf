use std::path::PathBuf;

use super::*;
use crate::package::tarball;
use crate::pkg::tree::Tree;
use crate::pkg::{package, xar};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const MAIN: &str = "refs/heads/main";
const FEATURE: &str = "refs/heads/feature/arch-v2";
const BINARY_BYTES: &[u8] = b"the attested kbf-installer";
const GH_BYTES: &[u8] = b"the pinned gh";
const ROOT: &str = "{\"mediaType\":\"trustedroot\"}\n";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-pkg-macos-asset-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn plist() -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/macos/com.komira.kbf-installer.plist"),
    )
    .expect("the committed property list")
}

/// What a package is made of, each part replaceable: the good package is `Parts::good()`.
struct Parts {
    binary: Option<Vec<u8>>,
    binary_mode: u32,
    plist: Option<Vec<u8>>,
    plist_mode: u32,
    log_dir: bool,
    gh: Option<Vec<u8>>,
    gh_mode: u32,
    root: Option<Vec<u8>>,
    conf: Option<Vec<u8>>,
    state_mode: u32,
    extra_file: Option<&'static str>,
    in_state: bool,
    extra_dir: Option<&'static str>,
    version: String,
}

impl Parts {
    fn good() -> Parts {
        Parts {
            binary: Some(BINARY_BYTES.to_vec()),
            binary_mode: 0o755,
            plist: Some(plist()),
            plist_mode: 0o644,
            log_dir: true,
            gh: Some(GH_BYTES.to_vec()),
            gh_mode: 0o755,
            root: Some(ROOT.as_bytes().to_vec()),
            conf: Some(pkg_trust::conf(MAIN).into_bytes()),
            state_mode: STATE_MODE,
            extra_file: None,
            in_state: false,
            extra_dir: None,
            version: format!("0.1.0+{}", &COMMIT[..12]),
        }
    }

    /// The package's bytes.
    fn build(&self) -> Vec<u8> {
        let mut t = Tree::rooted();
        if self.log_dir {
            t.dir(LOG_DIR).unwrap();
        }
        t.dir_mode(STATE_DIR, self.state_mode).unwrap();
        if let Some(binary) = &self.binary {
            t.file(BINARY, self.binary_mode, binary.clone()).unwrap();
        }
        if let Some(plist) = &self.plist {
            t.file(PLIST, self.plist_mode, plist.clone()).unwrap();
        }
        let trust = |t: &mut Tree, name: &str, mode: u32, bytes: &Option<Vec<u8>>| {
            if let Some(bytes) = bytes {
                t.file(&format!("{TRUST_DIR}/{name}"), mode, bytes.clone())
                    .unwrap();
            }
        };
        trust(&mut t, "gh", self.gh_mode, &self.gh);
        trust(&mut t, "trusted_root.jsonl", 0o644, &self.root);
        trust(&mut t, "installer.conf", 0o644, &self.conf);
        if let Some(extra) = self.extra_file {
            t.file(extra, 0o644, b"x".to_vec()).unwrap();
        }
        if self.in_state {
            t.file(&format!("{STATE_DIR}/state.json"), 0o600, b"{}".to_vec())
                .unwrap();
        }
        if let Some(dir) = self.extra_dir {
            t.dir(dir).unwrap();
        }
        package(&t, &self.version).unwrap()
    }
}

/// The inputs of the checks: the tarball of `BINARY_BYTES`, and `FEATURE`-or-`MAIN`.
struct Inputs {
    dir: PathBuf,
    tarball: PathBuf,
    gh: PathBuf,
}

impl Inputs {
    fn new(name: &str) -> Inputs {
        let dir = scratch(name);
        let (tarball, gh) = (dir.join("kbf-installer.tar.gz"), dir.join("gh"));
        std::fs::write(&tarball, tarball_of(BINARY_BYTES)).unwrap();
        std::fs::write(&gh, GH_BYTES).unwrap();
        Inputs { dir, tarball, gh }
    }

    fn check<'a>(&'a self, expect_ref: &'a str, gh: bool) -> Check<'a> {
        Check {
            binary_tarball: &self.tarball,
            expect_ref,
            gh: gh.then_some(self.gh.as_path()),
        }
    }
}

fn tarball_of(bytes: &[u8]) -> Vec<u8> {
    tarball("kbf-installer", bytes).unwrap()
}

fn found(parts: &Parts, name: &str) -> Vec<String> {
    let inputs = Inputs::new(name);
    problems(&parts.build(), COMMIT, &inputs.check(MAIN, true)).expect("the inputs read")
}

#[test]
fn the_package_of_the_attested_binary_that_trusts_its_branch_has_no_problem() {
    assert_eq!(found(&Parts::good(), "good"), Vec::<String>::new());
    // The development branch's package trusts that branch, and only when told so.
    let inputs = Inputs::new("good-feature");
    let mut parts = Parts::good();
    parts.conf = Some(pkg_trust::conf(FEATURE).into_bytes());
    let pkg = parts.build();
    assert_eq!(
        problems(&pkg, COMMIT, &inputs.check(FEATURE, true)).unwrap(),
        Vec::<String>::new()
    );
    // gh is optional: a verify job has no downloaded copy to compare with.
    assert_eq!(
        problems(&pkg, COMMIT, &inputs.check(FEATURE, false)).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn a_package_that_is_not_the_attested_one_names_each_difference() {
    // Catches: a package holding another binary than the attested tarball's (a rebuild,
    // a stale file), one that trusts another branch than it is attested for (a main
    // package that believes a feature branch), a missing or empty pinned gh or one that
    // is not the pinned bytes, a trusted root that is not JSON lines, a property list
    // that starts without its gate, a state directory open to others, files and
    // directories nobody listed, bytes in the state directory, the wrong commit in the
    // version, and a wrong mode on the binary or on gh. Each is a mutant of the good package.
    let case = |what: &str, f: &dyn Fn(&mut Parts), want: &str| {
        let mut parts = Parts::good();
        f(&mut parts);
        let got = found(&parts, what);
        assert!(
            got.iter().any(|p| p.contains(want)),
            "{what}: wanted a problem with {want:?}, got {got:#?}"
        );
    };
    case(
        "binary",
        &|p| p.binary = Some(b"another".to_vec()),
        "is not the binary of",
    );
    case(
        "binary-mode",
        &|p| p.binary_mode = 0o775,
        "has mode 0775, not 0755",
    );
    case(
        "ref",
        &|p| p.conf = Some(pkg_trust::conf(FEATURE).into_bytes()),
        "trusts refs/heads/feature/arch-v2, but this package is attested for refs/heads/main",
    );
    case(
        "conf-extra",
        &|p| p.conf = Some(b"SOURCE_REFS=refs/heads/main\nRUN_AS_UID=0\n".to_vec()),
        "is not exactly `SOURCE_REFS=",
    );
    case(
        "no-conf",
        &|p| p.conf = None,
        "holds no usr/local/kbf-installer/trust/installer.conf",
    );
    case(
        "no-gh",
        &|p| p.gh = None,
        "holds no usr/local/kbf-installer/trust/gh",
    );
    case(
        "empty-gh",
        &|p| p.gh = Some(Vec::new()),
        "trust/gh is empty",
    );
    case(
        "other-gh",
        &|p| p.gh = Some(b"another gh".to_vec()),
        "is not the pinned gh",
    );
    case(
        "gh-mode",
        &|p| p.gh_mode = 0o644,
        "trust/gh has mode 0644, not 0755",
    );
    case(
        "no-root",
        &|p| p.root = None,
        "holds no usr/local/kbf-installer/trust/trusted_root.jsonl",
    );
    case(
        "bad-root",
        &|p| p.root = Some(b"not json\n".to_vec()),
        "not JSON",
    );
    case(
        "no-binary",
        &|p| p.binary = None,
        "holds no usr/local/kbf-installer/bin/kbf-installer",
    );
    case(
        "no-plist",
        &|p| p.plist = None,
        "holds no Library/LaunchDaemons/com.komira.kbf-installer.plist",
    );
    case(
        "plist-mode",
        &|p| p.plist_mode = 0o666,
        "has mode 0666, not 0644",
    );
    case(
        "no-log-dir",
        &|p| p.log_dir = false,
        "holds no directory Library/Logs/kbf-installer",
    );
    case(
        "ungated",
        &|p| {
            p.plist = Some(
                String::from_utf8(plist())
                    .unwrap()
                    .replace("<false/>", "<true/>")
                    .into_bytes(),
            )
        },
        "the job is not started at load",
    );
    case(
        "state-mode",
        &|p| p.state_mode = 0o755,
        "private/var/db/kbf-installer has mode 0755, not 0700",
    );
    case(
        "extra-file",
        &|p| p.extra_file = Some("usr/local/bin/other"),
        "holds usr/local/bin/other, which the package does not install",
    );
    case(
        "extra-dir",
        &|p| p.extra_dir = Some("opt/other"),
        "holds the directory opt/other",
    );
    case("state-content", &|p| p.in_state = true, "is to be empty");
    case(
        "version",
        &|p| p.version = "0.1.0+ffffffffffff".to_owned(),
        "does not end +0123456789ab",
    );
    case(
        "no-plus",
        &|p| p.version = "0.1.0".to_owned(),
        "does not end +0123456789ab",
    );
}

#[test]
fn a_file_that_is_not_a_package_or_lacks_a_part_or_runs_a_script_is_refused() {
    // Catches: a payload that is not a package (a deb or a tarball renamed), a package
    // with no Payload, and one with a Scripts member (a script would run as root at
    // install: the contract's packages have none).
    let inputs = Inputs::new("shape");
    let check = inputs.check(MAIN, true);
    let one = |bytes: &[u8]| problems(bytes, COMMIT, &check).unwrap();
    assert!(one(b"!<arch>\n")[0].contains("not a xar archive"));
    let good = Parts::good().build();
    let parts = crate::pkg::open::members(&good).unwrap();
    let with = |extra: Option<(&str, &[u8])>, drop: Option<&str>| {
        let mut members: Vec<(&str, &[u8])> = parts
            .iter()
            .filter(|(n, _)| Some(n.as_str()) != drop)
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        members.extend(extra);
        xar::xar(&members)
    };
    let scripted = one(&with(Some(("Scripts", b"a script")), None));
    assert!(
        scripted
            .iter()
            .any(|p| p.contains("not exactly Bom, PackageInfo and Payload")),
        "{scripted:?}"
    );
    let bare = one(&with(None, Some("Payload")));
    assert!(
        bare.iter()
            .any(|p| p.contains("not exactly Bom, PackageInfo and Payload")),
        "{bare:?}"
    );
    // The same bytes with the identifier changed: the receipt would be another package's.
    let mut other = parts.clone();
    let info = String::from_utf8(other["PackageInfo"].clone()).unwrap();
    other.insert(
        "PackageInfo".to_owned(),
        info.replace("com.komira.kbf-installer", "com.example.other")
            .into_bytes(),
    );
    let members: Vec<(&str, &[u8])> = other
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    assert!(
        one(&xar::xar(&members))
            .iter()
            .any(|p| p.contains("does not name com.komira.kbf-installer"))
    );
    // A payload that is not gzip.
    let mut broken = parts.clone();
    broken.insert("Payload".to_owned(), b"not gzip".to_vec());
    let members: Vec<(&str, &[u8])> = broken
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    assert!(
        one(&xar::xar(&members))
            .iter()
            .any(|p| p.starts_with("Payload: "))
    );
    // A ref no host accepts is refused whatever the package holds.
    let bad = problems(&good, COMMIT, &inputs.check("main", true)).unwrap();
    assert!(
        bad.iter().any(|p| p.contains("is not refs/heads/<branch>")),
        "{bad:?}"
    );
}

#[test]
fn the_asset_is_copied_under_its_name_and_listed_once_and_verified_where_it_is() {
    // Catches: an asset under another name than `kbf-installer-<commit>-darwin-arm64.pkg`
    // (the release and the sums would not know it), a package listed twice, a refused
    // package written anyway, and `--verify-only` accepting a file not named as the asset.
    let inputs = Inputs::new("asset");
    let pkg = inputs.dir.join("p.pkg");
    std::fs::write(&pkg, Parts::good().build()).unwrap();
    let out = inputs.dir.join("dist");
    let check = inputs.check(MAIN, true);
    let said = asset(&pkg, COMMIT, &Mode::Add(&out), &check).unwrap();
    let name = format!("kbf-installer-{COMMIT}-darwin-arm64.pkg");
    assert_eq!(file_name(COMMIT), name);
    assert_eq!(said[0], format!("packaged {name}:"));
    assert_eq!(
        std::fs::read(out.join(&name)).unwrap(),
        std::fs::read(&pkg).unwrap()
    );
    let sums = std::fs::read_to_string(out.join("SHA256SUMS.darwin-arm64")).unwrap();
    assert_eq!(
        sums,
        format!("{}  {name}\n", Sha256::of(&std::fs::read(&pkg).unwrap()))
    );
    let again = asset(&pkg, COMMIT, &Mode::Add(&out), &check).unwrap_err();
    assert!(again.to_string().contains("already lists"), "{again}");
    // Verified in place: only under its own name.
    let placed = asset(&out.join(&name), COMMIT, &Mode::VerifyOnly, &check).unwrap();
    assert!(placed[0].contains("trusting refs/heads/main"), "{placed:?}");
    let misnamed = asset(&pkg, COMMIT, &Mode::VerifyOnly, &check).unwrap_err();
    assert!(misnamed.to_string().contains("is not named"), "{misnamed}");
    // A package for another branch is refused and nothing is written.
    let elsewhere = inputs.dir.join("elsewhere");
    let refused = asset(
        &pkg,
        COMMIT,
        &Mode::Add(&elsewhere),
        &inputs.check(FEATURE, true),
    )
    .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("attested for refs/heads/feature/arch-v2"),
        "{refused}"
    );
    assert!(!elsewhere.join(&name).exists());
    // A commit that is not one.
    let bad = asset(&pkg, "main", &Mode::VerifyOnly, &check).unwrap_err();
    assert!(matches!(bad, Error::NotACommit(_)), "{bad}");
}

#[test]
fn the_paths_are_the_ones_of_the_installers_mac_layout() {
    // Catches: the package installing to a place the installer does not look (the
    // binary, the trust directory, the state directory, the gate the property list waits
    // on), as the gate and `setup` once disagreed: every path here is compared with
    // `kbf_installer::layout`'s macOS paths.
    use kbf_host::path::Root;
    use kbf_installer::layout::{Layout, Platform as Host};
    let layout = Layout::new(Root::host(), Host::MacOs);
    let rel = |p: kbf_host::path::HostPath| p.as_str().trim_start_matches('/').to_owned();
    assert_eq!(rel(layout.install_root()), pkg::PREFIX);
    assert_eq!(rel(layout.trust_dir()), TRUST_DIR);
    assert_eq!(rel(layout.gh()), format!("{TRUST_DIR}/gh"));
    assert_eq!(
        rel(layout.trusted_root()),
        format!("{TRUST_DIR}/trusted_root.jsonl")
    );
    assert_eq!(
        rel(layout.settings()),
        format!("{TRUST_DIR}/installer.conf")
    );
    assert_eq!(
        layout.state_dir().as_str(),
        format!("/var/db/kbf-installer")
    );
    // /var is a link to /private/var: the package holds the real path.
    assert_eq!(STATE_DIR, "private/var/db/kbf-installer");
    assert_eq!(pkg::GATE, layout.installer_toml().as_str());
    assert_eq!(BINARY, format!("{}/bin/kbf-installer", pkg::PREFIX));
}

#[test]
fn a_tarball_without_the_installer_cannot_be_the_binary_the_package_holds() {
    // Catches: the comparison skipped when the tarball names another binary (a package
    // checked against the wrong asset passing), and a tarball that is not one.
    let inputs = Inputs::new("other-tarball");
    std::fs::write(&inputs.tarball, tarball("kbf", BINARY_BYTES).unwrap()).unwrap();
    let got = problems(&Parts::good().build(), COMMIT, &inputs.check(MAIN, true)).unwrap();
    assert!(
        got.iter()
            .any(|p| p.ends_with("kbf-installer.tar.gz holds no kbf-installer")),
        "{got:?}"
    );
    std::fs::write(&inputs.tarball, b"not a tarball").unwrap();
    let got = problems(&Parts::good().build(), COMMIT, &inputs.check(MAIN, true)).unwrap();
    assert!(
        got.iter().any(|p| p.contains("holds no kbf-installer")),
        "{got:?}"
    );
}
