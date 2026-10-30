//! `xtask-fake`: what this crate's tests run in place of `cargo`, of a built
//! `kbf-server`, of `gh` and of `sudo`, so `build-commit stamps` is tested
//! without building the server, `verify-assets attestations` without GitHub, and
//! `sudo-test` and `crun-tests` without root or systemd.
//!
//! - Run under any name but `kbf-server`, `gh`, `sudo`, `syft`, `uname`, `id`, `systemd-sysusers` or
//!   `systemd-tmpfiles`, it is cargo: it
//!   appends its arguments as one line to `target/debug/cargo-args` in its working
//!   directory, "builds" by hard-linking itself to `target/debug/kbf-server`, and
//!   writes the stamp that build embeds to `target/debug/kbf-server.stamp`:
//!   `KBF_BUILD_COMMIT_OVERRIDE` when set, else `XTASK_FAKE_HEAD`. An override that is
//!   not 12 lowercase hex digits fails the build with the build script's message. With
//!   `--no-run` it builds nothing: it prints the file `cargo-messages` in its working
//!   directory, the JSON messages a test wrote for it.
//! - As cargo run as `cargo deny ...` (`advisories`), it prints the file `deny-stderr` in its
//!   working directory on stderr, as `cargo deny --format json` does, and exits with the
//!   number in `deny-exit` (default 0).
//! - Run as `kbf-server`, it prints `kbf-server 0.1.0+<stamp>` from the stamp file next
//!   to it.
//! - Run as `kbf`, it prints `kbf 0.1.0+<stamp>` for `--version`, with the stamp from the
//!   file `kbf.stamp` next to it, and for `--version --json` the contents of the file
//!   `kbf.json` next to it (the identity a test wrote for it, to plant a bad one). With
//!   no `kbf.stamp` next to it, it reads both from a trailer on its own file (`kbf_says`
//!   below), so a copy packaged into a tarball and extracted elsewhere says what the
//!   tarball's bytes say. With `XTASK_FAKE_ANNOUNCE` set, it also prints the path it was
//!   run as on stderr. When what the trailer says `--version --json` prints is exactly
//!   `fail` or `hang` (a line end after it), it does that instead: exits 1 or never
//!   answers. Two more: `orphan` (a line end after it) leaves a process holding
//!   its output for a few seconds and exits 0; and `tamper <name>` (the line ends, and the
//!   rest of the trailer is what it prints) first appends a byte to `../assets/<name>` and
//!   rewrites that file's line in `../assets/SHA256SUMS`, as a binary run in the directory
//!   `release fetch-previous` runs it in could. What a test asks of it by an environment
//!   variable, the empty environment of `release version-facts` does not carry, so two
//!   more files beside it do: `kbf.env` (when it exists, the fake overwrites it with `ran` and one `NAME=value`
//!   line per variable it was given) and `kbf.mode` (`fail`: print the answer, then exit 1;
//!   `hang`: never answer, for two minutes).
//! - Run as `kbf-installer`, it is the STUB of the real kbf-installer, which the package
//!   build and its macOS install test ship until the real tool is built there (one path
//!   change in `crates/xtask/tests/pkg_macos.rs`): it prints `kbf-installer-stub` and
//!   its arguments on stdout and on stderr, and exits 0, so `--version` passes the
//!   signing check and a job that runs it leaves its arguments in its log.
//!   With `XTASK_FAKE_INSTALLER_VERSION=<stamp>` and the one argument `--version` it
//!   prints `kbf-installer 0.1.0+<stamp>` instead.
//! - Run as `gh attestation verify <file> <flags>`, it appends its arguments as one line
//!   to `gh-args` next to it, and verifies the file when `attested` next to it holds
//!   the line `<SHA-256 of the file> <flags>`: the store of attestations, each the file
//!   it covers and the exact flags it verifies under.
//! - Run as `gh attestation verify <file> --bundle <bundle> [--custom-trusted-root
//!   <root>] <flags>`, it verifies the file when the JSON object in `<bundle>` lists, in
//!   `fakeAttestations`, the line `<SHA-256 of the file> <flags>` (the flags without the
//!   two that name files). It appends one line to `gh-env` next to it: the proxy and
//!   no-proxy variables it ran under (`NAME=value`, empty when unset).
//! - Run as `gh api <path>`, it prints the value under the key `<path>` of the JSON
//!   object in `gh-api.json` next to it, or `gh: Not Found (HTTP 404)` on stderr and
//!   exits 1 when there is none: the GitHub API, as a test wrote it.
//! - Run as `gh api <path> -X POST --input -`, it reads a JSON body on stdin. For
//!   `repos/<repo>/git/tags` it stores a tag object under `repos/<repo>/git/tags/<id>` (its
//!   tagger is `komira-release-bot[bot]`) and answers `{"sha": <id>}`. For
//!   `repos/<repo>/git/refs` it stores the ref under `repos/<repo>/git/ref/tags/<name>`, or
//!   fails with HTTP 422 `Reference already exists` when that key is there. No call updates
//!   a ref. `gh-fault` next to it plants one fault: `fail-post:<code>[:<times>]` (the next
//!   POSTs fail with HTTP `<code>`), `no-sha` (a tag object is answered with no id),
//!   `garbage` (a reply that is not JSON), `refuse-ref` (a ref is refused with HTTP 422
//!   `Validation Failed`), `race` (another run made the ref first, with another message),
//!   `lose-ref-502` / `lose-ref-422` (the next ref is stored, and its reply is lost as that
//!   HTTP status), `fail-get:<code>` (every GET of a ref fails with that status), or
//!   `draft-release:<tag>:<times>` (the first `<times>` GETs of that tag's release answer
//!   404, as GitHub answers a release that is still a draft; it counts in `gh-draft-asked`).
//! - Run as `gh api -X <METHOD> <path> [--input -]` (the method first: the issue calls of
//!   `advisories`), it appends `{"args": [...], "body": <JSON or null>}` as one line to
//!   `gh-calls` next to it, and answers `{"number": 42, "html_url": ...}`. It reads the
//!   body from stdin only with `--input -` on its arguments, as `gh` does: a call without
//!   one is sent with no body and refused with HTTP 422, as GitHub refuses an issue with
//!   no title (a POST to a path ending in `/cancel` or `/rerun-failed-jobs` needs none). With `gh-fault` next to it holding `empty-reply`, it answers a write with
//!   nothing at all (an HTTP 204), and with `fail-write` it refuses a write with HTTP 404, and with `conflict` with HTTP 409,
//!   and with `refuse-rerun` it refuses a write to a path ending in `/rerun-failed-jobs` with HTTP 403.
//! - Run as `gh release create|upload <tag> <files> --repo <repo> ...`, it copies each
//!   file into `releases/<tag>/` next to it (create makes that directory and fails if
//!   it exists; a file already there fails the command, as GitHub refuses an asset
//!   name twice), then records the release's assets under
//!   `repos/<repo>/releases/tags/<tag>` in `gh-api.json`. `gh release download <tag>
//!   --dir <dir>` copies every file of `releases/<tag>/` into `<dir>`.
//! - Run as `sudo`, it appends `<working directory>: <arguments>` as one line to
//!   `sudo-args` next to it, and exits 0 (the tests it was asked to run passed).
//! - Run as `uname` (by name, from PATH, or by path), it is a host that is no platform of
//!   ours: `-s` prints `Linux` and `-m` prints `riscv64` (or `XTASK_FAKE_MACHINE` when set). Run as `id`, `-u` prints 1000
//!   (a user that is not root).
//! - Run as `syft`, it prints the file `syft.json` next to it on stdout: what syft prints of an
//!   image. (`pkg-image-sbom` runs it with an empty environment, so what it was given is tested
//!   with `env` in `src/pkg_image_sbom/tests.rs`.)
//! - Run as `systemd-sysusers` or `systemd-tmpfiles` (by name, from PATH, or by path), it
//!   appends `<name> <arguments>` as one line to `systemd-args` next to the fake's own
//!   file and exits 0: the host's systemd, as `packaging/install.sh` calls it.
//!
//! `XTASK_FAKE_DEFECT` plants one defect of the real build: `ignores-override` (the
//! build embeds the head whatever the override), `accepts-bad` (a malformed override
//! builds), `fails-other` (a malformed override fails, but not on the override),
//! `build-fails` (every build fails), `json-fails` (`kbf --version --json` exits 1), `stamp-fails` (every build with an override
//! fails), `vanish` (the build stamped `b2b2b2b2b2b2` deletes the fake cargo, so the
//! next build cannot start) or `version-fails` (`kbf-server` exits 1). Of the
//! attestation store: `accepts-all` (every file verifies, whatever the flags) or
//! `ignores-bytes` (a file verifies when any attestation has its flags). Of a bundle:
//! `bundle-any-file` (a bundle verifies any file whose flags it lists) or
//! `bundle-any-commit` (`--source-digest` is not compared) or `bundle-deb-bundle-any` (a
//! bundle named for a `.deb` verifies any file whose flags it lists). Of the API:
//! `api-fails` (every `gh api` exits 1 with an HTTP 502) or `api-garbage` (every `gh
//! api` prints something that is not JSON). Of releases: `release-fails` (every `gh
//! release` command exits 1). Of sudo:
//! `tests-fail` (it exits 101, as a failed test binary does) or `fails:<a>,<b>,...`
//! (it exits 101 when one of its arguments is one of `<a>`, `<b>`, ..., so some units
//! of several fail). Of systemd: `systemd-fails` (every systemd tool exits 1).
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use sha2::Digest;

