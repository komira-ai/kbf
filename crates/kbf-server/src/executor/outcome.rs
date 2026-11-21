//! How a job ends, and what the operation's callers are answered.
//!
//! The job service reports a job's state, its exit code, why it ended and, for one that did
//! not start, a code; nothing else. So the task tool's own exit status carries the contract:
//! exit 0 means the action ran, whatever its own exit code, and the pack holds the rest. Any
//! other ending says why the action did not run, and the executor submits the operation's next
//! attempt or answers its callers ([`Settled::retried`], [`Settled::status`]).
//!
//! **The promotion rule.** A job's pack is read, and its outputs promoted to CAS and
//! action-cache entries, only from a clean result: the job's state is succeeded, and its
//! result's state is succeeded, its reason is exit and its exit code is 0. A result that says
//! anything else, or leaves any of it out, is not clean, however complete the pack the job
//! already wrote: the pack is never read (its key is left to the janitor) and the row's class
//! decides the rest. Nothing written to the store is promoted for having been written.
//!
//! Each row's class is what the code below decides, and a test reads this table and holds the
//! code to it:
//!
//! | the job | class | what follows |
//! |---|---|---|
//! | succeeded, its result succeeded, reason exit, exit code 0 | `ingest` | whatever the frontend's ingest of the pack says, and an action that exited nonzero because its host could not read a declared input is run again ([`InputFailure`]) |
//! | any state but failed to start, its result naming an input failure (`input_failure`, or reason input-failed) | `input-failed` | `UNAVAILABLE` naming the host and the agent's code: the host could not provide or read the job's inputs, rerun |
//! | failed to start, its result naming an input failure | `input-failed` | `UNAVAILABLE` naming the host and the agent's code: the host could not provide the job's inputs, rerun |
//! | succeeded with a nonzero exit code | `infra` | `UNAVAILABLE`: infrastructure, rerun |
//! | succeeded with no exit code | `infra` | `UNAVAILABLE`: not a clean result, rerun |
//! | succeeded for a reason other than exit | `infra` | `UNAVAILABLE`: not a clean result, rerun |
//! | succeeded, its result in another state | `infra` | `UNAVAILABLE`: not a clean result, rerun |
//! | failed, reason wall limit | `timeout` | `DEADLINE_EXCEEDED`: the action's own timeout, since nothing cuts a job's wall limit |
//! | failed, reason memory | `out-of-memory` | killed at its own limit: rerun, sized again from its class's history; at the last attempt `FAILED_PRECONDITION` with reason `ACTION_OUT_OF_MEMORY` |
//! | failed, reason exit | `infra` | `UNAVAILABLE`: the task tool exited nonzero, rerun |
//! | failed, any other reason | `infra` | `UNAVAILABLE`: rerun |
//! | cancelled by anyone else | `infra` | `UNAVAILABLE`: rerun |
//! | lost | `lost` | the container is gone with no result, the host's fault: rerun |
//! | not found after it was seen | `lost` | the service no longer knows the id: rerun |
//! | failed to start quota-unavailable | `lost` | the host's scratch quota is out of order: rerun |
//! | failed to start files-unsupported | `infra` | `UNAVAILABLE`: the host cannot write the files of a job, rerun |
//! | failed to start invalid-file | `invalid` | `INVALID_ARGUMENT`: a file of the job is ours to fix |
//! | failed to start no-capacity | `infra` | `UNAVAILABLE`: rerun |
//! | failed to start start-timeout | `infra` | `UNAVAILABLE`: rerun |
//! | failed to start exclusive-not-ready | `infra` | `UNAVAILABLE`: rerun |
//! | failed to start input-unavailable | `infra` | `UNAVAILABLE`: a host path the job named could not be read on its host, rerun |
//! | failed to start host-unsafe | `infra` | `UNAVAILABLE`: rerun |
//! | failed to start, any code, its detail naming an object of its tree the store answered 404 for | `infra` | `UNAVAILABLE`: the frontend drops what its memos say of the object and tells its packs of the absence, and the next attempt's tree is checked against the store, rerun |
//! | failed to start, any code, its detail saying the host refused the job's tree list as not mountable (`tree list not supported: <reason>`) | `list-refused` | `UNAVAILABLE`: the frontend takes its miss path, and the next attempt's tree is written again (the first time) or as manifests (after); the run is rerun without spending an attempt, up to [`LIST_REFUSALS`] times in an operation |
//! | failed to start egress-unchecked | `infra` | `UNAVAILABLE`: the host's agent could not check the limits of the job's network, rerun |
//! | failed to start, any other code or detail | `infra` | `UNAVAILABLE`: rerun |
//! | in a state this server does not know | `infra` | `UNAVAILABLE`: rerun |
//! | refused at submit no-capable-host | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit wall-time-not-positive | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit wall-time-above-ceiling | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit job-id-conflict | `infra` | `UNAVAILABLE`: another spec holds the id, rerun under the next attempt's id |
//! | refused at submit as written (`INVALID_ARGUMENT`) | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit with a code this server does not know | `infra` | `UNAVAILABLE`: rerun |
//! | refused at submit otherwise | `infra` | `UNAVAILABLE`: rerun |
//!
//! A submit the job service did not take because it is busy or cannot be reached (no answer
//! within the call's limit, a broken or reset connection, `RESOURCE_EXHAUSTED` or another status
//! of its load: [`busy`](super::busy)) is not in the table: it decides nothing, and the same
//! submit is sent again, under the same id, until the service answers or a caller cancels; it
//! spends no attempt.
//!
//! Every `infra`, `input-failed` and `lost` row is run again, as the operation's next attempt, at most
//! [`RESUBMITS`] times; then the callers are answered `UNAVAILABLE`. So is `out-of-memory`, and
//! the callers are answered as the table says. A `list-refused` row is run again too, but its
//! first [`LIST_REFUSALS`] in an operation spend none of those attempts. A status names the code, never the job: a job
//! carries the arguments and the mounts. The agent's detail of a start it refused is a
//! code (`quota-unavailable`, ...) or text that can quote those: only the code is
//! read, from the start of the detail, and only the detail's length is logged.
//!
//! **A lost object.** The detail of a start that failed on any code is also read for one thing
//! more: 64 lower-case hex digits right before `: upstream 404`, which is how tierstore says the
//! store has no object of a SHA-256 the job's tree names (`cannot fetch manifest <sha256>:
//! upstream 404`) ([`lost_object`]); or else an object key under `cas/` right after
//! `source missing: `, which is how tierstore says the store has no object a span of the tree is
//! read from (`404 source missing: <key>`) ([`source_missing`]). The store is a general-purpose
//! one, and its objects can vanish while the frontend's state still names them; and a store can
//! answer "not found" for a while for an object it holds. Only those digits or that key are
//! taken, and logged; the callers get a fixed phrase. The run is `infra`
//! ([`Verdict::StoreLost`]): before the next attempt the frontend drops what its memos say of
//! the object, tells its packs of the absence (a pack is forgotten only once a second look, some
//! time after the first absence, confirms it), and writes that attempt's tree checked against
//! the store, so a pack forgotten has its blobs reported missing ([`Settled::Missing`]), which
//! the client uploads again.
//!
//! **A refused tree list.** A start that failed on any code is read for one more thing, before
//! the lost object: the text `tree list not supported: ` anywhere in the detail
//! ([`list_refused`]), which is the `Display` of tierstore's `ListVersionUnsupported`, said by a
//! node whose reader refuses a version 2 tree list that has no readable L2 sidecar
//! (`cas/l2/<id>`; the reason follows: `v1_list`, `missing_sidecar`, `unknown_cell_digest` or
//! `unknown_version`; tierstore's `docs/design/fetch-cache-serve.md`, 3.5). The tree is not
//! mountable through tierstore, which is not the action's fault and not the host's: the run is
//! `list-refused` ([`Verdict::ListRefused`]) and the frontend takes its miss path
//! ([`Ingest::list_refused`](super::Ingest::list_refused)): first it writes the action's tree
//! list again with its sidecar republished, then it gives the action's jobs the tree as
//! manifests, which tierstore's file system serves without the mount provider. Such a run does
//! not spend an attempt of [`RESUBMITS`]; the operation has [`LIST_REFUSALS`] of them, and a
//! refusal after those is run again as any `infra` ending. The text is read before
//! [`lost_of`]: when a detail holds both, the tree list is what the host could not use, and the
//! lost object, if there is one, is found again by the next attempt's refusal.
//!
//! **An input the host could not read.** A job service result says the job's inputs failed on its
//! host with `input_failure` (the agent's code: `input-unreadable`, `host-port-lost`), its reason
//! then being input-failed. That is the host's failure whatever the job's state, reason and exit,
//! so it is read before anything else of the result ([`input_failed`]): it is read before a wall
//! limit, a memory kill or a cancel by another client too, and the run is `input-failed`
//! ([`Settled::InputFailed`]): run again as an `infra` ending is, and at the last attempt
//! answered `UNAVAILABLE` with a message that names the host and the code. Only the code's leading
//! run of lower-case letters, digits and dashes is taken. A start that failed is read for the
//! refused tree list and the lost object first, which the frontend acts on. The job service at
//! this revision cannot be asked to place the next attempt away from the host, so the next job
//! goes where the service places it.
//!
//! The agent sets `input_failure` only on a job that did not succeed, and the task tool exits 0
//! whenever the action ran, so an action whose compiler could not read a declared input because
//! the host's tree answered EIO comes back as a clean result with the action's exit code 1.
//! Until the host reports that itself, the frontend's ingest is asked about such a result too
//! ([`Ingest::unreadable_input`](super::Ingest::unreadable_input)): when the action's standard
//! error holds the compiler's own could-not-open line for one of its declared input files with
//! the text of EIO, the run is `input-failed` as above, naming that file and the host.
//!
//! What a job that did not end well wrote goes with the status as a detail, cut to a cap
//! ([`output`](super::output)); the code and the message above are unchanged by it.
//!
//! A job this executor cancelled for its callers is not judged here: they are answered
//! `CANCELLED`.

