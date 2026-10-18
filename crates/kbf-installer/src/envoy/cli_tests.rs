use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::cli::{self, Outcome};
use super::fetch::CurlFetcher;
use super::testing::*;
use crate::fixtures::{FakeRunner, me, ran, sha256_hex};
use crate::run::RunAs;

fn words(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_owned).collect()
}

/// A manifest file the host's owner wrote, closed to group and other writes.
fn manifest_file(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("manifest.json");
    fs::write(&path, text).expect("manifest");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
    path
}

fn good_manifest(bytes: &[u8]) -> String {
    manifest(VERSION, URL, &sha256_hex(bytes))
}

fn say(o: &Outcome) -> serde_json::Value {
    serde_json::from_str(&o.stdout).unwrap_or_else(|e| panic!("{e}: {o:?}"))
}

/// Catches a dry run that does something: without `--apply`, `fetch` reads and checks
/// the manifest, says what it would do (JSON and text), exits 0, and downloads nothing,
/// runs nothing, makes no directory, no state file and no scratch.
///
/// Mutant seen red: the apply flag ignored (the dry run installs).
#[test]
fn fetch_without_apply_is_a_dry_run() {
    let bytes = envoy();
    let (host, dir) = scratch_host("cli-dry");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.clone()));
    let e = env(&host, &runner, &fetcher);
    let file = manifest_file(&dir, &good_manifest(&bytes));
    let args = words(&format!("fetch --manifest {}", file.display()));

    let text = cli::run(&e, &args);
    assert_eq!((text.code, text.stderr.as_str()), (0, ""));
    assert!(
        text.stdout.contains(&format!("would fetch {URL}")),
        "{}",
        text.stdout
    );
    assert!(
        text.stdout.contains("dry run: nothing changed"),
        "{}",
        text.stdout
    );

    let mut json_args = args.clone();
    json_args.push("--json".to_owned());
    let json = say(&cli::run(&e, &json_args));
    assert_eq!(json["ok"], true);
    assert_eq!(json["apply"], false);
    assert_eq!(json["installed"], false);
    assert_eq!(json["id"], id(&bytes).to_string());
    assert_eq!(json["sha256"], sha256_hex(&bytes));

    assert!(fetcher.calls().is_empty());
    assert!(runner.calls().is_empty());
    assert!(!dir.join("var/lib/kbf-installer").exists());
    assert!(!dir.join("var/lib/kbf-deploy/state.json").exists());
    assert!(names(&dir.join("var/lib/kbf-deploy")).is_empty());
}

