# tierstore-bench

A measurement tool for tierstore: the put-to-ack, cold-read, copy-lag and tree-read cells of the
tierstore paper, run against a test instance of the binary, and the verbs around them (preflight,
provision, fault proxy, teardown check, report).

Every verb is built. Each module's header says what it holds.

## What it never holds

The result JSON, the CSV and the code name roles (`hot`, `cold`, `instance`, `proxy`,
`generator`), never a host, an address, a bucket or a key. Those come from flags and from a
private run file (`--run-file PATH`, JSON, kept outside every repository). Secrets are in the run
file only, never in argv. The request log (`requests.jsonl` in the result directory) is the one
file that names buckets and keys; it is the harness-side proof of what the tool sent, and stays
with the private run directory.

## The test prefix

Every verb that loads a configuration needs a test prefix: `test_prefix` in the run file, or
`--test-prefix PREFIX` (the flag wins). It is lowercase letters, digits and dashes, a stem and a
closing dash (`bench7-`). Every test bucket and every scoped account id (`hot`, `cold`) must start
with it, or the verb stops before any store call and names the offending value. `plan` prints what
the run file resolves to, one `key=value` per line and nothing else: `test_prefix=`, then for each
store its `bucket=`, and for `hot` and `cold` its `account=` and, right after it, the `policy=`
(`<test prefix>tierstore-bench-<role>`) that `provision` creates for that account, so two
campaigns on one store do not share a policy. It reads the run file and sends
nothing, so a script can compare it with its own site file.

`provision --apply` changes only what it made. It looks (read-only) for the bucket and the account
before it makes either: one that is on the store and not in `provision-state.json` of the result
directory is someone else's, and the verb stops before changing anything. The account's secret is
given to `mc` on its standard input, never in its arguments.

## Pre-made buckets: `provision --premade`

MinIO cannot confine admin user, policy or quota actions to a name prefix: its policy engine
ignores `Resource` for admin actions, so a key allowed to create a policy and attach it to a user
can grant itself anything. So the admin work is split between two keys.

| who | holds | does |
|---|---|---|
| the held script `admin_mint.sh --campaign ...`, run by a person with the store's root key | everything | per campaign: the bucket, its 30 GiB quota, the bucket-only policy `<test prefix>tierstore-bench-<role>`, the scoped account and the attachment; and, with `--remove`, the reverse |
| the key this tool holds (the `admin` file of `ops.json`) | `admin:ServerInfo`, `admin:GetBucketQuota`; `s3:GetBucketLocation`, `s3:ListBucket`, `s3:ListBucketVersions`, `s3:DeleteBucket`, `s3:ForceDeleteBucket` on the campaign's buckets (`<prefix>*`); `s3:DeleteObject` and `s3:DeleteObjectVersion` on their objects (`<prefix>*/*`) | `preflight` (`mc admin info`), `provision --premade`, and the removal of a bucket's objects and the bucket itself |

That key has no `admin:ListUsers`, `admin:CreatePolicy`, `admin:AttachUserOrGroupPolicy`,
`admin:CreateUser`, `admin:SetBucketQuota` or `s3:CreateBucket`; every command `provision` runs
without `--premade` needs at least one of them.

`provision --premade` (with or without `--apply`) runs none of `mb`, `quota set`, `admin policy
create`, `admin user add` and `admin policy attach`, and does not list accounts. For each role of
the run (`hot`, `cold`) it:

1. checks that the test bucket is there (`mc ls` of that bucket); if it is not, or may not be
   listed, the verb stops and names the held script;
2. checks the quota with `mc quota info`: no quota, or one over 30 GiB, stops the verb (a smaller
   one passes);
3. with every role's checks passed, runs the two probes: the scoped account (the key pair the run
   file holds for the role) must list its own bucket, and must be refused a read of the live
   bucket the ops file names;
4. writes the bucket and the account to `provision-state.json` with `"premade": true`, so the
   teardown's guard finds them. The record is written only after the probes pass; the key of a
   run that made its own has no `premade` field, and a record without it reads as before.

If the scoped account can read a live bucket the verb stops and does not remove it (it holds no
right to): it says to run `admin_mint.sh --campaign ... --remove` now. The plan and the dry run
say `premade` on every line, and `provision.json` has mode `apply-premade` or `dry-run-premade`.
Without `--premade` nothing changes.

## Commands