use js_proto::v1::{Job, JobResult, JobState, RefusalCode, ResultReason, StartFailureCode};
use kbf_front::ERROR_DOMAIN;
use kbf_job_client::Error;
use kbf_proto::google::rpc::{self, ErrorInfo};
use prost::Message;
use tonic::Code;

use super::busy::busy;
use super::traits::{LostObject, MissPath};

/// How many times an operation is submitted again after a run that ended for a reason of the
/// farm's (`infra`, `lost`, `out-of-memory`): each is a new attempt, a new job and a new key.
pub(crate) const RESUBMITS: u32 = 2;

/// How many runs of one operation that ended in a refused tree list are run again without
/// spending an attempt of [`RESUBMITS`]: the first has the frontend write the list again with its
/// L2 sidecar, the second has it give the action's jobs the tree as manifests. A refusal after
/// these is an ordinary `infra` ending. Counted by the operation's task, from zero: an operation
/// taken up from the state store starts its count again.
pub(crate) const LIST_REFUSALS: u32 = 2;

/// The `ErrorInfo.reason` of an action killed at its memory limit at its last attempt.
pub const OUT_OF_MEMORY_REASON: &str = "ACTION_OUT_OF_MEMORY";

/// The `ErrorInfo.reason` of a Mac action refused for naming no Xcode (`xcode`): a request
/// error, the caller's to fix.
pub const NO_XCODE_REASON: &str = "MAC_ACTION_NAMES_NO_XCODE";