/// Catches `--apply` not doing the fetch, and the verbs after it not agreeing: apply
/// installs and pins; `verify`, `path` and `list` then answer in text and JSON; a repeat
/// is `unchanged`; a dry run of an installed build says so.
///
/// Mutants seen red: `path` printing the directory; `list` marking nothing.
#[test]
fn the_verbs_agree_after_an_apply() {
    let bytes = envoy();
    let (host, dir) = scratch_host("cli-apply");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.clone()));
    let e = env(&host, &runner, &fetcher);
    let file = manifest_file(&dir, &good_manifest(&bytes));
    let fetch = format!("fetch --manifest {} --apply", file.display());
    let id = id(&bytes);
    let target = envoy_root(&dir).join(id.as_str());

    let done = cli::run(&e, &words(&format!("{fetch} --json")));
    assert_eq!(done.code, 0, "{done:?}");
    let v = say(&done);
    assert_eq!(
        (v["done"].as_str(), v["current"].as_str()),
        (Some("installed"), Some(id.as_str()))
    );
    assert!(v["previous"].is_null());
    let text = cli::run(&e, &words(&fetch));
    assert!(text.stdout.starts_with("unchanged "), "{}", text.stdout);
    assert!(
        text.stdout
            .contains(&format!("pinned: current {id} previous none")),
        "{}",
        text.stdout
    );

    let verify = cli::run(&e, &words("verify"));
    assert_eq!(
        verify.stdout,
        format!("ok {id} sha256 {}\n", sha256_hex(&bytes))
    );
    let verify = say(&cli::run(&e, &words("verify --json")));
    assert_eq!(verify["path"], target.join("envoy").to_str().expect("utf8"));

    let path = cli::run(&e, &words("path"));
    assert_eq!(path.stdout, format!("{}\n", target.join("envoy").display()));
    assert_eq!(
        say(&cli::run(&e, &words("path --json")))["path"],
        target.join("envoy").to_str().expect("utf8")
    );

    let list = cli::run(&e, &words("list"));
    assert_eq!(list.stdout, format!("{id} current\n"));
    let list = say(&cli::run(&e, &words("list --json")));
    assert_eq!(list["builds"][0]["id"], id.to_string());
    assert_eq!(list["builds"][0]["current"], true);
    assert_eq!(list["builds"][0]["previous"], false);

    let dry = say(&cli::run(
        &e,
        &words(&format!("fetch --manifest {} --json", file.display())),
    ));
    assert_eq!(dry["installed"], true);
    let dry = cli::run(&e, &words(&format!("fetch --manifest {}", file.display())));
    assert!(
        dry.stdout.contains("is already installed"),
        "{}",
        dry.stdout
    );

    // A second build makes the first the previous.
    let other = fake_envoy(62, "1.40.0");
    let other_manifest = manifest_file(&dir, &manifest("1.40.0", URL, &sha256_hex(&other)));
    let fetcher = FakeFetcher::serving(other.clone());
    let e = env(&host, &runner, &fetcher);
    let second = cli::run(
        &e,
        &words(&format!(
            "fetch --manifest {} --apply",
            other_manifest.display()
        )),
    );
    assert_eq!(second.code, 0, "{second:?}");
    let third = fake_envoy(62, "1.41.0");
    let third_manifest = manifest_file(&dir, &manifest("1.41.0", URL, &sha256_hex(&third)));
    let fetcher = FakeFetcher::serving(third.clone());
    let e = env(&host, &runner, &fetcher);
    assert_eq!(
        cli::run(
            &e,
            &words(&format!(
                "fetch --manifest {} --apply",
                third_manifest.display()
            ))
        )
        .code,
        0
    );
    let list = cli::run(&e, &words("list"));
    let lines: Vec<&str> = list.stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with(" -") && l.starts_with("1.31.2-")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with(" previous") && l.starts_with("1.40.0-")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.ends_with(" current") && l.starts_with("1.41.0-")),
        "{lines:?}"
    );
}

/// Catches the exit codes of the contract drifting: a manifest that is not valid (http
/// url, unknown field), a wrong hash, a wrong machine, a wrong version are refusals (3,
/// `REFUSED` with the code on stderr, or `{"ok":false,...}` with `--json`); a fetch that
/// fails is 1; a manifest that cannot be used is 2; nothing pinned is 5. Each leaves
/// nothing installed.
///
/// Mutants seen red: a refusal exiting 1; the JSON error dropping the code.
#[test]
fn exit_codes_follow_the_contract() {
    let bytes = envoy();
    let sha = sha256_hex(&bytes);
    let extra = format!(
        "{{\"envoy\":{{\"linux-x86_64\":{{\"version\":\"1.31.2\",\"url\":\"{URL}\",\
         \"sha256\":\"{sha}\",\"mirror\":\"x\"}}}}}}"
    );
    let cases: Vec<(&str, String, Vec<u8>, u8, &str)> = vec![
        (
            "http",
            manifest(VERSION, "http://upstream.example/envoy", &sha),
            bytes.clone(),
            3,
            "envoy-manifest",
        ),
        ("unknown-field", extra, bytes.clone(), 3, "envoy-manifest"),
        (
            "hash",
            good_manifest(&bytes),
            fake_envoy(62, "7.7.7"),
            3,
            "envoy-hash",
        ),
        (
            "arch",
            manifest(VERSION, URL, &sha256_hex(&fake_envoy(183, VERSION))),
            fake_envoy(183, VERSION),
            3,
            "envoy-wrong-platform",
        ),
        (
            "version",
            manifest("1.99.0", URL, &sha),
            bytes.clone(),
            3,
            "envoy-version",
        ),
    ];
    for (name, text, served, code, word) in cases {
        let (host, dir) = scratch_host(&format!("cli-exit-{name}"));
        let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(served));
        let e = env(&host, &runner, &fetcher);
        let file = manifest_file(&dir, &text);
        let args = words(&format!("fetch --manifest {} --apply", file.display()));
        let out = cli::run(&e, &args);
        assert_eq!(out.code, code, "{name}: {out:?}");
        assert!(out.stdout.is_empty(), "{name}");
        assert!(
            out.stderr.starts_with(&format!("REFUSED {word}: ")),
            "{name}: {}",
            out.stderr
        );
        let mut json = args.clone();
        json.push("--json".to_owned());
        let v = say(&cli::run(&e, &json));
        assert_eq!(
            (v["ok"].as_bool(), v["code"].as_str(), v["exit"].as_u64()),
            (Some(false), Some(word), Some(3)),
            "{name}"
        );
        assert!(v["message"].as_str().is_some_and(|m| !m.is_empty()));
        assert!(names(&envoy_root(&dir)).is_empty(), "{name}");
    }

    let (host, dir) = scratch_host("cli-exit-fetch");
    let runner = envoy_runner();
    let fetcher = FakeFetcher::new(|_| Err(super::fetch::FetchError::TimedOut));
    let e = env(&host, &runner, &fetcher);
    let file = manifest_file(&dir, &good_manifest(&bytes));
    let out = cli::run(
        &e,
        &words(&format!("fetch --manifest {} --apply", file.display())),
    );
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.starts_with("FAILED envoy-fetch: "),
        "{}",
        out.stderr
    );

    let out = cli::run(&e, &words("verify"));
    assert_eq!(
        (out.code, out.stderr.starts_with("FAILED envoy-unpinned")),
        (5, true),
        "{out:?}"
    );
    let v = say(&cli::run(&e, &words("verify --json")));
    assert_eq!(v["exit"], 5);
}

