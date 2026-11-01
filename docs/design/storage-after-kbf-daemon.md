# Storage after `kbf storage-and-daemon`: each node's cold store is its own tierstore

Status: DESIGN; the held scripts of the migration are not written. Claims are tagged **[code]** (file and function in
this repository, or in the tierstore repository where named) or **[proposed]**.

## 1. The design

Each kbf storage member points its `--cold-store` at the tierstore on its own node, over loopback, with the S3 client kbf
already has:

```
--cold-store 's3://kbf-cas/av2dev1/?endpoint=http://127.0.0.1:9300&conditional-put=true'
--cold-store-credentials-file <the node's scoped key>
```

- tierstore's own cold tier stays the existing MinIO bucket, with a local read cache on flash and no hot tier. tierstore listens on loopback
  and on the node address; the node address is for metrics scraping only. [proposed]
- The storage node stays a kbf process and keeps the index, the action cache, the closure check, the digest checks, the
  placement map and the chain. Nothing in kbf's storage logic changes: the cold store is already an `ObjectStore`
  (`put_new`, `get_range`, `delete`, `list`), and `S3Store` is the same `objstore_s3` client tierstore uses for its own
  tiers. [code: `crates/kbf-objstore/src/lib.rs` `pub use objstore_s3`; `crates/kbf-server/src/stores.rs` `S3Uri`]
- The key is a scoped key: read and write under the farm prefix, no delete prefix (tierstore `keys.md`). [proposed]
- `--hot-store` (the kbf write tier) is left as it is. Bucket, prefix and key layout do not change, so every existing
  segment stays readable under its key and nothing moves. [proposed]

## 2. Facts the design rests on

- **Identity.** An `s3://` cold store is Registrable: it fetches the farm-wide registration and compares endpoint, bucket
  and prefix with its flag (`same_store`). [code: `crates/kbf-server/src/cold_apply.rs`; `storage_node.rs`,
  `storage_half.rs`] **Decision: register `http://127.0.0.1:9300` farm-wide (bucket `kbf-cas`, prefix `av2dev1/`).** Every
  node then matches the same string while pointing at its own, different tierstore. That is safe only because of an
  invariant the design relies on: **every node's tierstore fronts the same cold bucket and prefix.** A node whose
  tierstore were pointed elsewhere would match and not be noticed. (The other way, not registering the string, leaves
  each node on its flag with a permanent `identity_mismatch`, and nothing is lost by it but a standing warning.)
  `--cold-store-credentials-file` (on main) gives the node its scoped key, not `AWS_*`.
- **Integrity checks stay in kbf.** tierstore does not hash a storage node's objects. kbf hashes a blob against its digest
  on write (`VerifiedBlob`), checks the SHA-256 of the exact range on every read (marking the object `Corrupt` or
  `Missing`, never serving the bytes), and on the daemon `Segments` path `PutRange` hashes on receipt and `Commit`
  refuses an unhashed record. [code: `crates/kbf-front/src/cache/read.rs`, `crates/kbf-front/src/segments.rs`,
  `cache/upload.rs`; tierstore `limits.md`]
- **The standing tail lists only when an epoch is owed.** `fill::round` makes a pass only when the epoch the frontend sent
  is above what the node acknowledged; a pass is 2 to 8 listings (`settle`, `PASSES` = 8) of the whole `<root>cas/`
  prefix, with no cursor. Joins (and drains, per the design document) bump the epoch, and a node's restart owes one pass;
  at main there is no listing otherwise. List cost is **owed epochs x passes per epoch x objects in the prefix**, and the
  prefix only grows because nothing deletes segments. [code: `crates/kbf-server/src/fill.rs` `round`, `settle`,
  `registered`; `crates/kbf-front/src/cache/tail.rs`; `crates/kbf-server/src/members/fence.rs`;
  `docs/design/rebalance-handoff.md`]
- **Visibility: tierstore writes through.** With tierstore v0.3.0 configured with `[server]`, `[cold_store]` (the
  existing bucket) and `[local_cache]`, and no hot tier, a create-only PUT is answered only after the upstream has
  acknowledged it; the local copy is made afterwards, is best effort, and a skipped copy still succeeds. Multipart parts
  and the completion pass straight through. So the cold store holds every object before kbf sees its PUT succeed, no
  copy lag exists, and node B's rebuild or tail pass lists what node A wrote. [code: tierstore at tag v0.3.0,
  `crates/tierstore/src/server/local_cache/keep.rs` (module doc steps 1 to 4, `put_through`) and
  `local_cache/mod.rs` (`part_through`, `complete_through`)] **The design relies on tierstore running with no hot tier;
  the write-back tiered mode must not be configured, and a config with a hot tier would void this paragraph** (an
  acknowledged object could then be only on flash).

## 3. Migration, one node at a time

1. Drain the node. [proposed]
2. Run the held staging script (installs and starts the node's tierstore, checks it against the bucket). [proposed]
3. Run the held repoint script (writes the key file and the new `--cold-store`, restarts kbf). [proposed]
4. Check from the attempt-5 measurement that tierstore lookups are at least 95% of kbf's cold-store GETs; the
   denominator is `kbf_cold_store_requests_total{op}` (kbf#2191, merged), counted per node. [proposed]
5. Rollback is the repoint script with the old flag. Nothing waits for a copy: tierstore wrote every object through to the
   bucket before kbf saw its PUT succeed. [proposed]

## 4. Later, only if measured need (not decisions)

- Switching off the kbf write tier (`--hot-store`, "shape B"). Both owners already write their own segment to the cold
  store (`FlushesAll`), so this would not add cold bytes; open points are what the chain's acknowledgement waits for, and
  put-to-ack and ingress CPU. [code: `crates/kbf-server/src/storage_node.rs`, `fill.rs`]
- A per-node collector over a per-node GC key. No code deletes a segment today (`Cache::collect` has no caller outside
  tests); it must cover the epoch cadence plus one settle, or learn of tail adoptions. [code:
  `crates/kbf-front/src/cache.rs` `collect`]
- Packs on the same tierstore, under their own prefixes (`--pack-store-*`).
- Object Lock in tierstore. Not needed: the segment path passes no retention date (a search result, not a proof).
- Open questions: (A) what the chain's acknowledgement waits for with no write tier; (B) ingress CPU without a tier;
  (C) callers of `retain_until`; (D) how a collector learns of adoptions;
  (E) for tierstore: a latency histogram, list cost on a large
  prefix, GC-key behaviour, `start-after` paging, and `_journal/` and `_tiering/` hidden
  from listings.
