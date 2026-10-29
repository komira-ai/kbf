//! One build action, run as a job of `js-agent`: the task tool inside the job's container.
//!
//! The frontend leases a host, mints a presigned PUT for one key and a read grant for the action's
//! inputs, and starts a job whose arguments name both. The tool then
//! 1. reads the Action, its Command and its input root through the read grant ([`fetch`]), checking
//!    every blob's SHA-256 against its digest, and lays the input tree out under the working
//!    directory;
//! 2. runs the Command there ([`run`]): its argv, environment and working directory, stdout and
//!    stderr to files, under a deadline;
//! 3. collects the declared outputs as REAPI does ([`outputs`]): files, directories as Trees,
//!    symlinks, the executable bit;
//! 4. writes every output blob, stdout, stderr and Tree, and the [`manifest`], into ONE kbf-segments
//!    object ([`pack`]), the manifest as the record at offset 0;
//! 5. PUTs that object once to the presigned URL ([`put`]).
//!
//! `js-agent` returns nothing but an exit code, so the manifest travels inside the pack, and the exit
//! code says only whether the farm did its part: 0 when the action ran and its pack was stored
//! (whatever the action's own exit code, which is in the manifest), nonzero when fetching, packing or
//! storing failed. Nothing the tool writes is trusted: the frontend hashes every record of the pack,
//! checks its footer, and builds the action's result from its own hashing.
//!
//! # What the tool does not separate
//!
//! The Command runs as a child of the tool, under the same user. The presigned PUT URL is read
//! from `--pack-url-file` and the file is removed before the Command starts, so it is neither in
//! argv nor on disk while the Command runs. The grant token is read from `--grant-token-file`, which
//! is not removed, and its path stays in the tool's argv: the job's sandbox must keep that file and the
//! tool's `/proc` entry out of the Command's reach (a separate user or a separate process
//! namespace), or the Command can read the token. A process of the Command that leaves its process
//! group (for example with `setsid`) survives the group kill at the deadline; only such a sandbox
//! ends it. The tool holds each collected blob in memory twice and the pack once more, so its peak
//! is about three times the pack limit.

pub mod fetch;
pub mod manifest;
pub mod outputs;
pub mod pack;
pub mod put;
pub mod run;
pub mod task;
