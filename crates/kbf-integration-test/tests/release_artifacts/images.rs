//! The images in the pinned release set (docs/artifacts.md): an OCI image tarball of the frontend
//! one of the update coordinator and one of the task runner for each Linux architecture,
//! `kbf-<service>-image-<commit>-<platform>.oci.tar`. The frontend's is built from the `kbf` of
//! the job's own tarball, the coordinator's and the task runner's (they have no tarball) from the
//! `kbf-coordinator` and `kbf-task` the job built (the task runner's is the static musl build, which
//! runs in any Linux image). No pull request run attests them, so a step that goes missing would
//! turn `main` red or release an image nobody checked. [`rules`] reads the workflow and names a job that:
//!
//! - does not make an image an asset exactly once (`cargo xtask pkg-image-asset --service`,
//!   [`PACKAGES`]), or makes it for another platform than its own;
//! - gives that line another commit, platform, reference (`--kbf-tarball`, the tarball of this
//!   job's `kbf`; or `--binary`, the job's `kbf-coordinator` or `kbf-task`), image or output directory than the
//!   ones the rules name, or runs it under `if:` or `continue-on-error:`;
//! - makes an image an asset without, in steps before it that cannot be skipped, the context
//!   filled from that reference, the commit's time put in the environment
//!   (`cargo xtask source-date-epoch`), two `docker buildx build` runs and a `cmp` of their
//!   two files (the build is reproducible), or before the reference names the commit
//!   (`build-commit check-asset` of the tarball, `build-commit check` of the coordinator or the task runner);
//!
//! and names the verify job when it does not run `pkg-image-asset --verify-only` on each signed
//! image, with the commit (and for the frontend its tarball), in an unconditional step after the
//! attestation check.

use super::*;

/// The command that makes an image an asset, and the one that checks the signed one.
const VERB: &str = "cargo xtask pkg-image-asset ";

/// An image of the release set.
#[derive(Clone, Copy)]
struct Service {
    /// The word after `--service`.
    name: &'static str,
    /// The two files the image is built into, and the context directory.
    first: &'static str,
    second: &'static str,
    context: &'static str,
    /// The program the image holds (none for the frontend, whose `kbf` is in the tarball), and
    /// the path the Linux jobs build it at.
    program: &'static str,
    binary: &'static str,
}

const FRONTEND: Service = Service {
    name: "frontend",
    first: "target/image-out/1.oci.tar",
    second: "target/image-out/2.oci.tar",
    context: "target/image",
    program: "kbf",
    binary: "",
};

const COORDINATOR: Service = Service {
    name: "coordinator",
    first: "target/coordinator-image-out/1.oci.tar",
    second: "target/coordinator-image-out/2.oci.tar",
    context: "target/coordinator-image",
    program: "kbf-coordinator",
    binary: "target/$TARGET/release/kbf-coordinator",
};

const TASK: Service = Service {
    name: "task",
    first: "target/task-image-out/1.oci.tar",
    second: "target/task-image-out/2.oci.tar",
    context: "target/task-image",
    program: "kbf-task",
    binary: "target/$STATIC_TARGET/release/kbf-task",
};

impl Service {
    /// The `--service` word of `words`.
    fn is_of(self, words: &[&str]) -> bool {
        flag(words, "--service") == Some(self.name)
    }

    /// The flag and value that tie the asset line of `platform` to the file the image carries.
    fn reference(self, platform: &str) -> (&'static str, String) {
        match self.name {
            "frontend" => (
                "--kbf-tarball",
                format!("dist/kbf-$GITHUB_SHA-{platform}.tar.gz"),
            ),
            _ => ("--binary", self.binary.to_owned()),
        }
    }
}

/// Appends to `out` what is wrong with how `workflow` makes each image an asset, and checks it
/// after signing. An error when the workflow cannot be read.
pub fn rules(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let made = attested_assets::packaged_images(workflow)?;
    for service in [FRONTEND, COORDINATOR, TASK] {
        for platform in PACKAGES {
            let n = made
                .iter()
                .filter(|(s, p)| s == service.name && p == platform)
                .count();
            if n != 1 {
                out.push(format!(
                    "the {} image for {platform} is made an asset by {n} `cargo xtask pkg-image-asset` lines, not exactly one",
                    service.name
                ));
            }
        }
        for platform in PACKAGES {
            build_job(workflow, service, platform, out)?;
        }
    }
    for (service, platform) in &made {
        if !PACKAGES.contains(&platform.as_str()) {
            out.push(format!(
                "`cargo xtask pkg-image-asset --platform {platform}` makes an image that is not in `PACKAGES`: add it there and to docs/artifacts.md"
            ));
        }
        if ![FRONTEND, COORDINATOR, TASK]
            .iter()
            .any(|s| s.name == service)
        {
            out.push(format!(
                "`cargo xtask pkg-image-asset --service {service}` makes an image that is not in the release set: add it to docs/artifacts.md and to these rules"
            ));
        }
    }
    verify_job(workflow, out)
}

