//! `GET /v1/invocations/{id}/failures`: what an agent or the UI reads first about a
//! build that went wrong (`docs/design/invocations-api.md` 4.7).
//!
//! The route is a [`super::pages`] route like the others: the same authorization, page
//! tokens and record read. This module is its two halves that are not shared:
//!
//! - **The rows** ([`entry`], [`answer`]): every row whose outcome is not OK with exit 0,
//!   in first-seen order, each with its outcome, the last attempt's failure class and
//!   reason, every attempt, its outputs and usage, and the stdout and stderr as blob
//!   objects. Built under the records' lock from the record alone.
//! - **What the CAS adds** ([`fill`]), read after the lock is released: the command (its
//!   arguments, the names of its environment variables, its working directory, platform,
//!   timeout and output paths) from the Action and Command blobs, and the last
//!   [`TAIL_BYTES`] of each log. A blob the CAS no longer holds leaves `command` `null`
//!   with `command_available: false`, and a log's `tail` `null` with `tail_omitted`
//!   saying why. Both are read only when the blob route is on, so the page shows nothing
//!   the policy's `blobs.getAuthorizer` withholds.
//!
//! **Not redacted**: arguments and log bytes are shown as the build produced them
//! (CEO ruling for the open UI). Values of environment variables are not in the answer;
//! the Command blob that holds them is linked from the action's `blobs`.

use std::time::Duration;

use axum::http::header::{CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use kbf_front::{Cache, CacheError, MetaLog};
use kbf_invocation::{ActionRow, AttemptEnd, failure_name};
use kbf_objstore::ObjectStore;
use kbf_proto::reapi::{Action, Command, Platform};
use kbf_types::Digest;
use prost::Message;
use serde_json::{Map, Value, json};

use super::views::{Builder, attempt, digest_text, outcome, outputs, text, usage};

/// How much of the end of a log a failure carries: 16 KiB.
pub const TAIL_BYTES: u64 = 16 * 1024;

/// The largest log a tail is read from. The CAS checks a blob against its digest by
/// reading all of it, so a larger one is `tail_omitted: "too_large"`.
pub const TAIL_READ_BYTES: u64 = 64 * 1024 * 1024;

/// The most bytes of a command's strings an answer carries: 64 KiB.
pub const COMMAND_VIEW_BYTES: usize = 64 * 1024;

/// The largest Action or Command blob read for the command.
pub const COMMAND_READ_BYTES: u64 = 1024 * 1024;

/// How long one blob read may take before its part of the answer is left out.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// What `/failures` reads from the CAS for one failure it lists.
#[derive(Debug)]
pub(crate) struct Read {
    /// The failure's place in the page's `failures`.
    index: usize,
    /// The Action blob, for the platform and the timeout.
    action: Digest,
    /// The Command blob, once the recorder has read its digest from the Action.
    command: Option<Digest>,
    stdout: Option<Digest>,
    stderr: Option<Digest>,
}

/// `value` as the answer of a page of failures. Its strings are build output (log bytes,
/// arguments, a target the client named), so the body is made inert if anything reads it
/// as HTML: `<`, `>` and `&` are written `<`, `>` and `&`, which a JSON
/// parser reads back as the same characters, and `Content-Type` is `application/json`
/// with `nosniff`.
pub(super) fn respond(value: &Value) -> Response {
    let body = escape_html(&value.to_string());
    let mut answer = (StatusCode::OK, [(CONTENT_TYPE, crate::api::JSON)], body).into_response();
    answer
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    answer
}

/// `json` with each `<`, `>` and `&` written as a `\u` escape. Those characters are
/// never JSON syntax, so each one is inside a string, where the escape is the same
/// character.
fn escape_html(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            c => out.push(c),
        }
    }
    out
}

/// The answer of a page: the invocation's own answer and the page of failures `listed`
/// (`failures`, and the `next_page` and `next_page_token` when more follow).
pub(super) fn answer(invocation: Value, listed: Value) -> Value {
    let Value::Object(mut map) = listed else {
        unreachable!("a page of rows is an object")
    };
    map.insert("invocation".to_owned(), invocation);
    Value::Object(map)
}

