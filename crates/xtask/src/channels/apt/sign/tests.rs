use super::*;
use crate::channels::apt::testing::*;
use std::os::unix::fs::PermissionsExt;

const FPR: &str = "0123456789ABCDEF0123456789ABCDEF01234567";
const OTHER: &str = "FEDCBA9876543210FEDCBA9876543210FEDCBA98";

struct Setup {
    dir: kbf_testkit::dir::TestDir,
}

impl Setup {
    fn new(name: &str) -> Setup {
        let dir = scratch(name);
        std::fs::create_dir_all(dir.join("next/dists/stable")).expect("dir");
        std::fs::write(dir.join("next/dists/stable/Release"), "Suite: stable\n").expect("release");
        std::fs::write(dir.join("key.asc"), "PRIVATE KEY").expect("key");
        std::fs::write(dir.join("pass"), "secret").expect("pass");
        Setup { dir }
    }

    fn request<'a>(&'a self, fingerprints: &'a str) -> Request<'a> {
        Request {
            dir: Box::leak(self.dir.join("next/dists/stable").into_boxed_path()),
            work: Box::leak(self.dir.join("work").into_boxed_path()),
            fingerprints,
            key: Source::File(Box::leak(self.dir.join("key.asc").into_boxed_path())),
            passphrase: Source::File(Box::leak(self.dir.join("pass").into_boxed_path())),
        }
    }
}

/// An environment with no variable.
fn no_env(_: &str) -> Option<String> {
    None
}

#[test]
fn release_is_signed_clear_and_detached_and_both_are_verified() {
    // Catches: an index published unsigned, signed by another key than the subkey, or never
    // checked after signing.
    let s = Setup::new("sign-ok");
    let gpg = FakeGpg::default();
    let out = s.dir.join("out");
    let said = sign(&gpg, &s.request(FPR), &no_env, Some(&out)).expect("sign");
    assert_eq!(
        said,
        [format!(
            "signed Release with 1 subkey(s), {FPR}: InRelease and Release.gpg verify"
        )]
    );
    assert_eq!(std::fs::read_to_string(&out).expect("out"), "signed=true\n");
    let stable = s.dir.join("next/dists/stable");
    let clear = std::fs::read_to_string(stable.join("InRelease")).expect("InRelease");
    assert!(clear.contains(&format!("{FPR}!")) && clear.ends_with("Suite: stable\n"));
    assert!(
        std::fs::read_to_string(stable.join("Release.gpg"))
            .expect("gpg")
            .contains(&format!("{FPR}!"))
    );
    // The steps, in order: import, list, sign twice, verify twice.
    let calls = gpg.calls.borrow();
    let words: Vec<&str> = calls
        .iter()
        .map(|c| {
            [
                "--import",
                "--list-secret-keys",
                "--clearsign",
                "--detach-sign",
                "--verify",
            ]
            .into_iter()
            .find(|w| c.iter().any(|a| a == w))
            .expect("a step")
        })
        .collect();
    assert_eq!(
        words,
        [
            "--import",
            "--list-secret-keys",
            "--clearsign",
            "--detach-sign",
            "--verify",
            "--verify"
        ]
    );
    // The signing lines: the loopback pinentry, the passphrase as a file, never as an argument.
    for call in calls
        .iter()
        .filter(|c| c.iter().any(|a| a.ends_with("sign")))
    {
        assert_eq!(flag(call, "--pinentry-mode").as_deref(), Some("loopback"));
        assert_eq!(
            flag(call, "--passphrase-file").as_deref(),
            Some(s.dir.join("pass").to_str().expect("utf8"))
        );
        assert_eq!(flag(call, "--digest-algo").as_deref(), Some("SHA512"));
        assert!(
            !call
                .iter()
                .any(|a| a == "--passphrase" || a.contains("secret"))
        );
    }
    // The detached verify names the signature and the file it signs.
    let last = calls.last().expect("last");
    assert_eq!(
        last[last.len() - 2..],
        [
            stable.join("Release.gpg").display().to_string(),
            stable.join("Release").display().to_string()
        ]
    );
}