const OVERRIDE: &str = "KBF_BUILD_COMMIT_OVERRIDE";

fn defect(name: &str) -> bool {
    std::env::var("XTASK_FAKE_DEFECT").is_ok_and(|d| d == name)
}

fn server(argv0: &str) -> ExitCode {
    if defect("version-fails") {
        return ExitCode::FAILURE;
    }
    let stamp = std::fs::read_to_string(format!("{argv0}.stamp")).expect("stamp file");
    println!("kbf-server 0.1.0+{stamp}");
    ExitCode::SUCCESS
}

/// What follows the fake's own bytes in a copy a test packaged as `kbf`: this marker, the
/// stamp, a line end, then what `--version --json` prints.
const TRAILER: &[u8] = b"\nxtask-fake-kbf\n";

/// The stamp and the `--version --json` output of `kbf`: the files `<argv0>.stamp` and
/// `<argv0>.json` next to the fake when there is a stamp file; else the [`TRAILER`] at
/// the end of its own file, for a copy taken out of a release tarball, which has no
/// files next to it, so the tarball's bytes decide what it says.
fn kbf_says(argv0: &str) -> (String, String) {
    if let Ok(stamp) = std::fs::read_to_string(format!("{argv0}.stamp")) {
        let json = std::fs::read_to_string(format!("{argv0}.json")).expect("json file");
        return (stamp, json);
    }
    let bytes = std::fs::read(argv0).expect("the fake's own file");
    // The last one: the fake's own code holds the marker too.
    let at = bytes
        .windows(TRAILER.len())
        .rposition(|w| w == TRAILER)
        .expect("a stamp file next to the fake, or a trailer on it");
    let text = String::from_utf8(bytes[at + TRAILER.len()..].to_vec()).expect("utf-8 trailer");
    let (stamp, json) = text.split_once('\n').expect("a stamp line in the trailer");
    (stamp.to_owned(), json.to_owned())
}

