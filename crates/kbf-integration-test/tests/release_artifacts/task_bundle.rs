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
//! 3. fetch the pinned `js-bundle` with one command, `cargo xtask fetch-attested-tool` (url `v$JS_BUNDLE_VERSION`,
//!    sha256 `$JS_BUNDLE_SHA256_AARCH64_DARWIN`, the pinned `gh`, the jobscheduler repository, its `release.yml`,
//!    `refs/heads/main` and the commit `$JS_BUNDLE_SOURCE_DIGEST`), which checks the digest and the
//!    attestation of the downloaded file and takes the program out of that file (`xtask::fetch_attested`
//!    holds those checks); the plain `cargo xtask fetch` of that archive is refused;
//! 4. stage the signed binary alone in the tree directory (`cargo xtask stage-task-tree`), and `js-bundle pack` it
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
    // The archive is fetched, checked against its digest and its attestation, and unpacked by one command
    // (`cargo xtask fetch-attested-tool`): the plain `cargo xtask fetch` checks the digest only.
    for step in &steps {
        for line in lines(step).filter(|l| is_fetch_of_archive(l)) {
            out.push(format!(
                "`{DARWIN_JOB}`: the js-bundle archive is fetched with `cargo xtask fetch`, which does not check its attestation; use `cargo xtask fetch-attested-tool`: {line}"
            ));
        }
    }
    let is_gh = |l: &str| {
        args(l, "cargo xtask fetch ")
            .is_some_and(|a| flag(&a, "--out") == Some("$RUNNER_TEMP/tools/gh"))
    };
    let gh = one("fetch the pinned gh", &steps, out, is_gh);
    let is_fetch = |l: &str| args(l, "cargo xtask fetch-attested-tool ").is_some();
    let fetch = one(
        "fetch js-bundle, checked against its digest and its attestation",
        &steps,
        out,
        is_fetch,
    );
    if let Some(line) = line_of(&steps, fetch, is_fetch) {
        let a = args(line, "cargo xtask fetch-attested-tool ").unwrap_or_default();
        for (name, want) in [
            ("--url", RELEASE_URL),
            ("--sha256", SHA_VAR),
            (
                "--archive",
                "$RUNNER_TEMP/js-bundle-release/js-bundle-$JS_BUNDLE_VERSION-$TARGET.tar.gz",
            ),
            ("--member", "js-bundle-$JS_BUNDLE_VERSION-$TARGET/js-bundle"),
            ("--out", "$RUNNER_TEMP/tools/js-bundle"),
            ("--gh", "$RUNNER_TEMP/tools/gh"),
            ("--repo", "komira-ai/jobscheduler"),
            (
                "--signer-workflow",
                "komira-ai/jobscheduler/.github/workflows/release.yml",
            ),
            ("--source-ref", "refs/heads/main"),
            ("--source-digest", "$JS_BUNDLE_SOURCE_DIGEST"),
        ] {
            if flag(&a, name) != Some(want) {
                out.push(format!(
                    "`{DARWIN_JOB}`: the js-bundle fetch lacks `{name} {want}`: {line}"
                ));
            }
        }
    }
    let is_stage = |l: &str| args(l, "cargo xtask stage-task-tree ").is_some();
    let tree_copy = one("stage kbf-task as the bundle's tree", &steps, out, is_stage);
    let mut tree_dir = None;
    if let Some(line) = line_of(&steps, tree_copy, is_stage) {
        let a = args(line, "cargo xtask stage-task-tree ").unwrap_or_default();
        tree_dir = flag(&a, "--out");
        if flag(&a, "--binary") != Some("target/$TARGET/release/kbf-task") || tree_dir.is_none() {
            out.push(format!(
                "`{DARWIN_JOB}`: the tree of the bundle is not the signed `target/$TARGET/release/kbf-task` (`--binary`, `--out`): {line}"
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
    // The order: build, sign, stage, pack; gh, fetch, pack; pack, verify, package.
    let order: [(&str, Option<usize>); 8] = [
        ("the kbf-task build", build),
        ("the signing of kbf-task", sign),
        ("the staging of kbf-task into the tree", tree_copy),
        ("the fetch of the pinned gh", gh),
        ("the attested fetch of js-bundle", fetch),
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
    before(0, 1, "an unsigned binary would be staged", out);
    before(1, 2, "an unsigned binary would be staged", out);
    before(2, 5, "the pack would not hold the signed binary", out);
    before(3, 4, "the attestation is checked with the pinned gh", out);
    before(4, 5, "the pack needs the program", out);
    before(5, 6, "the layout is checked after it is written", out);
    before(6, 7, "the asset is made from a verified layout", out);
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
    // Asserts: each changed copy of the committed workflow is reported with the rule's own text:
    // the unconfined feature in the kbf-task build, the build for another target, the unsigned binary (the
    // signing step removed), the pack before the attested fetch, the archive pinned by another
    // variable, a missing source commit, another archive path or gh, the plain fetch of the archive, a pack of another directory, a verify of another layout, a
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
            changed(&w, " --source-digest \"$JS_BUNDLE_SOURCE_DIGEST\"", ""),
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
                "--sha256 \"$JS_BUNDLE_SHA256_AARCH64_DARWIN\" --archive",
                "--sha256 \"$GH_SHA256_AARCH64_DARWIN\" --archive",
            ),
            "lacks `--sha256 $JS_BUNDLE_SHA256_AARCH64_DARWIN`",
        ),
        (
            changed(
                &w,
                "--archive \"$RUNNER_TEMP/js-bundle-release/",
                "--archive \"$RUNNER_TEMP/other/",
            ),
            "lacks `--archive",
        ),
        (
            changed(
                &w,
                "--gh \"$RUNNER_TEMP/tools/gh\" --repo komira-ai/jobscheduler",
                "--gh gh --repo komira-ai/jobscheduler",
            ),
            "lacks `--gh $RUNNER_TEMP/tools/gh`",
        ),
        (
            changed(
                &w,
                "cargo xtask fetch-attested-tool --url",
                "cargo xtask fetch --url",
            ),
            "which does not check its attestation",
        ),
        (
            changed(
                &w,
                "stage-task-tree --binary \"target/$TARGET/release/kbf-task\"",
                "stage-task-tree --binary \"target/release/kbf-task\"",
            ),
            "the tree of the bundle is not the signed",
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
    // The order: the pack moved before the attested fetch of js-bundle.
    let fetch_at = w.find("      - name: Fetch js-bundle").expect("fetch step");
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
        &w[..fetch_at],
        pack_step,
        &w[fetch_at..pack_at],
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