#[test]
fn the_key_and_the_keyring_are_gone_when_it_ends_and_the_keyring_was_private() {
    // Catches: a private key or passphrase left on the runner's disk, and a keyring other users
    // can read while it exists.
    struct Peek(std::path::PathBuf, std::cell::RefCell<Option<u32>>);
    impl Gpg for Peek {
        fn run(&self, args: &[String]) -> Result<Answer, Error> {
            if args.iter().any(|a| a == "--import") {
                let mode = std::fs::metadata(&self.0)
                    .expect("home")
                    .permissions()
                    .mode();
                *self.1.borrow_mut() = Some(mode & 0o777);
            }
            FakeGpg::default().run(args)
        }
    }
    let s = Setup::new("sign-clean");
    let peek = Peek(s.dir.join("work/gnupg"), std::cell::RefCell::new(None));
    sign(&peek, &s.request(FPR), &no_env, None).expect("sign");
    assert_eq!(*peek.1.borrow(), Some(0o700));
    assert!(!s.dir.join("work/gnupg").exists());
    assert!(!s.dir.join("key.asc").exists() && !s.dir.join("pass").exists());
}

#[test]
fn a_failing_step_names_what_failed_and_still_removes_the_key() {
    // Catches: a failed signing that leaves the key behind, or an index published with a signature
    // that does not verify.
    for (when, said, why) in [
        (
            "--import",
            "no secret key",
            "gpg could not import the key: no secret key",
        ),
        (
            "--list-secret-keys",
            "not found",
            "the signing key is not among the secret keys that were imported",
        ),
        (
            "--clearsign",
            "bad passphrase",
            "gpg could not sign InRelease: bad passphrase",
        ),
        (
            "--detach-sign",
            "bad passphrase",
            "gpg could not sign Release.gpg: bad passphrase",
        ),
        (
            "InRelease",
            "BAD signature",
            "InRelease does not verify: BAD signature",
        ),
        (
            "Release.gpg",
            "BAD signature",
            "Release.gpg does not verify: BAD signature",
        ),
    ] {
        let s = Setup::new("sign-fail");
        let gpg = FakeGpg::default();
        gpg.refuse_when(
            &if when.starts_with("--") {
                when.to_owned()
            } else {
                format!(
                    "--verify {}",
                    s.dir.join("next/dists/stable").join(when).display()
                )
            },
            said,
        );
        let out = s.dir.join("out");
        let e = sign(&gpg, &s.request(FPR), &no_env, Some(&out))
            .unwrap_err()
            .to_string();
        assert!(e.contains(why), "{when}: {e}");
        assert!(!out.exists(), "{when}: nothing says it is signed");
        assert!(!s.dir.join("key.asc").exists() && !s.dir.join("work/gnupg").exists());
    }
}

#[test]
fn a_key_rotation_signs_with_each_subkey_and_checks_each_is_there() {
    // Catches: a rotation that signs with one key only, or signs before finding a key missing.
    let s = Setup::new("sign-two");
    let gpg = FakeGpg::default();
    let both = format!("{FPR}, {}", OTHER.to_ascii_lowercase());
    // A comma separates; spaces inside a fingerprint are tolerated.
    let said = sign(&gpg, &s.request(&both.replace(", ", ",")), &no_env, None).expect("sign");
    assert!(said[0].starts_with("signed Release with 2 subkey(s)"));
    let clear = std::fs::read_to_string(s.dir.join("next/dists/stable/InRelease")).expect("clear");
    assert!(clear.contains(&format!("{FPR}!")) && clear.contains(&format!("{OTHER}!")));
    assert_eq!(
        gpg.calls
            .borrow()
            .iter()
            .filter(|c| c.iter().any(|a| a == "--list-secret-keys"))
            .count(),
        2
    );

    let s = Setup::new("sign-missing");
    let gpg = FakeGpg::default();
    gpg.refuse_when(&format!("--with-colons {OTHER}"), "no such key");
    let e = sign(&gpg, &s.request(&format!("{FPR},{OTHER}")), &no_env, None)
        .unwrap_err()
        .to_string();
    assert!(e.contains(OTHER) && e.contains("no such key"), "{e}");
    assert!(
        !gpg.calls
            .borrow()
            .iter()
            .any(|c| c.iter().any(|a| a == "--clearsign"))
    );
}

