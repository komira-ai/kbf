# tierstore-bench

A measurement tool for tierstore: the put-to-ack, cold-read, copy-lag and tree-read cells of the
tierstore paper, run against a test instance of the binary, and the verbs around them (preflight,
provision, fault proxy, teardown check, report).

The cells (`run`) and `report` are built; `preflight`, `provision`, `fault-proxy` and
`verify-teardown` are stubs that say "not yet" and exit 2. Each module's header says what it holds.

## What it never holds

The result JSON, the CSV and the code name roles (`hot`, `cold`, `instance`, `proxy`,
`generator`), never a host, an address, a bucket or a key. Those come from flags and from a
private run file (`--run-file PATH`, JSON, kept outside every repository). Secrets are in the run
file only, never in argv. The request log (`requests.jsonl` in the result directory) is the one
file that names buckets and keys; it is the harness-side proof of what the tool sent, and stays
with the private run directory.

## Commands

```
tierstore-bench preflight        read-only checks and the before-snapshot
tierstore-bench provision        test buckets and scoped accounts (dry run unless --apply)
tierstore-bench run              cells cold-put, cold-read, g (first stage); a1, b, c (with a hot tier)
tierstore-bench fault-proxy      the loopback proxy that slows, refuses or blackholes the cold side
tierstore-bench verify-teardown  PASS/FAIL per check against the before-snapshot
tierstore-bench report           one table from the result JSON and CSV files
```

## Results

One JSON per cell (`schema_version`, see `result.rs`) and a CSV of raw samples with the git SHA,
the binary digest, the pass index and a quiet or busy flag on every row.

## Cells

`run --cell NAME [--cell NAME ...] --passes N --in-flight 1,16 --object-bytes 4096,1048576`
runs the cells in order. Every cell writes under `tierstore-bench/<run id>/` of its test bucket,
deletes what it wrote, and leaves in the result directory `<cell>-<arm>.json` per arm,
`samples.csv` (one row per counted request), `requests.jsonl` (every request the tool sent) and
`ledger.json` (cold bytes written by the campaign so far, against the caps).

Tierstore at v0.0.37 has no cold-only mode (`--hot` is a required flag). So the cold cells label
their paths in the arm names and the result notes say which they are:

| cell | arms | what the number is |
|---|---|---|
| `cold-put` | `direct`, `direct-mp`, `instance` | put-to-ack on the cold path: one PUT to the cold bucket (the floor), a multipart upload to it (the floor of the next), and a multipart upload through the instance, whose parts go straight to its cold tier (the instance runs two-tier, with a hot tier of its own) |
| `cold-read` | `direct`, `instance`, each with a `.ttfb` arm | a streamed read of objects only the cold tier holds: time to the last byte, and to the first |
| `g` | `instance`, `direct-cold`, `direct-hot`, `loopback-http`, `local-disk` | a read of the 18,105-file, 344,233,431-byte synthetic tree at each width, with the baselines |
| `a1` | `instance`, `hot` | put-to-ack of a plain put, acknowledged from the hot tier |
| `b` | `<size>-c<n>` and `.ttfb`, `.second`, `.cold-direct` | a read of a hot miss with copy-back, the second read, and the direct read |
| `c` | `<N>x<size>-c<n>-<mode>` | copy lag of a burst, found by one prefix LIST of the cold bucket a second |

The rules (also in `cells/mod.rs`): pass 0 of every arm is a discarded warm-up, arms are
interleaved across passes, the load gate is read before a pass and every few completions in it
(a trip ends the run and marks the pass busy), a cell is refused before it sends anything when
its cold bytes are over the pass, cell or campaign cap, requests are held to the rate caps, and
a p99 is filled in only from quiet passes with 1,000 requests or more. Fault arms (`latency`,
`bandwidth`, `refused`, `blackhole`, and `proxy-pass` as the control) need the fault proxy as a
`FaultSwitch`; a plain `run` has none and measures the healthy arms.

`report --result-dir DIR` prints one table per cell: per arm and window, requests, failures with
their time to failure, p50 and p95 with a bootstrap interval, p99 where it may be claimed, the
slowest, and the rate; labelled as measurements of a test instance on test buckets.
