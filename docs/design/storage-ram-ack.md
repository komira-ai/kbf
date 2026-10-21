# Acknowledging a storage write from RAM on both owners

**Planned. Nothing in this document is built.** Every statement under "Today" describes
the code; every other section describes a design. Until the change lands, a write is
acknowledged after it is durable on the primary and, unless the replica is lost (a repair
debt is journaled first), on the replica; a blob with no hot-disk room is durable in the
object store, and with one owner (R=1) there is one copy. [storage.md](storage.md),
[write-path.md](write-path.md) and [architecture-v2.md](architecture-v2.md) describe this.

The rules the design must keep:

1. Never report present what you cannot serve.
2. Anything durable that points at a blob is written only once the blob is on hot disk
   on at least one owner.
3. RAM is budgeted, a full budget pushes back, and the bytes not yet on disk are a metric.
4. Crash tests cover the new states.

Names: an **owner** is a node the map names for a blob; the **primary** is rank 1, the
**replica** the other (two owners at most, `MAX_REPLICATION` in
`kbf-storage/src/replicate/chain.rs`). **Hot disk** is the write tier (`kbf-tier`;
`PutStaged` in the node's metadata log); **cold** is the object store.

## Today: where a write is acknowledged

1. The client's write reaches the first non-Down write owner through the frontend
   (`kbf-front/src/proxy/bytestream.rs`, `kbf-route/src/write.rs`). The frontend neither
   buffers nor acknowledges. `BatchUpdateBlobs` reaches the same body through
   `Router::put` and `Storage::put`.
2. On the node, `write_in` (`kbf-storage/src/service/write.rs`) checks the map, reads the
   digest from the first message and plans the write (`plan` in `chain.rs`): a primary
   with a replica leg, or `Plan::Local` for R=1 and for a replica's own copy. A blob the
   node already holds is acknowledged at once.
3. `write_blob` opens the local store stream and, for a primary, a leg to the replica.
   `write_room` takes at most 4 MiB per write from `--write-bytes-in-flight` and holds it
   until the call returns.
4. `receive` sends each chunk to the local staging writer and to the leg; SHA-256 and CRC
   are checked as it passes. **Verification happens on both nodes**, because the replica
   runs the same steps with `Plan::Local`.
5. The replica's answer means "on my hot disk and indexed" (`Leg::finish`), or, when its
   own dedup found the blob, "already indexed" (`Leg::held`). A replica that cannot be
   reached (unavailable, stalled 30 s, no commit in 120 s) is "lost", not an error; a
   refusal fails the write.
6. **The primary's acknowledgement point** is `staged.commit().await` in `write_blob`.
   Before it, if the replica was lost, `chain.degrade` journals a durable repair debt.
   `commit` reaches `pool_commit` (`kbf-front/src/cache/pool.rs`): a blob of at most
   4 MiB (`BUFFERED_BLOB_BYTES`) is a `Vec` in RAM until then, becomes a `Whole` job, and
   the caller waits for the writer, which syncs the tier and applies one `PutStaged`.
   Larger blobs stream to a tier reservation and sync at the commit. No tier room or a
   full queue goes straight to the object store.
7. The client is answered.

The order today is replica durable, primary durable, client. No RAM tier of acknowledged
blobs exists: the only RAM a blob sits in is the pool's receive buffer. A failed
verification is `INVALID_ARGUMENT` or digest mismatch before any commit; a crash between
steps leaves the blob absent and the writer uploads it again.

Readers consult only the node's `MetaState` index (`find_missing`, `present`,
`is_durable`/`QueryWriteStatus`, `held`, `read_windows`, batch read, and the daemon's
Segments `locate`). The node's metadata log is a local single-voter log, so `PutStaged`
is a purely local commit; with more voters ([storage.md](storage.md#planned)) the drain
below would depend on a peer and the budget argument must be checked again.

Action-cache writes today: the frontend checks the closure with `Router::find_missing`
(primary only) and `Router::commit_with_closure` (`kbf-route/src/write.rs`) asks the AC
owner for what it lacks, copies those blobs there (`put_unowned`, a `Plan::Local` write)
and then sends `PutAction`, which `MetaState` applies only if every blob is `Present` in
the AC owner's own index. So rule 2 holds today on the AC owner by construction. On the
read side `Router::action_result` serves a hit only if every closure blob is held by an
owner (`kbf-route/src/closure.rs`); positives are cached per (blob, owner) for 45 s and
dropped when the router sees a new `Member.boot`. The cold flush (`cache/flush.rs`) is
hot to cold and is not touched by this design; `kbf_write_tier_unflushed_bytes` measures
that flush, so the new metric below does not reuse it.

## The new acknowledgement point

The primary answers the client when (a) its own copy is verified and registered in a RAM
**Held** set, and (b) the replica answered that its copy is verified and in its Held set
(or already indexed). The order is replica holds, primary holds, client. Writing to the
hot disk and applying `PutStaged` follow, independently on each node.

**Where Held lives.** A new map in `Cache` (`kbf-front`): digest to bytes, a budget
permit, and a state (`Queued`, `Writing`, `Failing{n}`). It is outside `MetaState` and
the Raft log: it must vanish at a restart and nothing durable may name it. The pool
already holds these bytes as a `Whole` job; the change is that the caller is released when
the entry is registered (in `pool_commit`) instead of after `PutStaged`, and the writer
removes the entry only after `put_staged` returned.

**Keying.** Registration is a `try_insert`. A second write of a digest already in Held is
a dedup acknowledgement: no second permit, bytes dropped. A drain therefore removes only
an entry whose permit it owns.

**Scope.** Only blobs of at most 4 MiB on the pool's `Whole` path. Larger blobs stream to
disk as they arrive and are never whole in RAM; R=1 and unowned copies have no second
owner. All of these stay acknowledged from disk. kbf-storage chooses the mode but only
`kbf-front` knows whether a put is pool-buffered, so `StreamedPut::commit(mode)` with
`Held` falls back to `Durable` there and counts `writethrough{reason}`. A replica learns
it is asked for a Held answer from a new leg header value; repair-worker copies do not
send it, because they need a durable copy.

**Replica lost at write time.** Write through, as today: journal the debt first, commit
`Durable`, acknowledge. There is no weaker mode. This includes the second owner taking a
client write as primary while the first owner is dead but not yet Down in the map: `plan`
in `chain.rs` makes it the primary with the first owner as the copy, the leg to the
first owner fails as unreachable, which is "lost" (`Leg`, `ReplicaLeg::lost`), and the
write goes through with a debt.

## Rule 1: reads and FindMissing

A reader looks in Held first, then in the index, as two steps in that order (the index
query runs inside one `MetaState` closure, so they cannot be a single snapshot). Held is
removed only after `put_staged` returned, so a reader that misses Held finds the index
entry. Sites: `find_missing`, `present`, `is_durable` (`QueryWriteStatus` reports the
full size for a held blob; this changes the sentence in [storage.md](storage.md)),
`held` and the dedup in `store_blobs` (skip a held blob only for non-durable callers),
`read_windows`/`located`/`read_blob` (a new `Source::Held(Bytes)`; windows are still
hashed), batch read, the write dedup, and Segments `locate`. A held blob has no object
location, so `locate` answers `None` and the caller falls back to a per-blob read.

After a restart Held is empty and the index holds only synced blobs, so the node answers
missing for what it lost.

## Rule 2: durable pointers

Plain FindMissing is unchanged: a blob is present if it is Held or indexed. Rule 2 needs
a durable question, which is a metadata header on `FindMissingBlobs` like
`MAP_GENERATION_HEADER`:

- `kbf-durable: only` answers from the index alone (a Held blob counts as missing).
- `kbf-durable: wait` first waits for the Held entries among the digests to drain,
  bounded by `--durable-wait` (default 10 s; startup-checked to be under half the 120 s
  chain commit bound), reports the still-Held digests missing on timeout, then answers
  from the index.

It is implemented as `Cache::durable(&[Digest])` and `Router::find_durable(digests, wait)`.

Every durable thing that names a blob, each barred or argued safe:

| Pointer | Disposition |
|---|---|
| AC entry (`PutAction`) | **Barred, two steps.** (a) `commit_closed_at`, before the record is committed, calls `find_durable(needed, wait)` on each blob's owners: every blob of the entry, the result included, must be durable on an owner. None: the commit fails (`UNAVAILABLE` on timeout, `Absent` if no owner has it). (b) The AC owner's `prepare_action_result` and `commit_action_record` call `Cache::durable(&closure)` before the unchanged `MetaState` check, so the non-owner `put_unowned` copy is durable too and a spurious `Absent` refusal cannot happen. Without (a) an entry could rest on a copy only the AC owner has and no reader consults, a permanent miss after both owners lose RAM. With it that needs both owners to lose their disks. Cost: one extra batched FindMissing per AC write. |
| Closure check on the read side | `closure_held` asks with `kbf-durable: only` and the router remembers only durable positives. An entry exists only after barrier (a), so its closure was durable when written; asking durable-only means a RAM-only positive is never cached and there is no 45 s window after a crash. A blob legitimately Held at read time is a miss (the action re-runs), never a wrong hit. |
| Control-log `Submit` | **Argued safe, not barred.** Submit is a request, not a claim that a result exists. It holds the action key and scheduling fields, not blobs. Execute reads the Action and Command before Submit. The daemon fetches inputs at run time; a blob the farm lost is `NOT_FOUND`, and the daemon reports `FAILED_PRECONDITION` with a `MISSING` violation (`kbf-daemon/src/lease.rs`, `kbf-server/src/missing.rs`), which the server turns into a failed result carrying that status. That is the status REAPI clients such as Buck2 re-upload and retry on: the action runs again and no missing blob is fetched as if present. The input check `find_missing_anywhere` counts Held blobs, which is right for rule 1 and costs nothing at Execute. Barring Submit would put a drain on every Execute; the `wait` flag makes that a small change if it is ever wanted. |
| `kbf-history` digest rows and `kbf-invocation` action rows | Argued safe: records of what happened (action digest, listing of output digests) for the UI. Nothing serves a blob or a hit from them. A listed lost output is a `NOT_FOUND` on download, as an evicted one is today. |
| Repair debts (`Repairs::owe`) | Argued safe: a debt is an obligation, not a claim. A debt whose blob the node no longer holds is dropped (`repair/worker.rs`, `settle_evicted`). Two changes: the worker's copies and probes use `Durable` mode and `kbf-durable: only`, and `settle_evicted`'s probe skips Held digests instead of dropping the debt. |
| `loss/<operation>` records | Not a blob pointer: the scheduler's lost-lease record, keyed by operation. |
| Result acknowledgement | Independent: the Result is accepted after `prepare` and the state store; the AC entry it leads to goes through the AC row. |

### As built (PR 4 of the plan)

The consumers of the durable question exist; nothing puts a blob in Held in a running
node yet, so each of them answers as it did and the tests put a blob there through
`Cache::hold_for_test`.

- `kbf-durable: wait` is parsed by the node (`Storage::find_durable_waiting`, which a
  storage that does not override answers as `find_durable`: no wait, a held blob missing)
  and `--durable-wait` (default 10 s, at least 1 s) bounds it; a start with a wait of half
  the 120 s chain commit bound or more is refused. `Cache::durable_waiting` and the AC
  owner's barrier (b) wait on a notification the drain's removal sends, not on a poll.
- Barrier (a) runs in `commit_closed_at` **after** the copies to the action's owner, so a
  blob no owner holds still ends the commit with the read's error, as before. It asks
  every blob's primary in one batched `wait` call, then the next owner for what the
  primary lacks (a blob durable on the replica only is enough). A blob no owner holds
  durably, and that some owner holds in RAM, is `UNAVAILABLE`; one that no owner holds at
  all (the action's owner has a copy, no reader asks it) is `Absent`.
- The closure check asks `kbf-durable: only` and so remembers only durable positives.
- The repair worker asks the owner `kbf-durable: only`. A node that receives a copy from a
  peer (a chain leg or the worker) dedups on the durable question, so a copy sent to an
  owner that holds the blob only in RAM is written. **PR 6** refines this with the leg
  header that asks for a Held answer: only a leg that asks for it dedups on Held.
- Not switched, on purpose: `settle_evicted` asks this node's own store the plain question
  (a Held blob is not evicted, and the debt stays); read repair's probe and
  `find_missing_anywhere` (the Execute input check) stay plain, as rule 1 and the table
  above say.
- Peer confirmation is `PeerConfirmer` (`kbf-storage/src/repair/confirm.rs`), run on the
  repair worker's ticks. `--ram-peer-grace` is 5 s. A peer that is down in the map, does
  not answer, or answers missing is owed a debt; a full set journals at once. Nothing calls
  `track` until PR 6.

## Rule 3: budget, backpressure and metrics

`--ram-ack-bytes` is per node. At hold time each node makes a **non-blocking**
`try_acquire`. On failure that node commits `Durable`, waiting for its own disk through
the pool (which is the backpressure), and counts `writethrough{reason=budget}`. No writer
ever waits on the budget, so there is no wait to bound and no cross-node deadlock: the
permit is freed only by a local drain, never by a peer. `write_room` is unchanged and
bounds bytes still arriving, so the worst case in RAM is `write_room` plus
`--ram-ack-bytes`, two flags, and the docs will say so. The only new waits are the
durable barriers, bounded by `--durable-wait`. A full pool queue already takes the
synchronous store path.

Metrics (beside the pool and write-tier metrics in `kbf-front/src/metrics.rs`):

- `kbf_storage_ram_unwritten_bytes` (required) and `kbf_storage_ram_unwritten_blobs`
- `kbf_storage_ram_unwritten_oldest_seconds`
- `kbf_storage_ram_budget_bytes`
- `kbf_storage_ram_drain_failures_total`
- `kbf_storage_ram_writethrough_total{reason=replica_lost|single_owner|large|queue_full|budget|store_path}`
- `kbf_storage_ram_unconfirmed_blobs`

## Drain and shutdown

The pool writer drains as it does today. `Staged`: remove the entry. `NoRoom` (tier full
or a disk error before the commit; today the blob goes to the store): keep the entry,
write the blob to cold, remove it on success. `Failed` (`PutStaged` failed; a re-commit is
safe): keep the entry as `Failing{n}`, retry with a capped backoff, never drop. An entry
that is not durable is **never evicted**: it is an acknowledged copy. Failure shows as
`drain_failures_total`, the oldest-age gauge and a warn log.

A graceful shutdown drains Held under its own bound, `--ram-drain-deadline`, after the
daemon's lease drain ends (`--drain-deadline-secs` bounds only that) and before the
storage half stops. A Draining member is not counted as filled for handoff
([rebalance-handoff.md](rebalance-handoff.md)) while it holds Held entries. `SIGKILL`
does not drain. `kbf-deploy meta-backup` refuses while `kbf_storage_ram_unwritten_bytes`
is not 0 ([storage-node.md](../deploy/storage-node.md)).

## Peer confirmation and the repair debt

After the acknowledgement both owners hold the blob in RAM. If one crashes, the survivor
holds the only copy and owes nothing. Each owner therefore keeps the digest in an
in-memory `unconfirmed` set until the peer is known durable. After its own drain, and
after `--ram-peer-grace` (default 5 s), it asks the peer once per batch with
`kbf-durable: only`. Present: forget. Missing: `Repairs::owe` (the existing durable
journal, off the ack path), and the existing worker copies the blob in `Durable` mode when
the peer is back. A restarted peer answers missing, so the survivor gets its debt. The
normal path does no IO: the debt is written only when the peer is not durable. The set is
capped; overflow journals the debt at once. `under_replicated_blobs` counts these debts.
Residuals: both owners crash inside the grace window (the kill-both case below), or the
survivor itself crashes before the peer's own probe fires (the peer then probes it).

## Failure cases

| Case | Expected |
|---|---|
| Kill the primary after the ack, before its disk write | On restart the index lacks the blob (the orphan block is removed) and the primary answers missing. The replica serves from Held or disk and drains; its confirmation probe finds the primary missing after the grace and journals a debt; the worker copies it back. |
| Kill the replica after the ack | Symmetric; the primary journals the debt. |
| Kill both after the ack, before either disk | Lost. Both answer missing; reads are `NOT_FOUND`; Execute's input check reports it and Buck2 re-uploads. No AC entry names it (barrier a); a replayed Submit fails `FAILED_PRECONDITION` with `MISSING` and the client re-uploads. After a whole-farm power loss the build re-runs; it never fetches a missing blob. |
| Disk full or tier refuses on write-behind | The entry stays; the blob goes to cold through the no-room path, or retries if cold also fails. Readable from RAM; the gauge grows; once the budget is full new writes degrade to `Durable` and wait for the disk. No OOM, no loss. |
| Restart mid-flush | Cold flush is unchanged. Mid write-behind (block synced, `PutStaged` not applied): the orphan is removed and the node lacks the blob, the same as "kill that node after the ack". A graceful restart drains first. |
| Third-node reader right after the ack | The router reads owners in rank order and asks the primary only for FindMissing; the primary holds it. No miss between ack and disk; the drain race is closed by the invariants below. |
| Replica down at write | Write through with a durable debt (today's behaviour). |
| Replica refuses | The write fails; the primary registers Held only after the join. |
| AC commit with a RAM-only closure blob | Waits at the barrier up to `--durable-wait`, then `UNAVAILABLE`; never `Ok` before the blob is on an owner's disk. |
| Budget full | That node commits `Durable`; the ack waits for its disk. |

Invariants: **I1** a node that answers present for a digest can serve exactly those bytes
now. **I2** a Held entry leaves only after the index shows it (or the store has it), and
never while not durable by another path. **I3** no `PutAction` is written for a closure
that is not durable on an owner; the other pointers are argued safe above. **I4** Held
bytes never exceed `--ram-ack-bytes`.

## Crash tests

Deterministic: gates, barriers and fault points; no sleeps and no wall-clock. "The ack came
before the disk" is asserted with positive events, not by polling a future once: close the
sync gates on both nodes, let the write return, then assert that writers are parked in
`sync` and zero syncs happened.

- T1 kill the primary after the ack: restart over the surviving log; the primary says
  missing; a router read returns the bytes from the replica; after the replica's gate
  opens its index has the blob and `unwritten_bytes` is 0; an explicit confirmation step
  finds the primary missing and journals exactly one debt. Mutant: ack before the replica
  registers.
- T2 kill the replica: the mirror.
- T3 kill both: both missing, the read is `NOT_FOUND`. For an AC commit, run it with
  gates closed, wait until the barrier reports it is parked, crash both: `action_result`
  is a miss and no entry exists. Positive twin: open the gates, the commit completes,
  crash both, the entry is present and the closure is on an owner. Mutants: delete
  barrier (a), delete barrier (b).
- T4 disk full: failing creates and failing store puts; the write is acknowledged and
  readable from Held; with a budget of one blob the next write completes only after the
  gate opens; `writethrough{budget}` is 1.
- T5 restart mid-flush: a hold on `PutStaged` after the sync; crash; the orphan is removed;
  the blob is absent on that node and intact on the peer.
- T6 third node and the drain race: park the drain after the index apply and before the
  entry removal; router read and FindMissing at both points return the bytes, in both
  lookup orders. Mutants: remove the entry before `put_staged` returns; look up the index
  before Held.
- T7 replica lost: the write returns only after the gate opens; the debt is journaled
  before the commit.
- T8 a blob over 4 MiB, and R=1, stay disk-acknowledged.
- T9 the gauge returns to 0; the budget is never exceeded under N writers.
- T10 duplicate in-flight writes of one digest: one permit, one removal, both
  acknowledged. Mutant: overwrite on insert (permit leak).
- T11 the closure check is durable-only: a RAM-only positive is not cached. Mutant: ask
  plain.
- T12 the repair worker copies in `Durable` mode and `settle_evicted` leaves a Held digest
  alone.

The harness these need does not exist yet. The existing helpers in
`kbf-front/tests/write_pool.rs` are private to that file, and its `until` and `bounded`
helpers poll or use a wall-clock timeout and are not used by the new tests. The work is
split across the PRs below: shared test support and a test-only drain seam first; the
two-real-`Cache` chain rig with the producer. The simulation cell has one server and
cannot host a two-owner seeded sweep (not planned); a one-node point (acknowledged from
RAM, crash, the client sees missing and re-uploads, no AC entry names it) is.

## Decisions and why

- **Budget default 1 GiB** (`--ram-ack-bytes`). The flag ships at 0 (off) until the last
  PR. `--durable-wait` is 10 s.
- **Replica down at write: write through.** The alternatives (one RAM copy, refuse, block
  until the map says Down) lose data on one crash, fail writes, or stall them.
- **Blobs over 4 MiB, and R=1, stay disk-acknowledged.** The first are never whole in
  RAM; the second has no second owner, so a RAM ack would be one copy.
- **Submit stays outside rule 2** (no wait header): a lost input becomes
  `FAILED_PRECONDITION` with `MISSING`, and the client re-uploads and re-runs.
- **An AC commit requires each closure blob durable on an owner**, the requirement that a durable pointer is written only once the blob is on hot
  disk on at least one owner (rule 2). The
  cheaper alternative (the AC owner's own disk only) can leave a permanent-miss entry
  after both owners lose RAM.
- **The closure check is durable-only and caches only durable positives**; keeping plain
  asks would accept a 45 s window after a crash.
- **Peer-confirmation grace is 5 s**: long enough that the normal path writes no debt,
  short enough that the single-copy window is small.
- **Daemon `Segments` uploads stay disk-acknowledged** (direct to each owner, acknowledged
  at `Commit`), as do `put_unowned` copies.
- **Acknowledged-but-lost non-AC blobs after a whole-farm power loss are accepted.** That
  follows from acknowledging from RAM: the loss needs both owners to lose RAM inside the
  write-behind window, and the consequence is a re-upload or a re-run, never a wrong hit.
  Turning the producer on by default waits for an explicit go on this point.

## Pull request plan

The producer is the last code change, behind `--ram-ack-bytes` (default 0), so every
barrier exists before anything can be held.

1. This document, and the corrected description of the router's boot rule in
   [storage.md](storage.md). Docs only.
2. Test support and a behaviour-free trait change: shared rig, positive-event waits, a
   `PutStaged` hold, `Switch` moved to `kbf-testkit`; `StreamedPut::commit(mode)` with
   `Held` implemented as `Durable`. No behaviour change.
3. The Held set, all the readers, `Cache::durable`, the `kbf-durable` header and
   `Router::find_durable`; a test-only insert seam, no producer.
4. The rule-2 consumers: AC barriers (a) and (b), the durable-only closure check, the
   repair worker's `Durable` copies and Held skip, peer confirmation.
5. Budget, drain, metrics, shutdown drain, the `meta-backup` refusal; a test-only hold as
   the only producer.
6. The acknowledgement point behind the flag, the leg header, the chain rig, the crash
   tests. The "durable, then acknowledged" statements change to the as-built behaviour
   behind the flag, in these places:
   - `write-path.md`: section 2 (the bullet that says "never report a blob present before
     its bytes are durable"), section 3.1 (the closing guarantee), 6.3 (what an
     acknowledgement means now), 6.4 (crash and restart: a synced block with no commit
     may now hold acknowledged blobs, lost on that node by design), 6.12 (the
     "no RAM acknowledgement" line).
   - `architecture-v2.md`: 4.1 (the contract), section 8 (Writes and AC bullets), section
     12 (failure modes), section 15 (decisions).
   - `guides/storage.md`: "Placement and replication" and "The write tier: what is durable
     when" (add a row for the state after the answer and before the disk: one node loss
     survives, both do not).
   - `deploy/storage-node.md`: "Metadata backup and restore" (the `meta-backup`
     refusal).
   - `storage.md`: Writes ("So a blob is never reported present before its bytes are
     durable", the ByteStream paragraph, `QueryWriteStatus`) and the PutAction check in
     the closure-check section.
7. Turn it on by default, deploy docs, an alert on `unwritten_oldest_seconds`, after a
   farm run and an explicit go.
