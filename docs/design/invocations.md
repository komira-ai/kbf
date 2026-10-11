# Invocations: a build's id, its record and its page

A user who runs a build against the farm should be able to open one link and see what the
farm did for that build: each action it ran or answered from the cache, where it ran, how
long it waited and ran, how it ended, its outputs, its stdout and stderr, and what it
used. This document designs the server side of that: the id, the record, where the record
is kept, the read API and the link. A rich UI is out of scope; the API is what a UI (or a
script) reads.

**Everything in this document is planned** unless it says "today". Section 1 lists what
exists.

Vocabulary: an **invocation** is one run of a build tool (one `buck2 build`, one
`bazel test`), REAPI's word for it. The page a person opens is the *build page*; its
record is the *invocation record*.

## 1. What exists today

Read on `main`:

- **No invocation.** Nothing in `crates/` reads REAPI `RequestMetadata`: the type is
  generated in `kbf-proto` and never used. `ExecutionService::execute` reads only the
  authentication result (`kbf_auth::metadata`) and drops the rest of the call's headers.
- **Per-operation state lives about a minute.** The scheduler keeps a finished operation
  for `FINISHED_RETENTION` (60 s, `kbf-sched/src/scheduler.rs`); then WaitExecution
  answers NOT_FOUND. All scheduler state is in memory and is lost on restart.
  `FarmMachine` (`kbf-server/src/machine.rs`) holds the waiters, the started leases and
  the node registry beside it, with the same retention.
- **Reruns are log lines.** `kbf-sched/src/requeue.rs` names each requeue (`Silent`,
  `Replaced`, `Reconnected`, `NotStarted`, `SessionSwitch`, `OutOfMemory`,
  `NodeMemoryPressure`); they are drained to tracing and not stored.
- **Results are durable, unindexed.** Every OK result, a non-zero exit included, is
  stamped (`Stamp`: worker, queued, worker start and completion times) and stored as an
  `ActionResult` blob in the CAS (`Cache::prepare_action_result`). Its outputs, stdout and
  stderr are CAS digests inside it. An action-cache entry (`result: Digest`) is written
  only for exit 0 without `do_not_cache`. A failed action's result blob therefore has no
  index entry pointing at it once the operation is dropped.