/// Appends a byte to the release file `../assets/<name>` and makes its line of
/// `../assets/SHA256SUMS` say the new digest: the change a binary run in a sibling of
/// `assets` can make to a release that was verified before it ran.
fn tamper(name: &str) {
    let assets = Path::new("../assets");
    let file = assets.join(name);
    let mut bytes = std::fs::read(&file).expect("the release file");
    bytes.push(b'x');
    std::fs::write(&file, &bytes).expect("write the release file");
    let digest = hex::encode(sha2::Sha256::digest(&bytes));
    let sums = assets.join("SHA256SUMS");
    let text = std::fs::read_to_string(&sums).expect("SHA256SUMS");
    let mut edited = String::new();
    for line in text.lines() {
        let listed = line.split_once("  ").map(|(_, n)| n);
        let new = match listed {
            Some(n) if n == name => format!("{digest}  {name}"),
            _ => line.to_owned(),
        };
        edited.push_str(&new);
        edited.push('\n');
    }
    std::fs::write(&sums, edited).expect("write SHA256SUMS");
}

/// What a test asked of `kbf` in the files beside it (see the top of this file): the
/// environment it was given is written to `<argv0>.env`, if that file exists, and
/// `<argv0>.mode` makes it fail or hang. The exit code to leave with, if it is not to answer.
fn kbf_asked(argv0: &str) -> Option<ExitCode> {
    let env_log = format!("{argv0}.env");
    if Path::new(&env_log).exists() {
        let mut seen = String::from("ran\n");
        for (name, value) in std::env::vars() {
            seen.push_str(&format!("{name}={value}\n"));
        }
        std::fs::write(&env_log, seen).expect("env log");
    }
    match std::fs::read_to_string(format!("{argv0}.mode")).as_deref() {
        // It answers as it should and then fails: only its status can refuse it.
        Ok("fail") => {
            print!("{}", kbf_says(argv0).1);
            Some(ExitCode::FAILURE)
        }
        // Long enough for any limit a test sets; a limit that is broken leaves the test
        // running until the test runner's per-test limit ends it.
        Ok("hang") => {
            std::thread::sleep(std::time::Duration::from_secs(120));
            Some(ExitCode::FAILURE)
        }
        _ => None,
    }
}