/// What the callers of a Mac action that names no Xcode are told.
pub(crate) const NO_XCODE_MESSAGE: &str = "a Mac action must name the Xcode it runs with: give \
     the platform property xcode=<build>, such as xcode=16C5032a (the Xcode's \
     ProductBuildVersion); with none it would run with whichever Xcode the Mac has selected, \
     which an update of the Mac can change";

/// What an ended job comes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The action ran and its pack is to be ingested.
    Ingest,
    /// The job did not produce a result.
    Settled(Settled),
    /// The job did not start because the host refused its tree list as not mountable through
    /// tierstore: the frontend takes its miss path before the run ends as
    /// [`Settled::ListRefused`].
    ListRefused,
    /// The job did not start because the store does not have an object of its tree, `object`:
    /// the frontend's state is told before the run ends as `settled`.
    StoreLost {
        /// The object the store answered 404 for.
        object: LostObject,
        /// How the run ends: `infra`.
        settled: Settled,
    },
}

/// A job that did not produce a pack, by what follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// Infrastructure: the action runs again.
    Infra(String),
    /// The action's timeout: the caller's.
    Timeout,
    /// Killed at its own memory limit.
    OutOfMemory,
    /// Lost to the host or the job service: the action runs again.
    Lost(&'static str),
    /// The action cannot run here: the caller's to fix.
    Invalid(String),
    /// A Mac action that names no Xcode: the caller's to fix, as [`Self::Invalid`], with its
    /// own reason ([`NO_XCODE_REASON`]).
    NoXcode,
    /// Input blobs the store no longer has, which the index said it held: the frontend forgot
    /// them, and the client is to upload them again and execute again.
    Missing(Vec<kbf_types::Digest>),
    /// The host refused the job's tree list as not mountable through tierstore, and the frontend
    /// took `0` as its miss path: the action runs again, and the first
    /// [`LIST_REFUSALS`] of these in an operation spend no attempt.
    ListRefused(MissPath),
    /// The job's host could not provide or read its inputs: the farm's failure, never the
    /// action's, and the action runs again.
    InputFailed(InputFailure),
}

