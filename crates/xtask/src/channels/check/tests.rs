use std::cell::RefCell;

use super::*;
use kbf_testkit::dir::TestDir;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
/// What `kbf --version --json` prints for build 3 of workspace version 1.2.0 at [`COMMIT`]: the
/// native package version is 1.2.3.
const SAYS: &str = "{\"commit\":\"0123456789ab\",\"commit_full\":\"0123456789abcdef0123456789abcdef01234567\",\"seq\":3,\"version\":\"1.2.0\"}\n";
const URL: &str = "https://packages.komira.ai/apt";

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-check-{name}"))
}

/// A machine that answers for `apt-get`, `dpkg`, `dpkg-query`, `curl` and the installed binary, and does to a scratch root what each would do to a real one.
struct Machine {
    root: PathBuf,
    calls: RefCell<Vec<String>>,
    arch: &'static str,
    keyring: Vec<u8>,
    dpkg_version: &'static str,
    says: &'static str,
    /// Files below the root that the install does not create.
    omit: Vec<String>,
    /// Files and links below the root the install creates besides the package's files.
    enable: Vec<&'static str>,
    /// Does a process named `kbf` run?
    running: bool,
    /// A command line (joined) containing this text is refused.
    refuse: Option<&'static str>,
}

impl Machine {
    fn new(root: &Path) -> Machine {
        Machine {
            root: root.to_path_buf(),
            calls: RefCell::new(Vec::new()),
            arch: "amd64",
            keyring: vec![0x99, 0x01, 0x0d],
            dpkg_version: "1.2.3",
            says: SAYS,
            omit: Vec::new(),
            enable: Vec::new(),
            running: false,
            refuse: None,
        }
    }

    /// The refusal for the command line `line`, when the machine is told to refuse it.
    fn refusal(&self, line: &str) -> Option<Answer> {
        let text = self.refuse.filter(|text| line.contains(text))?;
        Some(Answer {
            ok: false,
            stdout: String::new(),
            stderr: format!("refused: {text}\n"),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn put(&self, rel: &str, bytes: &[u8]) {
        write(&self.root.join(rel), bytes).expect("write");
    }

    fn install(&self) {
        for rel in files() {
            if !self.omit.contains(&rel) {
                self.put(&rel, b"file");
            }
        }
        for link in &self.enable {
            self.put(link, b"link");
        }
        if self.running {
            self.put("proc/42/comm", b"kbf\n");
        }
        self.put("proc/43/comm", b"bash\n");
    }
}

fn answer(stdout: &str) -> Result<Answer, Error> {
    Ok(Answer {
        ok: true,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    })
}

impl Host for Machine {
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error> {
        let name = program
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        let line = format!("{name} {}", args.join(" "));
        self.calls.borrow_mut().push(line.clone());
        if let Some(refused) = self.refusal(&line) {
            return Ok(refused);
        }
        match (name.as_str(), args.first().map(String::as_str)) {
            ("dpkg", _) => answer(&format!("{}\n", self.arch)),
            ("dpkg-query", _) => answer(self.dpkg_version),
            ("apt-get", Some("install")) if line.contains("kbf=") => {
                self.install();
                answer("")
            }
            ("curl", _) => {
                let at = args.iter().position(|a| a == "--output").expect("--output");
                write(Path::new(&args[at + 1]), &self.keyring).expect("keyring");
                answer("")
            }
            ("kbf", Some("--version")) => answer(self.says),
            _ => answer(""),
        }
    }
}

fn apt<'a>(root: &'a Path) -> Apt<'a> {
    Apt {
        root,
        arch: "amd64",
        version: "1.2.3",
        commit: COMMIT,
        apt_url: URL,
        apt_get: Path::new("apt-get"),
        dpkg_query: Path::new("dpkg-query"),
        dpkg: Path::new("dpkg"),
        curl: Path::new("curl"),
    }
}

#[test]
fn a_clean_install_from_testing_is_checked_and_adds_the_repository_the_way_a_user_does() {
    // Catches: a source that is not deb822 with Signed-By and the suite testing, a package
    // installed at no version, and the order (update, upgrade, certificates, keyring, source, update, install).
    let dir = scratch("apt-ok");
    let machine = Machine::new(&dir);
    let said = check_apt(&machine, &apt(&dir)).expect("checked");
    assert_eq!(
        machine
            .calls()
            .iter()
            .map(|c| c.split(' ').next().unwrap().to_owned())
            .collect::<Vec<_>>(),
        [
            "dpkg",
            "apt-get",
            "apt-get",
            "apt-get",
            "curl",
            "apt-get",
            "apt-get",
            "dpkg-query",
            "kbf"
        ]
    );
    assert!(
        machine
            .calls()
            .contains(&"apt-get install -y --no-install-recommends kbf=1.2.3".to_owned())
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(SOURCES)).expect("source"),
        "Types: deb\nURIs: https://packages.komira.ai/apt\nSuites: testing\nComponents: main\nArchitectures: amd64\nSigned-By: /usr/share/keyrings/komira-archive-keyring.gpg\n"
    );
    assert!(
        machine
            .calls()
            .iter()
            .any(|c| c.ends_with("https://packages.komira.ai/apt/komira-archive-keyring.gpg"))
    );
    assert!(
        said[0].starts_with("check-apt: kbf 1.2.3 installed from the suite testing on amd64;"),
        "{said:?}"
    );
}

