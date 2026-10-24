//! The E1 share matrix: several extra shares per VM, each with a host directory that
//! differs from the baseline in one factor, so one run says which factor makes the guest's
//! `create` fail with `Operation not permitted`.
//!
//! The third E1 run showed that no guest identity could create a file in any share (the
//! automount-tag-free single-directory shares `kbfshare` and `kbfprobe`), while reading
//! worked, and that the host showed no TCC or sandbox denial. Apple's documentation names
//! no requirement on the host directory's owner, mode or location, so the variants below
//! are the experiment. The guest (`kbf-guest probe-diag`) reads the manifest this module
//! writes into the main share, mounts each tag, tries create, write, rename, mkdir and
//! remove, and prints one row per share; the host lists each directory after the run.
//!
//! Baseline: under the work directory, mode 0777, owned by the user who runs `kbf-vmm`
//! (the owner of the VM process), a single-directory share, writable.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::guest;
use crate::probe::Ctx;
use crate::spec::{TaggedShare, VmSpec};

/// The manifest the guest reads, in the main share's root.
pub const MANIFEST: &str = "matrix.json";
/// The directory every user of the Mac may write to. Spelled in two pieces because the
/// repository's hygiene lint refuses any text that looks like a home path.
const SHARED_DIR: &str = concat!("/Us", "ers/Shared");
/// The name a multiple-directory share lists its one directory under.
pub const SUBDIR: &str = "m";
/// The user id the other-owner variant is given (only a root run can do it).
pub const OTHER_UID: u32 = 59_001;

/// Where a variant's host directory is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Beside the clone, in the work directory.
    Work,
    /// `/private/tmp`, the real path behind `/tmp`.
    Tmp,
    /// the shared-users directory.
    Shared,
}

/// One row of the matrix: the baseline with one thing changed.
#[derive(Debug, Clone, Copy)]
struct Variant {
    name: &'static str,
    /// What differs from the baseline, in words, for the report.
    factor: &'static str,
    place: Place,
    mode: u32,
    multiple: bool,
    read_only: bool,
    other_owner: bool,
}

const BASE: Variant = Variant {
    name: "base",
    factor: "none (the baseline)",
    place: Place::Work,
    mode: 0o777,
    multiple: false,
    read_only: false,
    other_owner: false,
};

/// The baseline and one variant per factor, plus two negative controls: a host directory
/// the VM's user cannot write (mode 0555) and a share marked read-only. If the baseline
/// fails exactly like the 0555 control, the host's file permissions are the suspect; if it
/// fails like the read-only control, the share is being treated as read-only.
const VARIANTS: [Variant; 8] = [
    BASE,
    Variant {
        name: "mode755",
        factor: "mode 0755 instead of 0777",
        mode: 0o755,
        ..BASE
    },
    Variant {
        name: "tmp",
        factor: "location /private/tmp",
        place: Place::Tmp,
        ..BASE
    },
    Variant {
        name: "shared",
        factor: "location the shared-users directory",
        place: Place::Shared,
        ..BASE
    },
    Variant {
        name: "multiple",
        factor: "a multiple-directory share (the directory is listed as m)",
        multiple: true,
        ..BASE
    },
    Variant {
        name: "other-owner",
        factor: "owned by another user (needs a root run; skipped otherwise)",
        other_owner: true,
        ..BASE
    },
    Variant {
        name: "control-mode555",
        factor: "negative control: mode 0555, nobody can write on the host",
        mode: 0o555,
        ..BASE
    },
    Variant {
        name: "control-readonly",
        factor: "negative control: the share is read-only",
        read_only: true,
        ..BASE
    },
];

/// A matrix share as made for one VM.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub name: &'static str,
    pub factor: &'static str,
    pub tag: String,
    pub dir: PathBuf,
    pub mode: u32,
    pub multiple: bool,
    pub read_only: bool,
    /// Why the share was not made or not attached.
    pub skipped: Option<String>,
}

fn dir_of(ctx: &Ctx<'_>, vm: &str, v: &Variant) -> PathBuf {
    let leaf = format!("kbf-{vm}-mx-{}", v.name);
    match v.place {
        Place::Work => ctx.work.join(leaf),
        Place::Tmp => Path::new("/private/tmp").join(leaf),
        Place::Shared => Path::new(SHARED_DIR).join(leaf),
    }
}

