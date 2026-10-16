//! An action's shape: what stays the same across builds of one target's action, read
//! from its `Command` alone, and a short key over it ([`ShapeKey`]).
//!
//! buck2 sends no per-action identity (its `RequestMetadata` names none), and the
//! action digest changes with every input, so the shape is built from two things a
//! client cannot leave out: the program it runs and the paths it writes. Each is made
//! independent of the configuration and of the content hashes buck2 puts in paths:
//!
//! - **Output paths** (`pattern`): under `buck-out/<v>/<root>/` (`art`, `art-anon`,
//!   `art-bxl`, and the `gen` kinds of older buck2) everything up to the root goes,
//!   and with it the hashes buck2 puts where its owner's layout says:
//!   - a target's configuration hash after the cell (`<16 hex>` or
//!     `<16 hex>-<16 hex>`, kept aside as `cfg`), and the content hash or
//!     `output_artifacts` placeholder in the one segment right after its
//!     `__<name>__` (a hex directory further down is kept). So
//!     `buck-out/v2/art/cell/<cfg>/pkg/__t__/out.o`,
//!     `buck-out/v2/art/cell/pkg/__t__/output_artifacts/out.o` and
//!     `buck-out/v2/gen/cell/pkg/__t__/<content>/out.o` are all `cell/pkg/__t__/out.o`.
//!     A dynamic action's `__action__<k>__` stays, so its actions stay apart;
//!   - an anonymous target's configuration hash glued to its package, and the
//!     attributes or content hash right before its `__<name>__` (in content mode
//!     buck2 glues that one to the cell for the root package:
//!     `art-anon/anon<16 hex>/__t__/o` is `anon/__t__/o`). In content mode buck2
//!     also glues the cell to the package (`<cell><pkg>/…`), which stays glued: the
//!     key is stable, but not the one of the configuration-hash form;
//!   - a BXL function's execution-platform hash after the cell (`cfg`) and its
//!     arguments or content hash right after its `__<name>__`. buck2 writes these
//!     two (and an anonymous target's attributes hash) unpadded: 13 to 16 hex digits
//!     are read as a hash, so the 1 in 65 536 with fewer is kept.
//!
//!   Scratch paths (`buck-out/<v>/tmp/`) are left out. Any other path is kept as sent.
//! - **The program** (`program`): argv\[0\], normalized the same way when it is under
//!   `buck-out` (also after the `@` of an argument file), its file name when it is
//!   another absolute path, as sent otherwise; then, while the last word is a bare
//!   program name (no `/`: a program looked up on `PATH`, which may only run the next
//!   argument, as `env`, a shell or an interpreter does), the next argument that is
//!   not a flag or a `NAME=value` assignment, up to three words. A `-c`, or a cluster
//!   of single-letter flags ending in `c` (`-euc`), ends it, kept as the last word, so
//!   a `-c` script never enters the key (a script after `-e`, as Perl or Node take
//!   one, does: `-e` is a shell's flag too, so it is skipped). A buck2 genrule is
//!   `env <shell> <cell>/…/genrule.sh`, a prelude Python tool
//!   `<interpreter> <its script>`, a tool built in the repository (a `buck-out` path)
//!   its pattern alone. A bare-name compiler is read the same way: `cc -c a.c` is
//!   `cc -c`, `cc -o a.o a.c` is `cc a.o a.c`, `clang @<argsfile>` is
//!   `clang @<its pattern>`. The rule names no program: what it unwraps is told by
//!   shape (bare name, path, flag), so no list of runners can go stale.
//!
//! The key is SHA-256 of a version tag and the program's words and the sorted,
//! deduplicated output patterns, each length-prefixed, cut to 8 bytes: stable across
//! Rust releases and processes (never `DefaultHasher`), and a new tag when the
//! normalizer changes, so new keys never mix with old ones. Nothing the client's
//! `RequestMetadata` says enters it: a client that starts sending a label must not
//! split every key.

use std::fmt;

use sha2::{Digest as _, Sha256};

/// The tag hashed ahead of every shape: a new one for any change to what a shape is.
const VERSION: &[u8] = b"kbf-shape-v1";

/// How many program words the shape keeps at most (`busybox sh <script>`).
const MAX_WORDS: usize = 3;

/// A stable key over an action's [`Shape`]: the first 8 bytes of its SHA-256,
/// shown as 16 lowercase hex digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShapeKey([u8; 8]);

impl fmt::Display for ShapeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// What an action is, independent of configuration and input contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    /// The key over `program` and `outputs`.
    pub key: ShapeKey,
    /// The program's words (`program`).
    pub program: Vec<String>,
    /// The output patterns, sorted and deduplicated (`pattern`).
    pub outputs: Vec<String>,
    /// The configuration hash of the first output (in pattern order) whose path named
    /// one; `None` for an action whose outputs name none (buck2's content-hash paths).
    /// Not part of the key: one target built in two configurations has one key, and
    /// this tells them apart.
    pub cfg: Option<String>,
}