#[test]
fn a_package_missing_any_of_its_files_is_named() {
    // Catches: a package that installs without the files the units and the setup need.
    let dir = scratch("apt-files");
    let mut machine = Machine::new(&dir);
    machine.omit = files();
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    let want: Vec<String> = files()
        .iter()
        .map(|rel| format!("the package's file /{rel} is not installed"))
        .collect();
    assert_eq!(problems, want);
    assert_eq!(
        files(),
        [
            "usr/bin/kbf",
            "usr/lib/sysusers.d/kbf.conf",
            "usr/lib/tmpfiles.d/kbf.conf",
            "etc/needrestart/conf.d/kbf.conf",
            "usr/lib/systemd/system/kbf-all-in-one.service",
            "usr/lib/systemd/system/kbf-daemon.service",
            "usr/lib/systemd/system/kbf-frontend.service",
            "usr/lib/systemd/system/kbf-storage-and-daemon.service",
            "usr/lib/systemd/system/kbf-storage.service",
            "usr/lib/systemd/system/kbf.slice",
            "usr/share/kbf/trust/gh",
            "usr/share/kbf/trust/installer.conf",
            "usr/share/kbf/trust/trusted_root.jsonl",
        ]
    );
}

#[test]
fn a_unit_that_the_install_enabled_or_a_process_it_started_is_named() {
    // Catches: a postinst or a package link that enables or starts a unit on install, which the
    // package promises it does not; a unit of the modes counts, below `/etc` and below the
    // package's own unit directory.
    let dir = scratch("apt-enabled");
    let mut machine = Machine::new(&dir);
    machine.enable = vec![
        "etc/systemd/system/multi-user.target.wants/kbf-storage-and-daemon.service",
        "etc/systemd/system/kbf-daemon.service",
        "usr/lib/systemd/system/multi-user.target.wants/kbf-storage.service",
    ];
    machine.running = true;
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    assert_eq!(problems.len(), 4, "{problems:?}");
    for (problem, tail) in problems.iter().zip([
        "etc/systemd/system/kbf-daemon.service",
        "etc/systemd/system/multi-user.target.wants/kbf-storage-and-daemon.service",
        "usr/lib/systemd/system/multi-user.target.wants/kbf-storage.service",
    ]) {
        assert!(problem.starts_with("a unit is enabled: "), "{problems:?}");
        assert!(
            problem.ends_with(&format!("{tail} exists, and the package enables nothing")),
            "{problems:?}"
        );
    }
    assert_eq!(
        problems[3],
        "kbf is running (process 42), and the package starts nothing"
    );
}

#[test]
fn the_host_agents_units_links_and_account_are_named_when_the_package_carries_them() {
    // Catches: the native package shipping the host agent that hostkeeper replaced, which would
    // start a second updater beside it.
    let dir = scratch("apt-agent");
    let mut machine = Machine::new(&dir);
    machine.enable = vec![
        "usr/lib/systemd/system/kbf-host-local.service",
        "usr/lib/systemd/system/kbf-updater-agent.service",
        "usr/lib/sysusers.d/kbf-updater.conf",
        "usr/lib/systemd/system/multi-user.target.wants/kbf-updater-agent.service",
    ];
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    assert_eq!(
        problems,
        [
            "/usr/lib/systemd/system/kbf-host-local.service is installed: the native package carries no host agent"
                .to_owned(),
            "/usr/lib/systemd/system/kbf-updater-agent.service is installed: the native package carries no host agent"
                .to_owned(),
            "/usr/lib/sysusers.d/kbf-updater.conf is installed: the native package carries no host agent"
                .to_owned(),
            "a unit is enabled: ".to_owned()
                + &dir
                    .join("usr/lib/systemd/system/multi-user.target.wants/kbf-updater-agent.service")
                    .display()
                    .to_string()
                + " exists, and the package enables nothing",
        ]
    );
}