```
tierstore-bench preflight        read-only checks and the before-snapshot
tierstore-bench plan             the test prefix, bucket names, account ids and policy names a run file resolves to
tierstore-bench provision        test buckets and scoped accounts (dry run unless --apply)
tierstore-bench run              cells cold-put, cold-read, g (first stage); a1, b, c (with a hot tier)
tierstore-bench fault-proxy      the loopback proxy that slows, refuses or blackholes the cold side
tierstore-bench verify-teardown  PASS/FAIL per check against the before-snapshot
tierstore-bench report           one table from the result JSON and CSV files
```

## Fault proxy

`tierstore-bench fault-proxy --listen 127.0.0.1:PORT --upstream http://HOST:PORT` forwards HTTP/1.1
to the store with every header as received (`Host` too, so a SigV4 signature survives), never
buffering a body. `--mode` is `pass`, `latency=MS` (or `delay=MS`), `bandwidth=BYTES`, `refuse`,
`blackhole`, or `skew=SECS` (moves `Date` and `Last-Modified`; refused unless
`--allow-date-rewrite`). `--ttl-secs N` makes every fault fall back to `pass` after N seconds.
`--control FILE` holds one line, `MODE [ttl=SECS]`, read every 200 ms and applied when it changes
(write it whole and move it into place). `--result-dir DIR` gets `proxy-counters.json`: requests,
body bytes each way, refused and held connections, mode changes, expiries. The verb runs until
SIGTERM and writes the counters on the way out. A request opens its own connection to the store,
so run each proxied arm once with `pass` as the control.

## Results

One JSON per cell, arm and attempt (`schema_version`, see `result.rs`) and a CSV of raw samples
with the git SHA, the binary digest, the pass index, a quiet or busy flag and the attempt on every
row. The newer fields (`cold_state`, `cold_guard`, `cold_window`, `provenance.attempt`) are optional on
reading: a result written without them reads as a first attempt with no cold readings.

## The ops file

The verbs that look at the node (`preflight`, `provision`, `verify-teardown`) and `run` read
`ops.json` in the result directory. It is private (paths, units, ports, credential files), so it
is never committed and nothing of it is copied into a result. Shape:

```json
{
  "tierstore": {"binary": "...", "sha256": "...", "version": "0.0.37", "plan_flags": ["--hot"]},
  "live": {"unit": "...", "binary": "...", "port": 9300, "dirs": ["..."]},
  "test_instance": {"unit": "...", "port": 9301, "dirs": ["..."]},
  "live_buckets": {"cold": "..."},
  "admin": {"cold": "/path/to/admin-key.json"},
  "verifier": {"cold": "/path/to/reader-key.json"},
  "mc": "/path/to/mc",
  "node": {"roles": ["instance", "generator"], "link_interface": "..."},
  "cold_heal_status_file": "/path/to/heal.json",
  "paper": {"page": "...", "map": "..."}
}
```

A credentials file (`admin`, `verifier`) is JSON of `access_key_id` and `secret_access_key`.
`cold_heal_status_file` is JSON of `percent_done`, kept by the operator. `live_buckets`, `admin`,
`verifier`, `mc`, `node`, `cold_heal_status_file` and `paper` are optional; `tierstore`, `live`
(with `dirs`) and `test_instance` are not.

`verify-teardown` reads the store with the `verifier` key. That key needs `s3:ListAllMyBuckets`
(the bucket list) and read on the test bucket (`s3:GetObject`, `s3:ListBucket`,
`s3:ListBucketMultipartUploads`, `s3:GetBucketLocation`). `test-bucket-removed` passes only when
the bucket list, read with that key, does not name the test bucket and does name the role's
live bucket (`live_buckets`). It fails if the bucket is listed and fails if the list cannot be
read (`AccessDenied` included). The live-bucket condition closes a blind spot: a store lists a key
without `s3:ListAllMyBuckets` only the buckets that key may use, so a mis-scoped verifier sees an
empty list, and an empty list proves nothing. A list that does not name the live bucket fails with
"the verifier's bucket list does not show the live bucket <name>: the key lacks
s3:ListAllMyBuckets, so the list cannot prove removal", and a role with no `live_buckets` entry
fails with a message that says so. An `AccessDenied` on the
bucket itself is never read as removal; a `NoSuchBucket` on it is noted and does not decide
alone. `no-multipart-uploads` reads the uploads of the bucket whenever `s3:ListBucketMultipartUploads`
is granted, whether or not the other listings work.

### The live directories

