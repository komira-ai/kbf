# tierstore-bench

A measurement tool for tierstore: the put-to-ack, cold-read, copy-lag and tree-read cells of the
tierstore paper, run against a test instance of the binary, and the verbs around them (preflight,
provision, fault proxy, teardown check, report).

This is the scaffold: every verb but the schema, the configuration and the request log is a stub
that says "not yet" and exits 2. Each module's header says what it will hold.

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