#[test]
fn what_is_not_a_fingerprint_or_has_nothing_to_sign_is_refused_before_gpg_runs() {
    // Catches: signing with a key spec that is a name or a pattern, and gpg started with nothing to do.
    let s = Setup::new("sign-usage");
    let gpg = FakeGpg::default();
    for bad in [
        "",
        "abc",
        "0123456789ABCDEG0123",
        &format!("{FPR},"),
        &format!("{FPR},{FPR}"),
        &format!("{FPR}!"),
    ] {
        assert!(
            matches!(
                sign(&gpg, &s.request(bad), &no_env, None),
                Err(Error::Usage(_))
            ),
            "{bad:?}"
        );
    }
    assert!(sign(&gpg, &s.request("0123 4567 89AB CDEF"), &no_env, None).is_ok());
    std::fs::remove_file(s.dir.join("next/dists/stable/Release")).expect("remove");
    let e = sign(&gpg, &s.request(FPR), &no_env, None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("nothing to sign"));
    assert_eq!(
        gpg.calls.borrow().len(),
        6,
        "only the one good run called gpg"
    );
}

#[test]
fn a_work_directory_that_cannot_hold_the_keyring_is_an_error_and_an_old_keyring_is_replaced() {
    // Catches: a keyring of an earlier run reused, and a failure to make the directory ignored.
    let s = Setup::new("sign-work");
    std::fs::create_dir_all(s.dir.join("work/gnupg")).expect("old");
    std::fs::write(s.dir.join("work/gnupg/old-key"), "old").expect("old key");
    let gpg = FakeGpg::default();
    sign(&gpg, &s.request(FPR), &no_env, None).expect("sign");
    let s = Setup::new("sign-work-blocked");
    std::fs::write(s.dir.join("work"), "a file").expect("file");
    let e = sign(&gpg, &s.request(FPR), &no_env, None)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("create "), "{e}");
}

#[test]
fn the_key_files_are_removed_when_the_flags_are_refused_before_any_signing() {
    // Catches: a key and a passphrase left on the runner's disk because the verb stopped on a
    // fingerprint or a missing Release before it reached the cleanup.
    let s = Setup::new("sign-refused-removes-key");
    let gpg = FakeGpg::default();
    assert!(matches!(
        sign(&gpg, &s.request("abc"), &no_env, None),
        Err(Error::Usage(_))
    ));
    assert!(!s.dir.join("key.asc").exists() && !s.dir.join("pass").exists());
    std::fs::write(s.dir.join("key.asc"), "PRIVATE KEY").expect("key");
    std::fs::write(s.dir.join("pass"), "secret").expect("pass");
    std::fs::remove_file(s.dir.join("next/dists/stable/Release")).expect("remove");
    assert!(sign(&gpg, &s.request(FPR), &no_env, None).is_err());
    assert!(!s.dir.join("key.asc").exists() && !s.dir.join("pass").exists());
    assert!(gpg.calls.borrow().is_empty());
}

/// The environment of a signing job: the two variables of the secrets.
fn job_env(name: &str) -> Option<String> {
    match name {
        "APT_KEY" => Some("PRIVATE KEY FROM THE ENVIRONMENT".to_owned()),
        "APT_PASS" => Some("passphrase from the environment".to_owned()),
        _ => None,
    }
}

impl Setup {
    /// A request whose key and passphrase come from the variables `APT_KEY` and `APT_PASS`.
    fn env_request<'a>(&'a self, fingerprints: &'a str) -> Request<'a> {
        Request {
            key: Source::Env("APT_KEY"),
            passphrase: Source::Env("APT_PASS"),
            ..self.request(fingerprints)
        }
    }
}

