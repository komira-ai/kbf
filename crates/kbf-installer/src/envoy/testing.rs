//! What the Envoy tests share: a scratch Linux host, a fake Envoy (a file with an ELF
//! header that names the version it reports), a runner that plays `envoy --version` by
//! reading the very file it is asked to run, and a fetcher that serves bytes.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::EnvoyId;
use super::fetch::{FetchError, Fetcher};
use super::install::Env;
use super::manifest::EnvoyPin;
use crate::binary::Arch;
use crate::fixtures::{FakeRunner, elf, me, ran, sha256_hex};
use crate::layout::Layout;
use crate::run::Spawn;
use crate::testkit::host_root;

pub const VERSION: &str = "1.31.2";
pub const OTHER_VERSION: &str = "1.32.0";
pub const URL: &str = "https://upstream.example/envoy-1.31.2-linux-x86_64";

/// A fake Envoy for `machine` (62 x86_64, 183 aarch64) that reports `version`.
pub fn fake_envoy(machine: u8, version: &str) -> Vec<u8> {
    elf(machine, &format!("\n#fake-envoy-version={version}\n"))
}

/// The x86-64 fake of [`VERSION`].
pub fn envoy() -> Vec<u8> {
    fake_envoy(62, VERSION)
}

/// The pin for `bytes` as the manifest would name it.
pub fn pin(bytes: &[u8]) -> EnvoyPin {
    EnvoyPin {
        version: VERSION.to_owned(),
        url: URL.to_owned(),
        sha256: sha256_hex(bytes),
    }
}

pub fn id(bytes: &[u8]) -> EnvoyId {
    EnvoyId::new(VERSION, &sha256_hex(bytes)).expect("an id")
}

/// What real `envoy --version` prints, for `version`.
pub fn says(version: &str) -> String {
    format!(
        "envoy  version: 5c2eb58cfc2fb6d3df6e3dd3d5e1bd4cc8a5de5e/{version}/Clean/RELEASE/BoringSSL\n"
    )
}

/// A runner that plays `envoy --version`: it reads the file it is asked to run and
/// reports the version that file names.
pub fn envoy_runner() -> FakeRunner {
    FakeRunner::new(|spawn: &Spawn| {
        let bytes = fs::read(&spawn.program)?;
        let text = String::from_utf8_lossy(&bytes);
        let version = text
            .split("fake-envoy-version=")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .unwrap_or("");
        Ok(ran(0, &says(version), ""))
    })
}

/// A fetcher that writes what `write` says to the destination, and records its calls.
/// What a fake fetcher does with the destination it is given.
type Write = Box<dyn Fn(&Path) -> Result<(), FetchError>>;

pub struct FakeFetcher {
    write: Write,
    calls: RefCell<Vec<(String, PathBuf, u64)>>,
}

impl FakeFetcher {
    pub fn new(write: impl Fn(&Path) -> Result<(), FetchError> + 'static) -> FakeFetcher {
        FakeFetcher {
            write: Box::new(write),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// Serves `bytes`.
    pub fn serving(bytes: Vec<u8>) -> FakeFetcher {
        FakeFetcher::new(move |dest| fs::write(dest, &bytes).map_err(|e| failed(&e)))
    }

    pub fn calls(&self) -> Vec<(String, PathBuf, u64)> {
        self.calls.borrow().clone()
    }
}

pub fn failed(e: &io::Error) -> FetchError {
    FetchError::Failed {
        status: "exit 1".to_owned(),
        detail: e.to_string(),
    }
}

impl Fetcher for FakeFetcher {
    fn fetch(&self, url: &str, dest: &Path, cap: u64) -> Result<(), FetchError> {
        self.calls
            .borrow_mut()
            .push((url.to_owned(), dest.to_owned(), cap));
        (self.write)(dest)
    }
}

/// A Linux scratch host and its directory.
pub fn scratch_host(name: &str) -> (Layout, PathBuf) {
    host_root(&format!("installer-envoy-{name}"))
}

/// The verbs' environment on a test host: every user is the test's own, so nothing is
/// switched, and the files are "root's" because the test owns them.
pub fn env<'a>(host: &'a Layout, runner: &'a FakeRunner, fetcher: &'a FakeFetcher) -> Env<'a> {
    Env {
        host,
        arch: Arch::X86_64,
        runner,
        fetcher,
        verify_as: me(),
        fetch_as: me(),
        owner_uid: me().uid,
        timeout: Duration::from_secs(5),
        cap: 1024 * 1024,
    }
}

/// `<host>/var/lib/kbf-installer/envoy`.
pub fn envoy_root(dir: &Path) -> PathBuf {
    dir.join("var/lib/kbf-installer/envoy")
}

/// The files directly in `dir`, by name.
pub fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// A manifest with `entry` as the `linux-x86_64` Envoy and `extra` in the entry's object.
pub fn manifest(version: &str, url: &str, sha256: &str) -> String {
    format!(
        "{{\"schema\":1,\"commit\":\"0f4a3825305cc79733ef098eae9725948eb5d6db\",\
         \"envoy\":{{\"linux-x86_64\":{{\"version\":\"{version}\",\"url\":\"{url}\",\
         \"sha256\":\"{sha256}\"}}}}}}"
    )
}
