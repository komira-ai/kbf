# `kbf daemon` reads through a tierstore on loopback

Status: built (`crates/kbf-node/src/tierstore.rs`, `crates/kbf-daemon/src/packed.rs`). Operator
reference: [kbf-modes.md](../deploy/kbf-modes.md), the `--tierstore-endpoint` paragraph.

## 1. What it is for

A build node that is not a storage node (a Mac running `kbf daemon` with the `native` driver)
reads every input blob from the frontend: over `--cas`, or from the storage nodes of the map the
frontend sends. A tierstore on that node, in front of the farm's object store with its
`[local_cache]` on local disk, can serve the blobs that are in packs from the node's own disk.
`--tierstore-endpoint` gives the daemon that read path. The frontend stays the source of truth:
the tierstore is a cache that may miss, be stale, be damaged or be down, and none of that may
fail a build.

## 2. Placement

A pack is one object of the object store holding many blobs; the frontend's pack index
(`kbf.worker.v1.PackIndex/Locate`) says which pack holds a blob and where. Blobs that are not in
a pack have no key in the object store the tierstore could serve, so the read path covers the
blobs the index places: packs written by frontends and daemons (`cas/f/`, `cas/d/`, `cas/b/`)
and storage nodes' flushed segments once they are indexed (`cas/<member>/...`).

- The first read of a blob asks the index (the same call `--pack-read` makes, over `--cas`).
- The answer is kept in memory (`kbf_node::tierstore::placed::Placed`, at most 65536 places,
  the oldest going first). A pack is written once under its key and never changed, so a kept
  place stays true while the pack exists.
- A later read of the blob is found from the kept place: it asks nothing of the frontend, not
  even its index, and goes on while the frontend cannot be reached.
- A place whose pack turns out gone (the tierstore answers 404) or whose bytes are not the blob
  is forgotten, so the next read asks the index again.

## 3. Loopback only

The endpoint must be `http://` to a loopback IP literal (any `127.*` address, or `::1`) with an
optional port; a name, `localhost` included, a path, a query, a fragment and user info are
refused at start (`NotLoopback`, naming what is wrong). A name is refused because what it
resolves to is not known to stay on the host.

The client (`kbf_node::tierstore::client::TierClient`) makes one kind of request, a signed
ranged `GET`, and is built so that the request cannot leave the host whatever the endpoint
answers or the environment says:

- no proxy: `HTTP_PROXY`, `ALL_PROXY` and the system's proxy settings are not read;
- no redirect: a `3xx` is a failed read, and no second request is made;
- no name resolution: the address is the literal of the endpoint.

The key is a read-only key of the tierstore, read from a file (mode 0600 or 0400), never a flag
or an environment variable.

## 4. Reads and fallback

Every read the tierstore does not serve goes to the plane underneath (the frontend), exactly as
without the flag, and succeeds or fails as it would have:

| What happens | Counted as | The read |
|---|---|---|
| the tierstore serves the bytes and they are the digest | `served` | served from the tierstore |
| the index places the blob nowhere, or the frontend serves no index | `elsewhere` | the frontend's |
| the index cannot be asked, or the tierstore is down or answers an error | `unasked` | the frontend's |
| the tierstore answers 404 or a short range | `fallback_lost` | the frontend's |
| the bytes are not the digest (never served) | `fallback_damaged` | the frontend's |
| the blob is over 64 MiB | `elsewhere` | the frontend's |

Only a `206` whose `Content-Range` is the range asked for and whose body is exactly that long is
taken; a body is read in chunks and refused at the first byte past its range. Nothing is tried
twice: a refused connection costs one attempt and the read falls back. A tierstore that accepts
and never answers holds a read for the request timeout (60 s); at most 16 reads ask at once
(`PackedCas`'s turns), and a cancelled read gives its turn back.

A read from a pack holds the blob in memory (`kbf_pack::Reader`), which is why blobs over
64 MiB are not asked of the tierstore: they are read from the frontend, streamed to disk, as
without the flag. They lose the cache, not correctness. Streaming them through the tierstore
would need a streaming `GET` in the object-store client and is not built.

## 5. What does not change

- Without `--tierstore-endpoint` no tierstore is built or contacted; the start, the plane and its
  answers are the ones the daemon had before the flag (the golden tests of
  `crates/kbf-node/src/tierstore/tests.rs` and `.../tests/plane/transcript.rs`).
- Outputs are stored where they always were.
- `--pack-read` keeps its behaviour and its counts: a lost or damaged pack fails its read and is
  counted in `lost` or `damaged`.

## 6. Compatibility

The heartbeat's `PackReadCounts` gains two fields, `fallback_lost = 7` and
`fallback_damaged = 8`. `lost` and `damaged` keep their shipped meaning, reads that failed. A
frontend of the previous release skips the two new fields and so counts a tierstore fallback in
none of `lost` and `damaged` (the read's outcome is the frontend's answer, which the frontend
sees for itself); a frontend of this release reads a daemon of the previous release with both at 0.
The tests are `a_frontend_of_the_previous_release_skips_the_fallback_counts` and
`counts_of_the_previous_release_decode_with_no_fallbacks`
(`crates/kbf-proto/tests/heartbeat_pack_reads.rs`). The frontend exports the two as
`kbf_pack_daemon_fallback_lost_total` and `kbf_pack_daemon_fallback_damaged_total`
([api.md](../api.md), "Pack reads by daemons").

The settings are new flags and `[packs]` keys of `kbf.toml`, all optional; a file written before
them reads as it did.

## 7. Where it runs

Only `kbf daemon`. `kbf storage-and-daemon` and `kbf all-in-one` refuse the flags at start: their
daemons read through their own storage member or in-process. `--pack-read` and
`--tierstore-endpoint` are refused together.
