//! Stand-ins for the tests of the apt verbs: S3 and CloudFront in memory, `gpg` that writes text
//! where a signature would be, and the files of a release.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

use super::index::sha256_hex;
use super::sign::Gpg;
use super::state::{PLATFORMS, asset_name};
use crate::channels::Error;
use crate::channels::aws::{Answer, Aws};
use kbf_testkit::dir::TestDir;

pub mod deb;

pub const BUCKET: &str = "apt-bucket";
pub const PRECONDITION: &str = "An error occurred (PreconditionFailed) when calling the PutObject operation: At least one of the pre-conditions you specified did not hold";
pub const NO_SUCH_KEY: &str = "An error occurred (NoSuchKey) when calling the GetObject operation: The specified key does not exist.";
pub const DENIED: &str =
    "An error occurred (AccessDenied) when calling the GetObject operation: Access Denied";

pub fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-apt-{name}"))
}

pub fn refused(stderr: &str) -> Answer {
    Answer {
        ok: false,
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

fn answer(stdout: &str) -> Answer {
    Answer {
        ok: true,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

pub fn etag(bytes: &[u8]) -> String {
    format!("\"{}\"", sha256_hex(bytes))
}

pub fn flag(args: &[String], name: &str) -> Option<String> {
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1).cloned()
}

type Store = BTreeMap<String, Vec<u8>>;
type Hook = Box<dyn FnOnce(&mut Store)>;

/// An S3 and a CloudFront in memory: objects under their keys, the conditions of `put-object`
/// judged as S3 judges them, every call recorded.
#[derive(Default)]
pub struct FakeAws {
    pub store: RefCell<Store>,
    pub calls: RefCell<Vec<Vec<String>>>,
    /// An answer for every call whose arguments, joined by spaces, contain the text.
    pub refusals: RefCell<Vec<(String, String)>>,
    /// Run once, just before a `put-object` whose key contains the text is judged.
    pub before_put: RefCell<Vec<(String, Hook)>>,
    /// What `get-object` of a missing key says.
    pub missing: RefCell<String>,
    /// What `get-object` and `put-object` print instead of an ETag.
    pub stdout: RefCell<Option<String>>,
}

impl FakeAws {
    pub fn new() -> FakeAws {
        let fake = FakeAws::default();
        *fake.missing.borrow_mut() = NO_SUCH_KEY.to_owned();
        fake
    }

    pub fn put(&self, key: &str, bytes: &[u8]) {
        self.store
            .borrow_mut()
            .insert(key.to_owned(), bytes.to_vec());
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.store.borrow().get(key).cloned()
    }

    pub fn refuse_when(&self, text: &str, stderr: &str) {
        self.refusals
            .borrow_mut()
            .push((text.to_owned(), stderr.to_owned()));
    }

    pub fn before_put_of(&self, key_part: &str, hook: Hook) {
        self.before_put
            .borrow_mut()
            .push((key_part.to_owned(), hook));
    }

    pub fn calls_of(&self, verb: &str) -> Vec<Vec<String>> {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c[1] == verb)
            .cloned()
            .collect()
    }

    pub fn keys_put(&self) -> Vec<String> {
        self.calls_of("put-object")
            .iter()
            .map(|c| flag(c, "--key").expect("a key"))
            .collect()
    }
}

impl Aws for FakeAws {
    fn run(&self, args: &[String]) -> Result<Answer, Error> {
        self.calls.borrow_mut().push(args.to_vec());
        let joined = args.join(" ");
        if let Some((_, stderr)) = self
            .refusals
            .borrow()
            .iter()
            .find(|(text, _)| joined.contains(text))
        {
            return Ok(refused(stderr));
        }
        let etag_json = |bytes: &[u8]| {
            self.stdout
                .borrow()
                .clone()
                .unwrap_or_else(|| format!("{{\"ETag\": {:?}}}", etag(bytes)))
        };
        match args[1].as_str() {
            "get-object" => {
                let key = flag(args, "--key").expect("a key");
                let Some(bytes) = self.get(&key) else {
                    return Ok(refused(&self.missing.borrow()));
                };
                std::fs::write(args.last().expect("a destination"), &bytes).expect("write");
                Ok(answer(&etag_json(&bytes)))
            }
            "put-object" => {
                let key = flag(args, "--key").expect("a key");
                let hooks = std::mem::take(&mut *self.before_put.borrow_mut());
                let (due, later): (Vec<_>, Vec<_>) = hooks
                    .into_iter()
                    .partition(|(part, _)| key.contains(part.as_str()));
                *self.before_put.borrow_mut() = later;
                for (_, hook) in due {
                    hook(&mut self.store.borrow_mut());
                }
                let body = std::fs::read(flag(args, "--body").expect("a body")).expect("body");
                let held = self.get(&key);
                let judged = match flag(args, "--if-match") {
                    Some(want) => held.as_deref().map(etag) == Some(want),
                    None => held.is_none(),
                };
                if !judged {
                    return Ok(refused(PRECONDITION));
                }
                self.put(&key, &body);
                Ok(answer(&etag_json(&body)))
            }
            // `cloudfront create-invalidation`: the tests read it from the recorded calls.
            _ => Ok(answer("{}")),
        }
    }
}

/// A `gpg` that writes text where a signature would be and checks nothing cryptographic.
#[derive(Default)]
pub struct FakeGpg {
    pub calls: RefCell<Vec<Vec<String>>>,
    /// An answer for every call whose arguments, joined by spaces, contain the text.
    pub refusals: RefCell<Vec<(String, String)>>,
}

impl FakeGpg {
    pub fn refuse_when(&self, text: &str, stderr: &str) {
        self.refusals
            .borrow_mut()
            .push((text.to_owned(), stderr.to_owned()));
    }
}

impl Gpg for FakeGpg {
    fn run(&self, args: &[String]) -> Result<Answer, Error> {
        self.calls.borrow_mut().push(args.to_vec());
        let joined = args.join(" ");
        if let Some((_, stderr)) = self
            .refusals
            .borrow()
            .iter()
            .find(|(text, _)| joined.contains(text))
        {
            return Ok(refused(stderr));
        }
        let signature = |mode: &str, whole: bool| {
            let input = std::fs::read_to_string(args.last().expect("an input")).expect("input");
            let key_ids: Vec<&String> = args
                .iter()
                .zip(args.iter().skip(1))
                .filter(|(a, _)| *a == "--local-user")
                .map(|(_, b)| b)
                .collect();
            let text = if whole {
                format!("-----BEGIN PGP SIGNED MESSAGE-----\n{key_ids:?}\n{input}")
            } else {
                format!("-----BEGIN PGP SIGNATURE-----\n{key_ids:?}\n")
            };
            std::fs::write(flag(args, "--output").expect("an output"), text).expect("write");
            assert!(args.iter().any(|a| a == mode));
        };
        if args.iter().any(|a| a == "--clearsign") {
            signature("--clearsign", true);
        } else if args.iter().any(|a| a == "--detach-sign") {
            signature("--detach-sign", false);
        }
        Ok(answer("ok"))
    }
}

/// The full commit the test releases are of.
pub const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The path in `dir` of the release package for the Debian architecture `arch`, as the release
/// asset is named.
pub fn asset(dir: &Path, arch: &str) -> std::path::PathBuf {
    let platform = PLATFORMS
        .iter()
        .find(|(a, _)| *a == arch)
        .map(|(_, p)| *p)
        .unwrap_or_default();
    dir.join(asset_name(COMMIT, platform))
}

/// The files of release `version` of [`COMMIT`]: its two packages, named as the release assets
/// (different bytes for each architecture).
pub fn release_files(dir: &Path, version: &str) {
    for arch in ["amd64", "arm64"] {
        let files = deb::files(arch.as_bytes(), b"notice");
        let bytes = deb::build(version, arch, &files);
        std::fs::write(asset(dir, arch), bytes).expect("write");
    }
}
