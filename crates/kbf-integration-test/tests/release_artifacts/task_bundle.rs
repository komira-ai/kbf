//! The darwin `kbf-task` bundle in the pinned release set (docs/artifacts.md, "The macOS `kbf-task`
//! bundle"): `kbf-task-bundle-<commit>-darwin-arm64.tar.gz`, a tarball of the OCI image layout
//! `js-bundle pack` writes for the signed `kbf-task`, and its SBOM. No pull request run attests it,
//! and the darwin job runs only for a labelled one, so the steps that make it are held to one
//! shape here. [`rules`] reads the workflow and names a darwin job that does not, each unconditional
//! and in this order:
//!
//! 1. build `kbf-task` once for `$TARGET`, `--release --locked`, with no feature (the macOS confine is the
//!    default; `unconfined-for-tests` is `no_darwin_task`'s to refuse everywhere);
//! 2. sign that binary (`cargo xtask sign-darwin`), as the other Mac tools are signed;
//! 3. fetch the pinned `js-bundle` archive (`cargo xtask fetch`, url `v$JS_BUNDLE_VERSION`, sha256
//!    `$JS_BUNDLE_SHA256_AARCH64_DARWIN`), verify its attestation (`gh attestation verify` for the jobscheduler
//!    repository, its `release.yml`, `refs/heads/main` and the commit `$JS_BUNDLE_SOURCE_DIGEST`), and only then
//!    take the program out of the same archive (the same url and sha256, `--extract tar.gz`);
//! 4. copy the signed binary into the tree directory, and `js-bundle pack` that directory
//!    (`--tree task=<dir>`), with the bundle named `kbf-task`, the entrypoint `kbf-task` and the macOS the
//!    binaries are built for (`--os-version "$MACOSX_DEPLOYMENT_TARGET"`);
//! 5. `js-bundle verify` the layout the pack wrote, then `cargo xtask package-task-bundle` it with the SBOM of
//!    `kbf-task` (`crates/kbf-task/kbf-task_bin.cdx.json`), the commit and `dist`.
//!
//! The three pin variables must hold a version, 64 hex digits and 40 hex digits; no other job may pack or
//! package a bundle; the asset is in [`ASSETS`] and docs/artifacts.md names its files and the pin
//! ([`doc_problems`]).

use super::*;

const PACK: &str = "$RUNNER_TEMP/tools/js-bundle pack ";
const VERIFY: &str = "$RUNNER_TEMP/tools/js-bundle verify ";
const PACKAGE: &str = "cargo xtask package-task-bundle ";
const SBOM: &str = "crates/kbf-task/kbf-task_bin.cdx.json";
const RELEASE_URL: &str = "https://github.com/komira-ai/jobscheduler/releases/download/v$JS_BUNDLE_VERSION/js-bundle-$JS_BUNDLE_VERSION-$TARGET.tar.gz";
const SHA_VAR: &str = "$JS_BUNDLE_SHA256_AARCH64_DARWIN";

/// The words of `line`, the double quotes of each dropped.
fn words(line: &str) -> Vec<&str> {
    line.split_whitespace()
        .map(|w| w.trim_matches('"'))
        .collect()
}

/// The value of the workflow variable `name` (a line `  NAME: value` of the file's `env`).
fn variable<'a>(workflow: &'a str, name: &str) -> Option<&'a str> {
    workflow.lines().find_map(|l| {
        l.strip_prefix("  ")?
            .strip_prefix(name)?
            .strip_prefix(": ")
            .map(str::trim)
    })
}

fn is_hex(text: &str, digits: usize) -> bool {
    text.len() == digits && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The index of the one step of `steps` with a line for which `found` is true; a problem named `what`
/// when there are none, several, or the step is under `if:` or `continue-on-error:`.
fn one(
    what: &str,
    steps: &[workflows::Step],
    out: &mut Vec<String>,
    found: impl Fn(&str) -> bool,
) -> Option<usize> {
    let at: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| lines(s).any(&found))
        .map(|(i, _)| i)
        .collect();
    match at.as_slice() {
        [i] if !steps[*i].conditional => Some(*i),
        [_] => {
            out.push(format!(
                "`{DARWIN_JOB}`: the step that {what} is under `if:` or `continue-on-error:`"
            ));
            None
        }
        _ => {
            out.push(format!(
                "`{DARWIN_JOB}` has {} steps that {what}, not exactly one",
                at.len()
            ));
            None
        }
    }
}