impl Shape {
    /// The shape of the action whose `Command` has `arguments` and names `outputs`
    /// (every output field, in any order, repeats allowed).
    #[must_use]
    pub fn of(arguments: &[String], outputs: &[String]) -> Self {
        let program = program(arguments);
        let mut patterns: Vec<(String, Option<String>)> =
            outputs.iter().filter_map(|o| pattern(o)).collect();
        patterns.sort();
        let cfg = patterns.iter().find_map(|(_, cfg)| cfg.clone());
        patterns.dedup_by(|a, b| a.0 == b.0);
        let outputs: Vec<String> = patterns.into_iter().map(|(p, _)| p).collect();
        let key = key(&program, &outputs);
        Self {
            key,
            program,
            outputs,
            cfg,
        }
    }
}

/// SHA-256 over the version tag, the program's words and the outputs, each part
/// preceded by its length and each list by its count, cut to 8 bytes.
fn key(program: &[String], outputs: &[String]) -> ShapeKey {
    let mut hash = Sha256::new();
    let mut part = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    part(VERSION);
    for list in [program, outputs] {
        part(&(list.len() as u64).to_le_bytes());
        for item in list {
            part(item.as_bytes());
        }
    }
    let digest = hash.finalize();
    let mut key = [0; 8];
    key.copy_from_slice(&digest[..8]);
    ShapeKey(key)
}

/// The words of the program an action runs (see the module docs); `["-"]` for an
/// empty argv.
#[must_use]
pub fn program(arguments: &[String]) -> Vec<String> {
    let Some((first, rest)) = arguments.split_first() else {
        return vec!["-".to_owned()];
    };
    let mut words = vec![word(first)];
    let mut rest = rest.iter();
    while words.len() < MAX_WORDS && words.last().is_some_and(|w| !w.contains('/')) {
        match rest.find(|a| is_inline(a) || !skipped(a)) {
            Some(inline) if is_inline(inline) => {
                words.push(inline.clone());
                break;
            }
            Some(arg) => words.push(word(arg)),
            None => break,
        }
    }
    words
}

/// The flag that hands a program an inline script, with any single-letter flags
/// before it (`<shell> -c '...'`, `<shell> -euc '...'`, `<interpreter> -Bc '...'`):
/// the program ends there. A compiler's `-c`, or a flag such as `-static`, ends it
/// too, which only makes the program shorter.
fn is_inline(arg: &str) -> bool {
    arg.strip_prefix('-')
        .is_some_and(|flags| flags.ends_with('c') && flags.bytes().all(|b| b.is_ascii_alphabetic()))
}

/// An argument a program's word skips on its way to the next: a flag, or a
/// `NAME=value` assignment (`env`'s).
fn skipped(arg: &str) -> bool {
    arg.starts_with('-') || arg.contains('=')
}

/// One program word: a `buck-out` path's pattern (`buck-out/tmp` for a scratch path),
/// the same after the `@` of an argument file (`@<pattern>`), an absolute path's file
/// name, any other argument as sent.
fn word(arg: &str) -> String {
    let (at, path) = match arg.strip_prefix('@') {
        Some(path) => ("@", path),
        None => ("", arg),
    };
    match buck_out(path) {
        Some(Found::Pattern(pattern, _)) => return format!("{at}{pattern}"),
        Some(Found::Scratch) => return format!("{at}buck-out/tmp"),
        None => {}
    }
    if arg.starts_with('/') {
        return basename(arg).to_owned();
    }
    arg.to_owned()
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The pattern of an output path, and the configuration hash it named, if any;
/// `None` for a scratch path, which is left out of the shape.
#[must_use]
pub fn pattern(path: &str) -> Option<(String, Option<String>)> {
    match buck_out(path) {
        Some(Found::Pattern(pattern, cfg)) => Some((pattern, cfg)),
        Some(Found::Scratch) => None,
        None => Some((path.to_owned(), None)),
    }
}

/// A path under a `buck-out` root kbf knows.
enum Found {
    /// Its pattern, and the configuration hash it named.
    Pattern(String, Option<String>),
    /// A scratch path (`buck-out/<v>/tmp/`).
    Scratch,
}

/// Whose outputs a `buck-out/<v>/<root>/` holds, which decides where buck2 puts its
/// hashes (its `make_hashed_path` for each kind of owner).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// A target: `<cell>/<cfg>/<pkg>/__<name>__/…`, or (content mode)
    /// `<cell>/<pkg>/__<name>__/<content>/…`.
    Target,
    /// An anonymous target: `<cell>/<cfg><pkg>/<attrs>/__<name>__/…`, or (content
    /// mode) `<cell><pkg>/<content>/__<name>__/…`. The attributes hash is unpadded.
    Anon,
    /// A BXL function: `<cell>/<exec>/<bxl path>/__<name>__/<args>/…`, or (content
    /// mode) `<cell>/<bxl path>/__<name>__/<content>/…`. The execution platform and
    /// arguments hashes are unpadded.
    Bxl,
}

