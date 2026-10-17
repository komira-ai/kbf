# The update page: versions per host, update available, update action

Status: design. Nothing in this file is built. Facts marked **[F]** were read from the code on the branches named; **[D]** is
decided in another document; **[P]** is proposed here; **[Q]** is an open question (section 8).

## 1. Summary

An operator wants to see, on one page, which commit each host runs, which release the farm wants, whether each host's update
step is done or stuck, and to press "Update to <release>". Today almost none of that is readable.

- **[F]** The frontend receives each host's `UpdateStatus` (`kbf.update.v1`) and discards it (`worker.rs`, the `Offer(_) |
  UpdateStatus(_)` arm returns nothing, "Read by a later change").
- **[F]** There is no desired-release state, no update policy and no update route. The one update route built is the release
  store (`PUT` and `GET /v1/releases`).
- **[F]** The UI is read-only and has no form, POST, session or CSRF handling (`kbf-ui`, `ui-data.md` section 8). Every write is
  gated on a loopback peer, no `Origin` header and a bearer token, so a browser form cannot reach it by design.

So the work is a read path first (routes, then a read-only page), and a write path second, which needs one decision (Q1) before
any code. This document designs both and says what blocks each. Rollout order and the per-host step belong to
`split-mode-updates.md`; the wire and the release store to `pull-updates.md`; this file only decides what the page reads, how
it writes, and how it is tested.

## 2. Rules the page obeys (from `ui-data.md`)

1. **Pages read the JSON API only** (6.1). Each page is a `Page { name, reads }`; its `Reader` refuses a URL that matches no
   declared pattern, and the page lists every URL it read as a link. The Updates page declares `/v1/update`, `/v1/nodes` and
   `/v1/releases`, and `/v1/members` if the role column needs it (Q4). It parses into `kbf-ui`'s own `api.rs` shapes, never the
   server's structs.
2. **Escaping by construction.** All markup goes through `html::Html`, which escapes every string and has no raw write. A
   commit, release name, refusal text, hold reason and node id are strings from hosts and are rendered as text. Red test: a
   refusal of `<script>alert(1)</script>` renders escaped.
3. **CSP and no script.** Every `Answer` carries `Content-Security-Policy: default-src 'none'; style-src 'self'; img-src 'self';
   script-src 'self'; frame-ancestors 'none'` and `X-Content-Type-Options: nosniff`. The page has no JavaScript: the confirm
   step is a second page, not a dialog.
4. **Page route contract.** The page is added to the `PAGES` array, so the contract test checks that every URL it reads is a
   declared route, and rejects dot segments, backslashes and control characters.
5. **Built fields keep their names; new keys are additive.** Timestamps are RFC 3339 strings, durations and sizes carry units
   (`_ms`, `_bytes`), and absent means not measured, never zero. A host that never sent `UpdateStatus` is `unsupported`, not
   `idle` (`pull-updates.md` section 5).
6. **Instance scoping.** Update data names no build and no instance, so it is a fleet read like `/v1/nodes`. Whether
   `/v1/update` is open or under the planned `farm.get` authority is Q3. Recommendation: open, because the commits are public
   build metadata and the host list is already open on `/v1/nodes`.
7. **Add the routes to the route table** in `ui-data.md` 6.2 in the same PR as each route.

## 3. What the page shows

Per host (a table, one row per node, sorted by node id; the page size and token are the API's `page_size` and `page_token`):

| Column | Source | Today |
|---|---|---|
| node, connected | `/v1/nodes` `node_id`, `connected` | **[F]** built |
| platform | `/v1/nodes` `software.os_name`, `os_version`; capability `arch` | **[F]** built, arch by inference from Hello |
| role (storage-and-daemon, daemon-only, Mac) | none | gap: `NodeView` has no role; `/v1/members` is keyed by member id (Q4) |
| running commit | `UpdateStatus.running_commit` | wire exists, dropped by the server |
| desired commit | `/v1/update` desired | no state, no route |
| update step state | `UpdateStatus.state` (IDLE, FETCHING, STAGED, ACTIVATED_UNCONFIRMED, CONFIRMED, REVERTED, REFUSED, MANUAL) | wire exists, dropped |
| last error | `UpdateStatus.refusal` with state REFUSED or REVERTED | wire exists, dropped; no machine-readable reason (Q6) |

The controller's richer phase (selected, cordoned with N leases left, waiting for write sessions, restarting, rebuilding,
held with a reason) is the `update/host/<id>` row. **[F]** No controller exists (`split-mode-updates.md` U4), so the first page
shows only the wire state and says so in a note; it does not invent phases.

Farm (a header block above the table):

- available releases: `/v1/releases`, each commit with its platforms and whether each is complete;
- desired commit and generation: `update/desired` `{generation, commit, set_at}`, shown as "none" before any is set;
- policy: `update/policy` (`paused`, `auto_nodes`, `auto_frontend`, `max_hosts_out`, and the rest), read-only in the first page.

Per-host "up to date" is derived by the server and delivered as `update.up_to_date`, not computed in the page, so the page and
a script agree (`pull-updates.md` 6.4 plans this field).

## 4. Routes

Built (**[F]**): `GET /v1/nodes`, `GET /v1/releases` (503 without `--releases-dir`), `PUT /v1/releases/...`, the node
cordon, drain and uncordon posts, `GET`/`POST /v1/frontend/drain`, and on this branch `GET /v1/members`.

To build, additive to the specification in `pull-updates.md` 6.3:

- `GET /v1/nodes` gains an `update` object per node: `state`, `running_commit`, `installed_commit`, `staged_commit`,
  `offered_generation`, `refusal`, `helper_present`, `up_to_date`. Held in memory per node, newest wins, refilled at the next
  `UpdateStatus` after a restart, like `software` (the `fleet.rs` header). Absent for a host that never sent one.
- `GET /v1/update` returns `{desired: {generation, commit, set_at} | null, policy, releases, hosts_total, hosts_up_to_date}`.
  Reads `update/desired`, `update/policy` and the release index.
- `POST /v1/update/desired {commit, expect_generation}` sets the desired commit **through the frontend's desired-state call**
  (the compare-and-set on `update/desired` of `pull-updates.md` 6.3). 409 if no platform is complete for the commit, 409 if
  `expect_generation` is stale, no-op (no new generation) if the commit equals the current one. Operator gate, as the other
  writes.
- `POST /v1/update/plan {commit}` (new, read-only despite POST: it takes a body, writes nothing, is on the operator gate) returns
  the preview: the hosts that would change, per host its current and target commit, the order the rollout would use, the
  blockers that would stop it, and a `plan_digest` over that content and the generation it was computed against.
  `POST /v1/update/desired` accepts the `plan_digest` and refuses a stale one with 409, so what the operator confirmed is what
  runs. Until the controller exists (U4) the plan lists hosts and commit changes only and says "order: not planned".

All new routes are documented in `docs/api.md` and listed in `GET /v1` and `/llms.txt`.

## 5. The action, authorization and confirmation

**Confirmation is built into the page [P].** "Update to <release>" is a link to `GET /ui/updates/confirm?commit=<c>`, which
reads `POST /v1/update/plan` (through the server, not from the browser), shows the plan (hosts, from-to commits, blockers) and a
form with the commit, the generation, the plan digest and a confirm button. A blocker renders the button disabled with the
reason. No single click changes the farm.

**Authorization today.** **[F]** A write needs a loopback peer, no `Origin`, the bearer token and a JSON content type. A
browser cannot send that, and `ui-data.md` 8 states the UI is read-only and a cookie must never make a POST. Two ways forward:

- **(a) Read-only page plus a command [P, first step].** The confirm page shows the exact command (`kbf update set --commit <c>
  --plan <digest>`) for the operator to run where the token is. Nothing in the security model changes. This ships the plan
  preview and the confirmation and is enough to prove the read side.
- **(b) A UI write path [Q1].** Designed in `split-mode-updates.md` 6.3: a session made from the operator's credential, a CSRF
  token in every form, `SameSite=Strict`, a server `Origin` check, roles (read, update, admin) and an audit record naming the
  actor. This reverses `ui-data.md` 8 ("no cookie can make a POST") and needs the TLS read listener (Q18 there), because a
  `Secure` cookie is not sent over plain HTTP. If chosen, `ui-data.md` 8 is amended in the same PR, with the invariant restated:
  a POST is accepted only with a valid session, a matching CSRF token, a same-origin `Origin` and a role, and a request lacking
  any one is refused with the check named.

Whichever is chosen, the write is the same `POST /v1/update/desired`; the page never writes the state store itself, and an
update to the update-controller is never a UI-only code path.

## 6. What exists and what to build

| Piece | State |
|---|---|
| `kbf-ui` page framework: `Page`, `Reader`, `Html`, CSP, contract test | built, GET only |
| `GET /v1/nodes`, `/v1/releases`, `/v1/members` | built (`/v1/releases` mounting is the open `--releases-dir` change) |
| `UpdateStatus` on the wire | built, ignored by the server |
| `update` object in `NodeView` | to build (PR 1) |
| `update/desired`, `update/generation`, `update/policy` rows, `GET /v1/update`, `POST /v1/update/desired`, `POST /v1/update/plan` | to build (PRs 2, 3) |
| Updates page, read-only, plus the confirm page with command | to build (PR 4) |
| UI session, CSRF, `Origin` check, roles, audit, form POST | to build only if Q1 chooses (b) (PR 5) |
| Controller phases, blockers, held reasons | not started (`split-mode-updates.md` U4, U10) |

## 7. PR split and tests

Each PR is red first: the test is committed failing against the old code, then the change makes it pass. Each lists a mutant
that must turn it red.

1. **Server keeps `UpdateStatus`; `NodeView.update`.** Tests: a host sending a staged-and-refused status shows it in
   `/v1/nodes`; a host that never sent one has no `update` key (and the page later says `unsupported`); a second status replaces
   the first; a restart empties it. Mutants: keep the oldest instead of the newest; default to `idle` for a silent host.
2. **Desired-state rows and `GET /v1/update`.** Tests: empty farm answers `desired: null`; after a set, the row and counts
   agree with `/v1/nodes`. Mutant: count a disconnected host as up to date.
3. **`POST /v1/update/desired` and `/v1/update/plan`.** Tests: 409 for an incomplete platform; same commit gives the same
   generation; different commit increments it by exactly one; stale `expect_generation` and stale `plan_digest` are 409; no
   token, an `Origin` header or a non-loopback peer is refused as for other writes; the plan changes when a host's state
   changes. Mutants: take the generation on every call; ignore the digest.
4. **The page.** Tests: every state of `UpdateStatus.State` and "unsupported" renders; a refusal string and a node id with
   markup render escaped; CSP and `nosniff` are on the answer; the page is in `PAGES` and the contract test fails if it reads an
   undeclared URL; a blocker disables the button and shows its reason; the confirm page is GET only and changes nothing (a
   counting `Source` sees zero writes). Mutants: raw-write a refusal; read `/v1/update` from an undeclared pattern; enable the
   button with blockers.
5. **UI write path (only under Q1 (b)).** Tests: POST without a session, without CSRF, with a foreign `Origin`, with a read
   role, with a stale digest: each refused and none changes `update/desired`; each accepted write leaves an audit record naming
   the actor. Mutants: skip each check in turn (five mutants, five red tests).

PRs 1 to 4 are independent of the controller. PR 4 is mountable only when the crate is mounted on a server, which is not yet
the case (`kbf-ui` lib docs: "Not mounted on a server yet").

## 8. Open questions

- **Q1 UI writes.** Choose (a) command only, or (b) a session and CSRF write path. Recommendation: ship (a) with PR 4, decide
  (b) when the TLS read listener exists, since (b) cannot work over plain HTTP and contradicts the written rule until amended.
- **Q2 Which credential makes a UI session.** Not settled in `split-mode-updates.md` 6.3. Needed only for (b).
- **Q3 Open or `farm.get`.** Recommendation: open, like `/v1/nodes`.
- **Q4 Role.** Add `role` to `NodeView`, or have the page join `/v1/members` by node id. Recommendation: `NodeView.role`, so
  the page makes one read per concern and the join is not duplicated in every consumer. Needs a node-to-member mapping that the
  code does not give today.
- **Q5 Skip, Roll back, Accept failing builds, Policy.** Out of scope here; each needs its own confirm page and the admin role,
  and they depend on the controller.
- **Q6 Machine-readable refusal.** `refusal` is free text. Until a reason code exists the page shows the text only and groups
  nothing by it.
- **Q7 Branch state.** The update work and this branch diverged. This design assumes the merge of this branch into the update
  branch is complete before PR 1; if not, `GET /v1/members` may be missing there.

## 9. What blocks implementation

- The change that mounts `--releases-dir` and the release fetch service on the servers (open) blocks `/v1/releases` being
  served, so blocks the releases list and the plan's completeness check.
- The server reading `UpdateStatus` (PR 1 above) blocks every per-host column.
- The root helper (open) blocks hosts reporting `helper_present` and real step states in production, not the page itself.
- The controller (U4) and poller (U10) block per-host phases, held reasons and preflight blockers, and are not needed for the
  first page.
- The TLS read listener blocks Q1 (b).