/// The first line of the step `at` for which `found` is true.
fn line_of(
    steps: &[workflows::Step],
    at: Option<usize>,
    found: impl Fn(&str) -> bool,
) -> Option<&str> {
    lines(&steps[at?]).find(|l| found(l))
}

/// Whether `line` is a `gh attestation verify` of the pinned archive for the jobscheduler release workflow.
fn verifies_archive(line: &str) -> bool {
    line.contains("/gh attestation verify ")
}

fn is_fetch_of_archive(line: &str) -> bool {
    args(line, "cargo xtask fetch ").is_some_and(|a| a.contains(&RELEASE_URL))
}

/// What differs between the workflow and the rules in the module docs: one line per difference.
pub fn rules(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    // The pin.
    for (name, ok, shape) in [
        (
            "JS_BUNDLE_VERSION",
            variable(workflow, "JS_BUNDLE_VERSION").is_some_and(|v| {
                v.split('.').count() == 3
                    && v.split('.')
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            }),
            "a version `x.y.z`",
        ),
        (
            "JS_BUNDLE_SHA256_AARCH64_DARWIN",
            variable(workflow, "JS_BUNDLE_SHA256_AARCH64_DARWIN").is_some_and(|v| is_hex(v, 64)),
            "64 lowercase hex digits",
        ),
        (
            "JS_BUNDLE_SOURCE_DIGEST",
            variable(workflow, "JS_BUNDLE_SOURCE_DIGEST").is_some_and(|v| is_hex(v, 40)),
            "a full commit: 40 lowercase hex digits",
        ),
    ] {
        if !ok {
            out.push(format!("the workflow variable {name} is not {shape}"));
        }
    }
    if !ASSETS.contains(&(
        xtask::task_bundle::NAME,
        xtask::task_bundle::PLATFORM.name(),
    )) {
        out.push(format!(
            "{} for {} is the `package-task-bundle` asset, but not in the pinned release set `ASSETS`",
            xtask::task_bundle::NAME,
            xtask::task_bundle::PLATFORM.name()
        ));
    }
    // No other job makes or packs a bundle.
    for step in workflows::run_steps(workflow)? {
        for line in step.script.lines().map(str::trim_start) {
            let bundle = line.starts_with(PACKAGE) || line.contains("/js-bundle ");
            if bundle && step.job != DARWIN_JOB {
                out.push(format!(
                    "the job `{}` runs `{line}`: only `{DARWIN_JOB}` makes the kbf-task bundle",
                    step.job
                ));
            }
        }
    }
    let steps = workflows::job_steps(workflow, DARWIN_JOB)?;
    let is_build = |l: &str| {
        args(l, "cargo build ").is_some_and(|a| a.windows(2).any(|w| w == ["-p", "kbf-task"]))
    };
    let build = one("build kbf-task", &steps, out, is_build);
    if let Some(line) = line_of(&steps, build, is_build) {
        let a = args(line, "cargo build ").unwrap_or_default();
        for want in ["--release", "--locked"] {
            if !a.contains(&want) {
                out.push(format!(
                    "`{DARWIN_JOB}`: the kbf-task build lacks {want}: {line}"
                ));
            }
        }
        if flag(&a, "--target") != Some("$TARGET") {
            out.push(format!(
                "`{DARWIN_JOB}`: the kbf-task build is not for `--target \"$TARGET\"`: {line}"
            ));
        }
        if a.iter().any(|w| {
            w.starts_with("--features")
                || w.starts_with("--all-features")
                || w.starts_with("--no-default-features")
        }) {
            out.push(format!(
                "`{DARWIN_JOB}`: the kbf-task build names features, but the confine is the default: {line}"
            ));
        }
    }
    let is_sign = |l: &str| {
        args(l, "cargo xtask sign-darwin ")
            .is_some_and(|a| a.first().is_some_and(|b| base(b) == "kbf-task"))
    };
    let sign = one("sign kbf-task", &steps, out, is_sign);
    let fetch_all: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| lines(s).any(is_fetch_of_archive))
        .map(|(i, _)| i)
        .collect();
    let is_unpack = |l: &str| {
        is_fetch_of_archive(l)
            && args(l, "cargo xtask fetch ")
                .is_some_and(|a| flag(&a, "--extract") == Some("tar.gz"))
    };
    let is_download = |l: &str| is_fetch_of_archive(l) && !is_unpack(l);
    let download = one(
        "fetch the pinned js-bundle archive",
        &steps,
        out,
        is_download,
    );
    let unpack = one("take js-bundle out of the archive", &steps, out, is_unpack);
    for &i in &fetch_all {
        for line in lines(&steps[i]).filter(|l| is_fetch_of_archive(l)) {
            let a = args(line, "cargo xtask fetch ").unwrap_or_default();
            if flag(&a, "--sha256") != Some(SHA_VAR) {
                out.push(format!(
                    "`{DARWIN_JOB}`: a fetch of the js-bundle archive is not pinned by `--sha256 \"{SHA_VAR}\"`: {line}"
                ));
            }
        }
    }
    if let Some(line) = line_of(&steps, unpack, is_unpack) {
        let a = args(line, "cargo xtask fetch ").unwrap_or_default();
        if flag(&a, "--member") != Some("js-bundle-$JS_BUNDLE_VERSION-$TARGET/js-bundle")
            || flag(&a, "--out") != Some("$RUNNER_TEMP/tools/js-bundle")
        {
            out.push(format!(
                "`{DARWIN_JOB}`: the unpack of the archive does not take `js-bundle-$JS_BUNDLE_VERSION-$TARGET/js-bundle` to `$RUNNER_TEMP/tools/js-bundle`: {line}"
            ));
        }
    }
    let verify = one(
        "verify the attestation of the js-bundle archive",
        &steps,
        out,
        verifies_archive,
    );
    if let Some(line) = line_of(&steps, verify, verifies_archive) {
        let a = words(line);
        let archive = flag(&a, "verify");
        let downloaded = download
            .and_then(|d| line_of(&steps, Some(d), is_download))
            .and_then(|l| flag(&words(l), "--out"));
        if archive.is_none() || archive != downloaded {
            out.push(format!(
                "`{DARWIN_JOB}`: the attestation check is not of the archive the fetch wrote ({downloaded:?}): {line}"
            ));
        }
        for (name, want) in [
            ("--repo", "komira-ai/jobscheduler"),
            (
                "--signer-workflow",
                "komira-ai/jobscheduler/.github/workflows/release.yml",
            ),
            (
                "--cert-oidc-issuer",
                "https://token.actions.githubusercontent.com",
            ),
            ("--source-ref", "refs/heads/main"),
            ("--source-digest", "$JS_BUNDLE_SOURCE_DIGEST"),
        ] {
            if flag(&a, name) != Some(want) {
                out.push(format!(
                    "`{DARWIN_JOB}`: the attestation check of the js-bundle archive lacks `{name} {want}`: {line}"
                ));
            }
        }
    }
    let tree_copy = one("copy kbf-task into the bundle's tree", &steps, out, |l| {
        l.starts_with("mkdir ") && l.contains("&& cp ")
    });
    let tree = tree_copy
        .and_then(|i| line_of(&steps, Some(i), |l| l.contains("&& cp ")))
        .map(words);
    let tree_dir = tree.as_ref().and_then(|w| {
        w.iter()
            .position(|x| *x == "-p")
            .and_then(|i| w.get(i + 1))
            .copied()
    });
    if let Some(w) = &tree {
        let cp = w.iter().position(|x| *x == "cp").map(|i| &w[i + 1..]);
        let expected = tree_dir.map(|d| format!("{d}/kbf-task"));
        if cp
            != Some(
                &[
                    "target/$TARGET/release/kbf-task",
                    expected.as_deref().unwrap_or("?"),
                ][..],
            )
        {
            out.push(format!(
                "`{DARWIN_JOB}`: the tree of the bundle is not the signed `target/$TARGET/release/kbf-task` copied to `<tree>/kbf-task`: {}",
                w.join(" ")
            ));
        }
    }
    let is_pack = |l: &str| l.starts_with(PACK);
    let pack = one("pack the bundle with js-bundle", &steps, out, is_pack);
    let mut layout = None;
    if let Some(line) = line_of(&steps, pack, is_pack) {
        let a: Vec<&str> = line
            .split_whitespace()
            .map(|w| w.trim_matches('"'))
            .collect();
        layout = flag(&a, "--out");
        let single = |w: &&str| w.trim_matches('\'').to_owned();
        let entry = a
            .iter()
            .position(|w| *w == "--entrypoint")
            .and_then(|i| a.get(i + 1))
            .map(single);
        if entry.as_deref() != Some("kbf-task=${tree:task}/kbf-task") {
            out.push(format!("`{DARWIN_JOB}`: the bundle's entrypoint is not `kbf-task=${{tree:task}}/kbf-task`: {line}"));
        }
        for (name, want) in [
            ("--name", "kbf-task"),
            ("--os-version", "$MACOSX_DEPLOYMENT_TARGET"),
        ] {
            if flag(&a, name) != Some(want) {
                out.push(format!(
                    "`{DARWIN_JOB}`: the js-bundle pack lacks `{name} {want}`: {line}"
                ));
            }
        }
        let want_tree = tree_dir.map(|d| format!("task={d}"));
        if flag(&a, "--tree") != want_tree.as_deref() {
            out.push(format!(
                "`{DARWIN_JOB}`: the js-bundle pack does not pack the directory the signed kbf-task was copied to (`--tree task=<tree>`): {line}"
            ));
        }
    }
    let is_bundle_verify = |l: &str| l.starts_with(VERIFY);
    let bundle_verify = one(
        "run js-bundle verify on the layout",
        &steps,
        out,
        is_bundle_verify,
    );
    if let Some(line) = line_of(&steps, bundle_verify, is_bundle_verify)
        && (words(line).get(2).copied() != layout || layout.is_none())
    {
        out.push(format!(
            "`{DARWIN_JOB}`: js-bundle verify does not check the layout js-bundle pack wrote ({layout:?}): {line}"
        ));
    }
    let is_package = |l: &str| l.starts_with(PACKAGE);
    let package = one("package the bundle", &steps, out, is_package);
    if let Some(line) = line_of(&steps, package, is_package) {
        let a = words(line);
        for (name, want) in [
            ("--sbom", SBOM),
            ("--commit", "$GITHUB_SHA"),
            ("--out", "dist"),
        ] {
            if flag(&a, name) != Some(want) {
                out.push(format!(
                    "`{DARWIN_JOB}`: package-task-bundle lacks `{name} {want}`: {line}"
                ));
            }
        }
        if flag(&a, "--layout") != layout || layout.is_none() {
            out.push(format!("`{DARWIN_JOB}`: package-task-bundle does not take the layout js-bundle pack wrote ({layout:?}): {line}"));
        }
    }
    // The order: build, sign, copy, pack; download, attestation, unpack, pack; pack, verify, package.
    let order: [(&str, Option<usize>); 9] = [
        ("the kbf-task build", build),
        ("the signing of kbf-task", sign),
        ("the copy of kbf-task into the tree", tree_copy),
        ("the download of the js-bundle archive", download),
        ("the attestation check of the archive", verify),
        ("the unpack of js-bundle", unpack),
        ("the pack", pack),
        ("js-bundle verify", bundle_verify),
        ("package-task-bundle", package),
    ];
    let before = |a: usize, b: usize, why: &str, out: &mut Vec<String>| {
        if let (Some(x), Some(y)) = (order[a].1, order[b].1)
            && x >= y
        {
            out.push(format!(
                "`{DARWIN_JOB}`: {} is not before {}: {why}",
                order[a].0, order[b].0
            ));
        }
    };
    before(0, 1, "an unsigned binary would be copied", out);
    before(1, 2, "an unsigned binary would be copied", out);
    before(2, 6, "the pack would not hold the signed binary", out);
    before(
        3,
        4,
        "the archive must be checked before it is taken from",
        out,
    );
    before(
        4,
        5,
        "the program must not run, or be taken, before its attestation is checked",
        out,
    );
    before(5, 6, "the pack needs the program", out);
    before(6, 7, "the layout is checked after it is written", out);
    before(7, 8, "the asset is made from a verified layout", out);
    Ok(())
}

