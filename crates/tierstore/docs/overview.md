# What tierstore is

tierstore is an S3-compatible object store server that puts a small, fast tier in front of a large, slow one. Clients speak S3 to one bucket. New objects are written to the hot tier, copied to the cold tier in the background, and removed from the hot tier when it fills. It is a Linux binary, `tierstore`, with no package and no container image.

This guide is for the person who installs and runs it. It describes what is on the main branch of the source today. A piece that is planned and not in the code is described only under a heading titled "Not in a release yet", so you can tell the two apart.

> Which release first carries what: the configuration file (`--config`, `--print-config`) came with v0.0.39; the OCI read front (`--oci-listen`) and the build node (`[local_cache]`, the read cache) with v0.0.40. `--check-config`, `local_cache.write_through` and the hot and cold counters on `/metricz` are on main and not in v0.0.40; the drain's `Connection: close` and lingering close, and `tierstore_drain_late_requests_total`, are on main and not in v0.0.41. The command-line flags of every other setting work in every release. The pages say where a setting needs a build of main.

## The shape of a deployment

One `tierstore` process runs three things over the same pair of tiers:

- a proxy, which answers reads and takes writes;
- the S3 front of that proxy, which checks signatures and serves one bucket, path-style, over plain HTTP;
- a reconciler, which copies written objects to the cold tier, applies deletes, evicts from the hot tier and publishes the hot tier's usage. Only one reconciler works at a time: it holds a lease in the hot tier, so any number of processes may run over the same tiers.

A tier is `memory` (lost when the process stops, for tests), `dir:<path>` (a directory of the local file system) or `s3:<bucket>` (a bucket of an S3-compatible store).

The intended v1 layout is a hot tier on SSD in front of a cold tier on spinning disks:

```text
client --S3--> tierstore --> hot tier  (dir: on SSD)
                         \-> cold tier (dir: on HDD, or an S3 bucket)
```

A write goes to the hot tier and is acknowledged once a journal entry for it is durable there. The reconciler later copies it to the cold tier. A read is served from the hot tier, else from the cold tier, and what it found in the cold tier is copied back into the hot tier. So the hot tier is also a cache of what was used lately. When the hot tier is full, new objects are written straight to the cold tier.

## What it guarantees and what it does not

- A put is create-only: a key that exists in either tier is refused with 412 `PreconditionFailed`. Objects are immutable.
- A put is acknowledged only after its journal entry is durable in the hot tier. Losing the hot tier loses objects that were not yet copied to the cold tier.
- It asks each tier the questions these guarantees rest on before it starts (see "The start-up self-test" in [operations](operations.md)) and refuses to start on a store that answers one wrongly.
- It does not scrub: nothing re-reads stored bytes to find rot. See [limits](limits.md).

## Where to go next

- [Install](install.md): the tarball, the account, the directories and the unit.
- [Keys and roles](keys.md): which key may do what, and the file rules.
- [Configuration](configuration.md): every flag and every key of the file.
- [Operations](operations.md): probes, usage, drain, eviction, a full disk, metrics.
- [Troubleshooting](troubleshooting.md): the messages the process prints and what to do.
- [Limits](limits.md).

## Not in a release yet

These are planned for the v1 shape and are not in the code on main. Nothing here can be configured today.

- **Client retries on 503.** Clients retrying a 503 with jittered waits, and the server waiting briefly for a free put slot before it answers 503.
