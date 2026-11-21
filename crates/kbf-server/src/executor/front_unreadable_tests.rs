use std::sync::Arc;

use bytes::Bytes;
use kbf_front::{Backend, Cache, CacheStorage, MemoryMetaLog, SegmentsConfig, VerifiedBlob};
use kbf_objstore::MemoryStore;
use kbf_proto::reapi::{Action, ActionResult, Command, Directory, DirectoryNode, FileNode};
use prost::Message as _;

use super::{MAX_NAMED, MAX_STDERR_BYTES, below_root, named, unreadable_input};

/// The incident's line, as the toolchain's clang (on musl) writes it.
const LINE: &str = "fatal error: cannot open file 'buck-out/zig/include/__assert': I/O error";

/// The path [`LINE`] names.
const HEADER: &str = "buck-out/zig/include/__assert";

/// Catches a line that is not exactly the compiler's could-not-open line with the text of EIO
/// being read as one, or one that is being missed: both spellings of EIO, after nothing or a
/// location, with a carriage return; never another error text, another diagnostic, text after
/// the error, an empty path, or a line that is not UTF-8.
#[test]
fn only_the_compilers_could_not_open_line_with_eio_names_a_path() {
    let stderr = [
        LINE,
        "src/a.cc:3:10: fatal error: cannot open file 'inc/b.h': Input/output error\r",
        "fatal error: cannot open file 'it's.h': I/O error",
        "fatal error: cannot open file 'c.h': No such file or directory",
        "fatal error: cannot open file 'd.h': Permission denied",
        "error: cannot open file 'e.h': I/O error",
        "note fatal error: cannot open file 'f.h': I/O error",
        "fatal error: cannot open file 'g.h': I/O error (os error 5)",
        "fatal error: cannot open file '': I/O error",
        "fatal error: cannot open file 'h.h' I/O error",
        "In file included from x.cc:1: something else",
    ]
    .join("\n");
    let mut bytes = stderr.into_bytes();
    bytes.extend_from_slice(b"\n\xff fatal error: cannot open file 'i.h': I/O error\n");
    assert_eq!(named(&bytes), vec![HEADER, "inc/b.h", "it's.h"]);
    assert!(named(b"").is_empty());
}

/// Catches an unbounded scan: at most [`MAX_NAMED`] paths are taken, the first ones.
#[test]
fn at_most_the_first_few_paths_are_taken() {
    let stderr: Vec<String> = (0..MAX_NAMED + 2)
        .map(|i| format!("fatal error: cannot open file '{i}.h': I/O error"))
        .collect();
    let taken = named(stderr.join("\n").as_bytes());
    let first: Vec<String> = (0..MAX_NAMED).map(|i| format!("{i}.h")).collect();
    assert_eq!(taken, first);
}

/// Catches a path read outside the input root: a relative path is joined to the working
/// directory, `.` and empty components are dropped, an absolute one is taken only under the exec
/// root, and `..`, nothing, or too deep a path is not looked at.
#[test]
fn a_path_is_resolved_below_the_input_root_or_not_at_all() {
    let parts = |v: &[&str]| Some(v.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>());
    assert_eq!(below_root("", "a/b.h"), parts(&["a", "b.h"]));
    assert_eq!(
        below_root("pkg", "./inc//b.h"),
        parts(&["pkg", "inc", "b.h"])
    );
    assert_eq!(below_root("pkg", "/kbf-work/a/b.h"), parts(&["a", "b.h"]));
    assert_eq!(below_root("", "../b.h"), None);
    assert_eq!(below_root("pkg/..", "b.h"), None);
    assert_eq!(below_root("", "/usr/include/b.h"), None);
    assert_eq!(below_root("", "/kbf-work"), None);
    assert_eq!(below_root("", "."), None);
    assert_eq!(below_root("", &"d/".repeat(256)), parts(&["d"; 256]));
    assert_eq!(below_root("", &"d/".repeat(257)), None);
}

/// A CAS in memory, and what it holds.
struct Cas {
    cache: Arc<Cache<MemoryMetaLog, MemoryStore>>,
    backend: Backend,
}

impl Cas {
    fn new() -> Self {
        let cache = Arc::new(Cache::memory());
        let node = Arc::new(CacheStorage::new(
            Arc::clone(&cache),
            SegmentsConfig::DEFAULT,
        ));
        Self {
            cache,
            backend: Backend::remote(node),
        }
    }

