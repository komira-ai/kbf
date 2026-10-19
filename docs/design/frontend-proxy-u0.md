# Frontend proxy, unit U0: what the code says about the design's assumptions

Status: report for `docs/design/frontend-proxy.md` (section 14, "Not verified"), written against `origin/feature/arch-v2` at the merge of #1095. Every verdict below names the code or the test it rests on. Tests are in `crates/kbf-server/tests/frontend_proxy_u0.rs`, `crates/kbf-server/src/storage_node_tests.rs` and `crates/kbf-state/tests/frontend_proxy_u0_postgres.rs`.

**How to read the tests.** A test that passes pins behaviour the design relies on. A test marked `#[ignore = "U0 red: ..."]` asserts what the design says and fails today; run it with `--ignored` (kbf-server) and remove the `ignore` in the commit that makes it pass. Red tests are in `kbf-server`, not in `kbf-state`, because the `state-postgres` job runs `kbf-state` with `--include-ignored`.

**What was not run.** The Postgres tests (`a_takeover_waits_for_a_fenced_write_that_has_passed_its_check`, `no_write_of_the_old_handle_commits_after_the_takeover`, and the two Unix-socket connect tests) compile (`cargo test -p kbf-state --features postgres --no-run`) but were not run on the box that wrote them: no throwaway Postgres could be started there. The `state-postgres` CI job runs the first two. The socket tests need `KBF_TEST_POSTGRES_SOCKET_DIR` and print "skipped" without it, so nothing runs them yet; U1 should add a socket-sharing variant to that job. Verdicts (b) and (d) therefore rest on code reading plus the parse test and the fake-store tests, and say so.

## Verdicts

