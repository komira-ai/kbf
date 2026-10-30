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
//!    stderr to files, under a deadline. The environment is the Command's own, preceded by `HOME`,
//!    `TMPDIR`, `XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH`, which name fresh directories
//!    `private/{home,tmp,cache}` (the last holding `clang/ModuleCache`) under the working
//!    directory, mode 0700, made new for the run (an existing `private` is refused), beside the input root and not in it. A job's image root is read-only
//!    and its `/tmp` is a small `noexec` tmpfs, so a compiler that writes under `~` or the
//!    temporary directory would otherwise fail; and since outputs are collected relative to the
//!    input root, nothing written there is packed. A variable the Command sets itself keeps its
//!    value. A directory that cannot be made fails the run start (exit 11);
//! 3. collects the declared outputs as REAPI does ([`outputs`]): files, directories as Trees,
//!    symlinks, the executable bit;
//! 4. writes every output blob, stdout, stderr and Tree, and the manifest, into ONE kbf-segments
//!    object ([`pack`]), the manifest as the record at offset 0, with the shared task pack types
//!    of `kbf-task-pack` (`TaskManifest`, `write_pack`, `read_manifest`), the same code the
//!    frontend that ingests the pack uses. The manifest is validated before anything is stored,
//!    and the pack is read back through the shape check ingest applies to its footer: a manifest
//!    or a pack ingest would refuse fails the task (exit 12) instead of being stored;
//! 5. PUTs that object to the presigned URL ([`put`]): one create-only PUT, repeated only after a
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
//! # The v2 key set (in progress)
//!
//! A v2 run is issued a fixed key set: several pack URLs and several large-object URLs
//! (`--put-urls-file`, or `--put-urls` with `--insecure-argv-secrets`; [`put_urls`]). Three
//! pieces for it exist, and only the first is used:
//!
//! - the key set flags, parsed and validated at startup. **Used, in part:** only the first pack
//!   slot, exactly as v1 uses its one pack URL; the rest of the set is checked and ignored;
//! - [`put::put_file`], a create-only PUT streamed from a file, for a blob over the record cap.
//!   **Not used yet:** nothing calls it;
//! - [`plan::plan`], the pure placement of output blobs into packs and large objects. **Not used
//!   yet:** nothing calls it, and the manifest has no field to name the packs or large objects,
//!   so a run still writes the one v1 pack.
//!
//! # What a failure means
//!
//! - A 412 means the pack key already holds an object, and it is final: the frontend mints a new
//!   key for every attempt at a lease, so a re-attempt of the task does not meet the old key.
//! - The `exit_code`, `timing` and `usage` in the manifest are written by this tool, which runs
//!   as the same user as the Command, so they are claims. Ingest does not trust them.
//! - The tool builds the pack in memory: the collected blobs, the segment writer's buffer and the
//!   finished pack are each about the size of the pack, so the peak is about three times the pack
//!   (about 384 MiB at the 128 MiB pack limit). A job's memory limit must allow for it; a task
//!   killed for passing it ends with 137, not with the pack code 12.
//!
//! # What the tool does not separate
//!
//! The Command runs as a child of the tool, under the same user. The presigned PUT URL is read
//! from `--pack-url-file`, not from argv, and the file is left in place for the executor that
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