/// `kbf --version` and `kbf --version --json`, as [`kbf_says`] has them.
#[allow(clippy::zombie_processes)] // `orphan` leaves its process behind on purpose
fn kbf(argv0: &str, args: &[String]) -> ExitCode {
    // Where a test sees which file was run.
    if std::env::var_os("XTASK_FAKE_ANNOUNCE").is_some() {
        eprintln!("xtask-fake: kbf runs as {argv0}");
    }
    if args == ["--hold"] {
        // The process `orphan` leaves behind: it ends by itself.
        std::thread::sleep(Duration::from_secs(3));
        return ExitCode::SUCCESS;
    }
    if let Some(code) = kbf_asked(argv0) {
        return code;
    }
    if args == ["--version", "--json"] {
        if defect("json-fails") {
            return ExitCode::FAILURE;
        }
        let json = kbf_says(argv0).1;
        let tampering = json.strip_prefix("tamper ");
        if let Some((name, rest)) = tampering.and_then(|t| t.split_once('\n')) {
            tamper(name);
            print!("{rest}");
            return ExitCode::SUCCESS;
        }
        match json.as_str() {
            "orphan\n" => {
                // It inherits the output, so the reader of it waits on it and not on us.
                Command::new(argv0)
                    .arg("--hold")
                    .spawn()
                    .expect("a process to leave behind");
            }
            "fail\n" => return ExitCode::FAILURE,
            "hang\n" => loop {
                std::thread::sleep(Duration::from_secs(3600));
            },
            _ => print!("{json}"),
        }
    } else {
        println!("kbf 0.1.0+{}", kbf_says(argv0).0);
    }
    ExitCode::SUCCESS
}

/// `kbf-installer`, the stand-in: its arguments, once on each output.
fn installer_stub(args: &[String]) -> ExitCode {
    // `XTASK_FAKE_INSTALLER_VERSION=<stamp>` makes `--version` answer as the real
    // installer does, `kbf-installer 0.1.0+<stamp>`, for the tests of `pkg-config
    // --version-of`.
    if let (Ok(stamp), [flag]) = (std::env::var("XTASK_FAKE_INSTALLER_VERSION"), args)
        && flag == "--version"
    {
        println!("kbf-installer 0.1.0+{stamp}");
        return ExitCode::SUCCESS;
    }
    let line = format!("kbf-installer-stub {}", args.join(" "));
    println!("{line}");
    eprintln!("{line}");
    ExitCode::SUCCESS
}

fn cargo(argv0: &str, args: &[String]) -> ExitCode {
    let dir = Path::new("target/debug");
    std::fs::create_dir_all(dir).expect("target/debug");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("cargo-args"))
        .expect("cargo-args");
    writeln!(log, "{}", args.join(" ")).expect("cargo-args");
    // `cargo deny ... check advisories --format json` (advisories): the file `deny-stderr`
    // in the working directory is what cargo-deny printed (JSON lines, on stderr), and
    // `deny-exit` (default 0) its exit status.
    if args.first().is_some_and(|a| a == "deny") {
        // The real cargo-deny takes `--format` and the other options before `check`;
        // `check` takes only the names of the checks.
        let after_check = args.iter().skip_while(|a| *a != "check").skip(1);
        if let Some(flag) = after_check.clone().find(|a| a.starts_with('-')) {
            eprintln!(
                "error: unexpected argument '{flag}' found\n\nUsage: cargo-deny check <WHICH>..."
            );
            return ExitCode::from(2);
        }
        eprint!(
            "{}",
            std::fs::read_to_string("deny-stderr").expect("deny-stderr")
        );
        let code = std::fs::read_to_string("deny-exit").unwrap_or_default();
        return ExitCode::from(code.trim().parse::<u8>().unwrap_or(0));
    }
    if defect("build-fails") {
        eprintln!("xtask-fake: build failed");
        return ExitCode::from(101);
    }
    // `cargo test --no-run --message-format=json` (sudo-test): the messages the test
    // wrote for it.
    if args.iter().any(|a| a == "--no-run") {
        print!(
            "{}",
            std::fs::read_to_string("cargo-messages").expect("cargo-messages")
        );
        return ExitCode::SUCCESS;
    }
    let head = std::env::var("XTASK_FAKE_HEAD").expect("XTASK_FAKE_HEAD");
    let lower_hex =
        |v: &str| (v.len() == 12) & v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let stamp = match std::env::var(OVERRIDE) {
        Ok(_) if defect("ignores-override") => head,
        Ok(_) if defect("stamp-fails") => {
            eprintln!("error: could not compile");
            return ExitCode::from(101);
        }
        Ok(v) if lower_hex(&v) => v,
        Ok(_) if defect("accepts-bad") => head,
        Ok(_) if defect("fails-other") => {
            eprintln!("error: linking failed");
            return ExitCode::from(101);
        }
        Ok(v) => {
            eprintln!("{OVERRIDE}={v:?}: expected exactly 12 lowercase hex digits");
            return ExitCode::from(101);
        }
        Err(_) => head,
    };
    let bin = dir.join("kbf-server");
    if !bin.exists() {
        std::fs::hard_link(argv0, &bin).expect("link the fake as kbf-server");
    }
    if defect("vanish") && stamp == "b2b2b2b2b2b2" {
        std::fs::remove_file(argv0).expect("remove the fake cargo");
    }
    std::fs::write(dir.join("kbf-server.stamp"), stamp).expect("stamp file");
    eprintln!("xtask-fake: built");
    ExitCode::SUCCESS
}

