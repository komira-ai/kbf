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

- tierstore's own cold tier stays the existing MinIO bucket; its hot tier is local flash. tierstore listens on loopback
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
  `storage_half.rs`] Two ways to run it, with these consequences:
  - *Registration is not the loopback string* (recommended). Each node runs on its flag with a permanent
    `identity_mismatch` and needs its own keys through `--cold-store-credentials-file` (on main), not `AWS_*`. Whether the
    frontend or operator API treats a mismatched node as a fault was not read.
  - *`http://127.0.0.1:9300` is registered farm-wide.* Every node matches the same string while pointing at a
    **different** tierstore. That is correct only because of an invariant the design relies on: **every node's tierstore
    fronts the same cold bucket and prefix.** The registration then describes a store no single address names, and a node
    whose tierstore is pointed elsewhere would match and not be noticed.
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
- **Visibility and copy lag.** An object node A wrote and its tierstore has not yet copied to MinIO is only in A's hot
  tier, so it is invisible to node B's tierstore. kbf reads it through its own location index, which is fine; but B's
  rebuild, or its tail pass at an owed handoff epoch, does not list it, and the join or drain fence is confirmed on those
  acknowledgements, so it can be confirmed without that object in the index. **Rule: no join or handoff while any node's
  copy lag is above 0.** This depends on tierstore exporting the copy lag, which `/metricz` does not do today. [code:
  tierstore `limits.md`, `metrics.md`; `fill.rs`; `rebalance-handoff.md`]

## 3. Migration, one node at a time

1. Drain the node. [proposed]
2. Run the held staging script (installs and starts the node's tierstore, checks it against the bucket). [proposed]
3. Run the held repoint script (writes the key file and the new `--cold-store`, restarts kbf). [proposed]
4. Check from the attempt-5 measurement that tierstore lookups are at least 95% of kbf's cold-store GETs; the
   denominator is `kbf_cold_store_requests_total{op}` (kbf#2191, merged), counted per node. [proposed]
5. Rollback is the repoint script with the old flag, after the node's tierstore shows no copy lag: an uncopied object
   read from the bucket directly would be marked `Missing`. [proposed]

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
  (C) how the frontend treats a mismatched node; (D) callers of `retain_until`; (E) how a collector learns of adoptions;
  (F) for tierstore: copy lag on `/metricz`, a latency histogram, hot-only objects in listings, list cost on a large
  prefix, GC-key behaviour, drain with an unfinished copy, `start-after` paging, and `_journal/` and `_tiering/` hidden
  from listings.
