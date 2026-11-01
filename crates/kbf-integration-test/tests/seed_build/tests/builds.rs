//! Spoiled copies of the seed jobs themselves: removed, off the hosted runners, built by
//! something other than plain cargo, or no longer building every binary they ship.

use super::*;

#[test]
fn a_removed_seed_job_is_refused() {
    // Catches: the cargo release job of a platform removed from artifacts.yml (or renamed,
    // which removes it as far as the release's needs are concerned).
    for job in SEED_JOBS {
        let mut w = real();
        w.artifacts = without_job(&w.artifacts, job);
        red(&w.problems(), &format!("has no seed job `{job}`"));
        let mut w = real();
        w.artifacts = in_job(
            &w.artifacts,
            job,
            &format!("  {job}:\n"),
            &format!("  {job}-gone:\n"),
        );
        red(&w.problems(), &format!("has no seed job `{job}`"));
    }
}

#[test]
fn a_seed_job_off_the_hosted_runners_is_refused() {
    // Catches: a seed job moved to a self-hosted runner, a custom label (a kbf farm's
    // runners), a runner group, an expression or a second label.
    for (job, line) in RUNNERS {
        for to in [
            "    runs-on: self-hosted\n",
            "    runs-on: [self-hosted, linux, x64]\n",
            "    runs-on: kbf-farm\n",
            "    runs-on:\n      group: kbf-builders\n",
            "    runs-on:\n      group: kbf-builders\n      labels: ubuntu-24.04\n",
            "    runs-on: ${{ vars.RELEASE_RUNNER }}\n",
            "    runs-on: [ubuntu-24.04, kbf]\n",
        ] {
            let found = spoil_artifacts(job, line, to);
            red(&found, &format!("the seed job `{job}`"));
            assert!(
                found.iter().any(|p| p.contains("hosted runner")),
                "{job} with {to:?}: {found:#?}"
            );
        }
    }
}

#[test]
fn a_seed_job_that_calls_another_workflow_is_refused() {
    // Catches: a seed job turned into a call of another workflow (a kbf farm's build), whose
    // runners and build this lint never reads.
    for (job, line) in RUNNERS {
        red(
            &spoil_artifacts(job, line, "    uses: ./.github/workflows/farm-build.yml\n"),
            &format!("the seed job `{job}` calls another workflow"),
        );
    }
}

#[test]
fn buck2_in_a_seed_job_is_refused() {
    // Catches: the kbf build of a seed job replaced by a Buck2 or Bazel build, or Buck2
    // fetched and run beside it in the same job.
    for (job, build) in KBF_BUILDS {
        for to in [
            "- run: buck2 build //crates/kbf-cli:kbf\n".to_owned(),
            "- run: bazel build //crates/kbf-cli:kbf\n".to_owned(),
            format!("- run: $RUNNER_TEMP/tools/buck2 build //...\n      {build}"),
            format!(
                "- uses: actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9 # v6.1.0\n        with:\n          path: buck-out\n          key: x\n      {build}"
            ),
        ] {
            let found = spoil_artifacts(job, build, &to);
            assert!(
                found
                    .iter()
                    .any(|p| p.contains(&format!("the seed job `{job}` names `"))),
                "{job} with {to:?}: {found:#?}"
            );
        }
        // With the build gone, the job no longer builds kbf with cargo either.
        red(
            &spoil_artifacts(job, build, "- run: buck2 build //crates/kbf-cli:kbf\n"),
            &format!("the seed job `{job}` has no unconditional plain"),
        );
    }
}

