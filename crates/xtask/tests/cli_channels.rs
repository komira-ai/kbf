//! `cargo xtask channels ...` through the built program: the exit status of each kind of end
//! (a pass, a failure, a usage error), what is printed where, and the verbs that need no program
//! outside. Nothing here reaches a network, a cloud or a signing key: the programs the verbs
//! run are paths of files that are not there.

use std::path::Path;
use std::process::{Command, Output};

use kbf_testkit::dir::TestDir;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// A fake bearer for the runner's token request; it must never be printed.
const BEARER: &str = "runner-bearer-for-the-test";

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// Runs `xtask channels <args>` with the runner's token variables unset.
fn channels(args: &[&str]) -> Output {
    Command::new(XTASK)
        .arg("channels")
        .args(args)
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        .output()
        .expect("run xtask")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

fn code(out: &Output) -> Option<i32> {
    out.status.code()
}

/// `xtask channels gate <args>` with `vars` set, and every other variable the gate may read unset.
fn gate(args: &[&str], vars: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(XTASK);
    cmd.args(["channels", "gate"]).args(args);
    for name in ["GATE_A", "GATE_B", "GATE_READ", "GATE_SECRET"] {
        cmd.env_remove(name);
    }
    for (name, value) in vars {
        cmd.env(name, value);
    }
    cmd.output().expect("run xtask")
}

#[test]
fn a_gate_with_no_setting_set_is_green_and_says_so_on_stdout() {
    // Catches: an unset repository variable turning the job red, or the notice going to stderr
    // where the runner does not read it; `enabled=false` is the job's output. A variable that
    // is empty is as unset as one that is absent.
    let dir = scratch("gate-none");
    let output = dir.join("output");
    let out = gate(
        &[
            "--label",
            "testing-apt-read",
            "--setting",
            "GATE_A=",
            "--setting",
            "GATE_B=",
            "--secret",
            "GATE_SECRET",
            "--output",
            output.to_str().expect("path"),
        ],
        &[("GATE_SECRET", "")],
    );
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        "::notice title=The testing-apt-read job is off::None of its settings (GATE_A, GATE_B, GATE_SECRET) is set, so it writes nothing and ends green.\n"
    );
    assert_eq!(text(&out.stderr), "");
    assert_eq!(
        std::fs::read_to_string(&output).expect("output"),
        "enabled=false\n"
    );
}

#[test]
fn a_gate_with_every_setting_set_is_enabled_and_never_prints_a_value() {
    // Catches: a secret's value reaching the log, and a configured job left off.
    let dir = scratch("gate-all");
    let output = dir.join("output");
    let out = gate(
        &[
            "--label",
            "j",
            "--setting",
            "GATE_A=a-setting-value",
            "--secret",
            "GATE_SECRET",
            "--require",
            "GATE_READ=true",
            "--output",
            output.to_str().expect("path"),
        ],
        &[("GATE_SECRET", "secret-value")],
    );
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "j: all its settings are set\n");
    assert_eq!(text(&out.stderr), "");
    assert!(!format!("{out:?}").contains("secret-value"));
    assert_eq!(
        std::fs::read_to_string(&output).expect("output"),
        "enabled=true\n"
    );
}

#[test]
fn a_gate_whose_required_output_is_not_true_is_off_and_green() {
    // Catches: the job that follows a disabled job failing for its own settings.
    let out = gate(
        &[
            "--label",
            "j",
            "--setting",
            "GATE_A=x",
            "--require",
            "GATE_READ=false",
        ],
        &[],
    );
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        "::notice title=The j job is off::GATE_READ is not true, so it writes nothing and ends green.\n"
    );
}

#[test]
fn a_gate_with_some_settings_set_is_a_failure_naming_the_missing_ones() {
    // Catches: a half-made configuration left silently off, and a value in the message.
    let out = gate(
        &[
            "--label",
            "j",
            "--setting",
            "GATE_A=hunter2",
            "--secret",
            "GATE_B",
        ],
        &[],
    );
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        "xtask: j: GATE_A set but GATE_B not: set all of them or none, so that a channel is never left half on\n"
    );
}