/// A gpg that records, when each step runs, the content and the mode of the files it is given.
struct Reading {
    seen: std::cell::RefCell<Vec<(String, String, u32)>>,
}

impl Gpg for Reading {
    fn run(&self, args: &[String]) -> Result<Answer, Error> {
        let import = args
            .iter()
            .position(|a| a == "--import")
            .and_then(|at| args.get(at + 1).cloned());
        for path in [import, flag(args, "--passphrase-file")]
            .into_iter()
            .flatten()
        {
            let mode = std::fs::metadata(&path).expect("file").permissions().mode();
            self.seen.borrow_mut().push((
                path.clone(),
                std::fs::read_to_string(&path).expect("content"),
                mode & 0o777,
            ));
        }
        FakeGpg::default().run(args)
    }
}

#[test]
fn a_key_and_a_passphrase_named_by_variables_are_written_0600_below_work_and_removed() {
    // Catches: secret material that reaches gpg in a file other users can read, in a file outside
    // --work, as an argument, or that stays on the disk after the verb.
    let s = Setup::new("sign-env");
    let reading = Reading {
        seen: std::cell::RefCell::default(),
    };
    let out = s.dir.join("out");
    let said = sign(&reading, &s.env_request(FPR), &job_env, Some(&out)).expect("sign");
    assert!(
        said[0].starts_with("signed Release with 1 subkey(s)"),
        "{said:?}"
    );
    assert_eq!(std::fs::read_to_string(&out).expect("out"), "signed=true\n");
    let work = s.dir.join("work");
    let key = work.join("apt-key.asc").display().to_string();
    let passphrase = work.join("apt-passphrase").display().to_string();
    let seen = reading.seen.borrow();
    assert_eq!(
        seen[0],
        (key, "PRIVATE KEY FROM THE ENVIRONMENT".to_owned(), 0o600)
    );
    assert_eq!(
        seen[1],
        (
            passphrase,
            "passphrase from the environment".to_owned(),
            0o600
        )
    );
    assert!(!work.join("apt-key.asc").exists());
    assert!(!work.join("apt-passphrase").exists());
    assert!(!work.join("gnupg").exists());
    // The files that were not asked for are not touched.
    assert!(s.dir.join("key.asc").exists() && s.dir.join("pass").exists());
}

#[test]
fn a_key_by_file_and_a_passphrase_by_variable_may_be_mixed() {
    // Catches: the two forms tied together, so that one of them cannot be given the other way.
    let s = Setup::new("sign-mixed");
    let gpg = FakeGpg::default();
    let request = Request {
        passphrase: Source::Env("APT_PASS"),
        ..s.request(FPR)
    };
    sign(&gpg, &request, &job_env, None).expect("sign");
    assert!(!s.dir.join("key.asc").exists());
    assert!(!s.dir.join("work/apt-passphrase").exists());
}

/// `(name, an environment, what the refusal says)`.
type Unset<'a> = (&'a str, &'a dyn Fn(&str) -> Option<String>, &'a str);

#[test]
fn a_variable_that_is_not_set_or_is_empty_is_refused_and_the_files_made_are_removed() {
    // Catches: a signing that goes on with an empty key or passphrase, a message that names no
    // variable, and a key file left when the passphrase turns out to be missing.
    let empty = |name: &str| {
        (name == "APT_PASS")
            .then(String::new)
            .or_else(|| job_env(name))
    };
    let no_key = |name: &str| (name != "APT_KEY").then(|| job_env(name)).flatten();
    let no_pass = |name: &str| (name != "APT_PASS").then(|| job_env(name)).flatten();
    let cases: [Unset; 3] = [
        ("key-unset", &no_key, "APT_KEY is not set, or is empty"),
        ("pass-unset", &no_pass, "APT_PASS is not set, or is empty"),
        ("pass-empty", &empty, "APT_PASS is not set, or is empty"),
    ];
    for (name, env, why) in cases {
        let s = Setup::new(name);
        let gpg = FakeGpg::default();
        let out = s.dir.join("out");
        let e = sign(&gpg, &s.env_request(FPR), env, Some(&out)).unwrap_err();
        assert_eq!(e, Error::Usage(why.to_owned()), "{name}");
        assert!(gpg.calls.borrow().is_empty(), "{name}");
        assert!(!out.exists(), "{name}");
        assert!(!s.dir.join("work/apt-key.asc").exists(), "{name}");
        assert!(!s.dir.join("work/apt-passphrase").exists(), "{name}");
    }
}