#[test]
fn a_farm_or_remote_execution_flag_in_a_seed_job_is_refused() {
    // Catches: a seed job's cargo pointed at a kbf farm: a remote executor or cache, a gRPC
    // farm address, a rustc wrapper (sccache, or one set by `--config` or a variable), in
    // the build line, the step's or the job's `env:`, or the workflow's.
    for (job, build) in KBF_BUILDS {
        let line = build.trim_start_matches("- run: ").trim_end();
        for to in [
            format!("- run: {line} --config build.rustc-wrapper=kbf-rustc\n"),
            format!("- run: RUSTC_WRAPPER=sccache {line}\n"),
            format!("- run: CARGO_BUILD_RUSTC=/opt/kbf/rustc {line}\n"),
            format!("- run: {line} -Z remote-executor\n"),
            format!("- run: {line} --remote_executor=grpc://\n"),
            format!("- env:\n          KBF_FRONTEND: grpcs://\n        run: {line}\n"),
            format!("- env:\n          SCCACHE_BUCKET: kbf-cache\n        run: {line}\n"),
        ] {
            let found = spoil_artifacts(job, build, &to);
            red(&found, &format!("the seed job `{job}`"));
        }
    }
    let mut w = real();
    w.artifacts = w.artifacts.replacen(
        "env:\n  CARGO_TERM_COLOR: always\n",
        "env:\n  CARGO_TERM_COLOR: always\n  RUSTC_WRAPPER: sccache\n",
        1,
    );
    assert_ne!(w.artifacts, read(ARTIFACTS), "the workflow env is spoiled");
    for job in SEED_JOBS {
        red(
            &w.problems(),
            &format!("the seed job `{job}` names `rustc`"),
        );
    }
    let mut w = real();
    w.artifacts = in_job(
        &w.artifacts,
        "linux-x86_64",
        "    runs-on: ubuntu-24.04\n",
        "    runs-on: ubuntu-24.04\n    env:\n      RUSTC_WRAPPER: /opt/kbf/rustc-remote\n",
    );
    red(&w.problems(), "the seed job `linux-x86_64` names `rustc`");
}

#[test]
fn a_build_that_is_not_plain_cargo_is_refused() {
    // Catches: a seed job's kbf built without `--locked` or `--release`, with another verb
    // or toolchain, under a shell of its own, or in a step that can be skipped.
    for (job, build) in KBF_BUILDS {
        let line = build.trim_start_matches("- run: ").trim_end();
        for (to, want) in [
            (line.replace(" --locked", ""), "do not follow `cargo build`"),
            (
                line.replace(" --release", ""),
                "do not follow `cargo build`",
            ),
            (
                line.replace("cargo build", "cargo +nightly build"),
                "compiles with `cargo +nightly`",
            ),
            (
                line.replace("cargo build", "cargo b"),
                "compiles with `cargo b`",
            ),
            (
                format!("{line} --features remote"),
                "it passes `--features`",
            ),
        ] {
            let found = spoil_artifacts(job, build, &format!("- run: {to}\n"));
            red(&found, want);
        }
        red(
            &spoil_artifacts(
                job,
                build,
                &format!("- shell: kbf-shell {{0}}\n        run: {line}\n"),
            ),
            "under a `shell:` of its own",
        );
        red(
            &spoil_artifacts(
                job,
                build,
                &format!("- if: ${{{{ false }}}}\n        run: {line}\n"),
            ),
            &format!("the seed job `{job}` has no unconditional plain"),
        );
    }
}

#[test]
fn a_build_line_is_read_word_by_word() {
    // Catches: the reader of a build line taking a non-plain build for a plain one, or a
    // line that compiles nothing for a build.
    assert_eq!(
        build_line(
            r#"KBF_BUILD_COMMIT="${GITHUB_SHA::12}" cargo build --locked --release --target "$TARGET" -p kbf-deploy --bins"#
        ),
        Build::Plain(Built {
            packages: vec!["kbf-deploy"],
            bins: vec![],
            all_bins: true,
            target: Some("$TARGET"),
        })
    );
    assert_eq!(build_line("cargo xtask package dist kbf"), Build::None);
    assert_eq!(build_line("cargo cyclonedx --format json"), Build::None);
    assert_eq!(
        build_line("cargo build --locked --release -p"),
        Build::NotPlain("`-p` has no value".to_owned())
    );
    assert_eq!(
        build_line("cargo build --locked --release -p --bins"),
        Build::NotPlain("`-p` has no value".to_owned())
    );
    assert_eq!(
        build_line("cargo rustc --locked --release -p kbf-cli"),
        Build::NotPlain("it compiles with `cargo rustc`".to_owned())
    );
    assert_eq!(
        build_line("RUSTFLAGS=-Cx cargo build --locked --release"),
        Build::NotPlain("it sets `RUSTFLAGS`".to_owned())
    );
}

