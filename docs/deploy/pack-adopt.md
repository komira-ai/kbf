# Adopting the segments the storage members hold into the pack index

Storage members flush their blobs to the cold store as segments: the blobs back to back, then a
footer that indexes them. That is the layout of a pack, so the pack index can point at a
segment where it lies. `kbf-pack-adopt` does that: it lists the members' bucket, reads the
footer of each segment (never its records), and records one pack per segment under the
segment's own key, with the digest, offset and length of every blob its footer lists. No byte
is copied, nothing is written to the bucket, and nothing is deleted from it. The only thing it
writes to the bucket or the database is the pack index, and only with `--mode apply`; it also
writes the checkpoint and report files you name.

It is a binary of its own (crate `kbf-pack-adopt`), not a verb of `kbf`. **It is an
operator-only step.** An apply writes to the production database, so it runs only when a
person who administers the farm has chosen to, at a time they picked. Nothing in kbf runs it:
no package script, no unit, no update and no CI job. A dry run writes nothing to the index or
anywhere else but the files you name.

## The order

1. **The frontends write packs.** `--pack-index` and the pack store flags
   ([kbf-modes.md](kbf-modes.md)) are on, with `--pack-store-bucket` set to the bucket the
   storage members flush to. The tool refuses `--mode apply` unless the bucket you give it as
   `--pack-store-bucket` is the bucket of the segments: a pack is recorded where its segment
   lies, and readers look for packs in the pack bucket.
2. **Let the members flush.** The tool adopts what is in the bucket. A block a member has
   staged on its hot store and not yet flushed is not a segment yet. A segment flushed after a
   run is found by the next run.
3. **Dry run.** `--mode dry-run` is the default. Read the report: the segments found, the blobs
   and bytes they list, the packs and rows an apply would add, the objects that are not
   segments, the keys in no known layout, and the problems.
4. **Apply.** The same command line with `--mode apply --pack-store-bucket <bucket>`.
5. **Run it again until it adds nothing.** An apply is idempotent on the pack's key: a pack
   recorded before changes nothing and is counted as already recorded, and a blob that has a
   row keeps it. The run after the last member flush should report `packs: new 0` and
   `rows: new 0`.

Daemons still upload their outputs to the storage members, so while daemons run, members keep
flushing new segments, and a run is only as current as the moment it listed. Run it again
later, and once more after the members are quiet. **Nothing deletes from the members**: they
go on serving everything they hold, and the index only adds a second way to find it. A pack
that no blob row points at (every blob of the segment has a row in another pack) is reported by
the index as unreferenced; no tool deletes it now, and one that did would delete a member's
segment, so do not run one over adopted keys until the members are retired.

## The command

```text
kbf-pack-adopt --source-store-endpoint http://s3.example:9000 --source-store-bucket kbf \
    --source-store-credentials-file /etc/kbf/adopt-keys.json \
    --pack-index 'postgres:///kbf?host=/run/postgresql' \
    --report-json adopt-report.json --checkpoint-file adopt-checkpoint.json
```