- **Usage is carried, rarely filled.** `kbf.worker.v1.ResourceUsage` (user and system CPU,
  peak memory of one process, wall time) rides in
  `ActionResult.execution_metadata.auxiliary_metadata`. Only the test-only local runtime
  fills it; the native and container drivers do not
  ([scheduler.md, Accounting](scheduler.md#accounting)).
- **Retention.** `kbf_meta::Retention`: blobs at least 7 days after their last touch,
  touches at most once a day, action-cache entries 30 days. Collection is never committed
  by the server and segments are never deleted
  ([storage.md, Planned](storage.md#planned)); **Pins**, named retention roots, are
  planned there too.
- **No HTTP read of a blob.** Blobs are served over REAPI ByteStream and CAS, and to
  daemons over the worker listener, all gRPC.
- **The operator API** ([api.md](../api.md)) serves `GET /v1/nodes`, the node actions,
  `/healthz` and `/readyz` on `--api-listen`, HTTP/1.1 or h2c, without TLS. Its reads
  answer anyone who reaches the address; the REAPI auth policy
  ([reapi-auth.md](../reapi-auth.md)) does not run there.
- **The client text channel is unused.** `ExecuteResponse` is built with `result`,
  `cached_result` and `status` only; `message` and `server_logs` are empty.

## 2. The id

### 2.1 Where it comes from

REAPI clients send a binary header,
`build.bazel.remote.execution.v2.requestmetadata-bin`, holding a serialized
`RequestMetadata`: `tool_details` (name, version), `action_id`, `tool_invocation_id`,
`correlated_invocations_id`, `action_mnemonic`, `target_id`, `configuration_id`.
`tool_invocation_id` "ties multiple actions together to a final result": it is the
invocation id.

**The invocation id is the client's `tool_invocation_id`.** The server does not mint one
for a client that sends one. The reason is that a client prints only its own id: a
server-minted id cannot reach the user without a client-side hook (section 5).

What the pinned clients send, read from source (not yet captured on the wire; PR 1 in
section 9 captures it):

| Client | `tool_invocation_id` | other fields |
|---|---|---|
| buck2 (release 2026-09-15) | the command's trace id, the UUID it prints as `Build ID: <uuid>` | `tool_details` = `buck2`, version hard-coded `0.1.0`; `action_id`, `correlated_invocations_id`, `action_mnemonic`, `target_id`, `configuration_id` all empty |
| Bazel | the command id (`--invocation_id`) | `correlated_invocations_id` = build request id; `action_id`, mnemonic, label, configuration when known; real version |

buck2 sends the header on GetActionResult, UpdateActionResult, Execute,
BatchUpdateBlobs, BatchReadBlobs, FindMissingBlobs and ByteStream Read and Write; not on
GetCapabilities. It does not call WaitExecution. A caller can choose the id ahead of time:
buck2 takes it from `BUCK_WRAPPER_UUID` when set.

### 2.2 Accepting it

- Parsed once per call by a tower layer in `kbf-front`, beside (not inside) the
  authentication layer, and put in the request's extensions as a `CallMetadata`.
- **A bad header never fails a call.** A header over 8 KiB, one that does not decode, or
  an id that is empty, longer than 128 bytes or not printable ASCII (`[!-~]`) is treated as
  absent and counted (`kbf_invocation_metadata_invalid`). A build must not break because
  of bookkeeping.
- Text fields (`target_id`, mnemonic, configuration) are kept up to 512 bytes each and
  truncated past that, marked truncated.

### 2.3 The fallback when no id is sent

A call without an id is not attributed to any client invocation. What happens depends on
the call:

- **Execute**: the server mints an id, `kbf-<uuid v4>`, for that one operation: an
  invocation of one action. Its link goes back in `ExecuteResponse.message` (section 5.2),
  which is the only way the user learns it.
- **GetActionResult, CAS, ByteStream**: not recorded per call; counted in farm-wide
  totals only. Minting an invocation per cache read would make one record per call.

The server does not group id-less calls by connection or peer: a buck2 daemon keeps one
connection for every command it runs, so a connection is not a build.

### 2.4 Ids are claims, not secrets

`tool_invocation_id` is chosen by the client. Two consequences:

- **Squatting.** Any caller can send another build's id. The record is keyed on the id
  alone, so the URL needs nothing else, but it keeps the `public` authentication
  metadata of its first caller as its *owner*. A call carrying the same id from a caller
  whose `public` metadata differs is recorded under its own record, keyed
  `<id>~<n>` (n counting from 2), and the owner's page lists those as `collisions`. Without
  a policy file every caller's metadata is empty, so all callers are one owner, as they
  are for authorization today.
- **Not a capability.** A UUID is hard to guess but is printed to consoles and CI logs;
  reading a page is decided by the authorizer (section 4.3), never by knowing the id.

## 3. The record

### 3.1 Per invocation

| Field | Source |
|---|---|
| `id` | section 2 |
| `tool`, `tool_version` | `tool_details` of the first call |
| `correlated_invocations_id` | first non-empty value seen |
| `owner` | `public` metadata of the first caller (2.4) |
| `instances` | instance names its calls used |
| `first_seen`, `last_seen` | server wall clock |
| `state` | `open`; `idle` after 10 minutes without a call; never "finished": REAPI has no end-of-build call |
| counters | Execute calls, of which answered from the cache, joined to a running twin, ran, failed; GetActionResult hits and misses; FindMissingBlobs blobs asked and missing; CAS and ByteStream bytes uploaded and downloaded |
| `complete` | false when any event was dropped (3.4) or a server restarted while it was open |

### 3.2 Per action

One row per action digest per invocation, in the order first seen.

| Field | Source |
|---|---|
| `action` | Action digest (SHA-256 and size; kbf accepts SHA-256 only) |
| `instance` | the call's instance name |
| `target`, `mnemonic`, `configuration`, `action_id` | `RequestMetadata`, when sent (buck2 sends none today) |
| `how` | `cache_hit` (GetActionResult), `cache_hit_execute` (Execute answered from the AC, operation `operations/cached/...`), `joined` (attached to a twin's operation), `ran` |
| `operation` | the REAPI operation name |
| `attempts` | each lease: lease id, worker (node id), queued, granted, started, ended, and how it ended: result, or the requeue reason (`requeue.rs`), or the failure class (`log.proto` `Failure`) with the daemon's reason text, capped at 1 KiB |
| `outcome` | `ok` with exit code; `failed` with the gRPC status code and message; `timed_out`; `refused` with the reason |
| `result` | the ActionResult blob digest (from `OpState::Completed`, or the AC entry for a hit) |

**Outputs, stdout, stderr and usage are not copied into the row.** They are inside the
ActionResult blob, which the read API opens on demand (4.2). The row holds one digest
where a copy would hold every output path. A joined action's attempts are those of the
operation it joined; the row stores the operation name and the attempts are copied into
every invocation that waited on it, so each page stands alone after the operation is
dropped.

The record does not hold the Command (its arguments and environment can hold secrets);
the action detail links the Action and Command blobs like any other blob.

### 3.3 Cost per action

Estimated from the field encodings, not measured (PR 3 measures it):

- core row (digests, lease, worker, three or four timestamps, outcome tag): 120 to 150 B;
- plus `RequestMetadata` text when sent: 50 to 200 B; plus a rerun about 40 B each.

So about 200 to 400 B an action. At the pilot rate of about 4 operations a second (the
comment on `FINISHED_RETENTION` in `scheduler.rs`) that is about 70 to 140 MB a day before cache hits;
recording GetActionResult hits multiplies it by the hit rate. This volume is why the
record is not written into the replicated log (3.5).

**A cap per invocation**: 1,000,000 action rows (about 400 MB at the high estimate).
Past it, actions are counted but not rowed, and the record says `rows_capped: true`.

### 3.4 The write path

- Each event (call seen, lease granted, requeue, finished) goes to a recorder task through
  a bounded channel. **A full channel drops the event and never waits**: dispatch and
  cache reads are never slowed by the record. A drop marks the invocation
  `complete: false` and increments `kbf_invocation_events_dropped`.
- The seams that emit: the new metadata layer (calls and their ids); Execute's submit and
  its cached and joined answers (`kbf-front/src/execution.rs`); GetActionResult
  (`action_cache.rs`); the lease grant and `Sent` in `FarmMachine`; the requeue drain that
  logs today (`take_requeues`); the outcome in `farm.rs`, which also gains the daemon's
  INFRA reason that is now only in a WARN line.
- The recorder holds open invocations in memory and flushes each one's new rows as a
  **chunk** every 10 s or 256 KiB, whichever comes first.

### 3.5 Where it is kept

Two stages:

**Stage 1, in memory** (PRs 3 and 4). The recorder keeps records in memory, bounded by
`--invocation-memory` (default 256 MiB), evicting the least recently seen idle
invocation. Records are lost on restart and the page says so. This is honest and useful
for the first deployments, which run `--meta-log=memory` and lose the index at restart
anyway.

**Stage 2, durable** (PR 7):

- A chunk is a `kbf.invocation.v1.Chunk` protobuf, stored **as a CAS blob** through the
  existing `Cache` write path: verified, packed into segments, in the object store like
  any blob. No new storage layer.
- The invocation's root (its header fields and the ordered list of chunk digests) is a
  small blob too, rewritten at each flush.
- The log holds one **pin** per invocation, `invocation/<id>` to the root's digest and a
  hold-until time: the Pins of [storage.md](storage.md#planned). A pin keeps its root and
  chunks from collection. One pin update per invocation per flush, about 100 B of log,
  instead of a row per action.
- This keeps the object store's contract (`kbf-objstore`: kbf never asks a store whether
  something exists; every key is known from the log) because chunks are ordinary CAS
  blobs reached from a pin.
- Adding the pin command is a new log format (`log.proto` is closed; `kbf-log` knows
  `Format::V1` only). It lands after, and in the same format bump as, the Pins work, and
  is ordered with the high-availability design (PR #303, open), which moves the farm's
  state into the replicated log.

A crash loses at most the unflushed window (10 s) of each open invocation; the next
server marks those invocations `complete: false`.

### 3.6 Retention

- The record: `--invocation-retention` (default 30 days after `last_seen`), the pin's
  hold-until time.
- **The artifacts are not pinned by default.** Outputs, stdout and stderr keep the CAS's
  own retention (7 days after their last touch today). Pinning every output of every build
  for 30 days would hold most of the cache. When a blob is gone the page still lists it,
  with `available: false`. `--invocation-pin-outputs=failed` (planned) pins the outputs,
  stdout and stderr of failed actions only, the case worth keeping.
- Until collection lands nothing is deleted, so retention is a promise of a floor, not a
  ceiling.

## 4. The read API

On the operator API listener (`--api-listen`), JSON, the conventions of
[api.md](../api.md). Wall times are RFC 3339 UTC strings; durations are integer
microseconds; digests are `"<sha256 hex>-<size>"`.

### 4.1 Routes

| Route | Answers |
|---|---|
| `GET /v1/invocations/{id}` | the invocation (3.1), with `url`, and links to the routes below |
| `GET /v1/invocations/{id}/actions` | a page of action rows (3.2); `?outcome=failed\|ok\|timed_out\|refused`, `?how=...` |
| `GET /v1/invocations/{id}/actions/{digest}` | one action: its row, plus its outputs, stdout, stderr and usage read from its ActionResult |
| `GET /v1/invocations/{id}/artifacts` | a page of output files and directories across its actions |
| `GET /v1/invocations/{id}/logs` | a page of stdout and stderr links, failed actions first |
| `GET /v1/invocations/{id}/metrics` | aggregates (4.4) |
| `GET /v1/blobs/{digest}?instance=<name>&name=<file>` | a blob's bytes: the HTTP read path that does not exist today |

`{id}` is percent-decoded; `instance` is a query parameter because REAPI instance names
may contain `/`.

### 4.2 Shapes

```json
GET /v1/invocations/0b8c...-...
{
  "id": "0b8c...",
  "url": "https://build.example.com/build/0b8c...",
  "tool": "buck2", "tool_version": "0.1.0",
  "correlated_invocations_id": null,
  "instances": [""],
  "first_seen": "2026-10-11T09:00:00Z", "last_seen": "2026-10-11T09:04:12Z",
  "state": "idle", "complete": true, "rows_capped": false,
  "counts": {"execute": 812, "cache_hit_execute": 40, "joined": 3, "ran": 769,
             "failed": 2, "ac_hit": 5120, "ac_miss": 812,
             "bytes_uploaded": 1234567, "bytes_downloaded": 89012345},
  "collisions": [],
  "links": {"actions": "/v1/invocations/0b8c.../actions", "artifacts": "...",
            "logs": "...", "metrics": "..."}
}
```

```json
GET /v1/invocations/{id}/actions/{digest}
{
  "action": "e3b0...-142", "instance": "",
  "target": null, "mnemonic": null, "configuration": null,
  "how": "ran", "operation": "operations/7-1042",
  "attempts": [
    {"lease": 88, "worker": "node-a", "queued": "...", "started": "...", "ended": "...",
     "end": {"requeue": "silent"}},
    {"lease": 91, "worker": "node-b", "queued": "...", "started": "...", "ended": "...",
     "end": {"result": "ok"}}
  ],
  "outcome": {"ok": {"exit_code": 1}},
  "result": "9f86...-512",
  "outputs": [{"path": "out/lib.a", "digest": "...", "executable": false,
               "available": true, "href": "/v1/blobs/...?instance=&name=lib.a"}],
  "stdout": {"digest": "...", "available": true, "href": "/v1/blobs/..."},
  "stderr": {"digest": "...", "available": true, "href": "/v1/blobs/..."},
  "usage": {"cpu_user_micros": 1200000, "cpu_system_micros": 80000,
            "peak_memory_bytes": 512000000, "wall_micros": 1500000},
  "blobs": {"action": "/v1/blobs/e3b0...-142", "command": "/v1/blobs/..."}
}
```

`usage` is `null` when the result carries no `ResourceUsage`, which today is every result
from a real driver. An output directory is listed as its tree digest; the artifacts route
expands trees one level per page entry, not recursively.

### 4.3 Who may read

Build pages hold stdout, stderr and outputs, which can hold secrets, so they cannot have
the operator API's "anyone who reaches the address" rule.

- The invocation and blob routes run the **REAPI auth policy** (`--reapi-auth-policy`),
  the same `Authenticator`: its `authenticate` takes the call's `http::request::Parts`,
  which an HTTP request on the API listener has too. One policy, one set of variants.
- Two new authorizer keys in the policy file: `invocations.getAuthorizer`, asked with the
  invocation's instance names (all must allow), and `blobs.getAuthorizer`, asked with the
  `instance` parameter. Either may be an `instanceNamePrefix` or any built authorizer.
- **Fail closed.** If the policy file has no `invocations` key, or there is no policy
  file, the invocation and blob routes answer `404` and the start line says
  `invocations=off`. `--invocations-open` turns them on without a policy, for a listener
  bound where only trusted people reach it, the same choice the operator API asks for
  today, made explicit.
- Today only `allow`, `deny`, `any` and `all` authentication is built; `jwt` and
  `tlsClientCertificate` are planned. Until they land, a public deployment protects its
  pages with an authenticating proxy in front of the listener. A planned `invocationOwner`
  authorizer allows a caller whose `public` metadata equals the record's owner.
- Writes: none in this design except `DELETE /v1/invocations/{id}` (planned, 3.6, 7),
  under the operator API's existing write rules (loopback peer and token).

### 4.4 Metrics

Per invocation, computed from the record at read time: counts as above; queue time and
run time (granted to ended) as min, p50, p90, max and sum; reruns by reason; CPU-seconds
and peak memory summed and maxed over actions that carry usage, with
`usage_actions: n` saying how many did. Farm-wide metrics (a scrape endpoint) are a
separate design.

### 4.5 Paging

- `?page_size=` (default 100, maximum 1000) and `?page_token=`; the answer carries
  `next_page_token`, absent on the last page.
- Rows are append-only and in first-seen order, so a token is the opaque position
  (chunk, offset) plus the filter it was made for; a page never skips or repeats a row
  while the build is still writing. A token used with another filter is `400`.

### 4.6 Blobs over HTTP

- `GET /v1/blobs/{digest}` reads through the same `Cache` read path as ByteStream Read,
  so bytes are verified against the digest before they are sent.
- `Content-Type: application/octet-stream` and `Content-Disposition: attachment` with
  `name` when given, so a browser does not render an artifact as a page;
  `?inline=text` serves `text/plain; charset=utf-8` for stdout and stderr only up to
  4 MiB.
- `Range` requests are honored. A malformed digest is `400`, an absent blob `404`.
- Each read is a touch, as on ByteStream.

## 5. The link

### 5.1 The configured URL

`--invocation-url-template <url>`, for example
`--invocation-url-template 'https://build.komira.dev/build/{id}'` on one deployment, or
`'http://farm.example:8081/build/{id}'` on another; there is no default and no default
host. Rules:

- It must contain `{id}` exactly once; `{id}` is replaced by the percent-encoded id.
- It must be an `http` or `https` URL after substitution. TLS is optional: the listener
  serves clear text, and a deployment that wants `https` puts a TLS-terminating proxy in
  front, as for the operator API today.
- A bad template stops the server before anything is bound (exit 2), naming the flag.
- Without it, the API's `url` fields are `null` and no message carries a link.

The server serves `GET /build/{id}`: `303 See Other` to `/v1/invocations/{id}`, or a
minimal HTML page of links (no script) when the request accepts `text/html` (PR 9). A
proxy may map any external path to it.

### 5.2 What a buck2 user sees

**Today, with no client change**, the link is the template with the id buck2 already
prints:

```
Build ID: 0b8c2f0e-...                 <- printed by buck2 at the start and the end
https://build.komira.dev/build/0b8c2f0e-...   <- the same id in the template
```

buck2 also writes the id with `--write-build-id <path>` and as `trace_id` in
`--build-report`, so a CI step can print the link after the build.

**On a failed action, the link is printed by buck2 itself.** The server puts

```
Invocation: <invocation url>  Action: <invocation url>/actions/<digest>
```

in `ExecuteResponse.message` on every Execute answer, and in the message of every
refusal status. buck2 shows `message` for failed actions only (as `Info:` under the
action's error), which is when the link matters. A successful build prints no link.

**What stock buck2 cannot do**: print a server-chosen URL on success. Its
`Buck UI: <url>` line is compiled in for Meta's internal build only, and no `.buckconfig`
key, `[buck2_re_client]` setting or plugin adds a URL. A configured `http_headers` value
cannot carry a per-build id either: the RE connection belongs to the daemon and outlives
commands.

**Changes outside kbf that would close the gap** (none needed for this design):

1. Upstream buck2: a `[buck2] build_url_template` (or similar) that prints
   `Build URL: <template with the trace id>` in open-source builds, where it now prints
   `Build ID:`.
2. Upstream buck2: fill `action_id`, `action_mnemonic`, `target_id` and
   `configuration_id` in `RequestMetadata`, which it sends empty, and the real version in
   `tool_details`. Until then the page groups by action digest and shows no target names.
3. A wrapper around buck2 that sets `BUCK_WRAPPER_UUID` and prints the link first. That is
   a deployment's choice; kbf ships no client wrapper.

**Caveats.** The farm learns of a build only through REAPI calls. A build served
entirely from buck2's local caches makes no call, and its page is `404` with
`"the farm saw no call with this id"`. Actions deduplicated across two concurrent buck2
commands, and downloads made late by buck2's materializer, may carry the other command's
id or none (read from source, not measured).

### 5.3 Bazel, later

Bazel sends a full `RequestMetadata`, so its invocations are recorded and its failed
actions carry the link the same way. Bazel prints a link on every build only through the
Build Event Service: with `--bes_backend` and `--bes_results_url=<base>/build/` it prints
`Streaming build results to: <base>/build/<invocation id>`. A BES ingester
(`google.devtools.build.v1.PublishBuildEvent`) would add what only the client knows:
targets, tests and their results, named artifacts. **Not planned in this plan**; the
recorder takes events through one interface keyed on the invocation id, so a BES
ingester is a second writer and not a second record. buck2 does not emit BES.

## 6. Privacy

- Recorded: the RequestMetadata fields of 2.1, the caller's `public` authentication
  metadata (never `private`), and what the farm itself did. Not recorded: peer
  addresses, other headers, Command arguments or environment.
- `target_id` and `configuration_id` reveal a repository's layout; outputs and logs can
  hold anything a build prints. Both are behind the authorizer (4.3).
- The invocation id joins the existing `execute` tracing span (with instance and
  caller), so server logs can be correlated with a page.
- An operator removes a record with `DELETE /v1/invocations/{id}` (planned): the pin goes,
  and collection (once it runs) frees chunks not shared with other records. Artifacts are
  CAS blobs and may be shared by other builds, so deleting a record does not delete them.

## 7. Failure modes

| Event | Effect on records |
|---|---|
| Recorder channel full | events dropped, builds unaffected, invocation `complete: false` |
| Server crash or restart | stage 1: all records lost, a page answers `404`; stage 2: at most 10 s of each open invocation lost, marked `complete: false` |
| Object store down at flush | chunk kept in memory and retried; past `--invocation-memory`, oldest unflushed rows dropped and `complete: false` |
| Failover (HA, planned) | the new leader's recorder starts from the pins; open invocations continue in new chunks |
| Blob collected | listed with `available: false`; the link answers `404` |
| Malformed or hostile header | treated as absent (2.2); never fails the call |
| Squatted id | recorded under `<id>~<n>` (2.4) |

## 8. Alternatives rejected

- **A farm-minted id for every build.** The user cannot learn it: no REAPI field shown on
  success, and buck2's printed id is its own. Kept only for the id-less Execute fallback.
- **Rows in the replicated log.** 70 to 140 MB a day into a log that has no compaction,
  in every snapshot and checkpoint digest, and a log format change per field.
- **Records as separate objects in the object store.** Needs a new key space whose keys
  the log must still record (the objstore contract) and its own retention; CAS blobs plus
  a pin reuse both.
- **Grouping by connection.** One buck2 daemon connection serves many commands.
- **Digest-only pages, as Buildbarn's browser.** Useful for one action (and section 4.6
  is that), but they do not answer "what did my build do".

## 9. Implementation plan

Ordered; each is one pull request, S (under a day) or M (a few days). Each test says which
defect it catches.

1. **(S) Parse `RequestMetadata`.** The `kbf-front` layer and `CallMetadata`; the id on the
   `execute` span. Tests: a header encoded by `prost` is read back field for field; a
   golden header captured from one real buck2 build against a test server is read with the
   expected UUID (catches reading the wrong header name or a wrong field number: plant
   field 2 for 3, the golden goes red); oversize, undecodable and non-printable ids are
   absent and the call still succeeds (plant "refuse on decode error", the call test goes
   red). Also settles the open fact in [failure-classes.md](failure-classes.md) O2 about
   which fields buck2 sends.
2. **(S) The record model, pure.** `kbf.invocation.v1` protos and an `Invocation` type
   built only from events; no clock, no I/O. Tests: every `how` and outcome; a requeued
   then finished action has two attempts in order; a joined action copies its twin's
   attempts; the row cap; squatting goes to `~2`. Measures bytes per action (3.3) on a
   recorded event stream and states it.
3. **(M) The in-memory recorder** wired to the seams of 3.4, with the bounded channel and
   memory bound. Tests: a full channel drops and marks `complete: false` without blocking
   (plant an awaiting send, the test's timeout catches it); eviction takes the least
   recently seen idle record; an end-to-end run with fake daemons (`kbf-testkit`) yields
   the expected rows for a hit, a run, a requeue and a failure.
4. **(M) The read API** (4.1 to 4.5 minus blobs) and the policy keys (4.3). Tests: paging
   while rows are appended never skips or repeats (property test); a token with another
   filter is `400`; with no `invocations` key the routes are `404` (plant "default allow",
   it goes red); an authorizer refusing one instance refuses the page.
5. **(S) Blobs over HTTP** (4.6). Tests: bytes equal ByteStream's; a store returning
   corrupt bytes yields an error, not the bytes (plant skipping verification, red);
   `Range`; `Content-Disposition`; `inline` refused for a blob over 4 MiB.
6. **(S) `--invocation-url-template` and `ExecuteResponse.message`.** Tests: template
   validation (no `{id}`, two, non-http) exits 2; a failed Execute's response carries both
   links; the id-less fallback mints a `kbf-` id whose page exists.
7. **(M) Durable records** (3.5 stage 2): chunks as CAS blobs, roots, the pin command in a
   new log format, recovery marking open invocations incomplete. After Pins; ordered with
   the HA work. Tests: restart keeps every flushed row; a crash loses at most the window;
   `kbf-sim-cell` gains the property "every pinned root's chunks are readable".
8. **(S) Retention and delete**: pin expiry, `DELETE`, `--invocation-pin-outputs=failed`,
   once collection runs. Tests: an expired pin is collected; a pinned failed action's
   stderr survives a collection that frees its successful neighbour's.
9. **(S) `GET /build/{id}`**: the redirect and the minimal HTML page.
10. **(S, separate track) `ResourceUsage` from the native and container drivers**, already
    planned in [scheduler.md](scheduler.md#accounting); until then `usage` is `null`.

PRs 1 to 6 give a working page that lives until restart; 7 makes it durable.

## 10. Open questions

Each with a recommendation; none is decided here.

1. **Whose id names the page: the client's `tool_invocation_id` or a farm-minted one?**
   Recommend the client's: it is the `Build ID` buck2 already prints and Bazel's
   invocation id, and a farm id cannot reach the user.
2. **Calls with no id.** Recommend 2.3: a one-action invocation for an id-less Execute,
   linked in its response; cache and CAS calls counted farm-wide only.
3. **Reading pages before real authentication exists.** Recommend fail closed (4.3): the
   routes are off unless the policy file names an `invocations` authorizer or
   `--invocations-open` is given; a public deployment fronts the listener with an
   authenticating proxy until `jwt` lands.
4. **How long to keep a build.** Recommend the record 30 days; artifacts on the CAS's own
   retention, failed actions' outputs and logs pinnable by flag (3.6). Pinning every
   build's outputs would hold most of the cache.
5. **Durable home.** Recommend CAS chunks plus one pin per invocation in the log (3.5),
   sequenced after Pins and the HA log work, with the in-memory stage shipped first.
6. **The link on a successful buck2 build.** Recommend: today, the printed `Build ID` in
   the template and the link in failed actions' messages; send the upstream buck2 change
   (5.2, items 1 and 2); kbf ships no client wrapper.
7. **Record cache hits as rows?** Recommend yes, capped (3.3): a page of a fully cached
   build would otherwise be empty, and "why was this a hit" is half of what people ask.
8. **The human path.** Recommend `/build/{id}` for the page (the word users say) and
   `/v1/invocations/{id}` for the API (REAPI's word).
