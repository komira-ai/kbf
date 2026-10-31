//! The live directories guard: a run writes nowhere near the live tierstore.
//!
//! The ops file lists the live tierstore's state and data directories (`live.dirs`). The result
//! directory, the campaign ledger and the ledger's lock file are written by `run`, and none of
//! them may be, contain, or lie inside one of those directories. The comparison is of resolved
//! paths: relative paths are made absolute, `..` is applied, and links are followed (also a link
//! whose target does not exist yet, and a path that does not exist yet, whose existing part is
//! resolved and the rest appended), so a link or a `..` cannot hide a live directory.

use std::path::{Component, Path, PathBuf};

use crate::error::BenchError;

/// Links followed along one path before the tool gives up resolving it further.
const MAX_LINKS: u32 = 32;

/// `path` made absolute, with `..` applied and every link followed. A part of the path that does
/// not exist is kept as written (after the parts that do).
#[must_use]
pub fn resolve(path: &Path) -> PathBuf {
    resolve_following(path, MAX_LINKS)
}

fn resolve_following(path: &Path, links_left: u32) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut out = PathBuf::new();
    // `absolute` leaves no `.`; the root and every name are pushed, `..` goes back one.
    for component in abs.components() {
        if component == Component::ParentDir {
            out.pop();
            continue;
        }
        out.push(component.as_os_str());
        if let Ok(real) = out.canonicalize() {
            out = real;
        } else if let (true, Ok(target)) = (links_left > 0, std::fs::read_link(&out)) {
            // A link to something that is not there (yet): writing through it makes it.
            let parent = out.parent().map(Path::to_owned).unwrap_or_default();
            out = resolve_following(&parent.join(target), links_left - 1);
        }
    }
    out
}

/// How `path` stands to the live directory `live`, if they overlap.
fn overlap(path: &Path, live: &Path) -> Option<&'static str> {
    if path == live {
        Some("is")
    } else if path.starts_with(live) {
        Some("is inside")
    } else if live.starts_with(path) {
        Some("contains")
    } else {
        None
    }
}

