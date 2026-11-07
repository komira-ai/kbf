//! `job-service plan`: the settings rendered from the flags, the plan printed from a
//! `kbf.toml`, and every refusal. Each test writes its files in a scratch directory of its own.

use std::path::Path;

use kbf_testkit::dir::TestDir;

use super::*;

/// A token in the token file that no output may hold: finding it anywhere is the token
/// having been read and printed.
pub(crate) const SECRET: &str = "kbf-plan-SECRET-token-0c9d8e7f6a5b4c3d";

pub(crate) const KBF_IMAGE: &str =
    "registry.example/kbf@sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// A frontend's `[executor]` table that has everything the switch keeps, on the daemons.
pub(crate) fn ready_config() -> String {
    format!(
        "# the frontend\n[scheduler]\nstate-store = \"postgres:///kbf\"\n\n[executor]\n\
         # where actions run\nexecutor = \"kbf-daemon\"\nexecutor-frontend-id = \"fe-a\"\n\
         executor-base-image = \"registry.example/base@sha256:{d}\"\n\
         executor-kbf-image = \"docker://{KBF_IMAGE}\"\n\
         executor-secret-delivery = \"files\"\nexecutor-host-slots = 8\n\n[packs]\n\
         pack-read = true\n",
        d = "ff".repeat(32)
    )
}

pub(crate) fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-deploy-cutover-{name}"))
}

