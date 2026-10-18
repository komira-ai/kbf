//! Test fixtures shared by the modules' tests: a runner that plays the programs, a
//! trusted host, tarballs and inboxes.

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;

use kbf_host::exec::Entry;
use kbf_host::path::HostPath;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;

use crate::layout::Platform;
use crate::run::{Ran, RunAs, Runner, Spawn};

/// The trust directory of a Linux host, and the files in it.
pub const TRUST_DIR: &str = "/usr/share/kbf-installer/trust";
pub const GH_PATH: &str = "/usr/share/kbf-installer/trust/gh";
pub const TRUSTED_ROOT_PATH: &str = "/usr/share/kbf-installer/trust/trusted_root.jsonl";
pub const SETTINGS_PATH: &str = "/usr/share/kbf-installer/trust/installer.conf";

pub const COMMIT: &str = "0f4a3825305cc79733ef098eae9725948eb5d6db";
pub const OTHER: &str = "1111111111111111111111111111111111111111";

/// What a fake program answers to a spawn.
type Answer = Box<dyn Fn(&Spawn) -> io::Result<Ran>>;

/// A runner that records every [`Spawn`] and answers as `answer` says.
pub struct FakeRunner {
    calls: RefCell<Vec<Spawn>>,
    answer: Answer,
}

impl FakeRunner {
    pub fn new(answer: impl Fn(&Spawn) -> io::Result<Ran> + 'static) -> FakeRunner {
        FakeRunner {
            calls: RefCell::new(Vec::new()),
            answer: Box::new(answer),
        }
    }

    pub fn calls(&self) -> Vec<Spawn> {
        self.calls.borrow().clone()
    }
}

impl Runner for FakeRunner {
    fn run(&self, spawn: &Spawn) -> io::Result<Ran> {
        self.calls.borrow_mut().push(spawn.clone());
        (self.answer)(spawn)
    }
}

pub fn ran(code: i32, stdout: &str, stderr: &str) -> Ran {
    Ran {
        code: Some(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        timed_out: false,
    }
}

/// What `kbf --version --json` prints for a build of `commit` (12 digits are shown).
pub fn version_json(commit: &str, profile: &str) -> String {
    format!(
        "{{\"commit\":\"{}\",\"min_protocol\":3,\"profile\":\"{profile}\",\"protocol\":4,\
         \"state_schema\":7,\"version\":\"0.1.0\"}}\n",
        &commit[..12]
    )
}

/// The paths of the trust list and every directory above them, as a bootstrap leaves
/// them: root's, 0755 (files 0755 and 0644).
pub fn trusted_host() -> FakeExec {
    trusted_host_on(Platform::Linux)
}

/// [`trusted_host`] for a host of `platform`: its trust directory (a Mac's is the pkg's `/usr/local/kbf-installer/trust`).
pub fn trusted_host_on(platform: Platform) -> FakeExec {
    let (parents, dir, gh, root_file): (&[&str], &str, String, String) = match platform {
        Platform::Linux => (
            &["/usr", "/usr/share", "/usr/share/kbf-installer"],
            TRUST_DIR,
            GH_PATH.to_owned(),
            TRUSTED_ROOT_PATH.to_owned(),
        ),
        Platform::MacOs => (
            &["/usr", "/usr/local", "/usr/local/kbf-installer"],
            "/usr/local/kbf-installer/trust",
            "/usr/local/kbf-installer/trust/gh".to_owned(),
            "/usr/local/kbf-installer/trust/trusted_root.jsonl".to_owned(),
        ),
    };
    let mut exec = FakeExec::new(0);
    let root = Owner { uid: 0, gid: 0 };
    for dir in parents.iter().copied().chain([dir]) {
        exec.entries.insert(
            HostPath::new(dir).expect("a path"),
            Entry::Dir {
                perms: Perms::new(0o755),
                owner: root,
            },
        );
    }
    for (file, mode) in [(gh.as_str(), 0o755), (root_file.as_str(), 0o644)] {
        exec.entries.insert(
            HostPath::new(file).expect("a path"),
            Entry::File {
                perms: Perms::new(mode),
                owner: root,
                bytes: Vec::new(),
            },
        );
    }
    exec
}

/// The test's own ids: what a stage's unprivileged user is when the test runs as no one
/// in particular.
pub fn me() -> RunAs {
    RunAs {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
    }
}

/// An ELF header (64-bit, little-endian) for `machine` (62 x86_64, 183 aarch64), then
/// `tail`.
pub fn elf(machine: u8, tail: &str) -> Vec<u8> {
    let mut bytes = vec![0u8; 64];
    bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
    bytes[18] = machine;
    bytes.extend_from_slice(tail.as_bytes());
    bytes
}

/// A thin 64-bit Mach-O header for `cpu` (0x0100000c arm64, 0x01000007 x86_64).
pub fn macho(cpu: u32, tail: &str) -> Vec<u8> {
    let mut bytes = vec![0u8; 32];
    bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    bytes[4..8].copy_from_slice(&cpu.to_le_bytes());
    bytes.extend_from_slice(tail.as_bytes());
    bytes
}

/// A tar entry for [`tar_gz`].
pub struct Item<'a> {
    /// The name as it is written in the header, unchecked.
    pub name: &'a [u8],
    pub kind: u8,
    pub data: &'a [u8],
}

pub fn file<'a>(name: &'a str, data: &'a [u8]) -> Item<'a> {
    Item {
        name: name.as_bytes(),
        kind: b'0',
        data,
    }
}

/// A gzip-compressed tar of `items`, each header written as given.
pub fn tar_gz(items: &[Item]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for item in items {
        let mut header = tar::Header::new_gnu();
        header.as_old_mut().name[..item.name.len()].copy_from_slice(item.name);
        header.set_size(item.data.len() as u64);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::new(item.kind));
        header.set_cksum();
        builder.append(&header, item.data).expect("append");
    }
    let tarred = builder.into_inner().expect("tar");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    io::Write::write_all(&mut gz, &tarred).expect("gzip");
    gz.finish().expect("gzip")
}

/// A scratch directory for a test, apart from the host's.
pub fn scratch_dir(name: &str) -> PathBuf {
    kbf_host::testkit::scratch(&format!("installer-{name}"))
}