fn gh(argv0: &str, args: &[String]) -> ExitCode {
    let dir = Path::new(argv0).parent().expect("the fake's directory");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gh-args"))
        .expect("gh-args");
    writeln!(log, "{}", args.join(" ")).expect("gh-args");
    match args[0].as_str() {
        "api" if args[1] == "-X" => return issues_write(dir, args),
        "api" if args.iter().any(|a| a == "--input") => return api_write(dir, args),
        "api" => return api(dir, &args[1]),
        "release" => return release(dir, args),
        _ => {}
    }
    let file = args
        .get(2)
        .filter(|_| args[..2] == ["attestation", "verify"])
        .expect("gh attestation verify <file> ...");
    let sha256 = hex::encode(sha2::Sha256::digest(std::fs::read(file).expect("the file")));
    if let Some(at) = args.iter().position(|a| a == "--bundle") {
        return verify_bundle(dir, &sha256, file, &args[at + 1], &args[3..]);
    }
    let flags = args[3..].join(" ");
    let store = std::fs::read_to_string(dir.join("attested")).unwrap_or_default();
    let by_bytes = !defect("ignores-bytes");
    let attested = store.lines().any(|line| {
        line.split_once(' ')
            .is_some_and(|(sha, f)| ((sha == sha256) | !by_bytes) & (f == flags))
    });
    if attested | defect("accepts-all") {
        return ExitCode::SUCCESS;
    }
    eprintln!("xtask-fake: no attestation of {file} verifies with {flags}");
    ExitCode::FAILURE
}

/// `flags` as one string, without the pair `--source-digest <commit>`.
fn without_source_digest(flags: &[&str]) -> String {
    let mut kept = Vec::new();
    let mut words = flags.iter();
    while let Some(word) = words.next() {
        if *word == "--source-digest" {
            words.next();
        } else {
            kept.push(*word);
        }
    }
    kept.join(" ")
}

/// `gh attestation verify <file> --bundle <bundle> [--custom-trusted-root <root>]
/// <flags>`: the verification a host makes with the bundle it was given. The bundle is
/// a JSON object whose `fakeAttestations` list holds the lines `<SHA-256 of the file it
/// covers> <flags it verifies under>`, as the file `attested` does for the attestations
/// GitHub stores. The proxy the call ran under is appended to `gh-env`, so a test sees
/// whether the verification was made with no way to reach the network.
fn verify_bundle(dir: &Path, sha256: &str, file: &str, bundle: &str, rest: &[String]) -> ExitCode {
    let mut env_log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gh-env"))
        .expect("gh-env");
    // Every variable a Go program reads to choose a proxy or to bypass one, empty when
    // unset (the program treats the two alike).
    let seen: Vec<String> = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ]
    .iter()
    .map(|name| format!("{name}={}", std::env::var(name).unwrap_or_default()))
    .collect();
    writeln!(env_log, "{}", seen.join(" ")).expect("gh-env");
    // The flags, without the two that name files.
    let mut flags: Vec<&str> = Vec::new();
    let mut words = rest.iter();
    while let Some(word) = words.next() {
        if matches!(word.as_str(), "--bundle" | "--custom-trusted-root") {
            words.next();
        } else {
            flags.push(word);
        }
    }
    // A missing bundle reads as empty: no attestation in it verifies, so the call fails
    // below with the same "does not verify" message.
    let text = std::fs::read_to_string(bundle).unwrap_or_default();
    let attestations: Vec<String> = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v["fakeAttestations"].as_array().map(|list| {
                list.iter()
                    .filter_map(|l| l.as_str().map(str::to_owned))
                    .collect()
            })
        })
        .unwrap_or_default();
    let by_commit = !defect("bundle-any-commit");
    let by_bytes = !(defect("bundle-any-file")
        || (defect("bundle-deb-bundle-any")
            && (bundle.ends_with(".deb.sigstore.json") || bundle.ends_with(".pkg.sigstore.json"))));
    let wanted = if by_commit {
        flags.join(" ")
    } else {
        without_source_digest(&flags)
    };
    let verified = attestations.iter().any(|line| {
        line.split_once(' ').is_some_and(|(sha, f)| {
            let f = if by_commit {
                f.to_owned()
            } else {
                without_source_digest(&f.split(' ').collect::<Vec<_>>())
            };
            ((sha == sha256) | !by_bytes) & (f == wanted)
        })
    });
    if verified | defect("accepts-all") {
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "xtask-fake: bundle {bundle} does not verify {file} with {}",
        flags.join(" ")
    );
    ExitCode::FAILURE
}

