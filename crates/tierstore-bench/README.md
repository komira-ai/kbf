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

## Results

One JSON per cell (`schema_version`, see `result.rs`) and a CSV of raw samples with the git SHA,
the binary digest, the pass index and a quiet or busy flag on every row.

## fill-paper

`tierstore-bench fill-paper --paper P.html --cells cells.json --result-dir DIR --out OUT.html`
writes the measured numbers into the paper: each element carrying `data-cell="<id>"` gets the
formatted value of the mapped result and `data-source="<file>#<metric>"` and `data-label`
(`test instance` or `farm`); `data-blocked` cells are left alone, and the rest of the HTML is
copied byte for byte. `--in-place` replaces `--out`; `--check` writes nothing and exits 2 listing
placeholders without a mapping, mappings without a result and filled values whose source is gone.
The mapping schema and the rules for each figure are in the header of `src/fill.rs`.