/// The roots under `buck-out/<v>/` (`art` today, `gen` before it) and whose outputs
/// each holds.
const ROOTS: &[(&str, Owner)] = &[
    ("art", Owner::Target),
    ("gen", Owner::Target),
    ("art-anon", Owner::Anon),
    ("gen-anon", Owner::Anon),
    ("art-bxl", Owner::Bxl),
    ("gen-bxl", Owner::Bxl),
];

/// What `path` is under a `buck-out` root kbf knows; `None` for any other path.
fn buck_out(path: &str) -> Option<Found> {
    let segments: Vec<&str> = path.split('/').collect();
    let at = segments
        .windows(3)
        .position(|w| w[0] == "buck-out" && (w[2] == "tmp" || ROOTS.iter().any(|r| r.0 == w[2])))?;
    let Some(&(_, owner)) = ROOTS.iter().find(|r| r.0 == segments[at + 2]) else {
        return Some(Found::Scratch);
    };
    let mut rest: Vec<&str> = segments[at + 3..].to_vec();
    let cfg = take_cfg(owner, &mut rest);
    let mut kept: Vec<&str> = Vec::with_capacity(rest.len());
    for (i, &segment) in rest.iter().enumerate() {
        // The hash right after a `__<name>__` (only the next segment: a hex directory
        // further down is the action's own).
        let after_name = i > 0 && is_name(rest[i - 1]);
        if after_name && hash_after_name(owner, segment) {
            continue;
        }
        // An anonymous target's attributes or content hash, right before its
        // `__<name>__`: a segment of its own, or (content mode, root package) glued
        // to the cell.
        let before_name = rest.get(i + 1).is_some_and(|s| is_name(s));
        if owner == Owner::Anon && before_name {
            if i > 0 && is_hex(segment) {
                continue;
            }
            if let Some(cell) = unglue_content_hash(segment) {
                kept.push(cell);
                continue;
            }
        }
        kept.push(segment);
    }
    Some(Found::Pattern(kept.join("/"), cfg))
}

/// Takes the configuration hash out of `rest` (the segments after the root) and
/// returns it: a segment of its own after the cell, or (anonymous targets) the first
/// 16 characters of the package's first segment.
fn take_cfg(owner: Owner, rest: &mut Vec<&str>) -> Option<String> {
    let after_cell = *rest.get(1)?;
    if is_cfg(after_cell) || owner == Owner::Bxl && is_unpadded_hash(after_cell) {
        rest.remove(1);
        return Some(after_cell.to_owned());
    }
    if owner == Owner::Anon
        && after_cell.len() > 16
        && after_cell.is_char_boundary(16)
        && is_hex(&after_cell[..16])
    {
        rest[1] = &after_cell[16..];
        return Some(after_cell[..16].to_owned());
    }
    None
}

/// Whether `segment`, right after a `__<name>__`, is a hash: a content hash, the
/// `output_artifacts` placeholder, or (BXL) the unpadded arguments hash.
fn hash_after_name(owner: Owner, segment: &str) -> bool {
    segment == "output_artifacts"
        || is_content_hash(segment)
        || owner == Owner::Bxl && is_unpadded_hash(segment)
}

/// The cell of an anonymous target's content-mode root-package segment
/// (`<cell><16 hex>`), if `segment` is one.
fn unglue_content_hash(segment: &str) -> Option<&str> {
    let cut = segment.len().checked_sub(16).filter(|&cut| cut > 0)?;
    let (cell, hash) = segment.split_at_checked(cut)?;
    is_hex(hash).then_some(cell)
}

/// A `__<name>__` segment: a target's (`__foo__`), a dynamic action's
/// (`__action__3__`), or an anonymous target's (`__foo__0__`).
fn is_name(segment: &str) -> bool {
    segment.len() > 4 && segment.starts_with("__") && segment.ends_with("__")
}

/// Lowercase hex digits only (true of an empty segment, which a `//` makes).
fn is_hex(text: &str) -> bool {
    text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A content hash in a path: 16 lowercase hex digits.
fn is_content_hash(segment: &str) -> bool {
    segment.len() == 16 && is_hex(segment)
}

/// A hash buck2 writes without padding (`{:x}` of a `u64`): 13 to 16 hex digits.
/// Fewer digits happen for 1 hash in 65 536, and are kept; a directory named by 13
/// or more hex digits is not expected where these sit.
fn is_unpadded_hash(segment: &str) -> bool {
    (13..=16).contains(&segment.len()) && is_hex(segment)
}

/// A configuration hash: 16 lowercase hex digits, or two joined by `-` (a target's
/// configuration and its execution platform's).
fn is_cfg(segment: &str) -> bool {
    match segment.split_once('-') {
        Some((target, exec)) => is_content_hash(target) && is_content_hash(exec),
        None => is_content_hash(segment),
    }
}

#[cfg(test)]
#[path = "shape_tests.rs"]
mod tests;
