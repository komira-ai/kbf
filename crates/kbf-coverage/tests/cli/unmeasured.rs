//! `kbf-coverage` end to end on crates whose lines measure `-`: a library with
//! `[lib] test = false` has no record in the tracefile at all, whatever untested code it
//! holds, so neither `check`'s advice nor `tighten` may take `-` for its lines where the
//! file has a count, or for a crate the file does not list yet.

use std::path::Path;
use std::process::{Command, Output};

use super::{MAYBE, REFUSED, baseline_of, ci_check, stderr, stdout, workspace};

/// Writes crate `name` under `root` as a library that says `[lib] test = false`, with
/// an untested function, as the verifier's probes did.
fn no_test_crate(root: &Path, name: &str) {
    let dir = root.join("crates").join(name);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir");
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\n\n[lib]\ntest = false\n"),
    )
    .expect("write Cargo.toml");
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub fn hidden(x: u32) -> u32 {\n    if x == 7 { x * 3 } else { x * 5 }\n}\n",
    )
    .expect("write lib.rs");
}

/// Catches (the verifier's probe, end to end): a crate no test binary instruments read
/// as covered. `notest` says `[lib] test = false`, so `cargo llvm-cov --workspace`
/// writes no record for it at all (its tracefile has no `SF:` line under
/// crates/notest), even after it gains the untested function below. Recorded `0 0`, it
/// measures `- -`; lowering to that would hide the new code. `check` must fail and
/// print the refusal with the hand edit it takes, and `tighten` must refuse from the
/// tracefile (no advice: measured here) and from the measured file (the softer advice),
/// leaving the file as it was.
#[test]
fn a_crate_no_test_binary_instruments_is_not_lowered_to_no_data() {
    let root = workspace("notest");
    no_test_crate(&root, "notest");
    std::fs::create_dir_all(root.join("target/coverage")).expect("mkdir");
    let recorded = "a 1 0\nb 0 -\nempty - -\nnotest 0 0\n";
    let o = ci_check(
        &root,
        recorded,
        &[
            "--write-baseline",
            "target/coverage/coverage-baseline.measured",
        ],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains(
            "| notest | 0/0 | - | - | - | 0/0 | - | - | - | 0 / 0 | MORE MISSED THAN BASELINE (lines, branches) |"
        ),
        "{out}"
    );
    assert!(
        out.ends_with(
            "`kbf-coverage tighten` cannot fix this failure: it only lowers counts, and it refuses:\n\n\
             - notest lines: recorded 0, measured -: nothing measured where the file has a count would \
             hide misses (a crate with `[lib] test = false` has no coverage record at all); give it a \
             test target back, or, if it has nothing to measure, put - in its line by hand in a \
             reviewed pull request\n\
             - notest branches: recorded 0, measured -: nothing measured where the file has a count \
             would hide misses (a crate with `[lib] test = false` has no coverage record at all); give \
             it a test target back, or, if it has nothing to measure, put - in its line by hand in a \
             reviewed pull request\n"
        ),
        "{out}"
    );
    let measured = "target/coverage/coverage-baseline.measured";
    let written = std::fs::read_to_string(root.join(measured)).expect("measured");
    assert!(
        written
            .lines()
            .any(|l| l.split_whitespace().eq(["notest", "-", "-"])),
        "{written}"
    );
    for (args, advice) in [
        (["--lcov", "lcov.info"], "\n"),
        (["--measured", measured], MAYBE),
    ] {
        let o = Command::new(env!("CARGO_BIN_EXE_kbf-coverage"))
            .current_dir(&*root)
            .arg("tighten")
            .args(args)
            .arg("--root")
            .arg(&*root)
            .arg("--baseline")
            .arg("./coverage-baseline")
            .output()
            .expect("run kbf-coverage tighten");
        assert_eq!(o.status.code(), Some(1), "{args:?}: {}", stderr(&o));
        assert_eq!(baseline_of(&root), recorded, "{args:?}");
        let err = stderr(&o);
        let lines: Vec<&str> = err.lines().collect();
        assert_eq!(
            lines[..3],
            [
                REFUSED.trim_end(),
                "  notest lines: recorded 0, measured -: nothing measured where the file has a count \
                 would hide misses (a crate with `[lib] test = false` has no coverage record at all); \
                 give it a test target back, or, if it has nothing to measure, put - in its line by \
                 hand in a reviewed pull request",
                "  notest branches: recorded 0, measured -: nothing measured where the file has a \
                 count would hide misses (a crate with `[lib] test = false` has no coverage record at \
                 all); give it a test target back, or, if it has nothing to measure, put - in its line \
                 by hand in a reviewed pull request",
            ],
            "{args:?}"
        );
        // Then the advice (after a measured file) or nothing (after a tracefile).
        assert_eq!(
            err,
            format!("{}{advice}", lines[..3].join("\n")),
            "{args:?}"
        );
    }
}