    /// Stores `bytes`; their digest.
    async fn put(&self, bytes: Vec<u8>) -> kbf_types::Digest {
        let blob = VerifiedBlob::hashed(Bytes::from(bytes));
        let digest = blob.digest();
        self.cache.store_blobs(vec![blob]).await.expect("stored");
        digest
    }

    async fn store(&self, message: &impl prost::Message) -> kbf_proto::reapi::Digest {
        kbf_front::digest_to_proto(&self.put(message.encode_to_vec()).await)
    }

    /// A directory holding `files` and `dirs`, stored.
    async fn dir(
        &self,
        files: &[&str],
        dirs: Vec<(&str, kbf_proto::reapi::Digest)>,
    ) -> kbf_proto::reapi::Digest {
        let directory = Directory {
            files: files
                .iter()
                .map(|name| FileNode {
                    name: (*name).to_owned(),
                    ..FileNode::default()
                })
                .collect(),
            directories: dirs
                .into_iter()
                .map(|(name, digest)| DirectoryNode {
                    name: name.to_owned(),
                    digest: Some(digest),
                })
                .collect(),
            ..Directory::default()
        };
        self.store(&directory).await
    }

    /// The action whose input root holds `buck-out/zig/include/__assert` and `pkg/inc/b.h`,
    /// run from `working_directory`.
    async fn action(&self, working_directory: &str) -> kbf_types::Digest {
        let include = self.dir(&["__assert"], Vec::new()).await;
        let zig = self.dir(&[], vec![("include", include)]).await;
        let buck_out = self.dir(&[], vec![("zig", zig)]).await;
        let inc = self.dir(&["b.h"], Vec::new()).await;
        let pkg = self.dir(&[], vec![("inc", inc)]).await;
        let root = self
            .dir(&[], vec![("buck-out", buck_out), ("pkg", pkg)])
            .await;
        let command = Command {
            working_directory: working_directory.to_owned(),
            ..Command::default()
        };
        let action = Action {
            command_digest: Some(self.store(&command).await),
            input_root_digest: Some(root),
            ..Action::default()
        };
        self.put(action.encode_to_vec()).await
    }
}

/// A result of exit code `exit_code` with `stderr` inline.
fn failed(exit_code: i32, stderr: &str) -> ActionResult {
    ActionResult {
        exit_code,
        stderr_raw: stderr.as_bytes().to_vec(),
        ..ActionResult::default()
    }
}

/// Catches an action's own failure taken for the host's, or the host's missed: the path is
/// named only when it is a file of the input root, as named from the working directory, the
/// action exited nonzero, and its standard error, inline or in the CAS, holds the line.
#[tokio::test]
async fn a_declared_input_named_by_the_line_is_the_answer() {
    let cas = Cas::new();
    let action = cas.action("").await;
    let read = |result: ActionResult| {
        let (backend, action) = (cas.backend.clone(), action);
        async move { unreadable_input(&backend, &action, &result).await }
    };
    assert_eq!(read(failed(1, LINE)).await.as_deref(), Some(HEADER));
    assert_eq!(
        read(failed(0, LINE)).await,
        None,
        "an exit 0 is the action's"
    );
    let in_pkg = "fatal error: cannot open file 'inc/b.h': I/O error";
    assert_eq!(read(failed(1, in_pkg)).await, None, "not from the root");
    let missing = "fatal error: cannot open file 'buck-out/zig/include/__config': I/O error";
    assert_eq!(read(failed(1, missing)).await, None, "not an input");
    let directory = "fatal error: cannot open file 'buck-out/zig/include': I/O error";
    assert_eq!(read(failed(1, directory)).await, None, "a directory");
    let no_parent = "fatal error: cannot open file 'buck-out/clang/include/x.h': I/O error";
    assert_eq!(read(failed(1, no_parent)).await, None, "no such directory");
    let outside = "fatal error: cannot open file '../etc/x.h': I/O error";
    assert_eq!(read(failed(1, outside)).await, None, "outside the root");
    let second = format!("{missing}\n{outside}\n{LINE}\n");
    assert_eq!(read(failed(1, &second)).await.as_deref(), Some(HEADER));
    let enoent =
        "fatal error: cannot open file 'buck-out/zig/include/__assert': No such file or directory";
    assert_eq!(read(failed(1, enoent)).await, None, "a missing file");
    assert_eq!(read(failed(1, "")).await, None, "no standard error");

    let from_pkg = cas.action("pkg").await;
    let result = failed(1, in_pkg);
    let answer = unreadable_input(&cas.backend, &from_pkg, &result).await;
    assert_eq!(answer.as_deref(), Some("inc/b.h"));
}