| Flag | Meaning |
|---|---|
| `--source-store-endpoint <url>` | the members' S3-compatible store, `http://host:port`. The tool only reads from it. A `file://` store is refused, because a local store evicts the segments a pack would point at (exit 2) |
| `--source-store-bucket <bucket>` | the bucket of the segments |
| `--source-prefix <prefix>` | the prefix of the members' keys, empty or ending in `/` (default `kbf/`) |
| `--source-store-region <region>` | the signing region (default `us-east-1`) |
| `--source-store-credentials-file <path>` | the key pair, as JSON `{"access_key_id": "...", "secret_access_key": "..."}`, in a file that group and others cannot access (mode 0600 or 0400). The secret is never a flag: a command line is world-readable. A key that may list and read the bucket is enough |
| `--pack-index <url>` | the Postgres state store that holds the pack index, as a URL with no password (a command line is world-readable), such as `postgres:///kbf?host=/run/postgresql`: the tool connects as the user that runs it. Its schema must have the pack tables (`kbf-installer setup`) |
| `--mode dry-run\|apply` | `dry-run` (the default) records nothing; `apply` records the packs |
| `--pack-store-bucket <bucket>` | with `--mode apply`, the bucket the frontends write packs to; refused unless it is `--source-store-bucket` |
| `--checkpoint-file <path>` | keeps the report so far here, written beside the file and renamed over it, and resumes from it. A checkpoint of another mode, prefix or source store (endpoint and bucket) is refused |
| `--report-json <path>` | writes the report here when the run ends, whichever way it ends |
| `--segments-per-second <n>` | the most segments handled a second. Each is one transaction on the index: start low on a live farm |
| `--footer-concurrency <n>` | footers read from the store at once (default 4, at most 64: each is a request to the members' store; more is refused with status 2) |
| `--limit-segments <n>` | stop after this many segments; the checkpoint says where |
| `--verify-sample <n>` | read one blob back from each of the first `n` segments and hash it (default 20 in a dry run, 0 with `--mode apply`) |
| `--fail-on-conflict` | end the run, with status 1, at a segment whose key the index holds as a pack with other contents. Without it a conflict is counted and listed |

There is no `--force`, and no flag skips a check.

## What it adopts

Under the prefix, the keys the members write are `<prefix>cas/<member>/<boot>/<seq>` (a
storage node's; writer `legacy/<member>`), `<prefix>cas/<epoch>/<seq>` (a metadata log's epoch;
writer `legacy/epoch-<epoch>`) and `<prefix><log>/cas/<epoch>/<seq>` (the epoch of another log;
writer `legacy/<log>/epoch-<epoch>`), each number in 16 lowercase hex digits. A segment under
any other key is counted as "key in no known layout" and left alone. So is an object that is
not a segment (shorter than a segment's trailer, a foreign or torn tail), one deleted between
the listing and the read of its footer, and one whose footer the store failed to read (counted
as unreadable and listed in the problems: run again from the start to bring it back).

The sample catches a footer that does not match its bytes. For each of the first `n` segments
it reads one blob with a ranged request and hashes it; a blob that does not hash to its
footer's digest, or cannot be read back, is counted and listed, and in an apply its segment is
not recorded. It checks `n` blobs, not every blob.

A dry run asks the index, for each segment, whether the pack is recorded already and which of
its digests have a row, and totals what recording would add. Each segment is looked at on its
own: a blob that is in two segments not yet recorded counts as new in both, so the number is an
upper bound. A pack the index holds under the segment's key with another writer, size or
number of blobs is counted as a conflict, as an apply would; a dry run cannot see the other
ways a pack can differ, so it reports no more conflicts than an apply does.

## The report and the checkpoint

The report is printed, and written with `--report-json`. Its fields, in the JSON:

| Field | Meaning |
|---|---|
| `mode`, `prefix`, `source` | what the run did, over what; `source` is the endpoint and the bucket |
| `objects`, `segments` | objects listed and read; of those, segments |
| `gone`, `short`, `not_a_segment`, `read_failed`, `foreign` | objects not adopted, by why |
| `blobs`, `bytes` | what the adopted segments list |
| `packs_new`, `packs_known` | packs the index lacked (recorded, or to be); packs it held already |
| `rows_new`, `rows_existing`, `bytes_new` | blobs that got a row; blobs of new packs whose digest had one already; the bytes of `rows_new` (a dry run only) |
| `conflicts`, `sampled`, `sample_bad` | packs held with other contents; blobs read back; of those, the ones that did not match |
| `problems` | the first 100 objects that were a problem, with `unreadable`, `conflict` or `mismatch` |
| `last_key`, `resumed_after`, `finished` | where the run got to; where it resumed from; whether the listing reached its end |

The report so far is the checkpoint. It is saved every 256 objects, after the records of those
objects are in the index, and when the run ends. Run the same command line again after a stop
(`--limit-segments`, a failure, a killed process) and it lists after `last_key` and carries the
counters on, so a run stopped and resumed ends with the report of one run. A checkpoint of a
run that finished is ignored: the same command line starts from the first key, because an
object can appear below keys already seen (a slow flush that finished after a later one), and
the next run must find it. A checkpoint of another mode, another prefix or another source
store is refused, with status 2. An object that was not adopted is not retried by a resumed
run: one whose footer could not be read (`unreadable`), a segment whose sampled blob did not
match (`mismatch`) and a segment whose key the index holds with other contents (`conflict`).
Run from the start (move the checkpoint away) to bring them back. A run that stops at
`--fail-on-conflict` has not handled the conflicting segment, so its checkpoint does not count
it, and a run resumed from that checkpoint meets it again.

## Exit status

0: the run finished, or stopped at `--limit-segments`. 2: refused; the command line, a file it
names or the checkpoint is not acceptable, and nothing was changed. 1: a call failed (the index,
the listing, a file write, or a conflict with `--fail-on-conflict`); the report is still printed
and written, and the checkpoint says how far the run got, so the records of the segments before
the failure are in the index. A run that finishes with problems in its report is status 0: read
the report.