#[test]
fn a_file_that_is_not_a_directory_below_the_systemd_directory_is_not_a_wants_entry() {
    // Catches: a stray file named like a unit's directory read as a directory holding a link.
    let dir = scratch("apt-stray");
    let mut machine = Machine::new(&dir);
    machine.enable = vec!["etc/systemd/system/stray"];
    assert!(check_apt(&machine, &apt(&dir)).is_ok());
}

#[test]
fn a_package_at_another_version_is_refused() {
    // Catches: apt installing a version other than the one asked.
    let dir = scratch("apt-version");
    let mut machine = Machine::new(&dir);
    machine.dpkg_version = "1.2.2";
    let Err(Error::Refused(problems)) = check_apt(&machine, &apt(&dir)) else {
        panic!("not refused");
    };
    assert_eq!(
        problems,
        ["dpkg says kbf \"1.2.2\" is installed, not \"1.2.3\""]
    );
}

#[test]
fn a_binary_that_names_another_commit_or_another_native_version_is_refused() {
    // Catches: a binary of another build in the package, and a version the package carries that
    // is not the one the binary computes from its own version and seq.
    let said = |says: &'static str| {
        let dir = scratch("apt-says");
        let mut machine = Machine::new(&dir);
        machine.says = says;
        check_apt(&machine, &apt(&dir)).unwrap_err().to_string()
    };
    let json = |commit: &str, rest: &str| format!("{{\"commit\":\"{commit}\"{rest}}}\n");
    let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    let wrong = |line: &str, why: &str| {
        format!(
            "`kbf --version --json` said {:?}: {why}",
            line.lines().next().unwrap_or("")
        )
    };
    for (says, why) in [
        (
            leak(json("aaaaaaaaaaaa", ",\"seq\":3,\"version\":\"1.2.0\"")),
            "its commit is Some(\"aaaaaaaaaaaa\"), not \"0123456789ab\"",
        ),
        (
            leak("{\"seq\":3,\"version\":\"1.2.0\"}\n".to_owned()),
            "its commit is None, not \"0123456789ab\"",
        ),
        (
            leak(json(
                "0123456789ab",
                ",\"commit_full\":\"0123456789abffffffffffffffffffffffffffff\",\"seq\":3,\"version\":\"1.2.0\"",
            )),
            "its commit_full is \"0123456789abffffffffffffffffffffffffffff\", not \"0123456789abcdef0123456789abcdef01234567\"",
        ),
        (
            leak(json("0123456789ab", ",\"seq\":4,\"version\":\"1.2.0\"")),
            "its native version is \"1.2.4\", not \"1.2.3\"",
        ),
        (
            leak(json("0123456789ab", ",\"version\":\"1.2.0\"")),
            "it has no numeric seq",
        ),
        (
            leak(json("0123456789ab", ",\"seq\":\"3\",\"version\":\"1.2.0\"")),
            "it has no numeric seq",
        ),
        (
            leak(json("0123456789ab", ",\"seq\":3,\"version\":\"1.2\"")),
            "version \"1.2\" is not three dot-separated decimal numbers",
        ),
        (
            leak(json("0123456789ab", ",\"seq\":3")),
            "version \"\" is not three dot-separated decimal numbers",
        ),
    ] {
        assert_eq!(said(says), wrong(says, why));
    }
    let e = said("kbf 1.2.3\n");
    assert!(
        e.starts_with("`kbf --version --json` said \"kbf 1.2.3\": not JSON ("),
        "{e}"
    );
    let e = said("");
    assert!(
        e.starts_with("`kbf --version --json` said \"\": not JSON ("),
        "{e}"
    );
}

#[test]
fn only_the_first_line_of_the_json_is_judged_and_a_commit_without_the_full_one_passes() {
    // Catches: a check that fails on a binary that prints more lines, or that insists on the
    // full commit a build without one does not print.
    let dir = scratch("apt-says-first-line");
    let mut machine = Machine::new(&dir);
    machine.says = "{\"commit\":\"0123456789ab\",\"seq\":3,\"version\":\"1.2.0\"}\nanything\n";
    assert!(check_apt(&machine, &apt(&dir)).is_ok());
}