#[test]
fn the_files_made_from_variables_are_removed_when_gpg_fails() {
    // Catches: a private key written from the environment and left on the disk when gpg refuses.
    let s = Setup::new("sign-env-fails");
    let gpg = FakeGpg::default();
    gpg.refuse_when("--import", "no secret key");
    let e = sign(&gpg, &s.env_request(FPR), &job_env, None)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("gpg could not import the key: no secret key"),
        "{e}"
    );
    assert!(!e.contains("PRIVATE KEY"), "{e}");
    assert!(!s.dir.join("work/apt-key.asc").exists());
    assert!(!s.dir.join("work/apt-passphrase").exists());
}

#[test]
fn an_old_file_in_the_place_of_the_key_is_replaced_and_what_cannot_be_replaced_is_an_error() {
    // Catches: a key written into a file that was there before with its mode (and its other
    // readers), and a write that fails without a word.
    let s = Setup::new("sign-env-old");
    std::fs::create_dir_all(s.dir.join("work")).expect("work");
    let old = s.dir.join("work/apt-key.asc");
    std::fs::write(&old, "an old key").expect("old");
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).expect("mode");
    let reading = Reading {
        seen: std::cell::RefCell::default(),
    };
    sign(&reading, &s.env_request(FPR), &job_env, None).expect("sign");
    assert_eq!(
        reading.seen.borrow()[0].1,
        "PRIVATE KEY FROM THE ENVIRONMENT"
    );
    assert_eq!(reading.seen.borrow()[0].2, 0o600);

    // A directory where the file belongs cannot be removed as a file.
    let s = Setup::new("sign-env-dir");
    std::fs::create_dir_all(s.dir.join("work/apt-key.asc")).expect("a directory");
    let e = sign(&FakeGpg::default(), &s.env_request(FPR), &job_env, None)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("write "), "{e}");

    // A work path that is a file cannot hold them.
    let s = Setup::new("sign-env-work-file");
    std::fs::write(s.dir.join("work"), "a file").expect("file");
    let e = sign(&FakeGpg::default(), &s.env_request(FPR), &job_env, None)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("write "), "{e}");
}

#[test]
fn exactly_one_form_of_each_secret_is_taken() {
    // Catches: both forms given, or neither, taken for one of them.
    let file = Path::new("key.asc");
    assert!(matches!(
        Source::choose("key", Some(file), None),
        Ok(Source::File(f)) if f == file
    ));
    assert!(matches!(
        Source::choose("key", None, Some("APT_KEY")),
        Ok(Source::Env("APT_KEY"))
    ));
    for (file, env) in [(Some(file), Some("APT_KEY")), (None, None)] {
        assert_eq!(
            Source::choose("passphrase", file, env).err(),
            Some(Error::Usage(
                "give exactly one of --passphrase-file and --passphrase-env".to_owned()
            ))
        );
    }
}

#[test]
fn the_process_environment_answers_for_a_variable_that_is_set_and_not_for_one_that_is_not() {
    // Catches: a lookup that reads nothing, or invents a value.
    assert!(process_env("PATH").is_some_and(|p| !p.is_empty()));
    assert_eq!(
        process_env("JS_XTASK_SIGN_TEST_VARIABLE_THAT_IS_NEVER_SET"),
        None
    );
}