/// The JSON object in `gh-api.json` in `dir`: the API's answers, by path.
fn api_store(dir: &Path) -> serde_json::Map<String, serde_json::Value> {
    let text = std::fs::read_to_string(dir.join("gh-api.json")).unwrap_or_else(|_| "{}".into());
    serde_json::from_str(&text).expect("gh-api.json is a JSON object")
}

fn api(dir: &Path, path: &str) -> ExitCode {
    if defect("api-fails") {
        eprintln!("gh: Server Error (HTTP 502)");
        return ExitCode::FAILURE;
    }
    if defect("api-garbage") {
        println!("<html>");
        return ExitCode::SUCCESS;
    }
    let fault = std::fs::read_to_string(dir.join("gh-fault")).unwrap_or_default();
    if let Some(code) = fault
        .strip_prefix("fail-get:")
        .filter(|_| path.contains("/git/ref/"))
    {
        eprintln!("gh: Error (HTTP {code})");
        return ExitCode::FAILURE;
    }
    if let Some(spec) = fault.trim().strip_prefix("draft-release:") {
        let (tag, times) = spec.rsplit_once(':').expect("draft-release:<tag>:<times>");
        if path.ends_with(&format!("/releases/tags/{tag}")) {
            let counter = dir.join("gh-draft-asked");
            let asked: u32 = std::fs::read_to_string(&counter)
                .ok()
                .and_then(|text| text.trim().parse().ok())
                .unwrap_or(0);
            std::fs::write(&counter, (asked + 1).to_string()).expect("gh-draft-asked");
            if asked < times.parse::<u32>().expect("a count") {
                eprintln!("gh: Not Found (HTTP 404)");
                return ExitCode::FAILURE;
            }
        }
    }
    let store = api_store(dir);
    match store.get(path).cloned() {
        Some(answer) => {
            println!("{answer}");
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("gh: Not Found (HTTP 404)");
            ExitCode::FAILURE
        }
    }
}

/// A tag object as the API stores it: the id is the hash of its parts.
fn tag_object(
    tag: &serde_json::Value,
    message: &serde_json::Value,
    object: &serde_json::Value,
    kind: &serde_json::Value,
) -> (String, serde_json::Value) {
    let id = hex::encode(sha2::Sha256::digest(format!(
        "{tag}{message}{object}{kind}"
    )))[..40]
        .to_owned();
    let stored = serde_json::json!({
        "tag": tag,
        "sha": id,
        "message": message,
        "object": { "type": kind, "sha": object },
        "tagger": { "name": "komira-release-bot[bot]" },
    });
    (id, stored)
}