#[test]
fn another_rustc_in_a_seed_job_is_refused() {
    // Catches: a seed job's cargo given another compiler, a shim that can hand its work to a
    // farm, by `RUSTC` or `CARGO_BUILD_RUSTC` in a step's `env:`, or written to `$GITHUB_ENV`
    // by an earlier step.
    for (job, build) in KBF_BUILDS {
        for to in [
            "- env:\n          RUSTC: /opt/kbf/remote\n        run: {line}\n",
            "- env:\n          CARGO_BUILD_RUSTC: /opt/kbf/remote\n        run: {line}\n",
            "- run: echo \"RUSTC=/opt/kbf/remote\" >> \"$GITHUB_ENV\"\n      {build}",
        ] {
            let line = build.trim_start_matches("- run: ").trim_end();
            let found = around_kbf_build(job, build, &to.replace("{line}", line));
            red(&found, &format!("the seed job `{job}` names `rustc`"));
        }
    }
}

#[test]
fn every_word_a_seed_job_may_not_name_is_refused_alone() {
    // Catches: an entry of `NOT_PLAIN` that no longer refuses the scalar holding it (each
    // other spoil may be caught by another rule first).
    for word in NOT_PLAIN {
        let to = format!("- env:\n          SEED_SPOIL: \"x-{word}-y\"\n        {{build}}");
        let (job, build) = KBF_BUILDS[1];
        let to = to.replace("{build}", &build.replacen("- run:", "run:", 1));
        red(
            &spoil_artifacts(job, build, &to),
            &format!("the seed job `{job}` names `{word}`"),
        );
    }
}

#[test]
fn a_step_named_after_a_bucket_is_not_refused() {
    // Catches: a refused word matching inside an ordinary one (`buck` in `bucket`), which
    // would refuse an innocent edit of a seed job.
    for (job, build) in KBF_BUILDS {
        let found = spoil_artifacts(
            job,
            build,
            &format!("- name: Copy to the bucket\n        {}", &build[2..]),
        );
        assert!(found.is_empty(), "{job}: {found:#?}");
    }
}

#[test]
fn a_cargo_option_before_the_verb_is_refused() {
    // Catches: `cargo --config ... build` or `cargo -Z ... build`, whose first word after
    // `cargo` is not the verb, read as a line that builds nothing.
    for (job, build) in KBF_BUILDS {
        for to in [
            build.replace("cargo build", "cargo --config build.jobs=1 build"),
            build.replace("cargo build", "cargo -Zunstable-options build"),
        ] {
            red(&spoil_artifacts(job, build, &to), "comes before the verb");
        }
    }
    assert_eq!(build_line("cargo --version"), Build::None);
}

#[test]
fn a_seed_job_that_takes_anything_but_the_checkout_is_refused() {
    // Catches: a seed job given `needs:` (another job's outputs), an artifact download (files
    // built elsewhere swapped in before the upload), a local action this lint never reads
    // or `defaults:` of its own (a shell that runs the build somewhere else).
    for (job, line) in RUNNERS {
        for (to, want) in [
            (format!("{line}    needs: [sums]\n"), "needs another job"),
            (
                format!("{line}    defaults:\n      run:\n        shell: farm-shell {{0}}\n"),
                "has `defaults:` of its own",
            ),
        ] {
            red(&spoil_artifacts(job, line, &to), want);
        }
    }
    for (job, build) in KBF_BUILDS {
        red(
            &around_kbf_build(
                job,
                build,
                "- uses: actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1\n        with:\n          name: farm-kbf\n      {build}",
            ),
            &format!("the seed job `{job}` downloads an artifact"),
        );
        red(
            &around_kbf_build(
                job,
                build,
                "- uses: ./.github/actions/farm-build\n      {build}",
            ),
            &format!("the seed job `{job}` runs the local action"),
        );
    }
}

