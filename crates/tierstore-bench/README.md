# tierstore-bench

A measurement tool for tierstore: the put-to-ack, cold-read, copy-lag and tree-read cells of the
tierstore paper, run against a test instance of the binary, and the verbs around them (preflight,
provision, fault proxy, teardown check, report).

This is the scaffold, with cell `cold-put` and the `cleanup` verb built: every other verb and cell
is a stub that says "not yet" and exits 2. Each module's header says what it will hold.

## cold-put and cleanup

`run --cell cold-put --run-id ID --in-flight 4,8,16 --puts N` sends N create-only puts per level
(`--object-bytes`, default 1 MiB) under `<prefix>/<ID>/` of the cold bucket, ending a level at its
time cap (`--level-secs`) or byte cap (`--pass-cap-gib`, `--cell-cap-gib`), and stopping the run
when the load gate trips, a `--stop-file` appears or the load cannot be read (exit status 2, with
the result of the level written). `--same-key --in-flight 16` instead sends 16 concurrent
create-only puts of different bytes to one key and reports the status counts and whether the
stored bytes are the winner's. Rates and latencies are reported raw, with no verdict.

The workers of a level share one thread. Each level's notes say how much of its wall time went to
making bodies (`client work: ...`); hashing and signing are inside the puts and are not separated,
so that share is a floor of the client's own work. A high share at large objects or high rates
means the client, not the store, may be bounding the PUT/s. The hidden `--proc-dir` and
`--sys-block-dir` point the load gate at a made-up tree for tests; a run that uses them says so in
its notes and on standard error.

`cleanup --run-id ID` deletes the keys the run's `requests.jsonl` lists as written (answered 200,
or sent and not answered), nothing else, and prints the counts written and deleted and the objects
still under the run prefix.

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
tierstore-bench cleanup          delete the objects a run of cold-put wrote, as its request log lists them
tierstore-bench fault-proxy      the loopback proxy that slows, refuses or blackholes the cold side
tierstore-bench verify-teardown  PASS/FAIL per check against the before-snapshot
tierstore-bench report           one table from the result JSON and CSV files
```

## Results

One JSON per cell (`schema_version`, see `result.rs`) and a CSV of raw samples with the git SHA,
the binary digest, the pass index and a quiet or busy flag on every row.
