use super::*;

#[test]
fn the_tool_is_the_path_buck2_names_for_its_label() {
    // Catches: the path of another target, of a label that only starts with the tool's, a line
    // with no path, or the label kept in the path, taken for the command line.
    let out = "toolchains//:cargo_1_95_0 /x/cargo\n\
               buildrules//cli:buildrules /repo/buck-out/v2/gen/buildrules/cli/buildrules\n";
    assert_eq!(
        tool_path(out),
        Some(PathBuf::from(
            "/repo/buck-out/v2/gen/buildrules/cli/buildrules"
        ))
    );
    for out in [
        "",
        "toolchains//:cargo_1_95_0 /x/cargo\n",
        "buildrules//cli:buildrules_unit /x/unit\n",
        "buildrules//cli:buildrules\n",
        "buildrules//cli:buildrules   \n",
        "buildrules//cli:buildrules buck-out/v2/gen/buildrules\n",
    ] {
        assert_eq!(tool_path(out), None, "{out:?}");
    }
}

#[test]
fn the_stale_inputs_are_checks_problem_lines_without_their_marks_and_fix() {
    // Catches: a line that is not the command line's (buck2's or cargo's words, an indented one)
    // taken for a problem, a problem lost (a stale one, or one without the `STALE:` mark such as a
    // missing generated file), the `buildrules:` or `STALE:` mark or the advice to run
    // `buildrules sync` kept, or a line with another ending dropped. The lines are the real
    // command line's (cli/src/check.zig of the pinned buildrules).
    let stderr = "\
buildrules: STALE: Cargo.lock changed since `buildrules sync` (recorded sha256 aa, now bb): run `buildrules sync` and commit the result
buildrules: a member has no cargo_package call: crates/x/BUCK
buildrules: STALE: crates/kbf-types/Cargo.toml changed since `buildrules sync` (recorded sha256 cc, now dd): run `buildrules sync` and commit the result
buildrules: STALE: a line of a later release
   buildrules: STALE: indented, so not the command line's
";
    assert_eq!(
        stale_inputs(stderr),
        [
            "Cargo.lock changed since `buildrules sync` (recorded sha256 aa, now bb)",
            "a member has no cargo_package call: crates/x/BUCK",
            "crates/kbf-types/Cargo.toml changed since `buildrules sync` (recorded sha256 cc, now dd)",
            "a line of a later release",
        ]
    );
    assert_eq!(stale_inputs(""), Vec::<String>::new());
    assert_eq!(stale_inputs("Compiling x\n"), Vec::<String>::new());
    assert_eq!(
        stale_inputs(
            "buildrules: buildrules.bzl is missing: run `buildrules sync` and commit the result"
        ),
        ["buildrules.bzl is missing"]
    );
}

#[test]
fn the_apply_command_names_the_run_the_repository_and_the_patch() {
    // Catches: a command for another run or repository, an artifact or file name that is not
    // the one the workflow uploads, or a download into a directory git tracks.
    let handoff = Handoff {
        patch: PathBuf::from("/runner/temp/buildrules-sync/buildrules-sync.patch"),
        summary: PathBuf::from("/runner/summary"),
        repo: "komira-ai/kbf".to_owned(),
        run_id: 19_000_000_001,
    };
    assert_eq!(
        handoff.apply_command(),
        "gh run download 19000000001 -R komira-ai/kbf -n buildrules-sync \
         -D buck-out/buildrules-sync-19000000001 && git apply \
         buck-out/buildrules-sync-19000000001/buildrules-sync.patch"
    );
}