/// A `gh api <path> -X POST --input -` call: the writes of the tag API, `git/tags` and
/// `git/refs`. Nothing updates a ref: GitHub's rulesets would refuse it, and so does this.
fn api_write(dir: &Path, args: &[String]) -> ExitCode {
    let path = args[1].as_str();
    let body: serde_json::Value = serde_json::from_reader(std::io::stdin()).expect("a JSON body");
    let fault_file = dir.join("gh-fault");
    let fault = std::fs::read_to_string(&fault_file).unwrap_or_default();
    if let Some(spec) = fault.strip_prefix("fail-post:") {
        let (code, times) = spec.split_once(':').unwrap_or((spec, "1"));
        let left = times.parse::<u32>().expect("a count") - 1;
        if left == 0 {
            std::fs::remove_file(&fault_file).expect("gh-fault");
        } else {
            std::fs::write(&fault_file, format!("fail-post:{code}:{left}")).expect("gh-fault");
        }
        eprintln!("gh: Error (HTTP {code})");
        return ExitCode::FAILURE;
    }
    if fault == "garbage" {
        println!("<html>");
        return ExitCode::SUCCESS;
    }
    let mut answers = api_store(dir);
    let repo = path.split("/git/").next().expect("repos/<repo>");
    let reply = if path.ends_with("/git/tags") && fault == "no-sha" {
        serde_json::json!({})
    } else if path.ends_with("/git/tags") {
        let (id, stored) = tag_object(
            &body["tag"],
            &body["message"],
            &body["object"],
            &body["type"],
        );
        answers.insert(format!("{repo}/git/tags/{id}"), stored);
        serde_json::json!({ "sha": id })
    } else {
        let name = body["ref"]
            .as_str()
            .and_then(|r| r.strip_prefix("refs/tags/"))
            .expect("a tag ref");
        let key = format!("{repo}/git/ref/tags/{name}");
        if fault == "refuse-ref" {
            eprintln!("gh: Validation Failed (HTTP 422)");
            return ExitCode::FAILURE;
        }
        if fault == "race" {
            // Another run made this tag first, with another message.
            std::fs::remove_file(&fault_file).expect("gh-fault");
            let (id, stored) = tag_object(
                &body["ref"],
                &serde_json::json!("other\n"),
                &body["sha"],
                &serde_json::json!("commit"),
            );
            answers.insert(format!("{repo}/git/tags/{id}"), stored);
            answers.insert(
                key.clone(),
                serde_json::json!({ "object": { "type": "tag", "sha": id } }),
            );
        }
        if answers.contains_key(&key) {
            write_answers(dir, answers);
            eprintln!("gh: Reference already exists (HTTP 422)");
            return ExitCode::FAILURE;
        }
        answers.insert(
            key,
            serde_json::json!({ "object": { "type": "tag", "sha": body["sha"] } }),
        );
        if let Some(status) = fault.strip_prefix("lose-ref-") {
            std::fs::remove_file(&fault_file).expect("gh-fault");
            write_answers(dir, answers);
            eprintln!("gh: Error (HTTP {status})");
            if status == "422" {
                eprintln!("Reference already exists");
            }
            return ExitCode::FAILURE;
        }
        serde_json::json!({})
    };
    write_answers(dir, answers);
    println!("{reply}");
    ExitCode::SUCCESS
}

/// A `gh api -X <METHOD> <path> [--input -]` call (the issue calls of `advisories`): the
/// call, with the body `gh` would have sent, appended to `gh-calls`.
fn issues_write(dir: &Path, args: &[String]) -> ExitCode {
    let sends_stdin = args.windows(2).any(|w| w == ["--input", "-"]);
    let body: serde_json::Value = if sends_stdin {
        serde_json::from_reader(std::io::stdin()).expect("a JSON body")
    } else {
        serde_json::Value::Null
    };
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gh-calls"))
        .expect("gh-calls");
    writeln!(log, "{}", serde_json::json!({ "args": args, "body": body })).expect("gh-calls");
    let needs_no_body = args[3].ends_with("/cancel") || args[3].ends_with("/rerun-failed-jobs");
    if body.is_null() && !needs_no_body {
        eprintln!("gh: Validation Failed (HTTP 422)");
        return ExitCode::FAILURE;
    }
    let fault = std::fs::read_to_string(dir.join("gh-fault")).unwrap_or_default();
    if fault == "conflict" {
        eprintln!("gh: Conflict (HTTP 409)");
        return ExitCode::FAILURE;
    }
    if fault == "refuse-rerun" && args[3].ends_with("/rerun-failed-jobs") {
        eprintln!("gh: Forbidden (HTTP 403)");
        return ExitCode::FAILURE;
    }
    if fault == "fail-write" {
        eprintln!("gh: Not Found (HTTP 404)");
        return ExitCode::FAILURE;
    }
    if fault == "empty-reply" {
        return ExitCode::SUCCESS;
    }
    let path = &args[3];
    println!(
        "{}",
        serde_json::json!({ "number": 42, "html_url": format!("https://github.com/{}/42", path.trim_start_matches("repos/")) })
    );
    ExitCode::SUCCESS
}

fn write_answers(dir: &Path, answers: serde_json::Map<String, serde_json::Value>) {
    std::fs::write(
        dir.join("gh-api.json"),
        serde_json::Value::Object(answers).to_string(),
    )
    .expect("gh-api.json");
}