/// What is wrong with the job `platform`'s steps around the line that makes the image of
/// `service` an asset.
fn build_job(
    workflow: &str,
    service: Service,
    platform: &str,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, platform)?;
    let asset = first(&steps, 0, |l| {
        args(l, VERB).is_some_and(|a| !a.contains(&"--verify-only") && service.is_of(&a))
    });
    let Some((asset_at, asset_step)) = asset else {
        return Ok(());
    };
    let words = lines(asset_step)
        .filter_map(|l| args(l, VERB))
        .find(|a| service.is_of(a))
        .unwrap_or_default();
    let who = format!("the {} image of `{platform}`", service.name);
    if asset_step.conditional {
        out.push(format!(
            "{who} is made an asset under `if:` or `continue-on-error:`"
        ));
    }
    let (reference_flag, reference) = service.reference(platform);
    for (name, want) in [
        ("--commit", "$GITHUB_SHA"),
        ("--platform", platform),
        (reference_flag, reference.as_str()),
        ("--image", service.first),
        ("--out", "dist"),
    ] {
        if flag(&words, name) != Some(want) {
            out.push(format!(
                "`cargo xtask pkg-image-asset` in `{platform}` has {name} {:?}, not `{want}`",
                flag(&words, name).unwrap_or("(none)")
            ));
        }
    }
    // Each earlier step must exist, before the asset, and not be skippable.
    let earlier =
        |what: &str, found: Option<(usize, &workflows::Step)>, out: &mut Vec<String>| match found {
            Some((at, step)) if at < asset_at => {
                if step.conditional {
                    out.push(format!(
                        "{who}: {what} is under `if:` or `continue-on-error:`"
                    ));
                }
            }
            _ => out.push(format!(
                "{who} is made an asset without {what} in a step before"
            )),
        };
    let context = service.context;
    let filled = |l: &str| match service.name {
        "frontend" => {
            l.starts_with("tar ")
                && l.contains(&format!("-xzf \"{reference}\""))
                && l.contains(&format!("-C {context} kbf"))
        }
        _ => l == format!("cp \"{}\" {context}/{}", service.binary, service.program),
    };
    earlier(
        &match service.name {
            "frontend" => format!(
                "the context taken out of this job's kbf tarball (`tar -xzf` into {context})"
            ),
            _ => format!(
                "the context filled with this job's {} (`cp` into {context})",
                service.program
            ),
        },
        first(&steps, 0, filled),
        out,
    );
    let (first_file, second_file) = (service.first, service.second);
    let builds: Vec<(usize, &workflows::Step)> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            lines(s).any(|l| {
                l.starts_with("docker buildx build ") && l.ends_with(&format!(" {context}"))
            })
        })
        .collect();
    let built = |dest: &str| {
        builds
            .iter()
            .copied()
            .find(|(_, s)| lines(s).any(|l| l.contains(&format!("dest={dest},"))))
    };
    earlier(
        "the commit's time in the environment (`cargo xtask source-date-epoch`)",
        first(&steps, 0, |l| {
            args(l, "cargo xtask source-date-epoch ").is_some_and(|a| {
                flag(&a, "--commit") == Some("$GITHUB_SHA")
                    && flag(&a, "--output") == Some("$GITHUB_ENV")
            })
        }),
        out,
    );
    earlier(
        "a `docker buildx build` into 1.oci.tar",
        built(first_file),
        out,
    );
    earlier(
        "a `docker buildx build` into 2.oci.tar",
        built(second_file),
        out,
    );
    earlier(
        "`cmp` of the two builds",
        first(&steps, 0, |l| {
            l == format!("cmp {first_file} {second_file}")
        }),
        out,
    );
    for (_, step) in &builds {
        for l in lines(step).filter(|l| l.starts_with("docker buildx build ")) {
            for must in [
                "--provenance=false",
                "--sbom=false",
                "--build-arg SOURCE_DATE_EPOCH",
            ] {
                if !l.contains(must) {
                    out.push(format!(
                        "{who}: a `docker buildx build` line lacks `{must}`, so the image is not reproducible"
                    ));
                }
            }
            // `type=oci` writes a layout with no manifest.json, which `docker image load` refuses.
            if !l.contains("--output \"type=docker,") {
                out.push(format!(
                    "{who}: a `docker buildx build` line does not write `--output \"type=docker,...\"`, so `docker image load` cannot read the image"
                ));
            }
        }
    }
    // The reference must name the commit before the image is made from it. (A frontend tarball
    // never checked at all is `identity`'s to name; the coordinator and the task runner are checked only here.)
    let checked = first(&steps, 0, |l| match service.name {
        "frontend" => args(l, "cargo xtask build-commit check-asset ")
            .is_some_and(|a| a.len() == 2 && a[0] == reference && a[1] == "$GITHUB_SHA"),
        _ => args(l, "cargo xtask build-commit check ")
            .is_some_and(|a| a.len() == 2 && a[0] == service.binary && a[1] == "$GITHUB_SHA"),
    });
    if service.name == TASK.name {
        // The task runner is mounted into the images of other actions, so it is the static build.
        earlier(
            "`rustup target add \"$STATIC_TARGET\"`",
            first(&steps, 0, |l| l == "rustup target add \"$STATIC_TARGET\""),
            out,
        );
        earlier(
            "a `cargo build` of kbf-task `--target \"$STATIC_TARGET\"`",
            first(&steps, 0, |l| {
                l.starts_with("cargo build ")
                    && l.contains("--target \"$STATIC_TARGET\"")
                    && l.contains("-p kbf-task")
            }),
            out,
        );
        earlier(
            "a `cargo xtask static-elf` check that kbf-task is statically linked",
            first(&steps, 0, |l| {
                l.starts_with("cargo xtask static-elf ") && l.contains(service.binary)
            }),
            out,
        );
    }
    let (verb, of) = match service.name {
        "frontend" => ("check-asset", "the kbf tarball".to_owned()),
        _ => ("check", format!("the {}", service.program)),
    };
    match (service.name, checked) {
        (_, Some((at, _))) if at > asset_at => out.push(format!(
            "{who} is made an asset before `cargo xtask build-commit {verb}` of {of}"
        )),
        ("frontend", None) => {}
        (_, None) => out.push(format!(
            "{who} is made an asset without `cargo xtask build-commit check` of {of} before it"
        )),
        _ => {}
    }
    Ok(())
}