/// Catches a manifest anyone could have written being read: one writable by group, a
/// missing one, one that is a link, one over the cap and a directory each exit 2 without
/// downloading anything.
///
/// Mutant seen red: the mode check on the manifest dropped.
#[test]
fn a_manifest_that_is_not_roots_alone_is_not_read() {
    let bytes = envoy();
    let (host, dir) = scratch_host("cli-manifest-file");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.clone()));
    let e = env(&host, &runner, &fetcher);
    let file = manifest_file(&dir, &good_manifest(&bytes));
    let run = |path: &Path| {
        cli::run(
            &e,
            &words(&format!("fetch --manifest {} --apply", path.display())),
        )
    };

    fs::set_permissions(&file, fs::Permissions::from_mode(0o664)).expect("chmod");
    let out = run(&file);
    assert_eq!(out.code, 2, "{out:?}");
    assert!(
        out.stderr.contains("writable by group or others"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.starts_with("FAILED envoy-manifest-file"),
        "{}",
        out.stderr
    );
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("chmod");

    assert_eq!(run(&dir.join("absent.json")).code, 2);
    let link = dir.join("link.json");
    std::os::unix::fs::symlink(&file, &link).expect("link");
    assert_eq!(run(&link).code, 2);
    assert_eq!(run(&dir).code, 2);
    let big = dir.join("big.json");
    fs::write(&big, vec![b' '; 1024 * 1024 + 1]).expect("big");
    fs::set_permissions(&big, fs::Permissions::from_mode(0o644)).expect("chmod");
    let out = run(&big);
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("over"), "{}", out.stderr);
    assert!(fetcher.calls().is_empty());
    assert_eq!(run(&file).code, 0);
}

/// Catches the root rule being dropped: `fetch`, `verify` and `path` exit 2 when the
/// caller is not the host's owner (root on a host), and `list` does not need it.
///
/// Mutant seen red: the root check removed from `execute`.
#[test]
fn every_verb_but_list_needs_root() {
    let bytes = envoy();
    let (host, dir) = scratch_host("cli-root");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(bytes.clone()));
    let mut e = env(&host, &runner, &fetcher);
    let file = manifest_file(&dir, &good_manifest(&bytes));
    e.owner_uid = me().uid + 1;
    for line in [
        format!("fetch --manifest {} --apply", file.display()),
        "verify".to_owned(),
        "path".to_owned(),
    ] {
        let out = cli::run(&e, &words(&line));
        assert_eq!(out.code, 2, "{line}: {out:?}");
        assert!(
            out.stderr.starts_with("FAILED not-root: "),
            "{line}: {}",
            out.stderr
        );
    }
    let out = cli::run(&e, &words("list"));
    assert_eq!((out.code, out.stdout.as_str()), (0, ""));
    assert!(fetcher.calls().is_empty());
}

