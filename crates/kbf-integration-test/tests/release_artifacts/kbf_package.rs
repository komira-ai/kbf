//! The `kbf` package in the pinned release set (docs/artifacts.md): `/usr/bin/kbf`, the units
//! of the modes, `kbf.slice`, the users and directories they run with, the needrestart
//! drop-in and the trust directory (the pinned `gh`, the Sigstore trusted root and the
//! branches whose attestations are trusted) are a deb and an rpm of their own, `kbf`. Each
//! Linux job builds both from one configuration step and makes the deb an asset,
//! `kbf-<commit>-<platform>.deb`; no pull request run attests it, so a step that goes missing
//! would turn `main` red or release a deb nobody checked. [`rules`] reads the workflow and
//! names a job that:
//!
//! - does not make the `kbf` deb an asset exactly once (`cargo xtask pkg-kbf-asset`,
//!   [`PACKAGES`]), or makes it for another platform than its own;
//! - gives that line another commit, platform, `--kbf-tarball` (the tarball of this job's
//!   `kbf`, which `build-commit check-asset` has checked), directory of the package or output
//!   directory than the ones the rules name, no pinned `gh` to compare the package's with, an
//!   `--expect-ref` other than the job's `PKG_SOURCE_REF` (the package trusts and is attested
//!   for that branch and no other, never a literal ref), or runs it under `if:` or
//!   `continue-on-error:`;
//! - runs it before `pkg-build` of the configuration, before `build-commit check-asset` of the
//!   `kbf` tarball, before `pkg-trust --source-ref "$PKG_SOURCE_REF"`, before the install test
//!   (`pkg-install-test --name kbf --format deb`) or before the upgrade test
//!   (`pkg-upgrade-test` of the same package), or any of them under `if:` or
//!   `continue-on-error:`;
//! - writes the configuration (`pkg-config`) without `--kbf` naming the `kbf` it packages (a
//!   package of another build's `kbf` has the same file name);
//! - fetches a tool or a deb whose `--sha256` is not a workflow variable holding 64 lowercase
//!   hex digits, or has no `PKG_SOURCE_REF` set to [`PKG_SOURCE_REF_EXPR`];
//!
//! and names the verify job when it does not run `pkg-kbf-asset --verify-only` on each signed
//! deb, with its tarball, the commit and `--expect-ref "$GITHUB_REF"`, in an unconditional step
//! after the attestation check.

use super::*;

/// The command that makes the deb an asset, and the one that checks the signed one.
const VERB: &str = "cargo xtask pkg-kbf-asset ";

/// Where `pkg-config` writes the `kbf` package's configuration, and `pkg-build` its packages.
const CONFIG: &str = "target/pkg/nfpm.yaml";
const BUILT: &str = "target/pkg/out";

/// The `--binary` of a line that packages the `kbf`.
fn packaged_kbf(line: &str) -> Option<&str> {
    let words = args(line, "cargo xtask package ")?;
    flag(&words, "--binary").filter(|b| base(b) == "kbf")
}

/// The `--kbf` of a line that writes the package configuration.
fn configured_kbf(line: &str) -> Option<&str> {
    let words = args(line, "cargo xtask pkg-config ")?;
    flag(&words, "--kbf")
}

/// Appends to `out` what is wrong with how `workflow` makes the `kbf` deb an asset, and
/// checks it after signing. An error when the workflow cannot be read.
pub fn rules(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let made = attested_assets::packaged_kbf_debs(workflow)?;
    for platform in PACKAGES {
        let n = made.iter().filter(|p| *p == platform).count();
        if n != 1 {
            out.push(format!(
                "the kbf package for {platform} is made an asset by {n} `cargo xtask pkg-kbf-asset` lines, not exactly one"
            ));
        }
    }
    for platform in &made {
        if !PACKAGES.contains(&platform.as_str()) {
            out.push(format!(
                "`cargo xtask pkg-kbf-asset --platform {platform}` makes a package that is not in `PACKAGES`: add it there and to docs/artifacts.md"
            ));
        }
    }
    for platform in PACKAGES {
        build_job(workflow, platform, out)?;
    }
    verify_job(workflow, out)
}