/// What is wrong with the verify job's check of each signed image.
fn verify_job(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let verify = workflows::job_steps(workflow, "verify-linux")?;
    let verified_at = first(&verify, 0, |l| {
        args(l, "cargo xtask verify-assets attestations ").is_some()
    })
    .map_or(usize::MAX, |(at, _)| at);
    for service in [FRONTEND, COORDINATOR, TASK] {
        for platform in PACKAGES {
            let image = format!(
                "dist/kbf-{}-image-$GITHUB_SHA-{platform}.oci.tar",
                service.name
            );
            let tarball = format!("dist/kbf-$GITHUB_SHA-{platform}.tar.gz");
            let found = first(&verify, 0, |l| {
                args(l, VERB).is_some_and(|a| {
                    // The coordinator and the task runner have no image tarball and the verify job has not its binary.
                    let reference = match service.name {
                        "frontend" => flag(&a, "--kbf-tarball") == Some(tarball.as_str()),
                        _ => flag(&a, "--kbf-tarball").is_none() && flag(&a, "--binary").is_none(),
                    };
                    a.contains(&"--verify-only")
                        && service.is_of(&a)
                        && flag(&a, "--image") == Some(image.as_str())
                        && reference
                        && flag(&a, "--platform") == Some(platform)
                        && flag(&a, "--commit") == Some("$GITHUB_SHA")
                })
            });
            let checked = found.is_some_and(|(at, step)| at > verified_at && !step.conditional);
            if !checked {
                out.push(format!(
                    "`verify-linux` does not run `cargo xtask pkg-image-asset --verify-only` on `{image}` in an unconditional step after the attestation check, so the image that was signed is not the one checked"
                ));
            }
        }
    }
    Ok(())
}