/// Catches arguments that are accepted loosely: every malformed command line exits 2 with
/// the usage on stderr and does nothing.
#[test]
fn a_bad_command_line_is_a_usage_error() {
    let (host, _dir) = scratch_host("cli-usage");
    let (runner, fetcher) = (envoy_runner(), FakeFetcher::serving(envoy()));
    let e = env(&host, &runner, &fetcher);
    for line in [
        "",
        "frobnicate",
        "verify extra",
        "verify --apply",
        "list --apply",
        "path --manifest m.json",
        "fetch",
        "fetch --manifest",
        "fetch --manifest a --manifest b",
        "fetch --manifest a --bogus",
        "--json",
    ] {
        let out = cli::run(&e, &words(line));
        assert_eq!(out.code, cli::USAGE, "{line:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{line:?}");
        assert!(
            out.stderr.contains("usage: kbf-installer envoy"),
            "{line:?}: {}",
            out.stderr
        );
    }
    assert!(fetcher.calls().is_empty() && runner.calls().is_empty());
}

/// Catches the minimal fetcher running something other than a bounded, https-only curl
/// as the fetch user: its argv (no curlrc, https also after a redirect, at most 3
/// redirects, no proxy, the size cap, the output and then the URL), an empty environment,
/// the user and the timeout; a program that fails, times out or cannot start is a
/// `FetchError`.
///
/// Mutants seen red: `--proto =https` dropped; the environment passed through.
#[test]
fn curl_runs_as_the_fetch_user_with_an_empty_environment() {
    let runner = FakeRunner::new(|_| Ok(ran(0, "", "")));
    let user = RunAs { uid: 7, gid: 8 };
    let fetcher = CurlFetcher {
        runner: &runner,
        curl: CurlFetcher::CURL.into(),
        run_as: Some(user),
        timeout: std::time::Duration::from_secs(600),
    };
    super::fetch::Fetcher::fetch(&fetcher, URL, Path::new("/work/download/envoy"), 4096)
        .expect("ok");
    let spawn = &runner.calls()[0];
    assert_eq!(spawn.program, Path::new("/usr/bin/curl"));
    let args: Vec<String> = spawn
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "-q",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-redirs",
            "3",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--noproxy",
            "*",
            "--max-filesize",
            "4096",
            "--output",
            "/work/download/envoy",
            URL,
        ]
    );
    assert!(spawn.env.is_empty());
    assert_eq!(spawn.run_as, Some(user));
    assert_eq!(spawn.cwd, Path::new("/work/download"));
    assert_eq!(spawn.timeout, std::time::Duration::from_secs(600));

    let dest = Path::new("/work/download/envoy");
    let broken = [
        FakeRunner::new(|_| {
            Ok(ran(
                22,
                "",
                "curl: (22) The requested URL returned error: 404\nmore\n",
            ))
        }),
        FakeRunner::new(|_| {
            let mut r = ran(0, "", "");
            r.code = None;
            r.timed_out = true;
            Ok(r)
        }),
        FakeRunner::new(|_| Err(std::io::Error::from_raw_os_error(libc::ENOENT))),
        FakeRunner::new(|_| {
            let mut r = ran(0, "", "");
            r.code = None;
            Ok(r)
        }),
    ];
    let mut said = Vec::new();
    for runner in &broken {
        let f = CurlFetcher {
            runner,
            curl: CurlFetcher::CURL.into(),
            run_as: None,
            timeout: std::time::Duration::from_secs(1),
        };
        said.push(
            super::fetch::Fetcher::fetch(&f, URL, dest, 1)
                .expect_err("fails")
                .to_string(),
        );
    }
    assert!(
        said[0].contains("exit 22") && said[0].contains("404") && !said[0].contains("more"),
        "{said:?}"
    );
    assert!(said[1].contains("out of time"), "{said:?}");
    assert!(said[2].contains("/usr/bin/curl"), "{said:?}");
    assert!(said[3].contains("signal"), "{said:?}");
}

/// Catches the real `envoy --version` output being misread: the line Envoy prints gives
/// the version in its second `/` field; a contrib build's longer line does too.
#[test]
fn the_version_is_read_from_envoys_own_line() {
    use super::install::parse_version;
    let real = b"\nenvoy  version: 5c2eb58cfc2fb6d3df6e3dd3d5e1bd4cc8a5de5e/1.31.2/Clean/RELEASE/BoringSSL\n";
    assert_eq!(parse_version(real).expect("parsed"), "1.31.2");
    assert!(parse_version(b"").is_err());
    assert!(parse_version(b"version: abc//x\n").is_err());
}