/// What is wrong with the job `platform`'s steps around the line that makes the deb an asset.
fn build_job(workflow: &str, platform: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, platform)?;
    let asset = first(&steps, 0, |l| {
        args(l, VERB).is_some_and(|a| !a.contains(&"--verify-only"))
    });
    let Some((asset_at, asset_step)) = asset else {
        return Ok(());
    };
    let words = lines(asset_step)
        .find_map(|l| args(l, VERB))
        .unwrap_or_default();
    let who = format!("the kbf package of `{platform}`");
    if asset_step.conditional {
        out.push(format!(
            "{who} is made an asset under `if:` or `continue-on-error:`"
        ));
    }
    let tarball = format!("dist/kbf-$GITHUB_SHA-{platform}.tar.gz");
    for (name, want) in [
        ("--commit", "$GITHUB_SHA"),
        ("--platform", platform),
        ("--kbf-tarball", tarball.as_str()),
        ("--package", BUILT),
        ("--expect-ref", "$PKG_SOURCE_REF"),
        ("--out", "dist"),
    ] {
        if flag(&words, name) != Some(want) {
            out.push(format!(
                "`cargo xtask pkg-kbf-asset` in `{platform}` has {name} {:?}, not `{want}`",
                flag(&words, name).unwrap_or("(none)")
            ));
        }
    }
    if flag(&words, "--gh").is_none() {
        out.push(format!(
            "`cargo xtask pkg-kbf-asset` in `{platform}` is not given the pinned gh (`--gh`) to compare the package's with"
        ));
    }
    // The package is built first, in a step that cannot be skipped.
    let built = first(&steps, 0, |l| {
        args(l, "cargo xtask pkg-build ").is_some_and(|a| {
            flag(&a, "--config") == Some(CONFIG) && flag(&a, "--out") == Some(BUILT)
        })
    });
    let build = "`cargo xtask pkg-build` of the kbf configuration";
    match built {
        Some((at, step)) if at < asset_at => {
            if step.conditional {
                out.push(format!(
                    "{who}: {build} is under `if:` or `continue-on-error:`"
                ));
            }
        }
        _ => out.push(format!(
            "{who} is made an asset without {build} in a step before"
        )),
    }
    // The trust directory names the branch, and the package is installed and upgraded to before
    // it is an asset, each in a step that cannot be skipped.
    let ordered =
        |found: Option<(usize, &workflows::Step)>, what: &str, out: &mut Vec<String>| match found {
            Some((at, step)) if at < asset_at && !step.conditional => {}
            Some((at, _)) if at < asset_at => out.push(format!(
                "{who}: {what} is under `if:` or `continue-on-error:`"
            )),
            _ => out.push(format!(
                "{who} is made an asset without {what} in a step before"
            )),
        };
    ordered(
        first(&steps, 0, |l| {
            args(l, "cargo xtask pkg-trust ")
                .is_some_and(|a| flag(&a, "--source-ref") == Some("$PKG_SOURCE_REF"))
        }),
        "`cargo xtask pkg-trust --source-ref \"$PKG_SOURCE_REF\"`",
        out,
    );
    ordered(
        first(&steps, 0, |l| {
            args(l, "cargo xtask pkg-install-test ").is_some_and(|a| {
                flag(&a, "--name") == Some("kbf")
                    && flag(&a, "--format") == Some("deb")
                    && flag(&a, "--package") == Some(BUILT)
            })
        }),
        "`cargo xtask pkg-install-test --name kbf --format deb` of the kbf package",
        out,
    );
    ordered(
        first(&steps, 0, |l| {
            args(l, "cargo xtask pkg-upgrade-test ").is_some_and(|a| {
                flag(&a, "--package") == Some(BUILT)
                    && flag(&a, "--old-installer").is_some()
                    && flag(&a, "--old-kbf").is_some()
            })
        }),
        "`cargo xtask pkg-upgrade-test` of the kbf package over the previous release's debs",
        out,
    );
    for step in &steps {
        for line in lines(step) {
            let pkg = line.starts_with("cargo xtask pkg-trust ")
                || line.starts_with("cargo xtask pkg-kbf-asset ");
            if pkg && line.contains("refs/heads/") {
                out.push(format!(
                    "`{platform}`: `{line}` names a literal branch: the package trusts and is attested for `$PKG_SOURCE_REF` and no other"
                ));
            }
        }
    }
    fetch_pins(workflow, platform, &steps, out);
    let env = workflows::job_env(workflow, platform)?;
    let got = env
        .iter()
        .find(|(k, _)| k == PKG_SOURCE_REF)
        .map(|(_, v)| v.as_str());
    if got != Some(PKG_SOURCE_REF_EXPR) {
        out.push(format!(
            "the job `{platform}` has {PKG_SOURCE_REF}: {}, not exactly `{PKG_SOURCE_REF_EXPR}`",
            got.unwrap_or("(none)")
        ));
    }
    // (A kbf tarball never checked at all is `identity`'s to name; here, one checked too late.)
    let checked = first(&steps, 0, |l| {
        args(l, "cargo xtask build-commit check-asset ")
            .is_some_and(|a| a.len() == 2 && a[0] == tarball && a[1] == "$GITHUB_SHA")
    });
    if checked.is_some_and(|(at, _)| at > asset_at) {
        out.push(format!(
            "{who} is made an asset before `cargo xtask build-commit check-asset` of the kbf tarball"
        ));
    }
    // The configuration is written from the `kbf` this job packages.
    let shipped = first(&steps, 0, |l| packaged_kbf(l).is_some())
        .and_then(|(_, s)| lines(s).find_map(packaged_kbf));
    let configured = first(&steps, 0, |l| configured_kbf(l).is_some())
        .and_then(|(_, s)| lines(s).find_map(configured_kbf));
    if configured != shipped {
        out.push(format!(
            "`{platform}` writes the package configuration with --kbf {configured:?}, not {shipped:?}, the kbf it packages"
        ));
    }
    Ok(())
}