#[test]
fn the_native_version_is_the_workspace_major_and_minor_and_the_build_seq() {
    // Catches: a native version that is not X.Y.Z, or one that keeps the workspace patch.
    assert_eq!(native_version("0.1.0", 7472), Ok("0.1.7472".to_owned()));
    assert_eq!(native_version("12.34.56", 0), Ok("12.34.0".to_owned()));
    for bad in [
        "0.1",
        "0.1.0.1.5",
        "0.1.x",
        "0..1",
        "",
        "0.1.0+g1",
        "v0.1.0",
        "-1.0.0",
    ] {
        assert_eq!(
            native_version(bad, 1),
            Err(format!(
                "version {bad:?} is not three dot-separated decimal numbers"
            )),
            "{bad}"
        );
    }
}

#[test]
fn an_armored_or_empty_keyring_is_refused_because_signed_by_takes_a_gpg_file() {
    // Catches: a keyring published as armor, which apt silently does not read from Signed-By.
    for keyring in [b"-----BEGIN PGP PUBLIC KEY BLOCK-----".to_vec(), Vec::new()] {
        let dir = scratch("apt-keyring");
        let mut machine = Machine::new(&dir);
        machine.keyring = keyring;
        let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
        assert!(
            e.contains("komira-archive-keyring.gpg is not a binary keyring"),
            "{e}"
        );
        assert!(!dir.join(SOURCES).exists());
    }
}

#[test]
fn a_program_that_fails_is_named_with_what_it_said_and_a_machine_of_another_architecture_is_refused()
 {
    // Catches: an apt-get update that fails (a signature apt could not verify) read as a pass.
    for (refuse, program) in [
        ("update", "apt-get update"),
        ("dist-upgrade", "apt-get dist-upgrade"),
        ("ca-certificates", "ca-certificates"),
        ("--output", "--output"),
        ("kbf=", "kbf=1.2.3"),
        ("--showformat", "dpkg-query"),
        ("--version", "kbf --version --json"),
        ("--print-architecture", "dpkg --print-architecture"),
    ] {
        let dir = scratch("apt-refused");
        let mut machine = Machine::new(&dir);
        machine.refuse = Some(refuse);
        let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
        assert!(
            e.contains(program) && e.contains(&format!("failed: refused: {refuse}")),
            "{refuse}: {e}"
        );
    }
    let dir = scratch("apt-arch");
    let mut machine = Machine::new(&dir);
    machine.arch = "arm64";
    let e = check_apt(&machine, &apt(&dir)).unwrap_err().to_string();
    assert_eq!(
        e,
        "this machine is arm64, not amd64: the check would install another package"
    );
}

#[test]
fn flags_out_of_their_form_are_usage_errors_before_anything_runs() {
    // Catches: a check that installs from a plain-HTTP repository or for an architecture it is
    // not on.
    let dir = scratch("apt-usage");
    let machine = Machine::new(&dir);
    for change in [
        |r: &mut Apt| r.version = "v1.2.3",
        |r: &mut Apt| r.commit = "0123456789ab",
        |r: &mut Apt| r.arch = "i386",
        |r: &mut Apt| r.apt_url = "http://packages.komira.ai/apt",
        |r: &mut Apt| r.apt_url = "https://packages.komira.ai/apt/",
    ] {
        let mut r = apt(&dir);
        change(&mut r);
        assert!(matches!(check_apt(&machine, &r), Err(Error::Usage(_))));
    }
    assert!(machine.calls().is_empty());
}

#[test]
fn the_image_is_updated_and_upgraded_before_certificates_and_curl_are_installed() {
    // Catches: the failure of the first release's amd64 install check, `apt-get install -y
    // ca-certificates curl` on the digest-pinned ubuntu:24.04 image refused with "you have held
    // broken packages" because the image's packages are older than the archive's: the apt-get
    // calls are `update`, then the upgrade, then the install of the two packages, before the
    // repository is added, and the second `update` follows the source file.
    let dir = scratch("apt-upgrade-order");
    let machine = Machine::new(&dir);
    check_apt(&machine, &apt(&dir)).expect("checked");
    let calls: Vec<String> = machine
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("apt-get "))
        .collect();
    assert_eq!(
        calls,
        [
            "apt-get update",
            "apt-get dist-upgrade -y -o Dpkg::Options::=--force-confold",
            "apt-get install -y --no-install-recommends ca-certificates curl",
            "apt-get update",
            "apt-get install -y --no-install-recommends kbf=1.2.3",
        ]
    );
}