/// Refuses a `paths` entry (a name for the message, and the path) that is, contains or is inside
/// one of the `live` directories, after resolving both.
///
/// # Errors
/// [`BenchError::LiveDir`] naming the path, how it overlaps and the live directory; also when
/// `live` is empty, since a guard with nothing to guard against is not one.
pub fn check_outside(paths: &[(&str, &Path)], live: &[PathBuf]) -> Result<(), BenchError> {
    if live.is_empty() {
        return Err(BenchError::LiveDir(
            "the ops file lists no live directory (live.dirs): the guard needs at least one"
                .to_owned(),
        ));
    }
    let live: Vec<(&PathBuf, PathBuf)> = live.iter().map(|dir| (dir, resolve(dir))).collect();
    for (what, path) in paths {
        let real = resolve(path);
        for (named, resolved) in &live {
            if let Some(how) = overlap(&real, resolved) {
                return Err(BenchError::LiveDir(format!(
                    "the {what} {} {how} the live directory {} (resolved to {})",
                    path.display(),
                    named.display(),
                    resolved.display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use kbf_testkit::dir::TestDir;

    use super::*;

    fn live_of(dir: &Path) -> (PathBuf, Vec<PathBuf>) {
        let live = dir.join("live");
        std::fs::create_dir_all(live.join("data")).unwrap();
        std::fs::create_dir_all(dir.join("work")).unwrap();
        (dir.to_owned(), vec![live])
    }

    fn refused(paths: &[(&str, &Path)], live: &[PathBuf]) -> String {
        check_outside(paths, live).unwrap_err().to_string()
    }

    /// Catches a guard that refuses a directory that has nothing to do with a live one, or one
    /// whose name only starts like a live one (`live-2` beside `live`).
    #[test]
    fn a_path_that_is_elsewhere_is_allowed() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-livedir-elsewhere");
        let (root, live) = live_of(dir.path());
        std::fs::create_dir_all(root.join("live-2")).unwrap();
        let ok = [
            ("result directory", root.join("work")),
            ("campaign ledger", root.join("live-2").join("ledger.json")),
            ("ledger lock", root.join("not-yet").join("ledger.json.lock")),
        ];
        let paths: Vec<(&str, &Path)> = ok.iter().map(|(w, p)| (*w, p.as_path())).collect();
        assert!(check_outside(&paths, &live).is_ok());
    }

    /// Catches each way of overlapping that the guard must see: the live directory itself, a
    /// place inside it (one that exists, one that does not exist yet), and a directory that
    /// contains it.
    #[test]
    fn a_path_that_is_contains_or_is_inside_a_live_directory_is_refused() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-livedir-overlap");
        let (root, live) = live_of(dir.path());
        let same = refused(&[("result directory", &root.join("live"))], &live);
        assert!(same.contains("result directory"), "{same}");
        assert!(same.contains(" is the live"), "{same}");
        let inside = refused(&[("campaign ledger", &root.join("live/data"))], &live);
        assert!(inside.contains("is inside the live directory"), "{inside}");
        let absent = refused(
            &[("campaign ledger", &root.join("live/new/ledger.json"))],
            &live,
        );
        assert!(absent.contains("is inside"), "{absent}");
        let above = refused(&[("result directory", &root)], &live);
        assert!(above.contains("contains the live directory"), "{above}");
        let lock = refused(
            &[("ledger lock", &root.join("live/ledger.json.lock"))],
            &live,
        );
        assert!(lock.starts_with("the ledger lock"), "{lock}");
    }

    /// Catches a guard that compares the text of the paths: `..` going out and back in, a
    /// relative path, and a live directory named through `..` are all the same place.
    #[test]
    fn dot_dot_and_relative_paths_are_resolved() {
        let dir = TestDir::under(std::env::temp_dir(), "tb-livedir-dotdot");
        let (root, live) = live_of(dir.path());
        let sneaky = root.join("work/../live/data");
        assert!(refused(&[("result directory", &sneaky)], &live).contains("is inside"));
        let named = vec![root.join("work/../live")];
        assert!(
            refused(&[("result directory", &root.join("live/data"))], &named).contains("is inside")
        );
        let mut relative = PathBuf::new();
        for _ in 0..30 {
            relative.push("..");
        }
        let relative = relative
            .join(root.strip_prefix("/").unwrap())
            .join("live/data");
        assert_eq!(resolve(&relative), resolve(&root.join("live/data")));
        // Not a live directory once the `..` has taken the path out of it.
        let out = root.join("live/../work/x");
        assert!(check_outside(&[("result directory", &out)], &live).is_ok());
    }

    /// Catches a link that hides a live directory: a link to it, a link inside a path that
    /// continues below it, and a link whose target is not there yet.
    #[cfg(unix)]
    #[test]
    fn links_are_followed() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::under(std::env::temp_dir(), "tb-livedir-links");
        let (root, live) = live_of(dir.path());
        symlink(root.join("live"), root.join("work/to-live")).unwrap();
        let through = refused(&[("result directory", &root.join("work/to-live"))], &live);
        assert!(through.contains("is the live"), "{through}");
        let below = refused(
            &[("campaign ledger", &root.join("work/to-live/data/l.json"))],
            &live,
        );
        assert!(below.contains("is inside"), "{below}");
        // The link points into the live directory at a place that is not there yet.
        symlink(root.join("live/later"), root.join("work/dangling")).unwrap();
        let dangling = refused(&[("result directory", &root.join("work/dangling"))], &live);
        assert!(dangling.contains("is inside"), "{dangling}");
        // A link named as the live directory resolves to the same place.
        symlink(root.join("live"), root.join("work/live-link")).unwrap();
        let by_link = vec![root.join("work/live-link")];
        assert!(
            refused(&[("result directory", &root.join("live/data"))], &by_link)
                .contains("is inside")
        );
        // A link to somewhere else is no overlap.
        symlink(root.join("work"), root.join("work/self")).unwrap();
        assert!(check_outside(&[("result directory", &root.join("work/self"))], &live).is_ok());
    }

    /// Catches a link loop sending the resolver round for ever.
    #[cfg(unix)]
    #[test]
    fn a_link_loop_ends() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::under(std::env::temp_dir(), "tb-livedir-loop");
        let (root, live) = live_of(dir.path());
        symlink(root.join("work/b"), root.join("work/a")).unwrap();
        symlink(root.join("work/a"), root.join("work/b")).unwrap();
        assert!(check_outside(&[("result directory", &root.join("work/a"))], &live).is_ok());
    }

    /// Catches a guard with nothing to guard against passing everything.
    #[test]
    fn an_empty_live_list_is_refused() {
        let err = refused(&[("result directory", Path::new("/x"))], &[]);
        assert!(err.contains("live.dirs"), "{err}");
    }
}