| | Assumption | Verdict |
|---|---|---|
| (a) | A `Fenced` old child survives and keeps serving relays | **Holds for the cooperative stand-down; one gap for a fence the child only discovers** |
| (b) | The Postgres Unix-socket URI works with the `kbf-state` backend | **Holds** (parse proven; connect not run) |
| (c) | The store key families match the code | **Wrong in names, incomplete, and the history rows are not what the table says** |
| (d) | The fence is checked atomically inside each write | **Holds for `txn`/`apply_batch`; fails for `reap_expired`; the "exempt appends" have no code path** |
| (e) | Nodes re-register blobs against a fresh store; a fresh floor does not lock nodes out | **Blobs: holds, but costs one restart per old boot when the id repeats. Floor: confirmed as the design stated it (a fresh map started at the build's own `PROTOCOL`), fixed in U9 (it starts at the oldest protocol the build accepts); the per-node `oldest_protocol` does not exist** |

## (a) A fenced child survives

What survives, with the evidence:

- **Scheduler farm.** `Durable::note_failure` sets a sticky `fenced` flag on the first `StoreError::Fenced` (`farm/durable.rs`, `note_failure`, `superseded`); held effects are dropped, nothing is granted, new work is refused `FAILED_PRECONDITION`. Existing tests, run green here: `a_superseded_scheduler_refuses_work_and_does_not_wait_for_the_store`, `a_fenced_store_ends_the_retry_of_a_refused_result`, `a_fenced_farm_cannot_finish_an_operation`, and in `handover`: `a_fenced_farm_answers_its_callers_from_what_the_successor_wrote`.
- **Membership sweeper**, the one background task `kbf --mode frontend` runs beside its listeners (`frontend.rs`, `background`). `members_rpc::sweep` logs a failed refresh or reap and tries again at the next beat. New test `the_membership_sweeper_outlives_its_fencing` runs it through five beats on a fenced handle: it does not finish.
- **Operator enrollment API** maps `Fenced` to a status (`enroll/api.rs`), history drops and counts the rows it cannot write (`kbf-history/src/writer.rs`, "A failed transaction drops its rows"). Neither exits.
- **Update controller** returns `ControllerError::Rows(Store(Fenced))` to its caller; it is a library, wired into no binary on this branch (`grep Controller::new` finds only tests). So "reconciler" has no running instance to fence yet.
- Nothing in `crates/kbf-server/src` exits the process on `Fenced`: the only exit paths are listener errors and the handover `departed` signal in `frontend.rs`.

**The gap.** The relay (`Farm::follow`, which answers a fenced farm's waiting callers from `done/` rows) is called only from `handover/outgoing.rs`, after `Stand::run` has called `Farm::fence()`. A farm that only discovered the fence (its own write came back `Fenced`, so `Durable.fenced` is set but `State.fenced` is not) stops writing and granting, and no code calls `follow()` for it: its callers wait on in-memory state that can no longer complete. The design's "does not exit, finishes its open streams from `done/` rows" holds only if U2 drives `follow()` from the sticky flag as well as from `stand-down`. This is a code-reading finding; no test is added because nothing could pass or fail it until U2 chooses the trigger.

## (b) The Postgres Unix-socket URI

`postgres:///kbf?host=/run/postgresql` parses to a single `Host::Unix("/run/postgresql")`, database `kbf`, no user, and `PostgresConfig::parse` accepts it (its `NoHost` check counts hosts, and a Unix host is one). With no user in the URL the pinned client (`postgres` 0.19 over `tokio-postgres` 0.7.18) connects as the operating-system user (`connect_raw.rs`: `None => whoami::username()`), which is what peer authentication matches, so the design's "no password in argv or env" form is right. Test `the_design_url_parses_to_a_unix_socket_host` passes; `the_user_less_design_url_opens_a_store_on_the_socket` and `a_url_with_the_role_opens_a_working_store_on_the_socket` do the connect and are the part not run here. `PostgresStore::open` also requires `synchronous_commit` on at connect (C6), which the design already says.

## (c) Key families

The store holds these families on this branch (from the key constructors, and for membership from a registration run in `membership_writes_these_key_families`):

| Code | Rows | In the design table? |
|---|---|---|
| Scheduler (`farm/durable.rs`) | `op/`, `queue/`, `lease/`, `session/`, `loss/`, `fate/`, `floor/`, `subject/`, `done/`, `cordon/`, `meta/epoch`, `meta/next_op`, `meta/next_seq`, `meta/alive`, `meta/generation` | `lease/`, `op/` yes; the epoch is `meta/epoch`, not `epoch`; the rest are not named |
| Membership (`members.rs`) | `map`, `member/`, `down/`, `instance/`, `deny/`, `meta/members_formed`, counters `member-id`, `boot/<id>` | "member map" only |
| Enrollment (`kbf-enroll-registry`) | `enroll/token/`, `enroll/node/`, `enroll/serial/`, `enroll/key/`, `enroll/adv/`, `enroll/req/`, `enroll/renew/`, `enroll/ca/generation`, and `deny/` (shared with membership) | no |
| Update controller (`update/rows.rs`) | `update/desired`, `update/generation`, `update/policy`, `update/slot`, `update/host/`, `update/order/<epoch>` | `update/*`, `desired` (it is `update/desired`) |
| History index (`kbf-history/src/keys.rs`) | `ui/streams`, `ui/invid/`, `ui/ichunk/<inv>/<writer>/<seq>`, `ui/obj/`, `ui/stream/`, `ui/inv/`, `ui/shape/` (root configurable) | no: the design names `history/<instance_id>/` and `log/<instance_id>/` |

Findings that change the design:

1. **There are no `history/<instance_id>/...`, `log/<instance_id>/...` or `frontend/<instance_id>` rows.** History and run logs are objects in the object store; the Store holds only their `ui/` index. No per-instance heartbeat row exists (`instance/<id>` is the storage member's id).
2. **The history index is not one-writer-per-row.** Only `ui/ichunk/...` carries the writer id, and that id is the start time in microseconds (`history/driver.rs`, `Options::default`), not the proposed instance UUID. `ui/invid/<inv>` (a header, `Cond::Any`), `ui/inv/` list entries, `ui/shape/` and the `ui/streams` counter are shared by every writer. Two children recording one invocation write the same header row. Test `only_the_history_chunk_rows_carry_the_writer_id` pins which builders take a writer.
3. **The old child's history appends are fenced today.** `HistoryFlags::open_with` hands history `Arc::clone(&state.store)`, the `Fenced` handle (`history/flags.rs:90`), and `Fenced` adds the generation check to every transaction. Test `the_history_index_of_a_superseded_frontend_is_refused_by_the_production_handle` drives the production path (`StateStoreUri::open`, `demote`, `open_after_handover`) and shows the refusal. The design's "appends, exempt from the fence" has no code path; red test `a_fenced_child_can_append_rows_under_its_own_writer_id`.
4. The release index is a directory of files (`release/store.rs`), and CAS blob ownership is in each node's metadata log, not in the Store.

U2's table should be generated from the list above, and the "who may write" rule needs a decision for the shared history rows (give them a per-writer key, or write them from the generation holder only).

## (d) The fence is atomic with the write

- `Fenced::apply_batch` appends `Txn::check(GENERATION, Cond::Version(v))` to each transaction (`kbf-state/src/fence.rs:105`), so the check commits or fails with the write. On Postgres the whole batch runs under the commit-head row lock, so a takeover cannot commit between A's check and A's commit: it queues. Test `an_ordinary_write_cannot_pass_a_takeover` (fake, passes). On a real server, `a_takeover_waits_for_a_fenced_write_that_has_passed_its_check` holds A's checked write open and asserts B's take is blocked on a lock, then ordered after A, then A refused; `no_write_of_the_old_handle_commits_after_the_takeover` races four writers against a takeover and asserts every committed row is older than the takeover. Those two are the section 8 test, and are not run here (above).
- **`Fenced::reap_expired` is check-then-act** (`fence.rs:137`): it reads the generation with a separate `get`, then reaps. Red test `a_reap_cannot_pass_the_fence_check_and_then_commit_after_a_takeover` interleaves B's takeover after A's read and A's reap still removes the row (`Ok(1)`). The effect is small (expired rows would be reaped by anyone) but it is the pattern the design forbids, and the sweeper calls it every beat. Fix: reap through one checked transaction.
- `demote` is passed through to the inner store, and the Postgres backend has none (it returns `Failed`), as the design says.

## (e) A fresh store

**Blobs.** A node's index is its own metadata log; the Store never held blob locations. Re-registering against a fresh store re-uploads nothing: test `a_node_given_a_new_member_id_by_a_fresh_store_keeps_its_blobs` shows every old blob present and the bucket unchanged. The caveat is the registration number. A fresh store hands out member ids from 1 and boots from 1, and `SetWriter` refuses a boot not after the latest the log has set for that member (`kbf-meta/src/state.rs:687`, `StaleWriter`). A node that gets the member id it already used exits 75 once per boot its log has set, restarting after `RestartSec=5`, and opens on the next one (`a_node_given_its_old_member_id_opens_only_after_one_restart_per_old_boot`: a node with four old boots needs four restarts, uploads nothing, loses no blob). It is in no map for that time, and the number of restarts is the node's lifetime boot count. Red test `the_first_registration_on_a_fresh_store_opens_a_node_that_had_run_before` asserts the design's reading. Options for U9 or the cutover script: seed the fresh store's `member-id` and `boot/<id>` counters above what the nodes report, or have the node's registration carry its latest boot so the store starts above it.

**Floor.** U0 confirmed what the design stated: the first map of an empty store had `min_protocol == PROTOCOL`, and `register` refused `protocol < PROTOCOL` using the build's constant, not the map's floor. Both were 1, so no lock-out could be shown; the first release that raised `PROTOCOL` would have locked out the nodes that were not updated. **Fixed in U9** (`members.rs`): the first map's floor is `OLDEST_PROTOCOL`, the oldest protocol the build accepts; every later map carries `max(stored floor, OLDEST_PROTOCOL)`, so the floor only rises; `register` reads the map first and judges the node against that floor, not against a constant. `members::PROTOCOL` and `OLDEST_PROTOCOL` are the worker protocol's constants (`worker::PROTOCOL_VERSION`, `OLDEST_PROTOCOL_VERSION`), so they cannot drift. Every constant is still 1, so test `a_fresh_stores_first_map_floor_is_the_oldest_protocol_the_build_accepts` stands in for a later build (`Members::with_oldest_protocol`), and `tests/members_floor.rs` covers the stored floor, a higher oldest and the order of a store error and a protocol refusal. Limits, as design section 9 states them: storage nodes do not enforce `Map::min_protocol` (nothing outside `members.rs` reads it but the proto conversion); the floor never falls, so a frontend rolled back to a build with a lower oldest keeps it (a flag day); an operator-set floor is not built. The design's rule R1 compares against `oldest_protocol`, a protocol each connected node reports, which is still nowhere in the code; the registration floor above is not that field. Daemons are a separate check (`kbf-daemon/src/daemon.rs:779` requires `Welcome.protocol_version == PROTOCOL_VERSION`), as C1 says.

## What to change in the design

1. Section 8's table: replace the guessed families with the table in (c); `meta/epoch`, `update/desired`; add enrollment and the scheduler's other rows; drop `history/`, `log/` and `frontend/` rows or say they are to be created.
2. Decide the shared history rows (`ui/invid/`, `ui/inv/`, `ui/shape/`, `ui/streams`), and give the old child an unfenced append handle for the per-writer ones, or stop saying it appends.
3. Section 5/8: name what drives `Farm::follow()` for a child that discovers the fence.
4. Section 8: reap through one checked transaction.
5. Section 11: the cutover script must deal with the member id and boot counters (seed them, or accept one restart per old boot per node).
6. Section 9: `oldest_protocol` is new code, not a field to compare with.