`live.dirs` lists the live tierstore's state and data directories on this node. It is required
and must not be empty. `run` resolves the result directory, the campaign ledger and the ledger's
lock file (`FILE.lock`) and each live directory (relative paths made absolute, `..` applied, links
followed, also a link to something that is not there yet) and refuses, before it creates anything,
a result directory, ledger or lock that is, contains or lies inside a live directory. `preflight`
does not check this: it writes `preflight.json` and `before.json` to the result directory, so keep
that directory clear of the live ones all the same.

## Attempts

`run --attempt N` (N from 1, 1 unless set) names the try of the run. The results of attempt N are
`<cell>-<arm>-a<N>.json` (an arm with a step suffix is `<cell>-<arm>.ttfb-a<N>.json`), and every
row of `samples.csv` ends in the attempt (column `attempt`). A run never writes over a result
file: if a result of one of its arms for its attempt is in the result directory it stops with
`ResultExists` before it sends one request, and a file that appears while it runs is not replaced
either (the file is made exclusively). To retry a cell and arm, run it again with the next
attempt number; every attempt is kept. `report` reads all of them: an arm that has more than one
attempt, or an attempt other than the first, is shown as `arm (attempt N)`, and its samples are
joined to its own attempt.

## Cells

`run --cell NAME [--cell NAME ...] --passes N --in-flight 1,16 --object-bytes 4096,1048576`
runs the cells in order. Every cell writes under `tierstore-bench/<run id>/` of its test bucket,
deletes what it wrote, and leaves in the result directory `<cell>-<arm>-a<N>.json` per arm and attempt,
`samples.csv` (one row per counted request), `requests.jsonl` (every request the tool sent) and
the campaign ledger.

The ledger counts the cold bytes written by the whole campaign against the caps. It is the file
`--campaign-ledger FILE` names, outside every result directory (a result directory can be cleaned;
the count must not restart). `run` stops if the file is missing, unless `--new-campaign` is given,
which makes it and refuses if it is already there. A pass is counted before it is sent (its planned
bytes), and the count is corrected to what was written when the pass ends; a run that dies leaves
the reservation counted. The file is replaced atomically on every save. A run holds the lock
file `FILE.lock` (made exclusively, holding the process id) from its start to its end, and a second
run on the same ledger is refused with the lock file named. A lock a dead run left is never broken
by the tool: check that the process is gone and delete the file. The ledger's directory must exist,
and the ledger must resolve (through links and `..`) to a place outside the result directory.

`run --open-loop on|off` (default `on`) says whether the put cells (`cold-put`, `a1`) add their
open-loop arms: 19 KiB at 20 puts a second for 600 s, and 1 MiB at 10 a second for 300 s, for every
path and in healthy runs only. They are about 60% of the bytes of a 1 MiB run (per path, 3,000 puts
of 1 MiB against the 1,000 of the closed-loop arm, per pass), so a closed-loop-only measurement at
1 MiB with six passes is `--open-loop off`: the plan is built without them, the ledger is asked to
admit the plan as built (worst pass, cell total and what is left of the campaign), and that run fits
the default caps where the same run with the arms does not. The other cells have no open-loop arms
and ignore the flag. The setting is recorded as `open_loop` in the provenance of every result (a
result that predates the field was run with the arms on) and the report names it in its header.

Tierstore at v0.0.37 has no cold-only mode (`--hot` is a required flag). So the cold cells label
their paths in the arm names and the result notes say which they are:

| cell | arms | what the number is |
|---|---|---|
| `cold-put` | `direct`, `direct-mp`, `instance` | put-to-ack on the cold path: one PUT to the cold bucket (the floor), a multipart upload to it (the floor of the next), and a multipart upload through the instance, whose parts go straight to its cold tier (the instance runs two-tier, with a hot tier of its own) |
| `cold-read` | `direct`, `instance`, each with a `.ttfb` arm | a streamed read of objects only the cold tier holds: time to the last byte, and to the first |
| `g` | `instance`, `direct-cold`, `direct-hot`, `loopback-http`, `local-disk` | a read of the 18,105-file, 344,233,431-byte synthetic tree at each width, with the baselines |
| `a1` | `instance`, `hot` | put-to-ack of a plain put, acknowledged from the hot tier |
| `b` | `<size>-c<n>` and `.ttfb`, `.second`, `.cold-direct` | a read of a hot miss with copy-back, the second read, and the direct read; per object the instance's reads come first and the direct read of the cold bucket last, so the miss is not timed against a store the harness has just read from |
| `c` | `<N>x<size>-c<n>-<mode>` | copy lag of a burst, found by one prefix LIST of the cold bucket every `1 / --list-poll-rps` seconds (2 a second unless set) |

