//! One build action, run as a job of `js-agent`: the task tool inside the job's container.
//!
//! The frontend leases a host, mints a presigned PUT for one key and a read grant for the action's
//! inputs, and starts a job whose arguments name both. The tool then
//! 1. reads the Action, its Command and its input root through the read grant ([`fetch`]), checking
//!    every blob's SHA-256 against its digest, and lays the input tree out under the working
//!    directory. The tree is read breadth-first with at most `--fetch-concurrency` requests in
//!    flight (32 by default) over pooled connections, and each file streams from the response to
//!    disk through its digest check instead of being held in memory;
//! 2. runs the Command there ([`run`]): its argv, environment and working directory, stdout and
//!    stderr to files, under a deadline;
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
//!    503 (the store's hot tier is full and nothing was stored), at most three more times, each
//!    after the wait the store's `Retry-After` names (whole seconds, at most 30; 2 s when it names
//!    none). Every other answer, 412 included, is final. The retries do not look at the URL's
//!    expiry: they wait at most 90 s in all, and a retry the store gets after the URL expired is
//!    refused (403), which is final, so the task fails (exit 13) and nothing is stored.
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

pub mod digest;
pub mod fetch;
pub mod outputs;
pub mod pack;
pub mod plan;
pub mod put;
pub mod put_urls;
pub mod run;
pub mod task;