/// What `tighten` says about a new crate `name` that measures `- -`.
fn new_crate_unmeasured(name: &str) -> String {
    format!(
        "{name}: not in the baseline, measured - -: a new crate whose lines measure - has no \
         coverage record (a crate with `[lib] test = false` has none at all, whatever code it \
         holds); give it a test target, or, if it has nothing to measure, put - in its line by \
         hand in a reviewed pull request"
    )
}

/// A workspace like the verifier's probes: crates `tested` with a record each in the
/// tracefile (9 lines and 2 branches, all run, as cargo llvm-cov wrote them), and
/// crates `no_test` that say `[lib] test = false` and so have none.
fn probe(name: &str, tested: &[&str], no_test: &[&str]) -> super::Workspace {
    let root = super::tighten_workspace(name);
    // `tighten_workspace`'s crates are not this probe's.
    std::fs::remove_dir_all(root.join("crates")).expect("clear crates");
    let mut lcov = String::new();
    for krate in tested {
        let dir = root.join("crates").join(krate);
        std::fs::create_dir_all(dir.join("src")).expect("mkdir");
        std::fs::write(dir.join("Cargo.toml"), "").expect("write Cargo.toml");
        lcov.push_str(&format!(
            "SF:{}\nBRF:2\nBRH:2\nLF:9\nLH:9\nend_of_record\n",
            dir.join("src/lib.rs").display()
        ));
    }
    for krate in no_test {
        no_test_crate(&root, krate);
    }
    std::fs::write(root.join("lcov.info"), lcov).expect("write lcov");
    std::fs::create_dir_all(root.join("target/coverage")).expect("mkdir");
    root
}

fn tighten_here(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-coverage"))
        .current_dir(root)
        .arg("tighten")
        .args(args)
        .arg("--root")
        .arg(root)
        .arg("--baseline")
        .arg("./coverage-baseline")
        .output()
        .expect("run kbf-coverage tighten")
}

/// Catches (the round-3 verifier's s7, s9 and s10, end to end): a new crate whose
/// lines measure `-` added by `tighten`, or offered to it by `check`. With that, a
/// `[lib] test = false` crate hides any code: added new (s7), it takes untested code
/// with it; split off (s9), a recorded crate's uncovered code moves into it, the old
/// line lowers to `0 0` and the new one reads `- -`; renamed to (s10), the old line is
/// removed and the new one reads `- -`. Each run is refused: `check` prints the refusal
/// instead of the command, and `tighten` refuses from the measured file `check` wrote
/// (with the softer advice) and from the tracefile (none), leaving the file
/// byte-identical.
#[test]
fn a_new_crate_measured_no_data_is_refused_added_split_or_renamed() {
    let recorded_header = "# Coverage ratchet.\n# crate lines-missed branches-missed\n";
    for (name, tested, no_test, recorded, new) in [
        (
            "s7-new",
            &["user", "victim"][..],
            "victim2",
            "user 0 0\nvictim 0 0\n",
            "victim2",
        ),
        (
            "s9-split",
            &["user", "victim"][..],
            "victim2",
            "user 0 0\nvictim 5 2\n",
            "victim2",
        ),
        (
            "s10-rename",
            &["user"][..],
            "vnew",
            "user 0 0\nvictim 0 0\n",
            "vnew",
        ),
    ] {
        let root = probe(name, tested, &[no_test]);
        let recorded = format!("{recorded_header}{recorded}");
        let measured = "target/coverage/coverage-baseline.measured";
        let o = ci_check(
            &root,
            &recorded,
            &[
                "--write-baseline",
                measured,
                "--run-id",
                "7",
                "--repo",
                "o/r",
            ],
        );
        assert_eq!(o.status.code(), Some(1), "{name}: {}", stderr(&o));
        let out = stdout(&o);
        assert!(!out.contains("gh run download"), "{name}: {out}");
        assert!(
            out.ends_with(&format!(
                "`kbf-coverage tighten` cannot fix this failure: it only lowers counts, and it \
                 refuses:\n\n- {}\n",
                new_crate_unmeasured(new)
            )),
            "{name}: {out}"
        );
        let written = std::fs::read_to_string(root.join(measured)).expect("measured");
        assert!(
            written
                .lines()
                .any(|l| l.split_whitespace().eq([new, "-", "-"])),
            "{name}: {written}"
        );
        for (args, advice) in [
            (["--measured", measured], MAYBE),
            (["--lcov", "lcov.info"], "\n"),
        ] {
            let o = tighten_here(&root, &args);
            assert_eq!(o.status.code(), Some(1), "{name} {args:?}: {}", stderr(&o));
            assert_eq!(baseline_of(&root), recorded, "{name} {args:?}");
            assert_eq!(stdout(&o), "", "{name} {args:?}");
            assert_eq!(
                stderr(&o),
                format!("{REFUSED}  {}{advice}", new_crate_unmeasured(new)),
                "{name} {args:?}"
            );
        }
    }
}