Besides latencies every result records what a reader needs to tell a measurement from a cap or a
busy machine:

- `rate_caps`: the cold, hot and LIST-poll request caps of the run (`--cold-rps`, `--hot-rps`,
  `--list-poll-rps`). A cell whose requests wait at a cap measures the cap; `report` names the caps
  under the table of every cell.
- per pass, `peak_in_flight`: the most requests in flight at the start of one request, which is the
  width the pass realised (at 16 asked and a cap of 20 requests a second it is far below 16).
- per pass, `disk_busy`: for each whole disk of the machine that sends the requests, the
  milliseconds it spent doing IO during the pass (`io_ticks` of `/proc/diskstats`) and that over the
  pass's wall time, over the driven window only (the cleanup hook is not in it). It is this
  machine's disks; a cold store on another host has to be read there. `dm-*` and `md*` devices
  (LVM, RAID) are in `/sys/block`, so a stacked volume is listed beside its physical disk; busy time
  is per device, so nothing is summed.
- per pass of an arm that has the fault proxy in its path, `proxy_requests`: the requests the proxy
  forwarded by HTTP method (the proxy's own `requests_by_method` counter) while the pass sent its
  requests (`during`) and while the arm's cleanup hook ran (`cleanup`: the bench's own cleanup,
  plus any background traffic that happened to land then; background traffic is not separated, and
  an arm with no cleanup hook has about none). Old results call the field `after`. The report gives
  them per request.

`report` also sets each `instance` put arm of `cold-put` beside the `direct-mp` arm of the same
size, width and mode (both are multipart uploads); the single PUT `direct` is not its partner. The
number of measured requests of an arm is `(passes - 1) x requests per pass`: 10,000 requests at
1,000 a pass is `--passes 11`.

The rules (also in `cells/mod.rs`): pass 0 of every arm is a discarded warm-up, arms are
interleaved across passes, the load gate is read before a pass and every few completions in it
(a trip ends the run and marks the pass busy), a cell is refused before it sends anything when
its cold bytes are over the pass, cell or campaign cap, requests are held to the rate caps, and
a p99 is filled in only from quiet passes with 1,000 requests or more. Fault arms (`latency`,
`bandwidth`, `refused`, `blackhole`, and `proxy-pass` as the control) need the fault proxy as a
`FaultSwitch`. `run --fault-control FILE --fault-result-dir DIR` names the control file and the
result directory of a running `fault-proxy` (its `--control` and `--result-dir`): a fault arm
writes the file, waits until the proxy's counters say it is in the mode, and fails the arm if it
does not get there. The cold store the cell reaches must be behind that proxy. Without the two
flags a `run` measures the healthy arms. Every line the switch writes is `MODE ttl=SECS`
(`--fault-ttl-secs`, 300 unless set), so a run that dies with a fault in force leaves a proxy that
falls back to `pass` by itself; the time must be longer than the longest stretch a cell holds one
mode. The file is replaced atomically.

`run` needs `--run-id`, `--git-sha`, `--campaign-ledger` (see above), the ops file (`ops.json`, see
above) and a passed `preflight` in the result directory: its `preflight.json` gives the binary
digest, versions and hardware that every result carries. `--attempt N` is optional (see Attempts).

## The cold store: state before every pass, and the guard

The load gate sees this machine only. Two things look at the cold store itself.

**State.** Preflight reads whether the cold store is healing once; a campaign of hours cannot
carry that reading. Before every pass of every arm `run` reads it again, through what preflight
uses and read-only: `mc admin info` of the cold role (its drives and whether any reports healing)
and the status file of the ops file (`cold_heal_status_file`, `percent_done`). It is healing if a
drive says so or the status file is under 100. A reading that cannot be made (`mc admin info`
fails, or the status file is not one) stops the run, and so does a reading that takes longer
than `--cold-state-timeout-secs` (30): the `mc admin info` that has not answered by then is
killed (that command only) and the run stops with `cold state: unreadable`, the status file
notwithstanding. A run with no `cold` role reads nothing.

**File mode.** When the ops file has no `admin` entry for `cold` and names a
`cold_heal_status_file`, `run` and `preflight` never run `mc` for the cold role: a writer on a host
that holds the admin key keeps the file, and the file is the reading. (With an `admin` entry for
`cold` nothing of this applies: `mc admin info` is read, and an admin key that fails is an error,
never a fall back to the file.) The file is JSON:

