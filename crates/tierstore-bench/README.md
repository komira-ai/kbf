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

One JSON per cell (`schema_version`, see `result.rs`) and a CSV of raw samples with the git SHA,
the binary digest, the pass index and a quiet or busy flag on every row.

## Cells

`run --cell NAME [--cell NAME ...] --passes N --in-flight 1,16 --object-bytes 4096,1048576`
runs the cells in order. Every cell writes under `tierstore-bench/<run id>/` of its test bucket,
deletes what it wrote, and leaves in the result directory `<cell>-<arm>.json` per arm,
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

Tierstore at v0.0.37 has no cold-only mode (`--hot` is a required flag). So the cold cells label
their paths in the arm names and the result notes say which they are:

| cell | arms | what the number is |
|---|---|---|
| `cold-put` | `direct`, `direct-mp`, `instance` | put-to-ack on the cold path: one PUT to the cold bucket (the floor), a multipart upload to it (the floor of the next), and a multipart upload through the instance, whose parts go straight to its cold tier (the instance runs two-tier, with a hot tier of its own) |
| `cold-read` | `direct`, `instance`, each with a `.ttfb` arm | a streamed read of objects only the cold tier holds: time to the last byte, and to the first |
| `g` | `instance`, `direct-cold`, `direct-hot`, `loopback-http`, `local-disk` | a read of the 18,105-file, 344,233,431-byte synthetic tree at each width, with the baselines |
| `a1` | `instance`, `hot` | put-to-ack of a plain put, acknowledged from the hot tier |
| `b` | `<size>-c<n>` and `.ttfb`, `.second`, `.cold-direct` | a read of a hot miss with copy-back, the second read, and the direct read |
| `c` | `<N>x<size>-c<n>-<mode>` | copy lag of a burst, found by one prefix LIST of the cold bucket every `1 / --list-poll-rps` seconds (2 a second unless set) |

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

`run` needs `--run-id`, `--git-sha`, `--campaign-ledger` (see above), and a passed `preflight` in the result directory: its
`preflight.json` gives the binary digest, versions and hardware that every result carries.

`report --result-dir DIR` prints one table per cell: per arm and window, requests, failures with
their time to failure, p50 and p95 with a bootstrap interval, p99 where it may be claimed, the
slowest, and the rate; labelled as measurements of a test instance on test buckets.