#[test]
fn every_shipped_binary_needs_its_own_unconditional_plain_build() {
    // Catches: a binary a platform ships (a tarball's, or the coordinator image's) whose plain
    // cargo build was removed from its seed job, or made skippable, while another plain build
    // (kbf's) stays, so the shipped file need not come from the seed build.
    for job in SEED_JOBS {
        let binaries = shipped(job);
        assert!(binaries.contains(&"kbf"), "{job} ships kbf: {binaries:?}");
        assert!(
            binaries.contains(&"kbf-host-install"),
            "{job}: {binaries:?}"
        );
        if job.starts_with("linux-") {
            assert!(
                binaries.contains(&"kbf-coordinator"),
                "{job} ships the coordinator image's binary: {binaries:?}"
            );
        }
        for binary in binaries {
            let (_, package) = SHIPPED_BINARIES
                .iter()
                .find(|(b, _)| *b == binary)
                .expect("every shipped binary has a package");
            red(
                &spoil_artifacts(job, &format!(" -p {package} "), " "),
                &format!(
                    "the seed job `{job}` has no unconditional plain `cargo build --locked --release` of `{binary}`"
                ),
            );
        }
        let host = "        run: KBF_BUILD_COMMIT=";
        let found = spoil_artifacts(job, host, &format!("        if: ${{{{ false }}}}\n{host}"));
        for binary in ["kbf-host-install", "kbf-updater", "kbf-deploy"] {
            red(&found, &format!("of `{binary}` (package `{binary}`)"));
        }
    }
    // A package built, but not the binary it ships.
    red(
        &spoil_artifacts(
            "darwin-arm64",
            "-p kbf-cli --bin kbf\n",
            "-p kbf-cli --bin kbf-cli-fake-exits\n",
        ),
        "the seed job `darwin-arm64` has no unconditional plain `cargo build --locked --release` of `kbf` (package `kbf-cli`)",
    );
    let task = "        run: cargo build --locked --release --target \"$STATIC_TARGET\" -p kbf-task --bin kbf-task\n";
    for job in ["linux-x86_64", "linux-arm64"] {
        red(
            &spoil_artifacts(job, task, &format!("        if: ${{{{ false }}}}\n{task}")),
            &format!(
                "the seed job `{job}` has no unconditional plain `cargo build --locked --release` of `kbf-task`"
            ),
        );
    }
}

#[test]
fn the_shipped_binaries_are_the_packages_targets() {
    // Catches: a row of `SHIPPED_BINARIES` naming a package that has no such binary (so a
    // build of the wrong package would satisfy it), or a shipped binary with no row.
    let root = crate::repo_root();
    for (binary, package) in SHIPPED_BINARIES {
        let manifest = std::fs::read_to_string(root.join(format!("crates/{package}/Cargo.toml")))
            .unwrap_or_else(|e| panic!("crates/{package}/Cargo.toml: {e}"));
        assert!(
            manifest.contains(&format!("[package]\nname = \"{package}\"\n")),
            "crates/{package}/Cargo.toml is not the package `{package}`"
        );
        let declared = manifest.contains(&format!("[[bin]]\nname = \"{binary}\"\n"));
        let default =
            binary == package && root.join(format!("crates/{package}/src/main.rs")).is_file();
        assert!(
            declared || default,
            "the package `{package}` has no binary `{binary}`"
        );
    }
    for job in SEED_JOBS {
        for binary in shipped(job) {
            assert!(
                SHIPPED_BINARIES.iter().any(|(b, _)| *b == binary),
                "`{binary}`, shipped for {job}, has no row in `SHIPPED_BINARIES`"
            );
        }
    }
}