/// Row `r` as the `index`th failure of its page, noting the CAS reads it needs and its
/// blobs.
pub(super) fn entry(b: &mut Builder, r: &ActionRow, base: &str, index: usize) -> Value {
    let at = format!("/failures/{index}");
    let listing = r.listing.as_ref();
    let mut logs = Vec::new();
    for (name, digest) in [
        ("stdout", listing.and_then(|l| l.stdout)),
        ("stderr", listing.and_then(|l| l.stderr)),
    ] {
        let mut log = b.log(format!("{at}/{name}"), digest, &r.instance, name);
        if let (Value::Object(map), Some(d)) = (&mut log, digest) {
            map.insert("bytes".to_owned(), d.size_bytes.into());
            for key in ["tail", "tail_bytes", "truncated"] {
                map.insert(key.to_owned(), Value::Null);
            }
        }
        logs.push(log);
    }
    b.reads.push(Read {
        index,
        action: r.action,
        command: r.command,
        stdout: listing.and_then(|l| l.stdout),
        stderr: listing.and_then(|l| l.stderr),
    });
    let stderr = logs.pop().unwrap_or(Value::Null);
    let stdout = logs.pop().unwrap_or(Value::Null);
    let action = digest_text(&r.action);
    json!({
        "action": action,
        "href": format!("{base}/actions/{action}"),
        "instance": r.instance,
        "target": text(r.target.as_ref()),
        "mnemonic": text(r.mnemonic.as_ref()),
        "outcome": outcome(r.outcome.as_ref()),
        "failure": failure(r),
        "attempts": r.attempts.iter().map(attempt).collect::<Vec<_>>(),
        "command": Value::Null,
        "command_available": false,
        "stdout": stdout,
        "stderr": stderr,
        "outputs": outputs(b, r, &format!("{at}/outputs")),
        "outputs_truncated": r.outputs_truncated,
        "usage": usage(listing.and_then(|l| l.usage)),
    })
}

/// The last attempt's failure class and the daemon's reason for it (the record keeps
/// at most [`kbf_invocation::REASON_CAP`] bytes of it, cut on a character); `null` when
/// the last attempt did not end in a failure (an action that ran to a non-zero exit, or
/// was refused).
fn failure(r: &ActionRow) -> Value {
    match r.attempts.last().and_then(|a| a.end.as_ref()) {
        Some(AttemptEnd::Failed(class, why)) => {
            json!({"class": failure_name(*class), "reason": why})
        }
        _ => Value::Null,
    }
}

/// Adds to `value`, a page of `/failures`, what the CAS holds for each failure in
/// `reads`: its command and the tails of its logs. Without the blob route (`blobs_on`
/// false) nothing is read and the page says so in each `tail_omitted`.
pub(crate) async fn fill<M, O>(
    cache: &Cache<M, O>,
    value: &mut Value,
    reads: &[Read],
    blobs_on: bool,
) where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    for read in reads {
        let Some(slot) = value.pointer_mut(&format!("/failures/{}", read.index)) else {
            continue;
        };
        if blobs_on && let Some(command) = command(cache, read).await {
            slot["command"] = command;
            slot["command_available"] = true.into();
        }
        for (name, digest) in [("stdout", read.stdout), ("stderr", read.stderr)] {
            let (Some(digest), Some(Value::Object(log))) = (digest, slot.get_mut(name)) else {
                continue;
            };
            match blobs_on {
                true => put_tail(log, tail(cache, &digest).await),
                false => put_tail(log, Err(Omitted::BlobsOff)),
            }
        }
    }
}

/// Why a log has no tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Omitted {
    /// The blob route is off, and with it every read of a blob's bytes.
    BlobsOff,
    /// The CAS does not hold the blob.
    NotHeld,
    /// Over [`TAIL_READ_BYTES`].
    TooLarge,
    /// The read failed or timed out.
    Unreadable,
}

impl Omitted {
    const fn name(self) -> &'static str {
        match self {
            Self::BlobsOff => "blobs_off",
            Self::NotHeld => "not_held",
            Self::TooLarge => "too_large",
            Self::Unreadable => "unreadable",
        }
    }
}

/// A log's tail: its text and how many bytes of the log it holds.
#[derive(Debug, PartialEq, Eq)]
struct Tail {
    text: String,
    bytes: u64,
}

/// Writes a read's answer into the log's blob object.
fn put_tail(log: &mut Map<String, Value>, read: Result<Tail, Omitted>) {
    let total = log.get("bytes").and_then(Value::as_u64).unwrap_or(0);
    match read {
        Ok(tail) => {
            log.insert("truncated".to_owned(), (tail.bytes < total).into());
            log.insert("tail_bytes".to_owned(), tail.bytes.into());
            log.insert("tail".to_owned(), tail.text.into());
        }
        Err(why) => {
            log.insert("tail_omitted".to_owned(), why.name().into());
        }
    }
}