```json
{"read_unix_ms": 1790000000000, "healing": true, "percent_done": 12.5,
 "drives": [{"id": "d-1a2b3c4d", "state": "ok", "healing": true}],
 "version": "RELEASE.<version>"}
```

`read_unix_ms` (integer, milliseconds since the Unix epoch), `healing` (bool) and `drives` (array of
`id`, `state`, `healing`, named as in a result) are required; `percent_done` and `version` are
optional; other fields are ignored. The reading is healing if `healing`, a drive or a
`percent_done` under 100 says so. The file is refused (`cold state: unreadable` for `run`, which
stops; a failed `store-version:cold` and `cold-heal-state` for `preflight`) if it is missing or not
this JSON, if `read_unix_ms` is older than `--cold-state-max-age-secs` (default 30, at least 1) before
now, or if it is more than 5 s in the future. The `cold_state` entry carries the file's
`read_unix_ms`, not the time the tool read the file, and `"source": "file"` (`"mc"` when `mc` was
asked). Preflight takes the store version from the file's `version`; without one the check
passes as `version unknown (file mode)` and the report records no cold store version. Only the
cold role has a file mode; a hot role still needs its `admin` entry.

The writer's contract: stamp `read_unix_ms` with the clock of the host that asked the store, at the
time of the ask; write the file only after `mc admin info` answered and was parsed; on any failure
write nothing, so the file ages out and the run stops. The tool compares the writer's stamp with
its own clock, so the two hosts' clocks must be in sync (NTP on both). A writer more than 5 s
ahead of the reader is refused as stamped in the future; a writer behind the reader spends that
difference from `--cold-state-max-age-secs`, so a file is refused sooner than its age alone says.

The file is a trust input: it decides whether the cold store is quiet. It is written by the
dev-box loop over ssh as the operator account, mode 0644, owned by that account (the user that
runs the tool), and it is read as a secret file is: the tool refuses it (the reading is
`cold state: unreadable` and `run` stops) if it is a symbolic link, is not a regular file (a FIFO
or a directory), is owned by another user, is writable by its group or by others, or is over
64 KiB. The same rule holds for the status file in `mc` mode.

Each reading is stamped into the `cold_state` array of the result of the arm, one entry per pass:

```json
"cold_state": [{"pass": 0, "healing": false, "percent_done": 100.0,
                "drives": [{"id": "d-1a2b3c4d", "state": "ok", "healing": false}],
                "read_unix_ms": 1790000000000}]
```

`drives` is the set the admin info listed, each drive named by the first eight hex digits of the
SHA-256 of its endpoint, so a change of the set shows without a host or path in the result.
`report` labels an arm `v1` only if the cold store was read before every one of its passes (the
warm-up included) and was healing in none; otherwise it prints `not labelled v1` with the passes
that saw it healing, or the passes that have no reading. `v1` here means only that: the figures
are still those of a test instance on test buckets.

**Guard.** The tool stops the run, as the load gate does (the pass is cut and marked busy, the
run ends, the results so far are written), when the cold store is used by something other than
the harness or is not healthy. The harness's own latency is measured, not guarded: for a small
write on an erasure-coded set of hard disks it is the quantity a cell measures, and a limit on it
stops a measurement of a store that is quiet and merely slow. The one latency limit left is a
backstop for a store that has stopped answering.

| flag | default | trips when |
|---|---|---|
| `--cold-abort-foreign-requests` | 10 | a pass saw more than this many requests at the cold store that this tool did not send |
| `--cold-counter-settle-secs` | 11 | (not a limit) the wait, tool silent, before each reading of the store's request counter |
| `--cold-abort-error-share` | 0.05 | the share of the window's requests that failed or timed out is above it (0 to 1) |
| `--cold-abort-p99-ms` | 30000 | the hung-store backstop: the p99 latency of the window is above it, milliseconds |
| `--cold-abort-window-secs` | 60 | the length of the window of the error share and the backstop, seconds |

It also trips, without a flag, when a drive of the cold store is not `ok` (or none is listed) or
the store is healing, read at the start of the run and at the end of every pass; when the
request counter cannot be read; and when it went backwards (the store restarted).