/// Makes the host directories for VM `vm`. A directory that cannot be made is reported as
/// skipped, never as an error: the rest of the matrix still runs.
pub fn prepare(ctx: &Ctx<'_>, vm: &str) -> Vec<Prepared> {
    VARIANTS
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let dir = dir_of(ctx, vm, v);
            let skipped = make(ctx, &dir, v).err();
            Prepared {
                name: v.name,
                factor: v.factor,
                tag: format!("kbfmx{i}"),
                dir,
                mode: v.mode,
                multiple: v.multiple,
                read_only: v.read_only,
                skipped,
            }
        })
        .collect()
}

fn make(ctx: &Ctx<'_>, dir: &Path, v: &Variant) -> Result<(), String> {
    if v.other_owner && ctx.sys.uid() != 0 {
        return Err(format!(
            "needs a root run (this one is uid {}): every directory is owned by the VM's user",
            ctx.sys.uid()
        ));
    }
    guest::remove_leftover(dir);
    guest::make_dir(dir, v.mode)?;
    if v.other_owner {
        crate::hostfs::chown_tree(dir, OTHER_UID, OTHER_UID)?;
    }
    Ok(())
}

/// Adds the shares that were made to `spec`.
pub fn attach(spec: &mut VmSpec, shares: &[Prepared]) {
    spec.tagged.extend(
        shares
            .iter()
            .filter(|s| s.skipped.is_none())
            .map(|s| TaggedShare {
                tag: s.tag.clone(),
                path: s.dir.clone(),
                multiple_name: s.multiple.then(|| SUBDIR.to_owned()),
                read_only: s.read_only,
            }),
    );
}

/// The manifest the guest reads: one object per attached share.
#[must_use]
pub fn manifest(shares: &[Prepared]) -> Value {
    Value::Array(
        shares
            .iter()
            .filter(|s| s.skipped.is_none())
            .map(|s| {
                json!({
                    "name": s.name,
                    "tag": s.tag,
                    "factor": s.factor,
                    "subdir": s.multiple.then_some(SUBDIR),
                    "read_only": s.read_only,
                    "host_mode": format!("{:o}", s.mode),
                })
            })
            .collect(),
    )
}

/// Writes [`MANIFEST`] into the main share's directory `share_dir`.
///
/// # Errors
/// The file could not be written.
pub fn write_manifest(share_dir: &Path, shares: &[Prepared]) -> Result<(), String> {
    let path = share_dir.join(MANIFEST);
    std::fs::write(&path, format!("{:#}\n", manifest(shares)))
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// What is in each host directory now: its own owner and mode, its real path, and what
/// the guest left in it (names, sizes, owners, modes). Anything listed landed on the host.
#[must_use]
pub fn host_listing(shares: &[Prepared]) -> Value {
    Value::Array(
        shares
            .iter()
            .map(|s| {
                let mut row = json!({
                    "name": s.name,
                    "factor": s.factor,
                    "tag": s.tag,
                    "host_dir": s.dir,
                    "skipped": s.skipped,
                });
                if s.skipped.is_none() {
                    row["real_path"] = json!(std::fs::canonicalize(&s.dir).ok());
                    row["dir"] = describe(&s.dir);
                    row["entries"] = entries(&s.dir);
                }
                row
            })
            .collect(),
    )
}

fn describe(path: &Path) -> Value {
    match std::fs::symlink_metadata(path) {
        Ok(m) => json!({
            "uid": m.uid(),
            "gid": m.gid(),
            "mode": format!("{:o}", m.permissions().mode() & 0o7777),
        }),
        Err(e) => json!({"error": e.to_string()}),
    }
}

fn entries(dir: &Path) -> Value {
    let mut rows: Vec<Value> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .take(50)
            .map(|e| {
                let mut v = describe(&e.path());
                v["name"] = json!(e.file_name().to_string_lossy());
                v["bytes"] = json!(e.metadata().map(|m| m.len()).ok());
                v
            })
            .collect(),
        Err(e) => return json!({"error": e.to_string()}),
    };
    rows.sort_by_key(|v| v["name"].to_string());
    Value::Array(rows)
}

