//! Packages of the base image left out of the kbf base tree (the list is `super::removed`),
//! taken out of the tree as `dpkg --purge` takes them out of a system, with no maintainer script
//! run; and paths of kept packages left out as dpkg's `path-exclude` leaves them out: the file
//! goes and the package's file list still names it (the minimal image leaves out its manuals and
//! most of its documentation that way).
//!
//! The image's dpkg database says what each package brings, so the removal is a function of the
//! pinned base and the list alone:
//!
//! - every path of the package's file list (`/var/lib/dpkg/info/<package>[:<arch>].list`) goes:
//!   a file or a symbolic link always, a directory only when nothing is left in it and no kept
//!   package lists it;
//! - its control files (`/var/lib/dpkg/info/<package>[:<arch>].<kind>`) go, its stanza leaves
//!   `/var/lib/dpkg/status`, and its lines leave `/var/lib/dpkg/diversions`;
//! - an alternative (`/var/lib/dpkg/alternatives/<name>`) whose every choice was a path of a
//!   removed package goes with its links (`/etc/alternatives/<name>`, the link it names, and the
//!   same for each of its followers);
//! - a symbolic link no package lists that pointed at a path the removal takes (at it or below
//!   it) goes too, and so on until none is left: a removed package's maintainer scripts made it
//!   (an enabled unit, such as apt's timers). A link that pointed at nothing already stays.
//!
//! What would leave the tree inconsistent is refused by name, and nothing is removed: a package
//! the base does not hold or whose file list is missing, a kept package that depends (`Depends`,
//! `Pre-Depends`) on a removed one with no kept alternative, a listed path below a symbolic
//! link, an alternative with a choice in a removed package and one in a kept package, a
//! symbolic link a kept package lists left pointing at or below a removed path, a path to leave
//! out that is not absolute, that no kept package lists, that is not there or that is a
//! directory, and a database file that cannot be read as dpkg writes it.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use super::{Entry, Rootfs};

/// The dpkg database, below the root.
const DPKG: [&str; 3] = ["var", "lib", "dpkg"];

/// The most symbolic links followed resolving one path, as the kernel's limit.
const HOPS: usize = 40;

/// A path below the root as its names.
type Path = Vec<String>;

/// `/a/b` of `path`.
fn shown(path: &[String]) -> String {
    path.iter().map(|p| format!("/{p}")).collect()
}

/// The names of `/var/lib/dpkg/<name>`.
fn dpkg(name: &str) -> Path {
    DPKG.iter()
        .map(|p| (*p).to_owned())
        .chain([name.to_owned()])
        .collect()
}

/// The names of the absolute path `path` (dpkg writes them as they are, so `/.` is the one name
/// `.`, which no entry has): `None` when it is not absolute.
fn absolute(path: &str) -> Option<Path> {
    let rest = path.strip_prefix('/')?;
    Some(rest.split('/').map(str::to_owned).collect())
}

/// The entry at `path`, not following a symbolic link.
fn at<'a>(root: &'a BTreeMap<String, Entry>, path: &[String]) -> Option<&'a Entry> {
    let (last, dirs) = path.split_last()?;
    let mut children = root;
    for part in dirs {
        let Entry::Dir { children: next, .. } = children.get(part)? else {
            return None;
        };
        children = next;
    }
    children.get(last)
}

/// What the directory at `path` holds; `None` when no directory is there.
fn dir_at<'a>(
    root: &'a BTreeMap<String, Entry>,
    path: &[String],
) -> Option<&'a BTreeMap<String, Entry>> {
    match at(root, path)? {
        Entry::Dir { children, .. } => Some(children),
        _ => None,
    }
}

/// The path `target`, read from the directory `dir`, with every symbolic link above its last
/// name followed in `root` (the last name is not followed); `None` past [`HOPS`] links.
fn canonical(root: &BTreeMap<String, Entry>, dir: &[String], target: &str) -> Option<Path> {
    let mut path: Path = if target.starts_with('/') {
        Vec::new()
    } else {
        dir.to_vec()
    };
    let names = |t: &str| -> Vec<String> {
        t.split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .rev()
            .map(str::to_owned)
            .collect()
    };
    let mut todo = names(target);
    let mut hops = 0;
    while let Some(part) = todo.pop() {
        if part == ".." {
            path.pop();
            continue;
        }
        path.push(part);
        if todo.is_empty() {
            break;
        }
        if let Some(Entry::Symlink { target }) = at(root, &path) {
            hops += 1;
            if hops > HOPS {
                return None;
            }
            path.pop();
            if target.starts_with('/') {
                path.clear();
            }
            todo.extend(names(target));
        }
    }
    Some(path)
}

