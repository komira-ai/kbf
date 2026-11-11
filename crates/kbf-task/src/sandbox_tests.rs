use std::path::{Path, PathBuf};

use super::*;

/// Catches the task hiding the wrong directories: the grants and the parent of its work
/// directory (`/scratch` for the default), or the work directory itself where that parent is
/// `/` or there is none, never `/`.
#[test]
fn a_task_hides_its_grants_and_its_scratch() {
    let grants = PathBuf::from(GRANTS_DIR);
    assert_eq!(
        hidden_for(Path::new("/scratch/work")),
        [grants.clone(), PathBuf::from("/scratch")]
    );
    assert_eq!(
        hidden_for(Path::new("/data/kbf/work")),
        [grants.clone(), PathBuf::from("/data/kbf")]
    );
    assert_eq!(
        hidden_for(Path::new("/work")),
        [grants.clone(), PathBuf::from("/work")]
    );
    assert_eq!(
        hidden_for(Path::new("work")),
        [grants.clone(), PathBuf::from("work")]
    );
    assert_eq!(hidden_for(Path::new("/")), [grants, PathBuf::from("/")]);
}

/// Catches the line not naming the action and how the sandbox came out, and a line written
/// where there is no sandbox to report.
#[test]
fn the_line_names_the_action_and_the_sandbox() {
    let digest = kbf_segments::sha256(b"an action");
    let line = |sandbox: Sandbox| notice(&digest, &sandbox);
    assert_eq!(
        line(Sandbox::Made),
        Some(format!(
            "kbf-task: action {digest}: sandbox: mount and pid namespaces"
        ))
    );
    assert_eq!(
        line(Sandbox::Incomplete {
            step: "uid_map",
            errno: 1
        }),
        Some(format!(
            "kbf-task: action {digest}: sandbox: incomplete (uid_map 1)"
        ))
    );
    assert_eq!(
        line(Sandbox::Unreported),
        Some(format!(
            "kbf-task: action {digest}: sandbox: unknown (no report from the child)"
        ))
    );
    assert_eq!(line(Sandbox::Unavailable), None);
    assert_eq!(Sandbox::Unavailable.to_string(), "none (Linux only)");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;

    use std::os::fd::AsRawFd as _;

    use super::*;
    use crate::child::{BIND, CHDIR, COVER, HOLD, KEEP, MKDIR, PRIVATE, SEAL, Step};

    /// The prefix of every bind source: a descriptor the child holds.
    const FD: &str = "/proc/self/fd/";

    /// What a step is, with the descriptor numbers left out (they differ between runs).
    #[derive(Debug, PartialEq, Eq)]
    enum Shape {
        Private,
        Hold(PathBuf),
        Cover(PathBuf),
        Mkdir(PathBuf),
        Keep(PathBuf),
        Seal(PathBuf),
        Bind(PathBuf),
        Chdir(PathBuf),
    }

    fn path(c: &CString) -> PathBuf {
        PathBuf::from(c.to_str().expect("utf-8"))
    }

    /// The steps as shapes; checks that every bind's source is a held descriptor and that each
    /// descriptor is held before any cover.
    fn shapes(layout: Layout) -> (Vec<Shape>, usize) {
        let steps = layout.into_steps();
        let first_cover = steps.iter().position(|(n, _)| *n == COVER);
        let mut holds = Vec::new();
        let shapes = steps
            .into_iter()
            .enumerate()
            .map(|(at, (number, step))| match (number, step) {
                (PRIVATE, Step::Private) => Shape::Private,
                (HOLD, Step::Hold { dir, fd }) => {
                    assert!(
                        first_cover.is_none_or(|cover| at < cover),
                        "held after a cover"
                    );
                    holds.push(format!("{FD}{}", fd.as_raw_fd()));
                    Shape::Hold(path(&dir))
                }
                (COVER, Step::Cover(dir)) => Shape::Cover(path(&dir)),
                (MKDIR, Step::Mkdir(dir)) => Shape::Mkdir(path(&dir)),
                (SEAL, Step::Seal(dir)) => Shape::Seal(path(&dir)),
                (CHDIR, Step::Chdir(dir)) => Shape::Chdir(path(&dir)),
                (number @ (KEEP | BIND), Step::Bind { from, to }) => {
                    let from = from.to_str().expect("utf-8").to_owned();
                    assert!(holds.contains(&from), "{from} is not held");
                    if number == KEEP {
                        Shape::Keep(path(&to))
                    } else {
                        Shape::Bind(path(&to))
                    }
                }
                (number, step) => panic!("step {number} {step:?}"),
            })
            .collect();
        (shapes, holds.len())
    }

    /// The system paths this host has as directories, each with the run's directory bound there.
    fn bound_here(private: &Path) -> Vec<(PathBuf, PathBuf)> {
        BOUND
            .iter()
            .filter_map(|(sub, target)| {
                std::fs::canonicalize(target)
                    .ok()
                    .filter(|t| t.is_dir())
                    .map(|t| (private.join(sub), t))
            })
            .collect()
    }

    fn on_the_way(dir: &Path) -> Vec<Shape> {
        let mut dirs: Vec<&Path> = dir.ancestors().collect();
        dirs.reverse();
        dirs.into_iter()
            .skip(1)
            .map(|d| Shape::Mkdir(d.to_owned()))
            .collect()
    }

    /// Catches the steps of a run's view changing or going out of order: every mount private
    /// first; the sources held before any cover; each hidden directory that exists covered (an
    /// absent one and `/` left alone); the input root and `private/` made and bound again inside
    /// the cover that holds them, before it is sealed; the run's directories bound at the
    /// system's temporary paths; the two bound again where one of those paths holds them (the
    /// test's scratch is under the system temporary directory); the working directory last.
    #[test]
    fn a_runs_view_is_these_steps_in_this_order() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-task-layout");
        let scratch = std::fs::canonicalize(&*dir).unwrap();
        let (root, private) = (scratch.join("in"), scratch.join("private"));
        for sub in ["in/w", "private"] {
            std::fs::create_dir_all(scratch.join(sub)).unwrap();
        }
        for (sub, _) in BOUND {
            std::fs::create_dir(private.join(sub)).unwrap();
        }
        let hidden = [scratch.join("absent"), scratch.clone(), PathBuf::from("/")];
        let layout = layout(&hidden, &private, &root, &root.join("w")).unwrap();
        let (got, held) = shapes(layout);

        let bound = bound_here(&private);
        let under_bound = |d: &Path| bound.iter().any(|(_, target)| d.starts_with(target));
        let mut want = vec![Shape::Private];
        // Held: the two kept for the cover, the bound sources, then the two kept again where
        // a bound path holds them.
        let mut holds = vec![Shape::Hold(root.clone()), Shape::Hold(private.clone())];
        holds.extend(bound.iter().map(|(from, _)| Shape::Hold(from.clone())));
        for keep in [&root, &private] {
            if under_bound(keep) {
                holds.push(Shape::Hold(keep.clone()));
            }
        }
        assert_eq!(held, holds.len());
        want.extend(holds);
        want.push(Shape::Cover(scratch.clone()));
        for keep in [&root, &private] {
            want.extend(on_the_way(keep));
            want.push(Shape::Keep(keep.clone()));
        }
        want.push(Shape::Seal(scratch.clone()));
        want.extend(bound.iter().map(|(_, target)| Shape::Bind(target.clone())));
        for keep in [&root, &private] {
            if under_bound(keep) {
                want.extend(on_the_way(keep));
                want.push(Shape::Keep(keep.clone()));
            }
        }
        want.push(Shape::Chdir(root.join("w")));
        assert_eq!(got, want);
    }

    /// Catches a run that hides nothing still making covers, or binding again what nothing
    /// hides: with no hidden directory and the run outside every bound path, the view is the
    /// bound paths alone.
    #[test]
    fn with_nothing_hidden_only_the_temporary_paths_are_bound() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-task-layout-bare");
        let scratch = std::fs::canonicalize(&*dir).unwrap();
        let private = scratch.join("private");
        std::fs::create_dir(&private).unwrap();
        for (sub, _) in BOUND {
            std::fs::create_dir(private.join(sub)).unwrap();
        }
        let (got, _) = shapes(layout(&[], &private, &scratch, &scratch).unwrap());
        assert_eq!(got.first(), Some(&Shape::Private));
        assert!(
            !got.iter()
                .any(|s| matches!(s, Shape::Cover(_) | Shape::Seal(_)))
        );
        let binds = got.iter().filter(|s| matches!(s, Shape::Bind(_))).count();
        assert_eq!(binds, bound_here(&private).len());
        assert_eq!(got.last(), Some(&Shape::Chdir(scratch.clone())));
    }

    /// Catches the run's directories bound again where nothing hides them, a bound target that
    /// is not a directory being bound, or the binds going anywhere but their targets: with the
    /// targets elsewhere than the run, and one of them absent, the view is exactly the cover, the
    /// two kept in it, the seal and the one bind.
    #[test]
    fn a_run_outside_every_bound_path_is_bound_again_only_in_its_cover() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-task-layout-own");
        let base = std::fs::canonicalize(&*dir).unwrap();
        let (scratch, elsewhere) = (base.join("scratch"), base.join("elsewhere"));
        let (root, private) = (scratch.join("in"), scratch.join("private"));
        for d in [&root, &private, &elsewhere] {
            std::fs::create_dir_all(d).unwrap();
        }
        let target = elsewhere.to_str().unwrap().to_owned();
        let absent = base.join("absent");
        let bound = [("tmp", target.as_str()), ("run", absent.to_str().unwrap())];
        let layout = layout_with(
            &bound,
            std::slice::from_ref(&scratch),
            &private,
            &root,
            &root,
        )
        .unwrap();
        let (got, held) = shapes(layout);
        let mut want = vec![
            Shape::Private,
            Shape::Hold(root.clone()),
            Shape::Hold(private.clone()),
            Shape::Hold(private.join("tmp")),
            Shape::Cover(scratch.clone()),
        ];
        for keep in [&root, &private] {
            want.extend(on_the_way(keep));
            want.push(Shape::Keep(keep.clone()));
        }
        want.extend([
            Shape::Seal(scratch.clone()),
            Shape::Bind(elsewhere.clone()),
            Shape::Chdir(root.clone()),
        ]);
        assert_eq!(got, want);
        assert_eq!(held, 3);
    }

    /// Catches a layout made for a run whose directories do not exist, or that names a path the
    /// child cannot carry: each is refused before anything starts.
    #[test]
    fn a_missing_directory_or_a_nul_in_a_path_is_refused() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-task-layout-bad");
        let missing = dir.join("missing");
        let Err(error) = layout(&[], &missing, &dir, &dir) else {
            panic!("a missing private directory was accepted");
        };
        assert!(matches!(error, RunError::Io { .. }), "{error}");
        let Err(error) = layout(&[], &dir, &missing, &dir) else {
            panic!("a missing root was accepted");
        };
        assert!(matches!(error, RunError::Io { .. }), "{error}");
        let nul = PathBuf::from(
            <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(b"/a\0b".to_vec()),
        );
        let Err(error) = layout(&[], &dir, &dir, &nul) else {
            panic!("a NUL in the working directory was accepted");
        };
        assert!(matches!(error, RunError::Invalid(_)), "{error}");
    }
}