/// An input a job's host could not provide or read, and the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InputFailure {
    /// The job's host, as the job service names it; empty when it named none.
    pub(crate) host: String,
    /// What failed.
    pub(crate) input: FailedInput,
}

/// What a host could not provide or read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FailedInput {
    /// The job service's result says so, with the agent's code (`input-unreadable`), cut to its
    /// leading code ([`agent_code`]); empty when the result named none.
    Reported(String),
    /// The action's own standard error says it could not open this declared input file, with
    /// the text of EIO: the path as the action named it.
    Unread(String),
}

impl InputFailure {
    /// What the callers are told: the input, the host, and that the farm failed.
    fn message(&self) -> String {
        let host = match self.host.as_str() {
            "" => "its host".to_owned(),
            host => format!("host {host}"),
        };
        match &self.input {
            FailedInput::Reported(code) if code.is_empty() => format!(
                "the farm could not provide the job's inputs on {host}: the job service \
                 reported an input failure (not the action's own failure)"
            ),
            FailedInput::Reported(code) => format!(
                "the farm could not provide the job's inputs on {host}: the job service \
                 reported {code} (not the action's own failure)"
            ),
            FailedInput::Unread(path) => format!(
                "the farm could not read the action's input {path:?} on {host}: opening it \
                 answered an I/O error (not the action's own failure)"
            ),
        }
    }
}

/// The codes an agent gives a start it refuses, as `js-agent-wire` spells them.
const QUOTA_UNAVAILABLE: &str = "quota-unavailable";
const FILES_UNSUPPORTED: &str = "files-unsupported";
const INVALID_FILE: &str = "invalid-file";
/// The agent could not check that the network the job would get is limited as it was told
/// (its firewall or its job network is missing or changed), and created nothing. The proto has
/// no start-failure code of its own for it: it is a detail of `AGENT_REFUSED`.
const EGRESS_UNCHECKED: &str = "egress-unchecked";

/// The code an agent's detail starts with: its leading run of lower-case letters, digits and
/// dashes. The rest is never read.
fn agent_code(detail: &str) -> &str {
    let end = detail
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        .unwrap_or(detail.len());
    &detail[..end]
}

/// What tierstore writes right after the SHA-256 of an object the store does not have.
const UPSTREAM_404: &str = ": upstream 404";

/// The SHA-256 of the object a start failure's `detail` says the store answered 404 for: the 64
/// lower-case hex digits right before [`UPSTREAM_404`], that are not the end of a longer run of
/// hex digits, where the status is not the start of a longer number (the first such), or
/// `None`. Nothing else of the detail is read.
pub(super) fn lost_object(detail: &str) -> Option<[u8; 32]> {
    let is_hex = |b: &u8| b.is_ascii_digit() || (b'a'..=b'f').contains(b);
    let bytes = detail.as_bytes();
    let mut from = 0;
    while let Some(found) = detail[from..].find(UPSTREAM_404) {
        let at = from + found;
        from = at + UPSTREAM_404.len();
        let ends = bytes.get(from).is_none_or(|b| !b.is_ascii_digit());
        let Some(start) = at.checked_sub(64) else {
            continue;
        };
        let alone = start == 0 || !is_hex(&bytes[start - 1]);
        let sha = sha256_of(&bytes[start..at]);
        if let (true, true, Some(sha)) = (ends, alone, sha) {
            return Some(sha);
        }
    }
    None
}

