# Backfilling the L2 sidecars of tree lists, and sampling live keys for the audit

A tree list is written at `cas/l/<id>`, `<id>` the SHA-256 of its bytes. With dual emission
(tierstore's `docs/design/fetch-cache-serve.md`, 3.5) the frontend also writes its L2 sidecar at
`cas/l2/<id>`, made from the SHA-256 of every 1 MiB cell of every object the list names. The
frontend writes a sidecar from cells the pack index already holds. When an object the list names
has no recorded cells (it was written before cells were recorded), the sidecar is not written and
the tree is recorded **unlisted** in the pack index.

`kbf-l2-backfill` gives those trees their sidecar, id-preserving: the list is not rewritten, and
its id does not change. For each unlisted tree it reads the list, reads once each object the list
names that has no recorded cells, records the cells in the pack index, writes the sidecar
create-only, and drops the tree's unlisted row. It does the work of
`kbf_front::action_tree::Sidecars::backfill`, the same engine as the frontend's miss path.

It also takes the live-key sample that `tierstore store audit --keys` reads (below).

It is a binary of its own (crate `kbf-l2-backfill`), not a verb of `kbf`. **It is an
operator-only step.** An apply writes to the production database and bucket, so it runs only
when a person who administers the farm has chosen to. Nothing in kbf runs it. A dry run writes
nothing to the index or the bucket.

## When to run it

1. **After the dual-emission release is deployed** to every frontend, so that no frontend writes
   a list without trying its sidecar. The backfill takes only trees recorded unlisted: a tree
   whose sidecar a frontend tried and could not write.
2. **Off-peak.** The backfill reads packs once: it reads every object it records cells of, whole,
   from the store. Use `--trees-per-second` to bound it; start low.
3. **Before readers roll** to the release that reads only the sidecar, so that recent trees have a
   sidecar before any reader needs it. `--within-days` (default 14) is how far back the unlisted
   trees are taken from, by when they were last recorded.
4. **Dry run first**, then the same command with `--mode apply`, then again until it reports
   `trees 0`.

## The command

```text
kbf-l2-backfill --store-endpoint http://s3.example:9000 --store-bucket kbf-packs \
    --store-credentials-file /etc/kbf/l2-backfill-keys.json \
    --pack-index 'postgres:///kbf?host=/run/postgresql' \
    --mode apply --trees-per-second 5 --report-json l2-backfill-report.json
```

| Flag | Meaning |
|---|---|
| `--store-endpoint <url>` | the S3-compatible store the frontends write their packs and tree lists to, `http://host:port`. A `file://` store is refused (exit 2) |
| `--store-bucket <bucket>` | the frontends' `--pack-store-bucket` |
| `--store-region <region>` | the signing region (default `us-east-1`) |
| `--store-credentials-file <path>` | the key pair, as JSON `{"access_key_id": "...", "secret_access_key": "..."}`, in a file that group and others cannot access (mode 0600 or 0400). The secret is never a flag. A dry run needs a key that may read; an apply, one that may read and create objects under `cas/l2/` |
| `--pack-index <url>` | the Postgres state store that holds the pack index, as a URL with no password, such as `postgres:///kbf?host=/run/postgresql`. Its schema must have the cell and unlisted-tree tables (migration 5) |
| `--mode dry-run\|apply` | `dry-run` (the default) writes nothing; `apply` reads the objects and writes the sidecars |
| `--within-days <n>` | take the unlisted trees last recorded within `n` days (default 14, at least 1) |
| `--trees-per-second <n>` | the most trees handled a second; each is a read of the list and, in an apply, of its unrecorded objects. A tree that took longer than the spacing is followed at once by the next |
| `--limit-trees <n>` | stop after `n` trees (at least 1); the report says `all handled false`. Run again to go on: a tree done is no longer unlisted |
| `--report-json <path>` | write the report here when the run ends, whichever way it ends |
| `--audit-keys-out <path>` | write the live-key sample here (below) |
| `--audit-keys <n>` | the sample's size (default 1000); needs `--audit-keys-out` |
| `--audit-seed <n>` | the sample's seed (default: from the clock); needs `--audit-keys-out` |

There is no `--force`, and no flag skips a check.

### What each mode does

- **Dry run** (no sample): lists the unlisted trees in the window and asks the store whether each
  list `cas/l/<id>` is there (a length request). It reads no object and writes nothing. The
  report says how many trees an apply would handle (`would write`) and how many lists are gone.
- **Apply**: for each tree, reads the list whole; `Sidecars::backfill` reads once each object it
  names that has no recorded cells (in windows of 8 MiB, hashing as they arrive), records the
  cells, writes the sidecar create-only (one already written is kept) and drops the unlisted row.
- **Dry run with `--audit-keys-out`**: the sample alone. No tree is looked at, so the run needs
  the index only (the store flags are still required, and not used).
- **Apply with `--audit-keys-out`**: the backfill, then the sample.

### What happens to a tree that cannot be done

| Count | Meaning | Unlisted row |
|---|---|---|
| `gone` | the store no longer has the list `cas/l/<id>` | dropped in an apply (nothing to write a sidecar of) |
| `lost` | an object the list names is not in the store: the tree is not present. The frontend's miss path republishes or rebuilds it | kept |
| `failed` | any other failure: the store or the index failed, or the bytes are not a version 2 list | kept |

The first 100 lost and failed trees are listed in the report with the engine's words.

## The report

Printed, and written as JSON with `--report-json`:

```json
{
  "mode": "apply",
  "backfill": {
    "within_days": 14, "trees_seen": 3, "written": 2, "would_write": 0, "gone": 1, "lost": 0,
    "failed": 0, "objects_read": 5, "bytes_read": 3145741, "finished": true, "problems": []
  },
  "audit": null
}
```

`backfill` is null when the run was the sample alone; `audit` is null when no sample was asked
for. `would_write` counts in a dry run, `written` in an apply. `audit` has `seed` (the one used:
pass it back as `--audit-seed` to take the same sample again), `requested`, `live_packs` (the
packs the index holds live) and `written`.

## The live-key sample for `tierstore store audit --keys`

`--audit-keys-out <file>` writes a uniform random sample of the keys of the packs the pack index
holds **live** (a pack some blob row points at; a pack with no live blob is never in it), for
the audit of tierstore's design 10.8, which checks that those keys exist in the store.

- **File format**: UTF-8, one key per line, each line ended by `\n`, sorted by key, no blank
  lines. Fewer than `--audit-keys` live packs: all of them. No live pack: an empty file. The file
  is written beside the target, synced and renamed over it.
- **Sampling**: the rank of a key is the SHA-256 of the seed as 8 little-endian bytes followed by
  the key's bytes, and the sample is the `--audit-keys` keys of least rank, found in one pass
  over the index with memory for that many keys. The same seed over the same live keys gives the
  same sample; another seed gives another.
- **Consistency**: the sweep reads the index a page at a time while frontends write, so a pack
  recorded or retired during the sweep may or may not be in the sample. The audit treats a key
  it cannot find as a finding to look at, not as proof.

```text
kbf-l2-backfill ... --audit-keys-out /var/lib/kbf/audit-keys.txt --audit-keys 2000
tierstore store audit --keys /var/lib/kbf/audit-keys.txt ...
```

## Exit status

0: the run finished, or stopped at `--limit-trees`. Lost trees are status 0: read the report.
2: refused; the command line or a file it names is not acceptable (a usage error included), and
nothing was changed. 1: a call failed (the index, a file write), or any tree `failed`; the report
is still printed and written, and the sidecars written before the failure stay written.