/// Catches a standard error read past its bound or not read from the CAS: one stored there is
/// read, one larger than [`MAX_STDERR_BYTES`] or not stored is not, and none answers `None`.
#[tokio::test]
async fn a_standard_error_in_the_cas_is_read_within_its_bound() {
    let cas = Cas::new();
    let action = cas.action("").await;
    let stored = cas.put(LINE.as_bytes().to_vec()).await;
    let with = |digest: kbf_proto::reapi::Digest| ActionResult {
        exit_code: 1,
        stderr_digest: Some(digest),
        ..ActionResult::default()
    };
    let result = with(kbf_front::digest_to_proto(&stored));
    let answer = unreadable_input(&cas.backend, &action, &result).await;
    assert_eq!(answer.as_deref(), Some(HEADER));

    let mut large = kbf_front::digest_to_proto(&stored);
    large.size_bytes = i64::try_from(MAX_STDERR_BYTES + 1).expect("fits");
    let answer = unreadable_input(&cas.backend, &action, &with(large)).await;
    assert_eq!(answer, None, "over the bound");

    let mut negative = kbf_front::digest_to_proto(&stored);
    negative.size_bytes = -1;
    let answer = unreadable_input(&cas.backend, &action, &with(negative)).await;
    assert_eq!(answer, None, "not a size");

    let absent = cas.put(Vec::new()).await;
    let mut absent = kbf_front::digest_to_proto(&absent);
    absent.hash = "ab".repeat(32);
    absent.size_bytes = 7;
    let answer = unreadable_input(&cas.backend, &action, &with(absent)).await;
    assert_eq!(answer, None, "not in the CAS");

    let mut bad = kbf_front::digest_to_proto(&stored);
    bad.hash = "not hex".to_owned();
    let answer = unreadable_input(&cas.backend, &action, &with(bad)).await;
    assert_eq!(answer, None, "not a digest");

    let none = ActionResult {
        exit_code: 1,
        ..ActionResult::default()
    };
    assert_eq!(unreadable_input(&cas.backend, &action, &none).await, None);
}

/// Catches an action, a command or a tree that cannot be read being taken as naming the input:
/// any of them missing from the CAS, absent, or not decoding answers `None`.
#[tokio::test]
async fn what_cannot_be_read_names_nothing() {
    let cas = Cas::new();
    let result = failed(1, LINE);
    let ask = |action: kbf_types::Digest| {
        let (backend, result) = (cas.backend.clone(), result.clone());
        async move { unreadable_input(&backend, &action, &result).await }
    };
    let unstored = kbf_types::Digest::parse(
        kbf_types::DigestFunction::Sha256,
        &format!("{}/9", "cd".repeat(32)),
    )
    .expect("a digest");
    assert_eq!(ask(unstored).await, None, "no action");
    assert_eq!(
        ask(cas.put(vec![0xff, 0xff]).await).await,
        None,
        "not an action"
    );

    let root = cas.dir(&[], Vec::new()).await;
    let no_command = Action {
        input_root_digest: Some(root.clone()),
        ..Action::default()
    };
    assert_eq!(ask(cas.put(no_command.encode_to_vec()).await).await, None);

    let command = cas.store(&Command::default()).await;
    let no_root = Action {
        command_digest: Some(command.clone()),
        ..Action::default()
    };
    assert_eq!(ask(cas.put(no_root.encode_to_vec()).await).await, None);

    let mut gone = root.clone();
    gone.hash = "ef".repeat(32);
    let dangling = cas.dir(&[], vec![("buck-out", gone)]).await;
    let dangling = Action {
        command_digest: Some(command),
        input_root_digest: Some(dangling),
        ..Action::default()
    };
    assert_eq!(
        ask(cas.put(dangling.encode_to_vec()).await).await,
        None,
        "a directory not in the CAS"
    );
}