/// What tierstore writes right before the key of an object a span of a tree is read from, when
/// the store does not have it.
const SOURCE_MISSING: &str = "source missing: ";

/// The longest object key a store takes, in bytes.
const MAX_KEY_BYTES: usize = 1024;

/// The key of the object a start failure's `detail` says is a missing source of the job's tree:
/// the run of key characters (ASCII letters and digits, `/`, `.`, `_`, `-`) right after
/// [`SOURCE_MISSING`], when it starts `cas/`, names something under it, and is at most
/// [`MAX_KEY_BYTES`] long (the first such), or `None`. Nothing else of the detail is read.
pub(super) fn source_missing(detail: &str) -> Option<String> {
    let is_key = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-');
    let mut rest = detail;
    while let Some(found) = rest.find(SOURCE_MISSING) {
        rest = &rest[found + SOURCE_MISSING.len()..];
        let end = rest.find(|c: char| !is_key(c)).unwrap_or(rest.len());
        let key = &rest[..end];
        let named = key
            .strip_prefix("cas/")
            .is_some_and(|under| !under.is_empty());
        if named && key.len() <= MAX_KEY_BYTES {
            return Some(key.to_owned());
        }
    }
    None
}

/// What tierstore's `ListVersionUnsupported` writes before its reason: `tree list not
/// supported: v1_list` (or `missing_sidecar`, `unknown_cell_digest`, `unknown_version`).
pub(super) const LIST_NOT_SUPPORTED: &str = "tree list not supported: ";

/// Whether a start failure's `detail` says a node refused the job's tree list as not mountable
/// through tierstore: [`LIST_NOT_SUPPORTED`] anywhere in it. Nothing else of the detail is read.
pub(super) fn list_refused(detail: &str) -> bool {
    detail.contains(LIST_NOT_SUPPORTED)
}

/// The object a start failure's `detail` says the store does not have: a manifest
/// ([`lost_object`]) first, else a source ([`source_missing`]).
pub(super) fn lost_of(detail: &str) -> Option<LostObject> {
    lost_object(detail)
        .map(LostObject::Manifest)
        .or_else(|| source_missing(detail).map(LostObject::Source))
}