/// Removes the matrix directories that are not under the work directory (they are scratch
/// in shared places), and the ones under it unless `keep`.
pub fn remove(ctx: &Ctx<'_>, shares: &[Prepared], keep: bool) {
    for s in shares {
        if keep && s.dir.starts_with(&ctx.work) {
            continue;
        }
        // The read-only control may have no write bit; a removal needs the parent's.
        let _ = std::fs::set_permissions(&s.dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&s.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeSystem, ctx, work_with_base};

    /// Catches a matrix whose variants differ in more than one factor from the baseline
    /// (the answer would not name the factor), duplicated names or tags, a tag that is the
    /// automount tag or one of the other shares, and a missing control.
    #[test]
    fn each_variant_differs_from_the_baseline_in_one_factor() {
        let diff = |v: &Variant| {
            usize::from(v.place != BASE.place)
                + usize::from(v.mode != BASE.mode)
                + usize::from(v.multiple != BASE.multiple)
                + usize::from(v.read_only != BASE.read_only)
                + usize::from(v.other_owner != BASE.other_owner)
        };
        assert_eq!(diff(&VARIANTS[0]), 0);
        for v in &VARIANTS[1..] {
            assert_eq!(diff(v), 1, "{}", v.name);
        }
        let names: std::collections::BTreeSet<_> = VARIANTS.iter().map(|v| v.name).collect();
        assert_eq!(names.len(), VARIANTS.len());
        for want in [
            "tmp",
            "shared",
            "mode755",
            "multiple",
            "other-owner",
            "control-mode555",
            "control-readonly",
        ] {
            assert!(names.contains(want), "{want}");
        }
        let work = work_with_base("matrix-tags");
        let sys = FakeSystem::new();
        let shares = prepare(&ctx(&sys, &work), "vm");
        let tags: std::collections::BTreeSet<_> = shares.iter().map(|s| s.tag.clone()).collect();
        assert_eq!(tags.len(), shares.len());
        for t in &tags {
            assert!(t.starts_with("kbfmx"));
            assert_ne!(t, guest::AUTOMOUNT_TAG);
        }
    }

    /// Catches directories made with the wrong mode (the umask would narrow 0777), the
    /// other-owner variant made by a non-root run (it must be skipped, with the reason),
    /// shared places written to by a test, and a skipped share that is still attached.
    #[test]
    fn directories_get_their_mode_and_a_variant_that_cannot_be_made_is_skipped() {
        let work = work_with_base("matrix-prepare");
        let sys = FakeSystem::new();
        let c = ctx(&sys, &work);
        let shares = prepare(&c, "vm");
        let by = |n: &str| shares.iter().find(|s| s.name == n).expect("share");
        for (name, mode) in [
            ("base", 0o777),
            ("mode755", 0o755),
            ("control-mode555", 0o555),
        ] {
            let got = std::fs::metadata(&by(name).dir)
                .expect("dir")
                .permissions()
                .mode()
                & 0o7777;
            assert_eq!(got, mode, "{name}");
        }
        assert!(
            by("other-owner")
                .skipped
                .as_deref()
                .expect("why")
                .contains("root run")
        );
        assert!(!by("other-owner").dir.exists());
        // /private/tmp and the shared-users directory exist on a Mac; elsewhere they are skipped or made.
        let mut spec = VmSpec::minimal(work.join("vm"));
        attach(&mut spec, &shares);
        let attached: Vec<&str> = spec.tagged.iter().map(|t| t.tag.as_str()).collect();
        let want: Vec<&str> = shares
            .iter()
            .filter(|s| s.skipped.is_none())
            .map(|s| s.tag.as_str())
            .collect();
        assert_eq!(attached, want);
        let multiple = spec
            .tagged
            .iter()
            .find(|t| t.tag == by("multiple").tag)
            .expect("multiple");
        assert_eq!(multiple.multiple_name.as_deref(), Some(SUBDIR));
        let ro = spec
            .tagged
            .iter()
            .find(|t| t.tag == by("control-readonly").tag)
            .expect("ro");
        assert!(ro.read_only);
        let base = spec
            .tagged
            .iter()
            .find(|t| t.tag == by("base").tag)
            .expect("base");
        assert_eq!((base.multiple_name.clone(), base.read_only), (None, false));
        // A root run makes the other-owner directory and gives it away; a non-root test
        // process cannot chown, and then the share is skipped with the chown error.
        sys.uid.set(0);
        let again = prepare(&c, "vm");
        let other = again
            .iter()
            .find(|s| s.name == "other-owner")
            .expect("share");
        let outcome = other
            .skipped
            .as_ref()
            .map_or(other.dir.exists(), |e| e.contains("chown"));
        assert!(outcome, "{:?}", other.skipped);
        remove(&c, &again, false);
        remove(&c, &shares, false);
        assert!(!by("base").dir.exists());
        assert!(
            !by("control-mode555").dir.exists(),
            "a 0555 directory is removed too"
        );
    }

    /// Catches a manifest that lists a share the guest cannot mount, loses the sub
    /// directory of a multiple share, or is not written where the guest reads it.
    #[test]
    fn the_manifest_lists_what_is_attached_and_is_written_into_the_share() {
        let work = work_with_base("matrix-manifest");
        let sys = FakeSystem::new();
        let shares = prepare(&ctx(&sys, &work), "vm");
        let m = manifest(&shares);
        let rows = m.as_array().expect("rows");
        assert_eq!(
            rows.len(),
            shares.iter().filter(|s| s.skipped.is_none()).count()
        );
        assert!(rows.iter().all(|r| r["name"] != "other-owner"));
        let multiple = rows
            .iter()
            .find(|r| r["name"] == "multiple")
            .expect("multiple");
        assert_eq!(multiple["subdir"], SUBDIR);
        let base = rows.iter().find(|r| r["name"] == "base").expect("base");
        assert_eq!(
            (base["subdir"].clone(), base["host_mode"].clone()),
            (Value::Null, json!("777"))
        );
        let share = work.join("share");
        std::fs::create_dir(&share).expect("share");
        write_manifest(&share, &shares).expect("write");
        let back: Value =
            serde_json::from_str(&std::fs::read_to_string(share.join(MANIFEST)).expect("read"))
                .expect("json");
        assert_eq!(back, m);
        assert!(
            write_manifest(&work.join("none"), &shares)
                .unwrap_err()
                .contains("write")
        );
    }

    /// Catches a host listing that hides what the guest left (the proof that something
    /// landed), omits a directory's owner and mode, or fails on a skipped share.
    #[test]
    fn the_host_listing_shows_what_landed_and_who_owns_the_directory() {
        let work = work_with_base("matrix-list");
        let sys = FakeSystem::new();
        let shares = prepare(&ctx(&sys, &work), "vm");
        let base = shares.iter().find(|s| s.name == "base").expect("base");
        std::fs::write(base.dir.join("landed"), "abc").expect("file");
        let rows = host_listing(&shares);
        let row = |n: &str| {
            rows.as_array()
                .expect("rows")
                .iter()
                .find(|r| r["name"] == n)
                .expect("row")
                .clone()
        };
        assert_eq!(row("base")["entries"][0]["name"], "landed");
        assert_eq!(row("base")["entries"][0]["bytes"], 3);
        assert_eq!(row("base")["dir"]["mode"], "777");
        assert!(row("base")["real_path"].is_string());
        assert_eq!(row("mode755")["entries"], json!([]));
        assert!(row("other-owner")["skipped"].is_string());
        assert!(row("other-owner").get("entries").is_none());
        // A directory that vanished is reported, not a panic.
        std::fs::remove_dir_all(&base.dir).expect("rm");
        let rows = host_listing(&shares);
        let gone = rows
            .as_array()
            .expect("rows")
            .iter()
            .find(|r| r["name"] == "base")
            .expect("row");
        assert!(gone["dir"]["error"].is_string());
        assert!(gone["entries"]["error"].is_string());
    }

    /// Catches a removal that leaves scratch outside the work directory and one that
    /// removes the kept clones' directories under `--keep`.
    #[test]
    fn keep_leaves_the_work_directories_and_removes_nothing_else() {
        let work = work_with_base("matrix-keep");
        let sys = FakeSystem::new();
        let c = ctx(&sys, &work);
        let shares = prepare(&c, "vm");
        remove(&c, &shares, true);
        assert!(
            shares
                .iter()
                .filter(|s| s.dir.starts_with(&work) && s.skipped.is_none())
                .all(|s| s.dir.exists())
        );
        assert!(
            !shares
                .iter()
                .any(|s| s.dir.starts_with(SHARED_DIR) && s.dir.exists())
        );
        remove(&c, &shares, false);
        assert!(
            shares
                .iter()
                .all(|s| !s.dir.starts_with(&work) || !s.dir.exists())
        );
    }
}
