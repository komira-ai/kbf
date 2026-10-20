//! A Mac installed by the Mac kit, as test fixtures: the tree, the LaunchDaemon plist in
//! both argument forms, the daemon's `kbf.toml`, and the kit's settings. The files are in
//! `testdata/kit_host/`; its `README.md` cites the kit for each fact and states the rollback
//! target. This module lays them out under a scratch root and checks them; it changes no
//! behaviour of `migrate`.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

/// The two releases: 40 `a` (previous) and 40 `b` (current). Obviously not commits.
pub const PREVIOUS: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const CURRENT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub const MANIFEST: &str = include_str!("../../testdata/kit_host/tree.manifest");
pub const PLIST_NEW: &str = include_str!("../../testdata/kit_host/launchd_new.plist");
pub const PLIST_OLD: &str = include_str!("../../testdata/kit_host/launchd_old.plist");
pub const KBF_TOML: &str = include_str!("../../testdata/kit_host/kbf.toml");
pub const KIT_CONF: &str = include_str!("../../testdata/kit_host/av2_mac.conf");
pub const README: &str = include_str!("../../testdata/kit_host/README.md");

pub const LABEL: &str = "com.komira.kbf-daemon";
pub const PLIST_PATH: &str = "/Library/LaunchDaemons/com.komira.kbf-daemon.plist";
pub const KBF_TOML_PATH: &str = "/usr/local/etc/kbf/kbf.toml";
pub const TREE: &str = "/usr/local/kbf";
/// What the kit's restart runs (`launchctl kickstart -k`), as arguments.
pub const KICKSTART: [&str; 3] = ["kickstart", "-k", "system/com.komira.kbf-daemon"];

/// Which argument form the host's job is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// `current/kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml`.
    New,
    /// `current/kbf daemon <flags>`.
    Old,
}

impl Form {
    pub fn plist(self) -> &'static str {
        match self {
            Form::New => PLIST_NEW,
            Form::Old => PLIST_OLD,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Link,
}

/// One line of the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    /// `None` for a link.
    pub mode: Option<u32>,
    pub user: String,
    pub group: String,
    /// The path on the host.
    pub path: String,
    /// A link's target, as the link holds it (relative).
    pub target: Option<String>,
    /// The fixture file whose bytes a file has (`@name`); other files get a placeholder.
    pub source: Option<String>,
}

/// The manifest's entries, in an order that makes parents first.
pub fn entries() -> Vec<Entry> {
    parse(MANIFEST)
}

/// `manifest`'s entries (one per non-comment line), in its order.
pub fn parse(manifest: &str) -> Vec<Entry> {
    manifest
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let kind = match f[0] {
                "dir" => Kind::Dir,
                "file" => Kind::File,
                "link" => Kind::Link,
                other => panic!("manifest kind {other:?}"),
            };
            let (user, group) = f[2].split_once(':').expect("owner:group");
            Entry {
                kind,
                mode: (f[1] != "-").then(|| u32::from_str_radix(f[1], 8).expect("octal mode")),
                user: user.to_owned(),
                group: group.to_owned(),
                path: f[3].to_owned(),
                target: (kind == Kind::Link).then(|| f[4].to_owned()),
                source: f
                    .get(4)
                    .filter(|_| kind == Kind::File)
                    .map(|s| s.trim_start_matches('@').to_owned()),
            }
        })
        .collect()
}

/// What a file of the tree holds when no fixture file is named: plainly not a certificate,
/// a key or a program.
pub fn placeholder(path: &str) -> String {
    if path.ends_with(".key") {
        "-----BEGIN PLACEHOLDER KEY-----\nnot a key: a test fixture\n-----END PLACEHOLDER KEY-----\n"
            .to_owned()
    } else if path.ends_with(".pem") {
        "-----BEGIN PLACEHOLDER CERTIFICATE-----\nnot a certificate: a test fixture\n-----END PLACEHOLDER CERTIFICATE-----\n"
            .to_owned()
    } else {
        format!("placeholder for {path}: a test fixture, not a program\n")
    }
}

pub(crate) fn source(name: &str) -> &'static str {
    match name {
        "av2_mac.conf" => KIT_CONF,
        "kbf.toml" => KBF_TOML,
        "launchd_new.plist" => PLIST_NEW,
        other => panic!("no fixture file {other:?}"),
    }
}

/// `host` under `root`.
pub fn at(root: &Path, host: &str) -> PathBuf {
    root.join(host.trim_start_matches('/'))
}

/// Lay the kit host out under `root`, with the job in `form`. Modes are applied; owners are
/// not (a test does not run as root), so a check of an owner reads [`entries`].
pub fn lay_out(root: &Path, form: Form) {
    for e in entries() {
        let path = at(root, &e.path);
        match e.kind {
            Kind::Dir => fs::create_dir_all(&path).expect("directory"),
            Kind::File => {
                fs::create_dir_all(path.parent().expect("parent")).expect("parent");
                let text = match &e.source {
                    // The job is the one fixture that depends on the form.
                    Some(_) if e.path == PLIST_PATH => form.plist().to_owned(),
                    Some(name) => source(name).to_owned(),
                    None => placeholder(&e.path),
                };
                fs::write(&path, text).expect("file");
            }
            Kind::Link => {
                fs::create_dir_all(path.parent().expect("parent")).expect("parent");
                symlink(e.target.as_deref().expect("target"), &path).expect("link");
            }
        }
    }
    // Modes last, deepest first: a 0400 key and a 0700 directory are set after what is in them.
    let mut modes: Vec<Entry> = entries().into_iter().filter(|e| e.mode.is_some()).collect();
    modes.sort_by_key(|e| std::cmp::Reverse(e.path.len()));
    for e in modes {
        let mode = e.mode.expect("mode");
        fs::set_permissions(at(root, &e.path), fs::Permissions::from_mode(mode)).expect("mode");
    }
}

/// The `<string>` values of the plist's `ProgramArguments`, in order.
pub fn program_arguments(plist: &str) -> Vec<String> {
    let start = plist.find("<array>").expect("an array") + "<array>".len();
    let end = plist.find("</array>").expect("an array end");
    plist[start..end]
        .lines()
        .filter_map(|l| l.trim().strip_prefix("<string>")?.strip_suffix("</string>"))
        .map(str::to_owned)
        .collect()
}

/// The `<string>` that follows `<key>name</key>` in the plist.
pub fn plist_string(plist: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = plist.lines().map(str::trim).collect();
    let at = lines
        .iter()
        .position(|l| *l == format!("<key>{name}</key>"))?;
    lines
        .get(at + 1)?
        .strip_prefix("<string>")?
        .strip_suffix("</string>")
        .map(str::to_owned)
}
