//! One build action, run as a job of `js-agent`: the task tool inside the job's container.
//!
//! The frontend leases a host, mints a presigned PUT for one key and a read grant for the action's
//! inputs, and starts a job whose arguments name both. The tool then
//! 1. reads the Action, its Command and its input root through the read grant ([`fetch`]), checking
//!    every blob's SHA-256 against its digest, and lays the input tree out under the working
//!    directory. The tree is read breadth-first with at most `--fetch-concurrency` requests in
//!    flight (32 by default) over pooled connections, and each file streams from the response to
//!    disk through its digest check instead of being held in memory. With `--fetch-batch`, blobs
//!    under 64 KiB are read in `/batch` requests of up to 256 digests. The grant is `https`
//!    unless `--allow-plain-http-grant` is given: a farm on its own network may serve it over
//!    plain HTTP, since the token is scoped to one action's inputs and expires. Without the flag
//!    an `http` grant URL is refused at startup (exit 14), before any request; either way the
//!    token is sent as a bearer header and is never logged;
//!    With `--locate` the tool first asks the listener's `GET /locate` once, and reads each input
//!    where the answer points (a byte range of a pack, or a whole large object, from the store the
//!    answer names, with the read grants it carries), checking the bytes as always and reading
//!    from the listener any blob whose direct read fails ([`fetch::LocatedSource`]); without the
//!    flag `/locate` is never asked. A line on standard error counts the blobs read each way;
//! 2. runs the Command there ([`run`]): its argv, environment and working directory, stdout and
//!    stderr to files, under a deadline. The environment is the Command's own, preceded by `HOME`,
//!    `TMPDIR`, `XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH`, which name fresh directories
//!    `private/{home,tmp,cache}` (the last holding `clang/ModuleCache`) under the working
//!    directory, mode 0700, made new for the run (an existing `private` is refused), beside the input root and not in it. A job's image root is read-only
//!    and its `/tmp` is a small `noexec` tmpfs, so a compiler that writes under `~` or the
//!    temporary directory would otherwise fail; and since outputs are collected relative to the
//!    input root, nothing written there is packed. A variable the Command sets itself keeps its
//!    value. A directory that cannot be made fails the run start (exit 11). The Command has no
//!    network: it runs in a user and network namespace of its own (only `lo`), where the host
//!    allows one, and a seccomp filter lets it and everything it starts make Unix sockets and,
//!    only when every namespace step succeeded, `AF_INET` and `AF_INET6` ones (loopback);
//!    otherwise Unix sockets only. No netlink, no io_uring. One line per action says which mode
//!    it ran in ([`network`]; this tool is non-dumpable from its first instruction, so the Command
//!    cannot take its sockets; the platform property `kbf-network=allow` lifts that for one action, and any
//!    other value of it fails the run start before the inputs are fetched);
//! 3. collects the declared outputs as REAPI does ([`outputs`]): files, directories as Trees,
//!    symlinks, the executable bit;
//! 4. plans where every output blob, stdout, stderr and Tree goes ([`plan`]) and writes them into
//!    one or more kbf-segments objects ([`pack`]) and, for a blob over the record cap, large
//!    objects, the manifest as the record at offset 0 of pack 0, with the shared task pack types
//!    of `kbf-task-pack` (`TaskManifest`, `write_pack_with`, `read_manifest_with`), the same code
//!    the frontend that ingests the packs uses. The manifest is validated before anything is
//!    stored, and pack 0 is read back through the shape check ingest applies to its footer: a
//!    manifest or a pack ingest would refuse fails the task (exit 12) instead of being stored;
//! 5. PUTs each object to its presigned URL ([`put`]): one create-only PUT per object, repeated only after a
//!    503 (the store's hot tier is full or it is taking as many puts as it may, and nothing was
//!    stored), at most seven more times, each after a wait of between half and one and a half
//!    times the store's `Retry-After` (whole seconds, taken as at most 30; 2 s when it names
//!    none), chosen at random for each wait. A connection that broke after it was made (a store
//!    that answers 503 and closes while the body is still being sent) is retried the same way.
//!    Every other answer, 412 included, is final (exit 13; a 412 that follows an interrupted
//!    attempt of the same PUT writes the line `put: 412 after an interrupted attempt of this
//!    put; the earlier attempt may have stored it: ...`, so that it can be counted, and a
//!    first-attempt 412 the line `put: the object already exists at ...`); 429, 500, 502 and 504
//!    are deliberately not retried. The retries do not look at the URL's expiry: the task's own waits add up to at
//!    most 60 s, after which the 503 is the failure, but each refused attempt can also wait at
//!    the store for a put permit before it is answered (15 s by default), so the last attempt
//!    begins at most about 165 s after the first by default; the URL is good for the run's wall
//!    limit plus the frontend's slack (`--task-url-slack-secs`), which must cover that. A retry
//!    the store gets after the URL expired is refused (403), which is final, so the task fails
//!    (exit 13) and nothing is stored.
//!
//! `js-agent` returns nothing but an exit code, so the manifest travels inside the pack, and the exit
//! code says only whether the farm did its part: 0 when the action ran and its pack was stored
//! (whatever the action's own exit code, which is in the manifest), nonzero when fetching, packing or
//! storing failed. Nothing the tool writes is trusted: the frontend hashes every record of the pack,
//! checks its footer, and builds the action's result from its own hashing.
//!
//! # The v2 key set
//!
//! A run is issued a fixed key set: pack URLs and large-object URLs (`--put-urls-file`, or
//! `--put-urls` with `--insecure-argv-secrets`; [`put_urls`]). The v1 `--pack-url-file` is a key
//! set of one pack and no large object. After the outputs are collected, [`plan::plan`] places
//! every blob: one over the record cap is a large object (PUT as raw bytes by [`put::put_reader`],
//! streamed from the file it was read from, never held whole), the others fill packs 1.. and then
//! pack 0, which keeps room for the manifest. More large blobs or packs than the key set has, or
//! a blob over the large cap, fails the task as `output-too-large` (exit 12) before anything is
//! stored. Pack 0 is built, validated and read back first and every other pack is built once to check it can be, one at a time, with each blob checked against its digest (a file changed after it was hashed fails the task as `output-changed`, exit 12); then the PUTs go in the order large
//! objects, packs `N-1` down to 1, and pack 0 last, so pack 0, which holds the manifest at offset
//! 0, is the commit point. Any refused PUT fails the task (exit 13) and later PUTs are not made.
//! A run that needs one pack and no large object writes the manifest as version 1 and the same
//! pack bytes as before.
//!
//! # What a failure means
//!
//! - A 412 means the pack key already holds an object, and it is final: the frontend mints a new
//!   key for every attempt at a lease, so a re-attempt of the task does not meet the old key.
//! - The `exit_code`, `timing` and `usage` in the manifest are written by this tool, which runs
//!   as the same user as the Command, so they are claims. Ingest does not trust them.
//! - The tool builds packs in memory. While pack 0 is built, the blobs read for it, the segment
//!   writer's buffer and the finished pack are each about the size of a pack. Pack 0 is then kept,
//!   and each other pack is built twice (once to check it before anything is stored, once to PUT
//!   it), one at a time, each time from the blobs read for it (every blob is checked against its
//!   digest as it is read) and the pack built from them. So the peak is about pack 0 plus two
//!   packs: three packs, about 384 MiB at the 128 MiB pack limit, and the blobs of every pack but
//!   pack 0 are read twice. A large object is streamed from its file and is never held. A job's
//!   memory limit must allow for it; a task killed for passing it ends with 137, not with the
//!   pack code 12.
//!
//! # What the tool does not separate
//!
//! The Command runs as a child of the tool, under the same user. The presigned PUT URL is read
//! from `--pack-url-file` or `--put-urls-file`, not from argv, and the file is left in place for the executor that
//! delivered it to remove; the Command can read it, and could copy it into a declared output,
//! which would then be stored in the content-addressable store. That is allowed: the URL permits
//! one create-only PUT of this task's own pack key and nothing else, so what the Command learns
//! from it, it could already produce as an output. The grant token is read from
//! `--grant-token-file`, which is not removed, and its path stays in the tool's argv: the job's
//! sandbox must keep that file and the tool's `/proc` entry out of the Command's reach (a
//! separate user or a separate process namespace), or the Command can read the token. The flag
//! forms `--pack-url` and `--grant-token`, accepted only with `--insecure-argv-secrets`
//! (komira-ai/kbf#1934), put the values in argv for every process on the host to read; they are
//! a stopgap until the job scheduler delivers job secrets as files, and are removed then. A
//! process of the Command that leaves its process group (for example with `setsid`) survives the
//! group kill at the deadline; only such a sandbox ends it.
//!
//! # The confinement probe
//!
//! `kbf-task --probe-confinement` runs a Command under the same confinement as an action's and
//! prints whether it holds, one `probe <name>: ok|FAIL|n/a` line per check ([`probe`]). It needs
//! no grant, server or network.

pub mod digest;
pub mod fetch;
pub mod network;
pub mod outputs;
pub mod pack;
pub mod plan;
pub mod probe;
pub mod put;
pub mod put_urls;
pub mod run;
pub mod task;

#[cfg(test)]
pub(crate) mod fake_http;

// The feature that lets a denied Command run with the network where no confine exists is for tests:
// a build for a system that is neither Linux nor macOS, without debug assertions (a release build),
// that turns it on does not compile. On Linux and macOS, which have a confine, the feature does
// nothing, so it is allowed there (a workspace-wide `--all-features` build).
#[cfg(all(
    feature = "unconfined-for-tests",
    not(target_os = "linux"),
    not(target_os = "macos"),
    not(debug_assertions),
    not(test)
))]
compile_error!(
    "the `unconfined-for-tests` feature of kbf-task is for test builds only; a release build must not enable it"
);