/// The last [`TAIL_BYTES`] of the log `d` names, read whole by the CAS to check its
/// digest and handed over in part. Invalid UTF-8 is replaced; a cut inside a code point
/// drops that code point's tail bytes.
async fn tail<M, O>(cache: &Cache<M, O>, d: &Digest) -> Result<Tail, Omitted>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    if d.size_bytes == 0 {
        return Ok(Tail {
            text: String::new(),
            bytes: 0,
        });
    }
    if d.size_bytes > TAIL_READ_BYTES {
        return Err(Omitted::TooLarge);
    }
    let from = d.size_bytes.saturating_sub(TAIL_BYTES);
    let read = async {
        let mut read = cache.read_windows(d, from, d.size_bytes).await?;
        let mut bytes = Vec::new();
        while let Some(window) = read.next().await {
            bytes.extend_from_slice(&window?);
        }
        Ok::<_, CacheError>(bytes)
    };
    let bytes = match tokio::time::timeout(READ_TIMEOUT, read).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(CacheError::NotFound(_))) => return Err(Omitted::NotHeld),
        Ok(Err(_)) | Err(_) => return Err(Omitted::Unreadable),
    };
    // A cut that starts inside a code point leaves continuation bytes first: drop them.
    let skip = if from > 0 {
        bytes
            .iter()
            .take(3)
            .take_while(|b| (**b & 0xC0) == 0x80)
            .count()
    } else {
        0
    };
    let kept = &bytes[skip..];
    Ok(Tail {
        text: String::from_utf8_lossy(kept).into_owned(),
        bytes: kept.len() as u64,
    })
}

/// The command of a failure, read from its Action and Command blobs; `None` when the
/// client never uploaded the Action, either blob is gone, too large or does not decode.
async fn command<M, O>(cache: &Cache<M, O>, read: &Read) -> Option<Value>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let command = read.command?;
    let action = Action::decode(blob(cache, &read.action).await?).ok()?;
    let command = Command::decode(blob(cache, &command).await?).ok()?;
    Some(render(&action, command))
}

/// A small blob's bytes, or `None`.
async fn blob<M, O>(cache: &Cache<M, O>, d: &Digest) -> Option<Bytes>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    if d.size_bytes > COMMAND_READ_BYTES {
        return None;
    }
    tokio::time::timeout(READ_TIMEOUT, cache.read_blob(d))
        .await
        .ok()?
        .ok()
}

/// The command as the answer shows it. The platform is the Action's, or the Command's
/// when the Action names none; environment variables are names only.
#[allow(deprecated)] // `output_files` and `output_directories`: REAPI before 2.1 sends only these.
fn render(action: &Action, command: Command) -> Value {
    let platform = action
        .platform
        .as_ref()
        .filter(|p| !p.properties.is_empty())
        .or(command.platform.as_ref());
    let Command {
        mut arguments,
        environment_variables,
        working_directory,
        mut output_paths,
        output_files,
        output_directories,
        ..
    } = command;
    output_paths.extend(output_files);
    output_paths.extend(output_directories);
    let platform = properties(platform);
    let mut names: Vec<String> = environment_variables.into_iter().map(|e| e.name).collect();
    // Strings are dropped from the end (output paths, then names, then arguments) until
    // the rest fit: a Command blob may be 1 MiB, and a page lists up to 100 of them.
    let mut size = working_directory.len()
        + [&arguments, &names, &output_paths]
            .iter()
            .flat_map(|list| list.iter().map(String::len))
            .sum::<usize>();
    let truncated = size > COMMAND_VIEW_BYTES;
    for list in [&mut output_paths, &mut names, &mut arguments] {
        while size > COMMAND_VIEW_BYTES {
            let Some(dropped) = list.pop() else { break };
            size -= dropped.len();
        }
    }
    json!({
        "arguments": arguments,
        "environment_names": names,
        "working_directory": working_directory,
        "platform": platform,
        "timeout_micros": timeout_micros(action),
        "output_paths": output_paths,
        "truncated": truncated,
    })
}

/// A platform's properties as a JSON object, name to value (the last of a repeated
/// name wins).
fn properties(platform: Option<&Platform>) -> Value {
    platform
        .map(|p| {
            p.properties
                .iter()
                .map(|p| (p.name.clone(), Value::from(p.value.clone())))
                .collect::<Map<String, Value>>()
        })
        .unwrap_or_default()
        .into()
}

/// The action's timeout in microseconds; `null` when it names none or a negative one.
fn timeout_micros(action: &Action) -> Value {
    let micros = action.timeout.as_ref().and_then(|t| {
        let secs = u64::try_from(t.seconds).ok()?;
        let nanos = u64::try_from(t.nanos).ok()?;
        secs.checked_mul(1_000_000)?.checked_add(nanos / 1_000)
    });
    micros.map_or(Value::Null, Value::from)
}

#[cfg(test)]
#[path = "failures_tests.rs"]
mod tests;