fn release(dir: &Path, args: &[String]) -> ExitCode {
    if defect("release-fails") {
        eprintln!("xtask-fake: release failed");
        return ExitCode::FAILURE;
    }
    let mut flags = std::collections::BTreeMap::new();
    let mut words = Vec::new();
    let mut rest = args[2..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--verify-tag" => {}
            flag if flag.starts_with("--") => {
                flags.insert(flag, rest.next().expect("a flag's value").as_str());
            }
            _ => words.push(arg.as_str()),
        }
    }
    let tag = words.remove(0);
    let store = dir.join("releases").join(tag);
    match args[1].as_str() {
        "create" => {
            std::fs::create_dir_all(dir.join("releases")).expect("releases");
            std::fs::create_dir(&store).expect("no release with this tag yet");
        }
        "download" => {
            for entry in std::fs::read_dir(&store).expect("the release") {
                let from = entry.expect("an asset").path();
                let to = Path::new(flags["--dir"]).join(from.file_name().expect("a name"));
                std::fs::copy(from, to).expect("download an asset");
            }
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    for file in words {
        let name = Path::new(file).file_name().expect("a file name");
        let mut asset = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(store.join(name))
            .expect("no asset of this name in the release yet");
        asset
            .write_all(&std::fs::read(file).expect("the file"))
            .expect("write the asset");
    }
    let mut names: Vec<String> = std::fs::read_dir(&store)
        .expect("the release")
        .map(|e| {
            e.expect("an asset")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    let assets: Vec<serde_json::Value> = names
        .iter()
        .map(|n| serde_json::json!({ "name": n }))
        .collect();
    let mut answers = api_store(dir);
    answers.insert(
        format!("repos/{}/releases/tags/{tag}", flags["--repo"]),
        serde_json::json!({ "tag_name": tag, "assets": assets }),
    );
    let text = serde_json::Value::Object(answers).to_string();
    std::fs::write(dir.join("gh-api.json"), text).expect("gh-api.json");
    ExitCode::SUCCESS
}

fn sudo(argv0: &str, args: &[String]) -> ExitCode {
    let dir = Path::new(argv0).parent().expect("the fake's directory");
    let cwd = std::env::current_dir().expect("working directory");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("sudo-args"))
        .expect("sudo-args");
    writeln!(log, "{}: {}", cwd.display(), args.join(" ")).expect("sudo-args");
    let defect_var = std::env::var("XTASK_FAKE_DEFECT").unwrap_or_default();
    let fails_on: Vec<&str> = defect_var
        .strip_prefix("fails:")
        .map(|list| list.split(',').collect())
        .unwrap_or_default();
    if defect("tests-fail") | args.iter().any(|a| fails_on.contains(&a.as_str())) {
        return ExitCode::from(101);
    }
    ExitCode::SUCCESS
}

fn systemd(name: &str, args: &[String]) -> ExitCode {
    let exe = std::env::current_exe().expect("the fake's own path");
    let dir = exe.parent().expect("the fake's directory");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("systemd-args"))
        .expect("systemd-args");
    writeln!(log, "{name} {}", args.join(" ")).expect("systemd-args");
    if defect("systemd-fails") {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// `syft ...`: prints the SBOM a test wrote for it, the file `syft.json` next to it.
fn syft(argv0: &str) -> ExitCode {
    let json = std::fs::read_to_string(Path::new(argv0).with_file_name("syft.json"));
    print!("{}", json.expect("syft.json"));
    ExitCode::SUCCESS
}

/// `uname -s` and `uname -m` of a host with no platform of ours.
fn uname(args: &[String]) -> ExitCode {
    match args {
        [a] if a == "-s" => println!("Linux"),
        [a] if a == "-m" => {
            println!(
                "{}",
                std::env::var("XTASK_FAKE_MACHINE").unwrap_or("riscv64".to_owned())
            );
        }
        _ => return ExitCode::FAILURE,
    }
    ExitCode::SUCCESS
}

/// `id -u` of a user that is not root.
fn id(args: &[String]) -> ExitCode {
    match args {
        [a] if a == "-u" => println!("1000"),
        _ => return ExitCode::FAILURE,
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let mut args = std::env::args();
    let argv0 = args.next().expect("argv0");
    let rest: Vec<String> = args.collect();
    let base = argv0.rsplit('/').next().unwrap_or(&argv0);
    if argv0.ends_with("/kbf-server") {
        server(&argv0)
    } else if argv0.ends_with("/kbf") {
        kbf(&argv0, &rest)
    } else if argv0.ends_with("/kbf-installer") {
        installer_stub(&rest)
    } else if argv0.ends_with("/gh") {
        gh(&argv0, &rest)
    } else if argv0.ends_with("/sudo") {
        sudo(&argv0, &rest)
    } else if argv0.ends_with("/syft") {
        syft(&argv0)
    } else if matches!(base, "systemd-sysusers" | "systemd-tmpfiles") {
        systemd(base, &rest)
    } else if argv0 == "uname" || argv0.ends_with("/uname") {
        uname(&rest)
    } else if argv0 == "id" || argv0.ends_with("/id") {
        id(&rest)
    } else {
        cargo(&argv0, &rest)
    }
}