#[test]
fn a_gate_flag_that_is_not_of_its_form_is_a_usage_error_with_status_2() {
    // Catches: a usage error of a channels verb exiting 1 like a failure, a setting without a
    // value taken for a name, and a value echoed in the message.
    for (flag, arg, message) in [
        (
            "--setting",
            "no-equals",
            "xtask: --setting is not NAME=VALUE (a name, `=`, a value)\n",
        ),
        (
            "--setting",
            "a=1",
            "xtask: --setting name \"a\" is not upper case letters, digits and `_`\n",
        ),
        (
            "--require",
            "READ",
            "xtask: --require is not NAME=VALUE (a name, `=`, a value)\n",
        ),
        (
            "--secret",
            "a=1",
            "xtask: --secret name \"a=1\" is not upper case letters, digits and `_`\n",
        ),
    ] {
        let out = channels(&["gate", "--label", "j", flag, arg]);
        assert_eq!(code(&out), Some(2), "{out:?}");
        assert_eq!(text(&out.stderr), message);
    }
}

#[test]
fn a_gate_name_given_twice_is_a_usage_error_with_status_2() {
    // Catches: a setting and a secret of one name, the second silently shadowing the first.
    let out = channels(&["gate", "--label", "j", "--setting", "A=1", "--secret", "A"]);
    assert_eq!(code(&out), Some(2), "{out:?}");
    assert_eq!(text(&out.stderr), "xtask: --secret A is given twice\n");
}

#[test]
fn a_release_of_two_packages_names_its_version_in_the_output_file() {
    // Catches: the verify job's `package-version` output missing or naming another version.
    let dir = scratch("apt-version");
    let commit = "0123456789abcdef0123456789abcdef01234567";
    for (platform, arch) in [("x86_64", "amd64"), ("arm64", "arm64")] {
        std::fs::write(
            dir.join(format!("kbf-native-{commit}-linux-{platform}.deb")),
            deb(arch, "0.1.7472"),
        )
        .expect("deb");
    }
    let output = dir.join("output");
    let out = channels(&[
        "apt",
        "version",
        "--debs",
        dir.to_str().expect("path"),
        "--commit",
        commit,
        "--output",
        output.to_str().expect("path"),
    ]);
    assert_eq!(code(&out), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "apt version: 0.1.7472\n");
    assert_eq!(
        std::fs::read_to_string(&output).expect("output"),
        "package-version=0.1.7472\n"
    );
    // The other architecture says another version: refused, and nothing more is written.
    std::fs::write(
        dir.join(format!("kbf-native-{commit}-linux-arm64.deb")),
        deb("arm64", "0.1.7473"),
    )
    .expect("deb");
    let out = channels(&[
        "apt",
        "version",
        "--debs",
        dir.to_str().expect("path"),
        "--commit",
        commit,
        "--output",
        output.to_str().expect("path"),
    ]);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert!(text(&out.stderr).contains("is version 0.1.7473, but"));
    assert_eq!(
        std::fs::read_to_string(&output).expect("output"),
        "package-version=0.1.7472\n"
    );
}