/// What differs between docs/artifacts.md and the bundle's files and pin in the workflow: one line each.
pub fn doc_problems(workflow: &str, doc: &str) -> Vec<String> {
    let mut out = Vec::new();
    let stem = format!(
        "{}-<commit>-{}",
        xtask::task_bundle::NAME,
        xtask::task_bundle::PLATFORM.name()
    );
    for file in [format!("`{stem}.tar.gz`"), format!("`{stem}.cdx.json`")] {
        if !doc.contains(&file) {
            out.push(format!("docs/artifacts.md does not list {file}"));
        }
    }
    let version = variable(workflow, "JS_BUNDLE_VERSION").unwrap_or_default();
    for (what, value) in [
        ("the js-bundle release", format!("`v{version}`")),
        (
            "the SHA-256 of the js-bundle archive",
            variable(workflow, "JS_BUNDLE_SHA256_AARCH64_DARWIN")
                .unwrap_or_default()
                .to_owned(),
        ),
        (
            "the commit the js-bundle release is attested at",
            variable(workflow, "JS_BUNDLE_SOURCE_DIGEST")
                .unwrap_or_default()
                .to_owned(),
        ),
    ] {
        if value.len() < 4 || !doc.contains(&value) {
            out.push(format!(
                "docs/artifacts.md does not state {what} the workflow pins: {value:?}"
            ));
        }
    }
    out
}