/// What is wrong with the verify job's check of each signed `kbf` deb.
fn verify_job(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let verify = workflows::job_steps(workflow, "verify-linux")?;
    let verified_at = first(&verify, 0, |l| {
        args(l, "cargo xtask verify-assets attestations ").is_some()
    })
    .map_or(usize::MAX, |(at, _)| at);
    for platform in PACKAGES {
        let deb = format!("dist/kbf-$GITHUB_SHA-{platform}.deb");
        let tarball = format!("dist/kbf-$GITHUB_SHA-{platform}.tar.gz");
        let found = first(&verify, 0, |l| {
            args(l, VERB).is_some_and(|a| {
                a.contains(&"--verify-only")
                    && flag(&a, "--package") == Some(deb.as_str())
                    && flag(&a, "--kbf-tarball") == Some(tarball.as_str())
                    && flag(&a, "--platform") == Some(platform)
                    && flag(&a, "--commit") == Some("$GITHUB_SHA")
                    && flag(&a, "--expect-ref") == Some("$GITHUB_REF")
            })
        });
        let checked = found.is_some_and(|(at, step)| at > verified_at && !step.conditional);
        if !checked {
            out.push(format!(
                "`verify-linux` does not run `cargo xtask pkg-kbf-asset --verify-only` on `{deb}` (with `--expect-ref \"$GITHUB_REF\"`) in an unconditional step after the attestation check, so the kbf package that was signed is not the one checked"
            ));
        }
    }
    Ok(())
}
