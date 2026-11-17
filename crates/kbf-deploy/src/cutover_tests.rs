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

/// A frontend's `[executor]` table that has everything the switch keeps, and names no executor.
pub(crate) fn ready_config() -> String {
    format!(
        "# the frontend\n[scheduler]\nstate-store = \"postgres:///kbf\"\n\n[executor]\n\
         # where actions run\nexecutor-frontend-id = \"fe-a\"\n\
         executor-base-image = \"registry.example/base@sha256:{d}\"\n\
         executor-kbf-image = \"docker://{KBF_IMAGE}\"\n\
         executor-base-tree = \"/opt/kbf/base-tree\"\n\
         \n[packs]\n\
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

/// Behaviour: the rendered flags are exactly the executor flags of the job service, and hold the token file's path and never its token.
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
        ]
    );
    assert!(flags.iter().all(|f| !f.contains(SECRET)));
}

/// Behaviour: the plan prints each key now and then, the flags and what is kept, exits 0, and
/// holds no token; the file is left as it was. A file that names no executor is `no executor`
/// now.
#[test]
fn the_plan_of_a_ready_file_prints_now_and_then() {
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
         \x20 executor now: no executor\n\
         \x20 executor: absent -> \"js-agent\"\n\
         \x20 executor-job-url: absent -> \"http://127.0.0.1:9443\"\n\
         \x20 executor-job-token-file: absent -> \"{token}\"\n\
         \x20 executor-job-ca-file: absent unchanged absent\n\
         the executor flags of the job-service mode:\n\
         \x20 --executor=js-agent\n\
         \x20 --executor-job-url=http://127.0.0.1:9443\n\
         \x20 --executor-job-token-file={token}\n\
         kept from the file (the executor needs them):\n\
         \x20 executor-frontend-id = \"fe-a\"\n\
         \x20 executor-base-image = \"registry.example/base@sha256:{d}\"\n\
         \x20 executor-kbf-image = \"docker://{KBF_IMAGE}\"\n\
         \x20 executor-base-tree = \"/opt/kbf/base-tree\"\n\
         to switch: kbf-deploy job-service apply with these flags and the restart's (a dry \
         run without --apply)\n",
        config = config.display(),
        d = "ff".repeat(32),
    );
    assert_eq!(out, expected);
    assert!(!out.contains(SECRET));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
}

/// Behaviour: a file with no `executor` key is `no executor` now, and one without an
/// `[executor]` table plans every key from absent; what it lacks fails the plan (exit 1) after
/// printing it, naming each missing key.
#[test]
fn a_file_that_lacks_the_kept_keys_fails_after_the_plan() {
    let dir = scratch("missing");
    let config = write(dir.path(), "kbf.toml", "[scheduler]\nstate-store = \"x\"\n");
    let t = target(dir.path(), &config, "http://127.0.0.1:9443", false);
    let (code, out, err) = planned(&t);
    assert_eq!(code, ExitCode::from(1));
    assert!(out.contains("  executor now: no executor\n"), "{out}");
    assert!(
        out.contains("  executor: absent -> \"js-agent\"\n"),
        "{out}"
    );
    assert!(!out.contains("back-out"), "{out}");
    assert!(
        out.contains("  MISSING: executor-frontend-id (a text value)\n"),
        "{out}"
    );
    assert_eq!(
        err,
        format!(
            "FAIL: not ready: [executor] in {} lacks executor-frontend-id (a text value); \
             executor-base-image (a text value); executor-kbf-image (a text value); \
             executor-base-tree (a text value)\n",
            config.display()
        )
    );
}

/// Behaviour: a file without the keys of the removed presigned path is ready, and each key of
/// it that the file still holds is missing, named for deletion, whatever its value; an empty
/// frontend id is missing too.
#[test]
fn a_removed_key_is_missing_until_it_is_deleted() {
    let table =
        |extra: &str| Executor::parse(Path::new("k"), &format!("[executor]\n{extra}")).unwrap();
    for key in super::REMOVED {
        for value in ["true", "\"files\""] {
            let held = Readiness::of(&table(&format!("{key} = {value}\n")));
            let want = format!("the deletion of {key}: the frontend refuses the flag");
            assert!(
                held.missing.iter().any(|m| m.starts_with(&want)),
                "{key}: {held:?}"
            );
        }
        let clear = Readiness::of(&table(""));
        assert!(
            clear.missing.iter().all(|m| !m.contains(key)),
            "{key}: {clear:?}"
        );
    }
    let empty_id = Readiness::of(&table("executor-frontend-id = \"\"\n"));
    assert!(
        empty_id
            .missing
            .contains(&"executor-frontend-id (a text value)".to_owned())
    );
}

/// Behaviour: a file that still names the deleted `kbf-daemon` executor plans to replace it
/// with `js-agent`.
#[test]
fn a_file_naming_the_deleted_daemon_executor_plans_to_replace_it() {
    let dir = scratch("old-key");
    let text = ready_config().replace(
        "# where actions run\n",
        "# where actions run\nexecutor = \"kbf-daemon\"\n",
    );
    let config = write(dir.path(), "kbf.toml", &text);
    let t = target(dir.path(), &config, "http://127.0.0.1:9443", false);
    let (code, out, err) = planned(&t);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(out.contains("  executor now: kbf-daemon\n"), "{out}");
    assert!(
        out.contains("  executor: \"kbf-daemon\" -> \"js-agent\"\n"),
        "{out}"
    );
}

/// Behaviour: the executor now is the file's `executor` text, `kbf-daemon` of an old file
/// included, and `no executor` when the key is absent or not text; every kind of value prints
/// as TOML, an absent key as `absent`.
#[test]
fn the_mode_is_the_executor_key_or_none() {
    let mode = |body: &str| {
        Executor::parse(Path::new("k"), &format!("[executor]\n{body}"))
            .unwrap()
            .mode()
    };
    assert_eq!(mode("executor = \"js-agent\"\n"), "js-agent");
    assert_eq!(mode("executor = \"kbf-daemon\"\n"), "kbf-daemon");
    assert_eq!(mode("executor = 3\n"), NO_EXECUTOR);
    assert_eq!(mode(""), NO_EXECUTOR);
    assert_eq!(Value::Other(toml::Value::Float(1.5)).to_string(), "1.5");
    assert_eq!(Value::Switch(true).to_string(), "true");
    assert_eq!(Value::Absent.to_string(), "absent");
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