#[test]
fn cargo_run_through_another_program_is_refused() {
    // Catches: a second, non-plain build added beside the valid one by running cargo through
    // `env`, `time` or a path to another cargo, which a reader of lines that start with
    // `cargo` would pass over.
    for (job, build) in KBF_BUILDS {
        for wrapped in [
            "env cargo build --release -p kbf-cli",
            "env -i PATH=/opt/kbf cargo build --locked --release -p kbf-cli",
            "time cargo build --release -p kbf-cli",
            "/opt/kbf/bin/cargo build --locked --release -p kbf-cli",
        ] {
            let first = wrapped.split_whitespace().next().expect("a word");
            red(
                &around_kbf_build(job, build, &format!("{{build}}      - run: {wrapped}\n")),
                &format!("cargo is run through `{first}`"),
            );
        }
    }
    assert_eq!(build_line("echo cargo xtask fetch"), Build::None);
}

#[test]
fn a_seed_build_into_another_target_directory_is_refused() {
    // Catches: a seed job's build sent to another target directory (`CARGO_TARGET_DIR` in a
    // step's, the job's or the workflow's `env:`, written to `$GITHUB_ENV`, or `--target-dir`),
    // so the plain build credited here is not the one in `target/` that packaging reads.
    for (job, build) in KBF_BUILDS {
        let line = build.trim_start_matches("- run: ").trim_end();
        for to in [
            format!("- env:\n          CARGO_TARGET_DIR: /opt/kbf/target\n        run: {line}\n"),
            format!(
                "- env:\n          CARGO_BUILD_TARGET_DIR: /opt/kbf/target\n        run: {line}\n"
            ),
            format!(
                "- run: echo \"CARGO_TARGET_DIR=/opt/kbf/target\" >> \"$GITHUB_ENV\"\n      {build}"
            ),
            format!("- run: {line} --target-dir /opt/kbf/target\n"),
        ] {
            red(
                &spoil_artifacts(job, build, &to),
                &format!("the seed job `{job}` names `target"),
            );
        }
    }
    let mut w = real();
    w.artifacts = w.artifacts.replacen(
        "env:\n  CARGO_TERM_COLOR: always\n",
        "env:\n  CARGO_TERM_COLOR: always\n  CARGO_TARGET_DIR: /opt/kbf/target\n",
        1,
    );
    assert_ne!(w.artifacts, read(ARTIFACTS), "the workflow env is spoiled");
    for job in SEED_JOBS {
        red(
            &w.problems(),
            &format!("the seed job `{job}` names `target_dir`"),
        );
    }
    let mut w = real();
    w.artifacts = in_job(
        &w.artifacts,
        "linux-arm64",
        "    runs-on: ubuntu-24.04-arm\n",
        "    runs-on: ubuntu-24.04-arm\n    env:\n      CARGO_TARGET_DIR: /opt/kbf/target\n",
    );
    red(
        &w.problems(),
        "the seed job `linux-arm64` names `target_dir`",
    );
}

#[test]
fn a_build_for_no_target_or_another_earns_no_credit() {
    // Catches: a seed job's build with `--target` dropped (its binaries land in
    // `target/release`, which no package step reads) or naming another triple, credited as the
    // plain build of what the job ships.
    for (job, build) in KBF_BUILDS {
        for to in [
            build.replace(" --target \"$TARGET\"", ""),
            build.replace("\"$TARGET\"", "\"$OTHER_TARGET\""),
        ] {
            assert_ne!(to, *build, "the build is spoiled");
            red(
                &spoil_artifacts(job, build, &to),
                &format!(
                    "the seed job `{job}` has no unconditional plain `cargo build --locked --release` of `kbf` (package `kbf-cli`)"
                ),
            );
        }
    }
    let task = " --target \"$STATIC_TARGET\" -p kbf-task --bin kbf-task\n";
    for job in ["linux-x86_64", "linux-arm64"] {
        red(
            &spoil_artifacts(job, task, " -p kbf-task --bin kbf-task\n"),
            &format!(
                "the seed job `{job}` has no unconditional plain `cargo build --locked --release` of `kbf-task`"
            ),
        );
    }
}
