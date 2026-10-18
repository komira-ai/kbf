//! Test fixtures shared by the modules' tests: a runner that plays the programs, a
//! trusted host, tarballs and inboxes.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use kbf_host::exec::Entry;
use kbf_host::path::HostPath;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;
use sha2::{Digest, Sha256};

use crate::layout::{Layout, Platform};
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

    /// Every program accepted and every binary a release build of `commit`.
    pub fn ok(commit: &'static str) -> FakeRunner {
        FakeRunner::new(move |s| Ok(play(s, commit, "release")))
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

/// The answer of a host where every program works: `gh`, `xattr` and `codesign` accept,
/// a staged binary reports `commit` and `profile`.
pub fn play(spawn: &Spawn, commit: &str, profile: &str) -> Ran {
    let name = spawn
        .program
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    match name {
        "xattr" => ran(
            1,
            "",
            "xattr: [Errno 93] No such xattr: com.apple.quarantine\n",
        ),
        "codesign" if spawn.args.first().is_some_and(|a| a == "--display") => ran(
            0,
            "",
            "CodeDirectory v=20400 size=1 flags=0x10002(adhoc,runtime) hashes=1\n",
        ),
        "gh" | "codesign" => ran(0, "", ""),
        _ => ran(0, &version_json(commit, profile), ""),
    }
}

/// The paths of the trust list and every directory above them, as a bootstrap leaves
/// them: root's, 0755 (files 0755 and 0644).
pub fn trusted_host() -> FakeExec {
    trusted_host_on(Platform::Linux)
}

/// [`trusted_host`] for a host of `platform`: its trust directory (a Mac's is under
/// `/usr/local/share`).
pub fn trusted_host_on(platform: Platform) -> FakeExec {
    let (parents, dir, gh, root_file): (&[&str], &str, String, String) = match platform {
        Platform::Linux => (
            &["/usr", "/usr/share", "/usr/share/kbf-installer"],
            TRUST_DIR,
            GH_PATH.to_owned(),
            TRUSTED_ROOT_PATH.to_owned(),
        ),
        Platform::MacOs => (
            &[
                "/usr",
                "/usr/local",
                "/usr/local/share",
                "/usr/local/share/kbf-installer",
            ],
            "/usr/local/share/kbf-installer/trust",
            "/usr/local/share/kbf-installer/trust/gh".to_owned(),
            "/usr/local/share/kbf-installer/trust/trusted_root.jsonl".to_owned(),
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

pub fn me_owner() -> Owner {
    let me = me();
    Owner {
        uid: me.uid,
        gid: me.gid,
    }
}

/// A scratch host on `platform`: its layout, and the ready directories.
pub fn scratch_layout(name: &str, platform: Platform) -> (kbf_host::testkit::ScratchHost, Layout) {
    let exec = kbf_host::testkit::ScratchHost::new(&format!("installer-{name}"));
    let host = Layout::new(
        kbf_host::path::Root::new(&exec.root).expect("absolute"),
        platform,
    );
    (exec, host)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
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

/// One component of a test release: its name and the tarball that carries it.
pub struct Part {
    pub name: &'static str,
    pub tarball: Vec<u8>,
}

/// A tarball holding the one file `name` with `bytes`.
pub fn part(name: &'static str, bytes: &[u8]) -> Part {
    Part {
        name,
        tarball: tar_gz(&[file(name, bytes)]),
    }
}

/// The bundle every test inbox carries.
pub const BUNDLE: &[u8] = b"{\"mediaType\":\"a bundle\"}\n";

/// Write an inbox entry at `dir` offering `commit` with `parts`: the tarballs
/// (`<name>.tar.gz`), `provenance.jsonl` and `offer.json`. Returns the offer's text.
pub fn write_inbox(dir: &Path, commit: &str, parts: &[Part]) -> String {
    fs::create_dir_all(dir).expect("entry");
    let mut components = Vec::new();
    for p in parts {
        let file = format!("{}.tar.gz", p.name);
        fs::write(dir.join(&file), &p.tarball).expect("tarball");
        components.push(format!(
            "{{\"name\":\"{}\",\"file\":\"{file}\",\"sha256\":\"{}\"}}",
            p.name,
            sha256_hex(&p.tarball)
        ));
    }
    fs::write(dir.join("provenance.jsonl"), BUNDLE).expect("bundle");
    let offer = format!(
        "{{\"schema\":1,\"commit\":\"{commit}\",\"bundle\":{{\"file\":\"provenance.jsonl\",\
         \"sha256\":\"{}\"}},\"components\":[{}]}}",
        sha256_hex(BUNDLE),
        components.join(",")
    );
    fs::write(dir.join("offer.json"), &offer).expect("offer");
    offer
}

/// A scratch directory for a test's inbox, apart from the host's.
pub fn scratch_dir(name: &str) -> PathBuf {
    kbf_host::testkit::scratch(&format!("installer-{name}"))
}