/// `text` written to `name` in `dir`, mode 0644.
pub(crate) fn write(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// The flags of a switch to `url` with `config`, and a token file holding [`SECRET`].
pub(crate) fn target(dir: &Path, config: &Path, url: &str, ca: bool) -> Target {
    Target {
        config: config.to_path_buf(),
        url: url.to_owned(),
        token_file: write(dir, "job-service.token", SECRET),
        ca_file: ca.then(|| dir.join("ca.pem")),
        hosts: 4,
        host_cpus: 16,
        host_memory_gib: 64,
        host_slots: 8,
    }
}

/// `plan`'s exit code, stdout and stderr.
fn planned(t: &Target) -> (ExitCode, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_plan(t, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Behaviour: the rendered flags are exactly the executor flags of the job service and the
/// worker shape, and hold the token file's path and never its token.
#[test]
fn the_flags_are_the_job_service_executor_flags_and_hold_no_token() {
    let dir = scratch("flags");
    let config = write(dir.path(), "kbf.toml", &ready_config());
    let t = target(dir.path(), &config, "https://192.0.2.30:8443", true);
    let token = t.token_file.display().to_string();
    let ca = dir.path().join("ca.pem").display().to_string();
    let flags = Settings::of(&t).unwrap().flags();
    assert_eq!(
        flags,
        [
            "--executor=js-agent".to_owned(),
            "--executor-job-url=https://192.0.2.30:8443".to_owned(),
            format!("--executor-job-token-file={token}"),
            format!("--executor-job-ca-file={ca}"),
            "--executor-hosts=4".to_owned(),
            "--executor-host-cpus=16".to_owned(),
            "--executor-host-memory-gib=64".to_owned(),
            "--executor-host-slots=8".to_owned(),
        ]
    );
    assert!(flags.iter().all(|f| !f.contains(SECRET)));
}

/// Behaviour: the plan prints each key now and then, the flags, what is kept and the
/// back-out, exits 0, and holds no token; the file is left as it was.
#[test]
fn the_plan_of_a_ready_file_prints_now_then_and_the_back_out() {
    let dir = scratch("plan");
    let text = ready_config();
    let config = write(dir.path(), "kbf.toml", &text);
    let t = target(dir.path(), &config, "http://127.0.0.1:9443", false);
    let token = t.token_file.display().to_string();
    let (code, out, err) = planned(&t);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert_eq!(err, "");
    let expected = format!(
        "kbf-deploy job-service plan (changes nothing)\n\
         config: [executor] in {config}\n\
         \x20 executor now: kbf-daemon\n\
         \x20 executor: \"kbf-daemon\" -> \"js-agent\"\n\
         \x20 executor-job-url: absent -> \"http://127.0.0.1:9443\"\n\
         \x20 executor-job-token-file: absent -> \"{token}\"\n\
         \x20 executor-job-ca-file: absent unchanged absent\n\
         \x20 executor-hosts: absent -> 4\n\
         \x20 executor-host-cpus: absent -> 16\n\
         \x20 executor-host-memory-gib: absent -> 64\n\
         \x20 executor-host-slots: 8 unchanged 8\n\
         the executor flags of the job-service mode:\n\
         \x20 --executor=js-agent\n\
         \x20 --executor-job-url=http://127.0.0.1:9443\n\
         \x20 --executor-job-token-file={token}\n\
         \x20 --executor-hosts=4\n\
         \x20 --executor-host-cpus=16\n\
         \x20 --executor-host-memory-gib=64\n\
         \x20 --executor-host-slots=8\n\
         kept from the file (the executor needs them):\n\
         \x20 executor-frontend-id = \"fe-a\"\n\
         \x20 executor-base-image = \"registry.example/base@sha256:{d}\"\n\
         \x20 executor-kbf-image = \"docker://{KBF_IMAGE}\"\n\
         \x20 executor-secret-delivery = \"files\"\n\
         back-out: the flags of the mode it runs now:\n\
         \x20 --executor=kbf-daemon\n\
         \x20 no --executor-job-url\n\
         \x20 no --executor-job-token-file\n\
         \x20 no --executor-job-ca-file\n\
         \x20 no --executor-hosts\n\
         \x20 no --executor-host-cpus\n\
         \x20 no --executor-host-memory-gib\n\
         \x20 --executor-host-slots=8\n\
         to switch: kbf-deploy job-service apply with these flags and the restart's (a dry \
         run without --apply)\n",
        config = config.display(),
        d = "ff".repeat(32),
    );
    assert_eq!(out, expected);
    assert!(!out.contains(SECRET));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
}

/// Behaviour: a file with no `executor` key names the default as the mode to go back to,
/// and one without an `[executor]` table plans every key from absent; what it lacks fails
/// the plan (exit 1) after printing it, naming each missing key.
#[test]
fn a_file_that_lacks_the_kept_keys_fails_after_the_plan() {
    let dir = scratch("missing");
    let config = write(dir.path(), "kbf.toml", "[scheduler]\nstate-store = \"x\"\n");
    let t = target(dir.path(), &config, "http://127.0.0.1:9443", false);
    let (code, out, err) = planned(&t);
    assert_eq!(code, ExitCode::from(1));
    assert!(out.contains("  executor now: kbf-daemon\n"), "{out}");
    assert!(
        out.contains("  executor: absent -> \"js-agent\"\n"),
        "{out}"
    );
    assert!(
        out.contains("  --executor=kbf-daemon (the default: the file has no key)\n"),
        "{out}"
    );
    assert!(
        out.contains("  MISSING: executor-frontend-id (a text value)\n"),
        "{out}"
    );
    assert_eq!(
        err,
        format!(
            "FAIL: not ready: [executor] in {} lacks executor-frontend-id (a text value); \
             executor-base-image (a text value); executor-kbf-image (a text value); the secret \
             delivery: executor-secret-delivery = \"files\", or executor-insecure-argv-secrets = \
             true for \"argv\"\n",
            config.display()
        )
    );
}

/// Behaviour: the secret delivery is kept as `argv` with its opt-in, and both at once is
/// what the executor refuses, so it is missing.
#[test]
fn the_secret_delivery_is_read_as_the_executor_reads_it() {
    let table =
        |extra: &str| Executor::parse(Path::new("k"), &format!("[executor]\n{extra}")).unwrap();
    let argv = Readiness::of(&table("executor-insecure-argv-secrets = true\n"));
    assert!(
        argv.kept
            .iter()
            .any(|k| k.contains("accepted by executor-insecure-argv-secrets"))
    );
    let both = Readiness::of(&table(
        "executor-insecure-argv-secrets = true\nexecutor-secret-delivery = \"files\"\n",
    ));
    assert!(
        both.missing.iter().any(|m| m.contains("refused with")),
        "{both:?}"
    );
    let empty_id = Readiness::of(&table("executor-frontend-id = \"\"\n"));
    assert!(
        empty_id
            .missing
            .contains(&"executor-frontend-id (a text value)".to_owned())
    );
}

/// Behaviour: the back-out flags carry each value the file holds now, whatever its kind.
#[test]
fn the_back_out_flags_carry_every_kind_of_value() {
    let table = Executor::parse(
        Path::new("k"),
        "[executor]\nexecutor = \"js-agent\"\nexecutor-hosts = 2\nexecutor-job-url = [1]\n\
         executor-job-ca-file = true\nexecutor-job-token-file = false\n",
    )
    .unwrap();
    assert_eq!(table.mode(), "js-agent");
    assert_eq!(
        table.back_out_flags(),
        [
            "--executor=js-agent",
            "--executor-job-url=[1]",
            "no --executor-job-token-file",
            "--executor-job-ca-file",
            "--executor-hosts=2",
            "no --executor-host-cpus",
            "no --executor-host-memory-gib",
            "no --executor-host-slots",
        ]
    );
    assert_eq!(Value::Other(toml::Value::Float(1.5)).to_string(), "1.5");
    assert_eq!(Value::Switch(true).to_string(), "true");
    let numbered = Executor::parse(Path::new("k"), "[executor]\nexecutor = 3\n").unwrap();
    assert_eq!(numbered.mode(), DEFAULT_EXECUTOR);
}

/// Behaviour: every bad flag is refused (exit 2) naming the flag, with nothing on stdout.
#[test]
fn bad_flags_are_refused_naming_the_flag() {
    let dir = scratch("refused");
    let config = write(dir.path(), "kbf.toml", &ready_config());
    let base = target(dir.path(), &config, "https://192.0.2.30:8443", true);
    let cases: Vec<(Target, &str)> = vec![
        (
            Target {
                ca_file: None,
                ..base.clone()
            },
            "--url is https: give --ca-file",
        ),
        (
            Target {
                url: "http://127.0.0.1:9443".to_owned(),
                ..base.clone()
            },
            "--ca-file is for an https --url",
        ),
        (
            Target {
                url: "ftp://x:1".to_owned(),
                ..base.clone()
            },
            "--url: ",
        ),
        (
            Target {
                url: "https://a b:1".to_owned(),
                ..base.clone()
            },
            "--url: \"https://a b:1\" is empty or holds",
        ),
        (
            Target {
                url: String::new(),
                ..base.clone()
            },
            "--url: \"\" is empty",
        ),
        (
            Target {
                url: "https://a\u{7f}:1".to_owned(),
                ..base.clone()
            },
            "--url: ",
        ),
        (
            Target {
                url: "https://a'b:1".to_owned(),
                ..base.clone()
            },
            "--url: ",
        ),
        (
            Target {
                token_file: "rel/token".into(),
                ..base.clone()
            },
            "--token-file: rel/token is not an absolute path",
        ),
        (
            Target {
                ca_file: Some("/etc/kbf/ca$1.pem".into()),
                ..base.clone()
            },
            "--ca-file: \"/etc/kbf/ca$1.pem\" is empty or holds",
        ),
        (
            Target {
                hosts: u64::MAX,
                ..base.clone()
            },
            "--hosts: 18446744073709551615 is too large",
        ),
        (
            Target {
                config: dir.path().join("none.toml"),
                ..base.clone()
            },
            "--config ",
        ),
    ];
    for (t, start) in cases {
        let (code, out, err) = planned(&t);
        assert_eq!(code, ExitCode::from(2), "{err}");
        assert_eq!(out, "");
        assert!(
            err.starts_with(&format!("REFUSED: {start}")),
            "{start}: {err}"
        );
    }
}

/// Behaviour: a path that is not UTF-8 is refused naming the flag.
#[test]
fn a_path_that_is_not_utf8_is_refused() {
    use std::os::unix::ffi::OsStrExt as _;
    let path = Path::new(std::ffi::OsStr::from_bytes(b"/etc/\xff"));
    let e = absolute("--token-file", path).unwrap_err();
    assert_eq!(e.to_string(), "--token-file: /etc/\u{fffd} is not UTF-8");
}

/// Behaviour: a file that is not TOML, or whose `executor` is not a table, is refused.
#[test]
fn a_file_that_is_not_toml_or_has_no_executor_table_is_refused() {
    let dir = scratch("not-toml");
    for (text, says) in [
        ("[executor\n", "is not TOML: "),
        ("executor = 1\n", "`executor` is not a table"),
    ] {
        let config = write(dir.path(), "kbf.toml", text);
        let (code, out, err) = planned(&target(dir.path(), &config, "http://127.0.0.1:1", false));
        assert_eq!((code, out.as_str()), (ExitCode::from(2), ""));
        assert!(err.contains(says), "{err}");
    }
}