fn workflow_text() -> String {
    std::fs::read_to_string(crate::repo_root().join(crate::ARTIFACTS_WORKFLOW))
        .expect("artifacts.yml")
}

fn found(workflow: &str) -> Vec<String> {
    let mut out = Vec::new();
    rules(workflow, &mut out).expect("the workflow reads");
    out
}

/// `workflow` with the one `from` replaced by `to`.
fn changed(workflow: &str, from: &str, to: &str) -> String {
    assert_eq!(
        workflow.matches(from).count(),
        1,
        "{from:?} is not once in the workflow"
    );
    workflow.replace(from, to)
}

#[test]
fn the_asset_table_the_workflow_and_the_pin_agree_on_the_darwin_kbf_task_bundle() {
    // Asserts, on the committed files: `ASSETS` lists kbf-task-bundle for darwin-arm64, the one
    // `cargo xtask package-task-bundle` line makes it (`attested_assets::packaged` reads it, so its SBOM
    // attestation and its provenance bundle are held like every tarball's), the steps before it build,
    // sign, copy, fetch the pinned js-bundle, verify its attestation, pack and verify the layout in the
    // module's order, the pin variables hold a version, a sha256 and a commit, no other job makes a
    // bundle, and docs/artifacts.md lists both files and states the pin.
    let workflow = workflow_text();
    assert_eq!(found(&workflow), Vec::<String>::new());
    let doc = std::fs::read_to_string(crate::repo_root().join("docs/artifacts.md")).expect("docs");
    assert_eq!(doc_problems(&workflow, &doc), Vec::<String>::new());
    let packaged = attested_assets::packaged(&workflow).expect("packaged");
    assert!(
        packaged.contains(&("kbf-task-bundle".to_owned(), "darwin-arm64".to_owned())),
        "{packaged:?}"
    );
}