/// Whether `path` or a directory above it is in `gone`.
fn within(gone: &BTreeSet<Path>, path: &[String]) -> bool {
    (1..=path.len()).any(|n| gone.contains(&path[..n]))
}

/// One stanza of `/var/lib/dpkg/status`: its text, the package, and what it needs and gives.
struct Stanza {
    text: String,
    package: String,
    /// Each `Depends` and `Pre-Depends` group: its text, and the names of its alternatives.
    needs: Vec<(String, Vec<String>)>,
    provides: Vec<String>,
}

/// The package name of the relation `relation` (`name:arch (>= 1)`, `name`).
fn relation_name(relation: &str) -> String {
    relation
        .trim()
        .split([' ', '(', ':'])
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The stanzas of the status file `text`, or why it is not one dpkg wrote.
fn stanzas(text: &str) -> Result<Vec<Stanza>, String> {
    let mut out = Vec::new();
    for block in text.split("\n\n").map(|b| b.trim_matches('\n')) {
        if block.is_empty() {
            continue;
        }
        let mut fields: BTreeMap<&str, String> = BTreeMap::new();
        let mut last = None;
        for line in block.lines() {
            if line.starts_with([' ', '\t']) {
                let Some(name) = last else {
                    return Err(format!("a continuation line before any field: {line:?}"));
                };
                let value: &mut String = fields.entry(name).or_default();
                value.push(' ');
                value.push_str(line.trim());
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                return Err(format!("a line that is not a field: {line:?}"));
            };
            fields.insert(name, value.trim().to_owned());
            last = Some(name);
        }
        let Some(package) = fields.get("Package").cloned() else {
            return Err(format!("a stanza with no Package field: {block:?}"));
        };
        let needs = ["Pre-Depends", "Depends"]
            .iter()
            .filter_map(|f| fields.get(f))
            .flat_map(|v| v.split(','))
            .map(|group| {
                let names = group.split('|').map(relation_name).collect();
                (group.trim().to_owned(), names)
            })
            .collect();
        let provides = fields
            .get("Provides")
            .map(|v| v.split(',').map(relation_name).collect())
            .unwrap_or_default();
        out.push(Stanza {
            text: block.to_owned(),
            package,
            needs,
            provides,
        });
    }
    Ok(out)
}

/// The package a file of `/var/lib/dpkg/info` belongs to (`<package>[:<arch>].<kind>`); `None`
/// for one of no package (`format`).
fn info_package(file: &str) -> Option<&str> {
    let (stem, _kind) = file.rsplit_once('.')?;
    stem.split(':').next()
}

/// One link group of `/var/lib/dpkg/alternatives/<name>`: the links, each with the name of its
/// `/etc/alternatives` entry (the master's first), and the path of each choice.
struct Alternative {
    links: Vec<(String, Path)>,
    choices: Vec<String>,
}

/// The alternative `name` from its file's text, or why it is not one dpkg wrote.
fn alternative(name: &str, text: &str) -> Result<Alternative, String> {
    let mut lines = text.lines();
    let bad = |why: &str| format!("/var/lib/dpkg/alternatives/{name}: {why}");
    lines.next().ok_or_else(|| bad("empty"))?;
    let master = lines
        .next()
        .and_then(absolute)
        .ok_or_else(|| bad("no absolute master link"))?;
    let mut links = vec![(name.to_owned(), master)];
    loop {
        let follower = lines
            .next()
            .ok_or_else(|| bad("the followers do not end"))?;
        if follower.is_empty() {
            break;
        }
        let link = lines
            .next()
            .and_then(absolute)
            .ok_or_else(|| bad("a follower with no absolute link"))?;
        links.push((follower.to_owned(), link));
    }
    let mut choices = Vec::new();
    while let Some(choice) = lines.next().filter(|l| !l.is_empty()) {
        choices.push(choice.to_owned());
        // The priority, then one path (or an empty line) per follower.
        for _ in 0..links.len() {
            lines
                .next()
                .ok_or_else(|| bad("a choice that does not end"))?;
        }
    }
    Ok(Alternative { links, choices })
}

/// The mode and text of `entry`, the entry at `path`, or why it cannot be read as a text file.
fn text_of(path: &[String], entry: &Entry) -> Result<(u32, String), String> {
    match entry {
        Entry::File { mode, bytes } => std::str::from_utf8(bytes)
            .map(|t| (*mode, t.to_owned()))
            .map_err(|_| format!("{}: not UTF-8", shown(path))),
        _ => Err(format!("{}: not a regular file", shown(path))),
    }
}

/// The mode and text of the file at `path`, `None` when there is none, or why it cannot be read.
fn text_at(
    root: &BTreeMap<String, Entry>,
    path: &[String],
) -> Result<Option<(u32, String)>, String> {
    at(root, path).map(|e| text_of(path, e)).transpose()
}

/// What the removal of a list of packages does to the tree, worked out before any of it is done.
#[derive(Default)]
struct Plan {
    /// Every path that goes, deepest first.
    gone: Vec<Path>,
    /// The new mode and text of the status and diversions files, by name in `/var/lib/dpkg`.
    rewrites: Vec<(&'static str, u32, String)>,
}

impl Rootfs {
    /// Leaves the packages `removed` and the paths `excluded` of kept packages out of the tree
    /// (the module's doc says how); a problem is added to `problems` for each thing that would
    /// leave it inconsistent, and then nothing is removed. Nothing to leave out changes nothing
    /// and reads nothing.
    pub(in crate::base_tree) fn remove_packages(
        &mut self,
        removed: &[&str],
        excluded: &[&str],
        problems: &mut Vec<String>,
    ) {
        if removed.is_empty() && excluded.is_empty() {
            return;
        }
        match self.plan(removed, excluded) {
            Ok(plan) => {
                for path in &plan.gone {
                    self.remove(path);
                }
                let dir: Path = DPKG.iter().map(|p| (*p).to_owned()).collect();
                for (name, mode, text) in plan.rewrites {
                    let bytes = Rc::from(text.into_bytes());
                    super::insert(&mut self.root, &dir, name, Entry::File { mode, bytes });
                }
            }
            Err(found) => problems.extend(found),
        }
    }

    /// The plan of the removal of `removed` and `excluded`, or every problem with it.
    fn plan(&self, removed: &[&str], excluded: &[&str]) -> Result<Plan, Vec<String>> {
        let mut problems = Vec::new();
        let status_path = dpkg("status");
        let (status_mode, status) = match text_at(&self.root, &status_path) {
            Ok(Some(file)) => file,
            Ok(None) => {
                return Err(vec![format!(
                    "/var/lib/dpkg/status: none, so the packages to leave out ({}) are not known",
                    removed.join(", ")
                )]);
            }
            Err(why) => return Err(vec![why]),
        };
        let stanzas =
            stanzas(&status).map_err(|why| vec![format!("/var/lib/dpkg/status: {why}")])?;
        let gone_packages: BTreeSet<&str> = removed.iter().copied().collect();
        let held: BTreeSet<&str> = stanzas.iter().map(|s| s.package.as_str()).collect();
        for package in &gone_packages {
            if !held.contains(package) {
                problems.push(format!(
                    "{package}: not a package of the base (/var/lib/dpkg/status does not list it)"
                ));
            }
        }
        let (kept, left): (Vec<&Stanza>, Vec<&Stanza>) = stanzas
            .iter()
            .partition(|s| !gone_packages.contains(s.package.as_str()));
        // What a package satisfies: its own name and each name it provides.
        let gives = |of: &[&Stanza]| -> BTreeSet<String> {
            of.iter()
                .flat_map(|s| std::iter::once(&s.package).chain(&s.provides))
                .cloned()
                .collect()
        };
        let (given, taken) = (gives(&kept), gives(&left));
        for stanza in &kept {
            for (group, names) in &stanza.needs {
                let lost = names.iter().any(|n| taken.contains(n));
                if lost && !names.iter().any(|n| given.contains(n)) {
                    problems.push(format!(
                        "{}: needs {group}, which only packages left out give",
                        stanza.package
                    ));
                }
            }
        }
        let lists = self.lists(&mut problems);
        let mut listed_by_kept: BTreeSet<Path> = BTreeSet::new();
        let mut listed_by_gone: BTreeMap<Path, &str> = BTreeMap::new();
        for (package, paths) in &lists {
            for path in paths {
                if gone_packages.contains(package.as_str()) {
                    listed_by_gone.insert(path.clone(), package);
                } else {
                    listed_by_kept.insert(path.clone());
                }
            }
        }
        for package in &gone_packages {
            if held.contains(package) && !lists.contains_key(*package) {
                problems.push(format!(
                    "{package}: no file list (/var/lib/dpkg/info/{package}.list or {package}:<arch>.list)"
                ));
            }
        }
        let mut plan = Plan::default();
        let mut gone: BTreeSet<Path> = BTreeSet::new();
        // Deepest first, so a directory is looked at after what it held.
        for (path, package) in listed_by_gone.iter().rev() {
            if let Some(above) = (1..path.len())
                .find(|n| matches!(at(&self.root, &path[..*n]), Some(Entry::Symlink { .. })))
            {
                problems.push(format!(
                    "{}: listed by {package} below the symbolic link {}, which the removal does not follow",
                    shown(path),
                    shown(&path[..above])
                ));
                continue;
            }
            let goes = match at(&self.root, path) {
                None => false,
                Some(Entry::Dir { children, .. }) => {
                    !listed_by_kept.contains(path)
                        && children
                            .keys()
                            .all(|name| gone.contains(&[path.clone(), vec![name.clone()]].concat()))
                }
                Some(_) => true,
            };
            if goes {
                gone.insert(path.clone());
            }
        }
        let info = dpkg("info");
        for name in dir_at(&self.root, &info)
            .into_iter()
            .flat_map(BTreeMap::keys)
        {
            if info_package(name).is_some_and(|p| gone_packages.contains(p)) {
                gone.insert([info.clone(), vec![name.clone()]].concat());
            }
        }
        for wanted in excluded {
            problems.extend(self.exclude(wanted, &listed_by_kept, &mut gone));
        }
        self.alternatives(&mut gone, &mut problems);
        loop {
            let (_, leftovers) = self.dangling(&gone, &listed_by_kept);
            if leftovers.is_empty() {
                break;
            }
            gone.extend(leftovers);
        }
        problems.extend(self.dangling(&gone, &listed_by_kept).0);
        let text: String = stanzas
            .iter()
            .filter(|s| !gone_packages.contains(s.package.as_str()))
            .map(|s| format!("{}\n\n", s.text))
            .collect();
        plan.rewrites.push(("status", status_mode, text));
        let diversions = dpkg("diversions");
        match text_at(&self.root, &diversions) {
            Ok(None) => {}
            Ok(Some((mode, text))) => {
                let lines: Vec<&str> = text.lines().collect();
                if lines.len().is_multiple_of(3) {
                    let kept: String = lines
                        .chunks(3)
                        .filter(|d| !gone_packages.contains(d[2]))
                        .map(|d| format!("{}\n{}\n{}\n", d[0], d[1], d[2]))
                        .collect();
                    plan.rewrites.push(("diversions", mode, kept));
                } else {
                    problems.push(format!(
                        "/var/lib/dpkg/diversions: {} lines, not three for each diversion",
                        lines.len()
                    ));
                }
            }
            Err(why) => problems.push(why),
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        plan.gone = gone.into_iter().rev().collect();
        Ok(plan)
    }

    /// Adds the path `wanted` of a kept package to `gone`, as dpkg's `path-exclude` leaves a path
    /// out (the file goes, the package's lists still name it); or what is wrong with it: not an
    /// absolute path, listed by no kept package (`listed`), not in the base, or a directory.
    fn exclude(
        &self,
        wanted: &str,
        listed: &BTreeSet<Path>,
        gone: &mut BTreeSet<Path>,
    ) -> Option<String> {
        let Some(path) = absolute(wanted) else {
            return Some(format!("{wanted}: not an absolute path"));
        };
        if !listed.contains(&path) {
            return Some(format!("{wanted}: no kept package lists it"));
        }
        match at(&self.root, &path) {
            None => Some(format!("{wanted}: not in the base")),
            Some(Entry::Dir { .. }) => Some(format!("{wanted}: a directory, not a file")),
            Some(_) => {
                gone.insert(path);
                None
            }
        }
    }

    /// Each package's file list, by package: the paths it names (`/.` left out); a list that
    /// cannot be read is a problem.
    fn lists(&self, problems: &mut Vec<String>) -> BTreeMap<String, Vec<Path>> {
        let info = dpkg("info");
        let mut lists: BTreeMap<String, Vec<Path>> = BTreeMap::new();
        for (name, entry) in dir_at(&self.root, &info).into_iter().flatten() {
            let Some(stem) = name.strip_suffix(".list") else {
                continue;
            };
            let package = stem.split(':').next().unwrap_or_default();
            let path = [info.clone(), vec![name.clone()]].concat();
            match text_of(&path, entry) {
                Ok((_, text)) => lists
                    .entry(package.to_owned())
                    .or_default()
                    .extend(text.lines().filter_map(absolute)),
                Err(why) => problems.push(why),
            }
        }
        lists
    }

    /// Adds to `gone` each alternative whose every choice is gone, with its links; one with a
    /// choice gone and one kept is a problem.
    fn alternatives(&self, gone: &mut BTreeSet<Path>, problems: &mut Vec<String>) {
        let dir = dpkg("alternatives");
        for (name, entry) in dir_at(&self.root, &dir).into_iter().flatten() {
            let path = [dir.clone(), vec![name.clone()]].concat();
            let group = match text_of(&path, entry).and_then(|(_, t)| alternative(name, &t)) {
                Ok(group) => group,
                Err(why) => {
                    problems.push(why);
                    continue;
                }
            };
            let lost = |choice: &String| {
                canonical(&self.root, &[], choice).is_some_and(|p| within(gone, &p))
            };
            let lost_count = group.choices.iter().filter(|c| lost(c)).count();
            if lost_count == 0 {
                continue;
            }
            if lost_count < group.choices.len() {
                problems.push(format!(
                    "{}: the alternative {name} keeps a choice of a kept package and loses one of a package left out",
                    shown(&path)
                ));
                continue;
            }
            gone.insert(path);
            for (link_name, link) in group.links {
                gone.insert(vec!["etc".to_owned(), "alternatives".to_owned(), link_name]);
                gone.insert(link);
            }
        }
    }

    /// The symbolic links that stay and that the removal of `gone` would leave pointing at a
    /// path that was there before (at it or below it): those a kept package lists, as problems,
    /// and the others, which a removed package's maintainer scripts made (an enabled unit, an
    /// alternative), as more paths to remove. A link that pointed at nothing already is kept.
    fn dangling(&self, gone: &BTreeSet<Path>, kept: &BTreeSet<Path>) -> (Vec<String>, Vec<Path>) {
        let mut problems = Vec::new();
        let mut leftovers = Vec::new();
        let mut todo: Vec<(Path, &BTreeMap<String, Entry>)> = vec![(Vec::new(), &self.root)];
        while let Some((dir, children)) = todo.pop() {
            for (name, entry) in children {
                let path = [dir.clone(), vec![name.clone()]].concat();
                if within(gone, &path) {
                    continue;
                }
                match entry {
                    Entry::Dir { children, .. } => todo.push((path, children)),
                    Entry::Symlink { target } => {
                        let lost = canonical(&self.root, &dir, target)
                            .is_some_and(|t| at(&self.root, &t).is_some() && within(gone, &t));
                        if !lost {
                            continue;
                        }
                        if kept.contains(&path) {
                            problems.push(format!(
                                "{}: a symbolic link a kept package lists, to {target}, which the packages left out remove",
                                shown(&path)
                            ));
                        } else {
                            leftovers.push(path);
                        }
                    }
                    Entry::File { .. } => {}
                }
            }
        }
        (problems, leftovers)
    }

    /// Removes the entry at `path`, whatever it is; nothing when its directory is not there.
    fn remove(&mut self, path: &[String]) {
        let place = path
            .split_last()
            .and_then(|(last, dirs)| super::dir_mut(&mut self.root, dirs).map(|c| (last, c)));
        if let Some((last, children)) = place {
            children.remove(last);
        }
    }

    /// Each set-uid or set-gid file of the tree, as a problem: the kbf base tree holds none.
    pub(in crate::base_tree) fn set_id_files(&self) -> Vec<String> {
        let mut found = Vec::new();
        let mut todo: Vec<(Path, &BTreeMap<String, Entry>)> = vec![(Vec::new(), &self.root)];
        while let Some((dir, children)) = todo.pop() {
            for (name, entry) in children {
                let path = [dir.clone(), vec![name.clone()]].concat();
                match entry {
                    Entry::Dir { children, .. } => todo.push((path, children)),
                    Entry::File { mode, .. } if mode & 0o6000 != 0 => {
                        let kind = match mode & 0o6000 {
                            0o4000 => "set-uid",
                            0o2000 => "set-gid",
                            _ => "set-uid and set-gid",
                        };
                        let message = format!(
                            "{}: a {kind} file (mode {mode:04o}); the kbf base tree holds none: leave out the package that brings it",
                            shown(&path)
                        );
                        found.push((path, message));
                    }
                    _ => {}
                }
            }
        }
        found.sort();
        found.into_iter().map(|(_, message)| message).collect()
    }
}