#[test]
fn the_summary_names_the_inputs_the_generated_files_the_changed_files_and_the_command() {
    // Catches: a summary that leaves out the stale inputs, says nothing of the table being
    // generated, leaves out the files the patch changes or the command, or tells the reader to
    // run `buildrules sync`.
    let text = summary_markdown(
        &["Cargo.toml changed since `buildrules sync`".to_owned()],
        &[
            "buildrules.lock.bzl".to_owned(),
            "third_party/buildrules/BUCK".to_owned(),
        ],
        "gh run download 7 -R o/r -n buildrules-sync -D d && git apply d/buildrules-sync.patch",
    );
    assert_eq!(
        text,
        "## The generated build table is stale\n\
\n\
`buildrules check` found inputs of the generated build table changed since it was made:\n\
\n\
- Cargo.toml changed since `buildrules sync`\n\
\n\
`buildrules.lock.bzl`, `buildrules.bzl` and `third_party/buildrules/` are generated from the Cargo \
files (`Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`) by `buildrules sync`, and nobody edits \
them by hand. This job ran the pinned `buildrules sync` and uploaded what it changed as the \
artifact `buildrules-sync`, a patch of these files:\n\
\n\
- `buildrules.lock.bzl`\n\
- `third_party/buildrules/BUCK`\n\
\n\
Read it, then apply it in a checkout of the branch, commit and push. Nothing runs on your machine \
but `gh` and `git`:\n\
\n\
```sh\n\
gh run download 7 -R o/r -n buildrules-sync -D d && git apply d/buildrules-sync.patch\n\
```\n\
\n\
If `git apply` refuses it, main has changed the table too: merge main into the branch and push, \
and the next run carries a new patch.\n"
    );
}

#[test]
fn every_error_says_what_is_wrong() {
    // Catches: an error whose words do not name the command, its status and what it printed,
    // the file, or for a stale table the inputs, the files and the command.
    let cases = [
        (
            Error::Tool {
                status: "exit status: 3".to_owned(),
                stderr: "no cell".to_owned(),
            },
            "`buck2 build buildrules//cli:buildrules` failed (exit status: 3): no cell",
        ),
        (
            Error::NoTool {
                stdout: "x y\n".to_owned(),
            },
            "`buck2 build buildrules//cli:buildrules --show-full-output` named no output of \
             buildrules//cli:buildrules: \"x y\\n\"",
        ),
        (
            Error::Check {
                status: "exit status: 2".to_owned(),
                stderr: "buildrules: check: unknown argument".to_owned(),
            },
            "`buildrules check` could not judge the table (exit status: 2): buildrules: check: \
             unknown argument",
        ),
        (
            Error::Sync {
                status: "exit status: 1".to_owned(),
            },
            "the generated build table is stale, and `buildrules sync` failed (exit status: 1); \
             its output is above",
        ),
        (
            Error::StillFails {
                status: "exit status: 1".to_owned(),
                stderr: "buildrules: STALE: x".to_owned(),
            },
            "`buildrules sync` ran, and `buildrules check` still fails (exit status: 1): \
             buildrules: STALE: x",
        ),
        (
            Error::Git {
                args: "diff --binary".to_owned(),
                status: "exit status: 128".to_owned(),
                stderr: "fatal: not a git repository".to_owned(),
            },
            "`git diff --binary` failed (exit status: 128): fatal: not a git repository",
        ),
        (
            Error::Write {
                path: PathBuf::from("/x/p"),
                why: "Is a directory (os error 21)".to_owned(),
            },
            "cannot write \"/x/p\": Is a directory (os error 21)",
        ),
        (
            Error::Stale {
                inputs: vec!["a changed".to_owned(), "b changed".to_owned()],
                files: vec!["f".to_owned(), "g".to_owned()],
                apply: "gh run download 1".to_owned(),
            },
            "the generated build table is stale: a changed; b changed. buildrules.lock.bzl, \
             buildrules.bzl and third_party/buildrules/ are generated from the Cargo files by \
             `buildrules sync`, which this job ran at the pinned version; what it changed (f, g) \
             is the artifact buildrules-sync of this run, made against this run's merge of the \
             branch into main. Apply it in the branch's checkout, then commit and push (if it \
             does not apply, merge main first): gh run download 1",
        ),
    ];
    for (error, words) in cases {
        assert_eq!(error.to_string(), words);
        assert_eq!(FarmError::from(error).to_string(), words);
    }
}