/// The 32 bytes `hex` (64 bytes, as its one caller cuts it) spells, when every byte is a
/// lower-case hex digit.
fn sha256_of(hex: &[u8]) -> Option<[u8; 32]> {
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut sha = [0_u8; 32];
    for (byte, pair) in sha.iter_mut().zip(hex.chunks_exact(2)) {
        *byte = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(sha)
}

/// The SHA-256 `sha` in lower-case hex.
pub(super) fn hex(sha: &[u8; 32]) -> String {
    sha.iter().map(|b| format!("{b:02x}")).collect()
}

/// An infrastructure ending, logged with the job id and the detail's length only.
fn infra(job: &Job, what: &str) -> Verdict {
    let detail_bytes = job.start_failure_detail.len();
    tracing::warn!(job = %job.id, detail_bytes, "{what}");
    Verdict::Settled(Settled::Infra(what.to_owned()))
}

/// The verdict on a job whose `result` says its inputs failed on its host (`input_failure`, or
/// reason input-failed), or `None`.
fn input_failed(job: &Job, result: &JobResult) -> Option<Verdict> {
    let named = result.input_failure.as_deref();
    if named.is_none() && result.reason() != ResultReason::InputFailed {
        return None;
    }
    let code = agent_code(named.unwrap_or_default()).to_owned();
    tracing::warn!(job = %job.id, host = %job.host_id, code = %code, "the host could not provide or read the job's inputs");
    Some(Verdict::Settled(Settled::InputFailed(InputFailure {
        host: job.host_id.clone(),
        input: FailedInput::Reported(code),
    })))
}

/// The verdict on `job`, which has ended, with its `result`.
pub(super) fn judge(job: &Job, result: &JobResult) -> Verdict {
    if job.state() == JobState::FailedToStart {
        return failed_to_start(job, result);
    }
    if let Some(verdict) = input_failed(job, result) {
        return verdict;
    }
    match job.state() {
        JobState::Succeeded => promote(job, result),
        JobState::Failed => match result.reason() {
            ResultReason::WallLimit => Verdict::Settled(Settled::Timeout),
            ResultReason::Memory => Verdict::Settled(Settled::OutOfMemory),
            ResultReason::Exit => infra(
                job,
                &format!(
                    "the task tool exited nonzero (exit code {:?})",
                    result.exit_code
                ),
            ),
            _ => infra(job, "the job failed for a reason this server does not know"),
        },
        JobState::Cancelled => infra(job, "the job was cancelled by another client"),
        JobState::Lost => {
            tracing::warn!(job = %job.id, "the job was lost with its host");
            Verdict::Settled(Settled::Lost("the job was lost with its host"))
        }
        _ => infra(job, "the job ended in a state this server does not know"),
    }
}

/// The verdict on `job`, which succeeded, by the promotion rule: its pack is ingested only
/// when `result` also says succeeded, for its exit, with exit code 0.
fn promote(job: &Job, result: &JobResult) -> Verdict {
    match (result.state(), result.reason(), result.exit_code) {
        (JobState::Succeeded, ResultReason::Exit, Some(0)) => Verdict::Ingest,
        (JobState::Succeeded, ResultReason::Exit, Some(code)) => {
            infra(job, &format!("the job succeeded with exit code {code}"))
        }
        (JobState::Succeeded, ResultReason::Exit, None) => {
            infra(job, "the job succeeded with no exit code")
        }
        (JobState::Succeeded, reason, _) => infra(
            job,
            &format!(
                "the job succeeded for a reason other than its exit ({})",
                reason.as_str_name()
            ),
        ),
        (state, _, _) => infra(
            job,
            &format!(
                "the job succeeded but its result says {}",
                state.as_str_name()
            ),
        ),
    }
}

/// The verdict on a job that did not start, with its `result`.
fn failed_to_start(job: &Job, result: &JobResult) -> Verdict {
    if list_refused(&job.start_failure_detail) {
        let detail_bytes = job.start_failure_detail.len();
        tracing::warn!(job = %job.id, detail_bytes, "the job failed to start: the host refused its tree list as not mountable through tierstore");
        return Verdict::ListRefused;
    }
    if let Some(object) = lost_of(&job.start_failure_detail) {
        let detail_bytes = job.start_failure_detail.len();
        let object_named = match &object {
            LostObject::Manifest(sha) => hex(sha),
            LostObject::Source(key) => key.clone(),
        };
        tracing::warn!(job = %job.id, detail_bytes, object = %object_named, "the job failed to start: the store does not have an object of its tree");
        let why = "the job failed to start: the store does not have an object of its tree";
        return Verdict::StoreLost {
            object,
            settled: Settled::Infra(why.to_owned()),
        };
    }
    if let Some(verdict) = input_failed(job, result) {
        return verdict;
    }
    match job.start_failure() {
        StartFailureCode::NoCapacity => infra(job, "the job failed to start: no-capacity"),
        StartFailureCode::StartTimeout => infra(job, "the job failed to start: start-timeout"),
        StartFailureCode::ExclusiveNotReady => {
            infra(job, "the job failed to start: exclusive-not-ready")
        }
        StartFailureCode::InputUnavailable => {
            infra(job, "the job failed to start: input-unavailable")
        }
        StartFailureCode::AgentRefused => match agent_code(&job.start_failure_detail) {
            QUOTA_UNAVAILABLE => {
                tracing::warn!(job = %job.id, "the host refused the start: its scratch quota is out of order");
                Verdict::Settled(Settled::Lost("the host's scratch quota is out of order"))
            }
            FILES_UNSUPPORTED => infra(job, "the host cannot write the files of a job"),
            INVALID_FILE => Verdict::Settled(Settled::Invalid(
                "the host refused a file of the job".to_owned(),
            )),
            "host-unsafe" => infra(job, "the job failed to start: host-unsafe"),
            EGRESS_UNCHECKED => infra(
                job,
                "the host refused to start the job: its agent could not check the limits of \
                 the job's network (egress-unchecked)",
            ),
            _ => infra(job, "the host refused to start the job"),
        },
        StartFailureCode::Unspecified => infra(job, "the job failed to start"),
    }
}

/// The ending of a job the service no longer knows, after it was seen.
pub(super) fn forgotten() -> Settled {
    Settled::Lost("the job service no longer knows the job")
}

/// What a submit that failed with `error` comes to; `None` when nothing is decided, because
/// nothing was sent, the answer was lost, or the service is busy ([`busy`]: a
/// `RESOURCE_EXHAUSTED` among them), and the same submit is sent again.
pub(super) fn refused(error: &Error) -> Option<Settled> {
    match error {
        error if busy(error) => None,
        Error::Unreachable(_) | Error::Transport(_) | Error::Decode(_) => None,
        Error::Refused(refusal) => Some(match refusal.code() {
            RefusalCode::JobIdConflict => Settled::Infra(
                "the job service holds another job under this job's id (job-id-conflict)"
                    .to_owned(),
            ),
            code @ (RefusalCode::NoCapableHost
            | RefusalCode::WallTimeNotPositive
            | RefusalCode::WallTimeAboveCeiling) => Settled::Invalid(format!(
                "the job service refused the job: {}",
                code.as_str()
            )),
            RefusalCode::Unspecified => Settled::Infra(
                "the job service refused the job for a reason this server does not know".to_owned(),
            ),
        }),
        Error::Status {
            code: Code::InvalidArgument,
            ..
        } => Some(Settled::Invalid(
            "the job service refused the job as written (INVALID_ARGUMENT)".to_owned(),
        )),
        Error::Status { code, .. } => Some(Settled::Infra(format!(
            "the job service did not take the job ({code:?})"
        ))),
        Error::TooMany(_) | Error::Stuck(_) => Some(Settled::Infra(
            "the job service gave an answer a submit does not have".to_owned(),
        )),
    }
}

impl Settled {
    /// Whether the operation is submitted again, if it has an attempt left ([`RESUBMITS`]):
    /// every ending of the farm's, and a tree list the host refused. The action's own timeout, an action that cannot run here and
    /// inputs the client is to upload again are answered at once.
    pub(super) const fn retried(&self) -> bool {
        matches!(
            self,
            Self::Infra(_)
                | Self::Lost(_)
                | Self::OutOfMemory
                | Self::ListRefused(_)
                | Self::InputFailed(_)
        )
    }

    /// The status the callers are answered with when this is the operation's last run.
    pub(super) fn status(&self) -> rpc::Status {
        let status = |code: Code, message: &str| rpc::Status {
            code: code as i32,
            message: message.to_owned(),
            details: Vec::new(),
        };
        match self {
            Self::Infra(why) => status(Code::Unavailable, why),
            Self::Lost(why) => status(Code::Unavailable, why),
            Self::InputFailed(failure) => status(Code::Unavailable, &failure.message()),
            Self::ListRefused(path) => status(
                Code::Unavailable,
                match path {
                    MissPath::Republish => {
                        "the host refused the job's tree list as not mountable (the list was \
                         written again with its sidecar)"
                    }
                    MissPath::Manifests => {
                        "the host refused the job's tree list as not mountable (the tree is \
                         given as manifests)"
                    }
                },
            ),
            Self::Timeout => status(Code::DeadlineExceeded, "the action ran out of time"),
            Self::OutOfMemory => with_reason(
                status(
                    Code::FailedPrecondition,
                    "the action passed its memory limit at every attempt",
                ),
                OUT_OF_MEMORY_REASON,
            ),
            Self::Invalid(why) => status(Code::InvalidArgument, why),
            Self::NoXcode => with_reason(
                status(Code::InvalidArgument, NO_XCODE_MESSAGE),
                NO_XCODE_REASON,
            ),
            Self::Missing(digests) => kbf_front::missing_status(digests),
        }
    }
}

/// `answer` with an `ErrorInfo` of kbf's domain and `reason` among its details.
fn with_reason(mut answer: rpc::Status, reason: &str) -> rpc::Status {
    let info = ErrorInfo {
        reason: reason.to_owned(),
        domain: ERROR_DOMAIN.to_owned(),
        metadata: Default::default(),
    };
    answer.details.push(prost_types::Any {
        type_url: super::output::ERROR_INFO.to_owned(),
        value: info.encode_to_vec(),
    });
    answer
}

#[cfg(test)]
#[path = "outcome_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "outcome_list_tests.rs"]
mod list_tests;

#[cfg(test)]
#[path = "outcome_input_tests.rs"]
mod input_tests;