*Requests from elsewhere.* At the start of the run and at the end of every pass (after the
pass's requests and its hook) the tool reads the store's S3 request counter
(`minio_s3_requests_total`, summed over its series, with `mc admin prometheus metrics` and the
cold admin key) and its own count of requests to the cold role (every request of the request
log, a part of a multipart upload each). The window of a pass runs from the reading before it to
the reading after it, so setup and cleanup requests of the tool are in its count. Foreign
requests are the growth of the counter less the tool's count, at least 0, and equal to the limit
is within it. The counter is cached by the store for 10 s, so each reading waits
`--cold-counter-settle-secs` with the tool silent first; a pass costs that much more. With 0 a
reading may show the counter as it was up to 10 s before and charge a pass with its neighbour's
requests. Default 10: a quiet store showed none in 30 s; a probe or an operator's listing may
add a few; a job or a person at work adds far more.

The tool's count can differ from the store's for reasons that are not foreign load, and neither
can trip a quiet pass on the tool's own traffic. `objstore-s3` resends a ranged read up to
`READ_RETRIES` (2) times inside one logged call: a failed attempt that reached the store is in
its counter and not in the tool's count, so such a read adds up to 2 foreign requests (a few in a
window; the limit of 10 leaves room). Foreign is the counter's growth less the tool's count,
floored at 0, so a request of the tool that never reached the store (a refused connection) hides
one foreign request in its window; it can only hide, never make one.

The instance copies to the cold store on its own, for minutes after an instance arm's pass, and
the tool reads no signal of when the copies end. So from the first pass that sent a request to
the instance on, no window of the run is judged for foreign requests; the passes of an arm
through the fault proxy and the window after one are not judged either. Those windows are
recorded with `judged` false and the reason in `unjudged_reason` (the drives, healing, the
counter and the error share are still judged). A missed foreign load is accepted; a trip on the
run's own instance traffic is not. A run that wants its direct arms judged keeps them in a run
of their own, without instance arms. A run with no cold admin key (heal status file mode) has no
counter: the guard then keeps to the error share and the backstop, and the health reading is the
one before each pass.

*The window.* The error share and the backstop look at the harness's own requests to the cold
role over a sliding window. Equal to a limit is within it. The window counts one entry per
request to the cold role: every step of the `cold` role is one, whatever its label (cell B's
`.cold-direct` reads of the cold bucket are counted), except the `.ttfb` step of a streamed
read, which is the time to the first byte of a request already counted and is not a second one. A
window of fewer than 20 requests is not judged; with 20 to 99 requests in it the p99 is the
slowest request, so one request over the backstop can stop the run. A failed request counts in
the share and, with the time it took to fail, in the p99. Only requests the harness sent to the
cold role are counted (steps of the `cold` role), and not the arms that put the cold store in a
fault on purpose (the arms through the fault proxy other than `proxy-pass`), whose requests are
meant to be slow or to fail.

The guard does not watch the instance arms' requests to the instance; the instance's own copy
traffic to the cold store is throttled only by the held `COPY_PARALLEL` site key.

The trip is in the result, in the pass it cut (`cold_guard`: `metric` one of `foreign_requests`,
`drives`, `healing`, `counter`, `unreadable`, `error_share`, `p99_ms`; `value`, `limit`,
`requests` in the window, `window_secs`, and `detail`, the numbers in words), and `report` lists
it under the table. The abort line names the condition and its numbers, for example

```
cold store guard: foreign_requests is 25, over the limit 10: the store counted 65 S3 requests over the pass and this tool sent 40, so 25 came from elsewhere
cold store guard: drives is 1, over the limit 0: 1 of 12 drives are not ok: ["offline"]
cold store guard: p99_ms is 31000, over the limit 30000: the hung-store backstop: the p99 of the 20 requests this tool sent over the last 60 s
```

Every pass of a run with a counter records what the guard read, so the paper can show the window
was quiet (`cold_window`):

```json
"cold_window": {"minio_requests": 40, "own_requests": 40, "foreign_requests": 0, "judged": true,
                "drives": 12, "drives_not_ok": 0, "healing": false}
```

`minio_requests` is the growth of the store's counter over the window, `own_requests` the tool's
count over the same window, `judged` false for a pass that was recorded and not judged (above).
`report` prints the same under the table. The flags are accepted by every verb that takes the
shared flags, like the load gate's.

`report --result-dir DIR` prints one table per cell: per arm, attempt and window, requests, failures
with their time to failure, p50 and p95 with a bootstrap interval, p99 where it may be claimed, the
slowest, and the rate; labelled as measurements of a test instance on test buckets. Under each
table: the cell's notes, the `v1` label or the reason it is refused for each arm, the passes cut
by the load gate or the cold store guard, and the notes of the passes.