/// A Debian package whose control says `kbf`, `version` and `arch`: an `ar` archive of
/// `debian-binary` and a gzip tar `control.tar.gz` that holds `./control`.
fn deb(arch: &str, version: &str) -> Vec<u8> {
    use std::io::Write;
    let control = format!("Package: kbf\nVersion: {version}\nArchitecture: {arch}\n");
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(control.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "./control", control.as_bytes())
        .expect("append");
    let gz = tar.into_inner().expect("tar").finish().expect("gz");
    let mut out = b"!<arch>\n".to_vec();
    for (name, content) in [("debian-binary", b"2.0\n".to_vec()), ("control.tar.gz", gz)] {
        write!(
            out,
            "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`",
            0,
            0,
            0,
            "100644",
            content.len()
        )
        .expect("header");
        out.push(b'\n');
        out.extend_from_slice(&content);
        if content.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

#[test]
fn flags_clap_refuses_are_usage_errors_too() {
    // Catches: a write that names a pool and none (or half of one), and a suite that is not one.
    for args in [
        &[
            "apt",
            "write",
            "--bucket",
            "b",
            "--distribution-id",
            "E1",
            "--hostname",
            "h",
            "--next",
            "n",
            "--debs",
            "d",
            "--version",
            "1.2.3",
        ][..],
        &[
            "apt",
            "write",
            "--bucket",
            "b",
            "--distribution-id",
            "E1",
            "--hostname",
            "h",
            "--next",
            "n",
            "--no-pool",
            "--debs",
            "d",
            "--commit",
            "0123456789abcdef0123456789abcdef01234567",
            "--version",
            "1.2.3",
        ][..],
        &[
            "apt", "read", "--suite", "beta", "--bucket", "b", "--out", "o",
        ][..],
    ] {
        let out = channels(args);
        assert_eq!(code(&out), Some(2), "{args:?}: {out:?}");
    }
}

#[test]
fn assume_without_the_runners_token_variables_fails_before_anything_is_written() {
    // Catches: a job without `id-token: write` going on with an empty token.
    let dir = scratch("assume-none");
    let env_file = dir.join("env");
    let out = channels(&[
        "assume",
        "--role-arn",
        "arn:aws:iam::123456789012:role/r",
        "--session-name",
        "kbf-testing-apt-read-1",
        "--region",
        "us-east-1",
        "--env-file",
        env_file.to_str().expect("path"),
        "--token-url",
        "",
    ]);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert_eq!(text(&out.stdout), "");
    assert_eq!(
        text(&out.stderr),
        "xtask: the runner offers no OIDC token (the job needs `permissions: id-token: write`)\n"
    );
    assert!(!env_file.exists());
}

#[test]
fn assume_masks_the_role_and_the_bearer_and_refuses_a_token_request_over_plain_http() {
    // Catches: the bearer sent in the clear, and a secret printed in the failure.
    let dir = scratch("assume-http");
    let env_file = dir.join("env");
    let out = Command::new(XTASK)
        .args([
            "channels",
            "assume",
            "--role-arn",
            "arn:aws:iam::123456789012:role/r",
            "--session-name",
            "kbf-testing-apt-read-1",
            "--region",
            "us-east-1",
            "--env-file",
        ])
        .arg(&env_file)
        .args(["--token-url", "http://127.0.0.1/token"])
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", BEARER)
        .output()
        .expect("run xtask");
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        format!("::add-mask::arn:aws:iam::123456789012:role/r\n::add-mask::{BEARER}\n")
    );
    let err = text(&out.stderr);
    assert!(
        err.starts_with("xtask: the OIDC token request failed: "),
        "{err}"
    );
    assert!(!err.contains(BEARER), "{err}");
    assert!(!env_file.exists());
}

#[test]
fn check_apt_names_the_program_it_could_not_start() {
    // Catches: a missing `dpkg` reported as a pass, or with no program named.
    let dir = scratch("check-spawn");
    let dpkg = dir.join("no-dpkg");
    let out = channels(&[
        "check-apt",
        "--arch",
        "amd64",
        "--version",
        "1.2.3",
        "--commit",
        "0123456789abcdef0123456789abcdef01234567",
        "--root",
        dir.to_str().expect("path"),
        "--dpkg",
        dpkg.to_str().expect("path"),
    ]);
    assert_eq!(code(&out), Some(1), "{out:?}");
    let err = text(&out.stderr);
    assert_eq!(
        err,
        format!(
            "xtask: {}: No such file or directory (os error 2)\n",
            dpkg.display()
        )
    );
}

#[test]
fn check_apt_with_a_version_out_of_its_form_is_a_usage_error() {
    // Catches: a version with a prefix reaching `apt-get install kbf=...`.
    let out = channels(&[
        "check-apt",
        "--arch",
        "amd64",
        "--version",
        "v1.2.3",
        "--commit",
        "0123456789abcdef0123456789abcdef01234567",
    ]);
    assert_eq!(code(&out), Some(2), "{out:?}");
    assert_eq!(
        text(&out.stderr),
        "xtask: --version \"v1.2.3\" is not <major>.<minor>.<patch>\n"
    );
}

#[test]
fn a_failed_sign_removes_the_key_and_the_passphrase_it_was_given() {
    // Catches: a private key left on the runner's disk when the signing fails.
    let dir = scratch("sign-fails");
    let (key, passphrase) = (dir.join("key.asc"), dir.join("passphrase"));
    std::fs::write(&key, "not a key").expect("key");
    std::fs::write(&passphrase, "phrase").expect("passphrase");
    let out = channels(&[
        "apt",
        "sign",
        "--dir",
        dir.join("dists/testing").to_str().expect("path"),
        "--work",
        dir.join("work").to_str().expect("path"),
        "--fingerprint",
        "0123456789ABCDEF0123456789ABCDEF01234567",
        "--key-file",
        key.to_str().expect("path"),
        "--passphrase-file",
        passphrase.to_str().expect("path"),
        "--gpg",
        dir.join("no-gpg").to_str().expect("path"),
    ]);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert!(text(&out.stderr).starts_with("xtask: "));
    assert!(!key.exists() && !passphrase.exists());
}

#[test]
fn a_failed_sign_by_environment_removes_the_files_it_made_and_never_prints_the_secrets() {
    // Catches: a key written from the environment and left on the runner's disk when the signing
    // fails, and a secret in the message.
    let dir = scratch("sign-env-fails");
    let work = dir.join("work");
    let out = Command::new(XTASK)
        .args(["channels", "apt", "sign", "--dir"])
        .arg(dir.join("dists/testing"))
        .arg("--work")
        .arg(&work)
        .args(["--fingerprint", "0123456789ABCDEF0123456789ABCDEF01234567"])
        .args([
            "--key-env",
            "SIGN_TEST_KEY",
            "--passphrase-env",
            "SIGN_TEST_PASS",
        ])
        .env("SIGN_TEST_KEY", "key-text-of-the-test")
        .env("SIGN_TEST_PASS", "passphrase-text-of-the-test")
        .output()
        .expect("run xtask");
    assert_eq!(code(&out), Some(1), "{out:?}");
    let said = text(&out.stderr);
    assert!(
        said.contains("dists/testing/Release does not exist"),
        "{said}"
    );
    assert!(!said.contains("text-of-the-test"), "{said}");
    assert!(!work.join("apt-key.asc").exists() && !work.join("apt-passphrase").exists());
}

#[test]
fn a_sign_given_both_forms_of_a_secret_or_neither_is_a_usage_error() {
    // Catches: two sources for one secret taken silently for one of them, and a sign with no key.
    let dir = scratch("sign-forms");
    let sign = |extra: &[&str]| {
        let out = channels(
            &[
                &[
                    "apt",
                    "sign",
                    "--dir",
                    "d",
                    "--work",
                    "w",
                    "--fingerprint",
                    "f",
                ][..],
                extra,
            ]
            .concat(),
        );
        (code(&out), text(&out.stderr))
    };
    let (status, said) = sign(&["--key-file", "k", "--key-env", "K", "--passphrase-env", "P"]);
    assert_eq!(status, Some(2), "{said}");
    assert!(
        said.contains("--key-file") && said.contains("--key-env"),
        "{said}"
    );
    let (status, said) = sign(&["--key-env", "K"]);
    assert_eq!(status, Some(2), "{said}");
    assert!(said.contains("--passphrase-file"), "{said}");
    let (status, said) = sign(&[
        "--passphrase-env",
        "P",
        "--passphrase-file",
        "p",
        "--key-env",
        "K",
    ]);
    assert_eq!(status, Some(2), "{said}");
    let (status, said) = sign(&["--passphrase-file", "p"]);
    assert_eq!(status, Some(2), "{said}");
    assert!(said.contains("--key-file"), "{said}");
    assert!(!dir.join("w").exists());
}

#[test]
fn an_apt_read_with_no_aws_program_fails_naming_it_and_writes_nothing() {
    // Catches: a read that reports success with nothing on disk.
    let dir = scratch("read-no-aws");
    let aws = dir.join("no-aws");
    let out_dir = dir.join("read");
    let out = channels(&[
        "apt",
        "read",
        "--suite",
        "testing",
        "--bucket",
        "apt-bucket",
        "--out",
        out_dir.to_str().expect("path"),
        "--aws",
        aws.to_str().expect("path"),
    ]);
    assert_eq!(code(&out), Some(1), "{out:?}");
    assert!(text(&out.stderr).contains(&aws.display().to_string()));
    assert!(!Path::new(&out_dir).join("etags.txt").exists());
}
