//! The wire form of the manifest. Private: the typed manifest is the API, so a caller cannot
//! build a manifest the reader would disagree with about spelling. No struct here refuses a
//! member it does not know: serde skips it (`deny_unknown_fields` is left off on purpose, B8).

use kbf_types::{Digest, DigestFunction};
use serde::{Deserialize, Serialize};

use super::{
    InputStats, LargeEntry, MANIFEST_VERSION_2, ManifestError, OutputDirectory, OutputFile,
    OutputSymlink, TaskManifest, Timing, Usage,
};

fn digest_of(field: String, text: &str) -> Result<Digest, ManifestError> {
    Digest::parse(DigestFunction::Sha256, text)
        .map_err(|source| ManifestError::Digest { field, source })
}

#[derive(Serialize, Deserialize)]
pub(super) struct Wire {
    pub(super) version: u32,
    lease: String,
    action: String,
    exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
    output_files: Vec<WireFile>,
    output_directories: Vec<WireDirectory>,
    output_symlinks: Vec<WireSymlink>,
    timing: WireTiming,
    usage: WireUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) packs: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) large: Option<Vec<WireLarge>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) objects: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) inputs: Option<InputStats>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct WireLarge {
    slot: u8,
    digest: String,
}

#[derive(Serialize, Deserialize)]
struct WireFile {
    path: String,
    digest: String,
    is_executable: bool,
}

#[derive(Serialize, Deserialize)]
struct WireDirectory {
    path: String,
    tree: String,
}

#[derive(Serialize, Deserialize)]
struct WireSymlink {
    path: String,
    target: String,
}

#[derive(Serialize, Deserialize)]
struct WireTiming {
    fetch_ms: u64,
    run_ms: u64,
    pack_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    walk_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    write_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct WireUsage {
    cpu_usage_us: u64,
    memory_peak_bytes: u64,
}

impl Wire {
    pub(super) fn from_manifest(m: &TaskManifest) -> Self {
        Self {
            version: m.version(),
            lease: m.lease.clone(),
            action: m.action.to_string(),
            exit_code: m.exit_code,
            stdout: m.stdout.map(|d| d.to_string()),
            stderr: m.stderr.map(|d| d.to_string()),
            output_files: m
                .output_files
                .iter()
                .map(|f| WireFile {
                    path: f.path.clone(),
                    digest: f.digest.to_string(),
                    is_executable: f.is_executable,
                })
                .collect(),
            output_directories: m
                .output_directories
                .iter()
                .map(|d| WireDirectory {
                    path: d.path.clone(),
                    tree: d.tree.to_string(),
                })
                .collect(),
            output_symlinks: m
                .output_symlinks
                .iter()
                .map(|l| WireSymlink {
                    path: l.path.clone(),
                    target: l.target.clone(),
                })
                .collect(),
            timing: WireTiming {
                fetch_ms: m.timing.fetch_ms,
                run_ms: m.timing.run_ms,
                pack_ms: m.timing.pack_ms,
                walk_ms: m.timing.walk_ms,
                hash_ms: m.timing.hash_ms,
                write_ms: m.timing.write_ms,
                commit_ms: m.timing.commit_ms,
            },
            usage: WireUsage {
                cpu_usage_us: m.usage.cpu_usage_us,
                memory_peak_bytes: m.usage.memory_peak_bytes,
            },
            packs: (m.version() == MANIFEST_VERSION_2).then_some(m.packs),
            large: (m.version() == MANIFEST_VERSION_2).then(|| {
                m.large
                    .iter()
                    .map(|l| WireLarge {
                        slot: l.slot,
                        digest: l.digest.to_string(),
                    })
                    .collect()
            }),
            objects: (m.objects.as_ref()).map(|o| o.iter().map(Digest::to_string).collect()),
            inputs: m.inputs,
        }
    }

    pub(super) fn into_manifest(self) -> Result<TaskManifest, ManifestError> {
        let stream = |field: &str, text: Option<String>| {
            text.map(|t| digest_of(field.to_owned(), &t)).transpose()
        };
        let mut output_files = Vec::with_capacity(self.output_files.len());
        for (i, f) in self.output_files.into_iter().enumerate() {
            output_files.push(OutputFile {
                digest: digest_of(format!("output_files[{i}].digest"), &f.digest)?,
                path: f.path,
                is_executable: f.is_executable,
            });
        }
        let mut output_directories = Vec::with_capacity(self.output_directories.len());
        for (i, d) in self.output_directories.into_iter().enumerate() {
            output_directories.push(OutputDirectory {
                tree: digest_of(format!("output_directories[{i}].tree"), &d.tree)?,
                path: d.path,
            });
        }
        let mut large = Vec::new();
        for (i, l) in self.large.into_iter().flatten().enumerate() {
            large.push(LargeEntry {
                digest: digest_of(format!("large[{i}].digest"), &l.digest)?,
                slot: l.slot,
            });
        }
        let objects = match self.objects {
            None => None,
            Some(texts) => Some(
                (texts.iter().enumerate())
                    .map(|(i, text)| digest_of(format!("objects[{i}]"), text))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        };
        Ok(TaskManifest {
            action: digest_of("action".to_owned(), &self.action)?,
            lease: self.lease,
            exit_code: self.exit_code,
            stdout: stream("stdout", self.stdout)?,
            stderr: stream("stderr", self.stderr)?,
            output_files,
            output_directories,
            output_symlinks: self
                .output_symlinks
                .into_iter()
                .map(|l| OutputSymlink {
                    path: l.path,
                    target: l.target,
                })
                .collect(),
            timing: Timing {
                fetch_ms: self.timing.fetch_ms,
                run_ms: self.timing.run_ms,
                pack_ms: self.timing.pack_ms,
                walk_ms: self.timing.walk_ms,
                hash_ms: self.timing.hash_ms,
                write_ms: self.timing.write_ms,
                commit_ms: self.timing.commit_ms,
            },
            usage: Usage {
                cpu_usage_us: self.usage.cpu_usage_us,
                memory_peak_bytes: self.usage.memory_peak_bytes,
            },
            // A run of objects wrote no pack; a version 1 run wrote one.
            packs: (self.packs).unwrap_or(if objects.is_some() { 0 } else { 1 }),
            large,
            inputs: self.inputs,
            objects,
        })
    }
}