#[test]
fn a_bundle_step_that_breaks_a_rule_is_named() {
    // Asserts: each planted change to the committed workflow is reported with the rule's own text:
    // the unconfined feature in the kbf-task build, the build for another target, the unsigned binary (the
    // signing step removed), the pack before the attestation check, the archive pinned by another
    // variable, a missing source commit, a pack of another directory, a verify of another layout, a
    // package of another layout, a conditional step, a second job that packs, and a pin that is not hex.
    let w = workflow_text();
    let build = "cargo build --locked --release --target \"$TARGET\" -p kbf-task --bin kbf-task";
    let cases: Vec<(String, &str)> = vec![
        (
            changed(
                &w,
                build,
                &format!("{build} --features unconfined-for-tests"),
            ),
            "names features, but the confine is the default",
        ),
        (
            changed(
                &w,
                build,
                "cargo build --locked --release --target x86_64-apple-darwin -p kbf-task --bin kbf-task",
            ),
            "is not for `--target \"$TARGET\"`",
        ),
        (
            changed(&w, build, "cargo build --locked -p kbf-task --bin kbf-task"),
            "lacks --release",
        ),
        (
            changed(
                &w,
                "cargo xtask sign-darwin \"target/$TARGET/release/kbf-task\"",
                "cargo xtask darwin-asset \"target/$TARGET/release/kbf-task\"",
            ),
            "has 0 steps that sign kbf-task",
        ),
        (
            changed(
                &w,
                "--source-digest \"$JS_BUNDLE_SOURCE_DIGEST\" --deny-self-hosted-runners",
                "--deny-self-hosted-runners",
            ),
            "lacks `--source-digest $JS_BUNDLE_SOURCE_DIGEST`",
        ),
        (
            changed(
                &w,
                "--source-ref refs/heads/main --source-digest \"$JS_BUNDLE_SOURCE_DIGEST\"",
                "--source-ref refs/heads/dev --source-digest \"$JS_BUNDLE_SOURCE_DIGEST\"",
            ),
            "lacks `--source-ref refs/heads/main`",
        ),
        (
            changed(
                &w,
                "--sha256 \"$JS_BUNDLE_SHA256_AARCH64_DARWIN\" --out \"$RUNNER_TEMP/tools/js-bundle\"",
                "--sha256 \"$GH_SHA256_AARCH64_DARWIN\" --out \"$RUNNER_TEMP/tools/js-bundle\"",
            ),
            "not pinned by `--sha256",
        ),
        (
            changed(
                &w,
                "--tree \"task=$RUNNER_TEMP/task-tree\"",
                "--tree \"task=target/release\"",
            ),
            "does not pack the directory the signed kbf-task was copied to",
        ),
        (
            changed(
                &w,
                "js-bundle verify \"$RUNNER_TEMP/task-bundle\"",
                "js-bundle verify \"$RUNNER_TEMP/other\"",
            ),
            "js-bundle verify does not check the layout",
        ),
        (
            changed(
                &w,
                "--layout \"$RUNNER_TEMP/task-bundle\"",
                "--layout \"$RUNNER_TEMP/other\"",
            ),
            "package-task-bundle does not take the layout",
        ),
        (
            changed(
                &w,
                "--sbom crates/kbf-task/kbf-task_bin.cdx.json --commit \"$GITHUB_SHA\" --out dist\n      - uses",
                "--sbom crates/kbf-cli/kbf_bin.cdx.json --commit \"$GITHUB_SHA\" --out dist\n      - uses",
            ),
            "package-task-bundle lacks `--sbom crates/kbf-task/kbf-task_bin.cdx.json`",
        ),
        (
            changed(
                &w,
                "      - name: Package the bundle\n",
                "      - name: Package the bundle\n        if: ${{ always() }}\n",
            ),
            "is under `if:` or `continue-on-error:`",
        ),
        (
            changed(
                &w,
                "JS_BUNDLE_SHA256_AARCH64_DARWIN: dd21",
                "JS_BUNDLE_SHA256_AARCH64_DARWIN: zz21",
            ),
            "JS_BUNDLE_SHA256_AARCH64_DARWIN is not 64 lowercase hex digits",
        ),
        (
            changed(
                &w,
                "      - name: The one tree of the bundle is the signed kbf-task\n",
                "      - name: Pack early\n        run: $RUNNER_TEMP/tools/js-bundle pack --out x\n      - name: The one tree of the bundle is the signed kbf-task\n",
            ),
            "has 2 steps that pack the bundle",
        ),
    ];
    for (workflow, rule) in &cases {
        let got = found(workflow);
        assert!(
            got.iter().any(|p| p.contains(rule)),
            "wanted {rule:?}: {got:?}"
        );
    }
    // Another job that packs a bundle.
    let other = format!(
        "{w}\n  other:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo xtask package-task-bundle --layout l --sbom s --commit c --out dist\n"
    );
    let got = found(&other);
    assert!(
        got.iter()
            .any(|p| p.contains("only `darwin-arm64` makes the kbf-task bundle")),
        "{got:?}"
    );
    // The order: the pack moved before the attestation check of the archive.
    let verify_at = w
        .find("      - name: The js-bundle archive is attested")
        .expect("verify step");
    let pack_at = w
        .find("      - name: Pack the signed kbf-task")
        .expect("pack step");
    let next = w[pack_at..]
        .find("      - name: The layout is a good bundle")
        .expect("next")
        + pack_at;
    let pack_step = &w[pack_at..next];
    let moved = format!(
        "{}{}{}{}",
        &w[..verify_at],
        pack_step,
        &w[verify_at..pack_at],
        &w[next..]
    );
    let got = found(&moved);
    assert!(
        got.iter().any(|p| p.contains("is not before the pack")),
        "{got:?}"
    );
    // The docs: a doc that lacks the files or the pin.
    let doc = std::fs::read_to_string(crate::repo_root().join("docs/artifacts.md")).expect("docs");
    let bare = doc.replace("kbf-task-bundle-<commit>-darwin-arm64.tar.gz", "x");
    assert!(
        doc_problems(&w, &bare)
            .iter()
            .any(|p| p.contains("does not list")),
        "the doc without the tarball name is accepted"
    );
    let unpinned = doc.replace("1f304ae75dbef20c9b01231b666b1e91e1f21743", "x");
    assert!(
        doc_problems(&w, &unpinned)
            .iter()
            .any(|p| p.contains("the commit the js-bundle release is attested at")),
        "the doc without the pin is accepted"
    );
}
