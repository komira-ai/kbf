# The update page: versions per host, desired version, update available (read-only)

Status: design. Nothing in this file is built. Facts marked **[F]** were read from the code on three lines of work:
**arch-v2** (`b74678f0`: `kbf-ui`, `kbf-auth`, `GET /v1/members`, `--api-listen`, `ui-data.md`, `split-mode-updates.md`),
**pull-updates** (`6561a838`: `kbf.update.v1`, the release store, the frozen specification `pull-updates.md` 6.3, which this
file calls "the spec") and the open change **#976** (`pu/upd5-12-desired-offers`, head `6935f7e6`, the desired release and the
update routes). **[D]** is decided in another document, **[P]** is proposed here, **[Q]** is open (section 8).

**Scope ruling.** The page is **read-only**. It has no update button and there is no browser sign-in in this design. An update
is made through the operator API or the CLI. A browser write path (an update button with an operator sign-in) was drafted in an
earlier revision of this file, is not designed further, and is tracked as a future, unscheduled issue, kbf#992 (section 9).

**Where the amendments live.** This file does not change `ui-data.md`: the UI stays read-only and ui-data Q16 (how a human
authenticates) stays as written there. It amends `pull-updates.md` 6.3 only by naming each change in section 5; the text edits
to that file are a separate pull request on the pull-updates line (section 7), because arch-v2 has no `pull-updates.md` to edit.
No route in section 5 beyond what #976 builds is assumed: each is an explicit amendment with its reason.

## 1. Summary

An operator sees, on one page, which commit each host runs, which release the farm wants, whether a newer release is
available, and whether each host's update step is done or stuck. The page shows the exact operator-API or CLI command that
would apply an update, with no token in it; the operator runs it. The page never writes.

The page reads the JSON API by GET only, like every other page (`ui-data.md`), and an agent calling the same routes sees
everything the page shows.

## 2. Facts that bound the design

- **[F]** The frontend receives each host's `UpdateStatus` and discards it (`worker.rs`, the `Offer(_) | UpdateStatus(_)` arm).
  No per-host update state is served.
- **[F]** #976 serves `GET /v1/update` **open** as `{desired, releases}` (`desired` is `{generation, commit, set_at_ms,
  paused}`), `POST /v1/update/desired` with `Offer` = `deny_unknown_fields {commit}`, and `POST /v1/update/pause` and
  `/resume` (`release/desired.rs`). **Today** (R4, `frontend-proxy.md` section 9) the offer is `deny_unknown_fields {commit,
  expect_generation?, rollback?}` and the desired view is `{generation, commit, set_at_ms, rollback}`; the text of this bullet
  below is #976's head, kept for the reasoning that follows it. Writes pass `operator_gate`. Its owner says it is gaining an optional `expect_generation`
  (409 on mismatch, no write) and a `policy` object in `GET /v1/update`; **this file builds on both and says so**:
  `GET /v1/update` = `{desired, releases, policy?}`, `Offer` = `{commit, expect_generation?}`. **Where pause lives.** At #976's head `policy_view` has no `paused` field: pause is `desired.paused`, and `policy` is null until a
  policy row exists. The page's rule: **use `policy.paused` only if that key is present, else `desired.paused`**, so a policy row
  without the key never reads as "running". A wrong "running" is a safety error (the operator runs an update into a paused
  farm or never resumes). #976 is asked to say where pause lives (Q11); this file changes its reading only after it does.
  The page also shows `policy.auto_nodes` and `policy.auto_frontend` when present.
- **[F]** A platform is complete when it has its own components: linux needs `kbf`, darwin needs `kbf`
  (`ReleasePlatform::components` in #976). The page shows completeness per platform with that component named.
- **[F]** `operator_gate` (`api.rs`): loopback peer, no `Origin` header, `Authorization: Bearer` compared in constant time
  against a digest, `Content-Type: application/json`. The token is read once from `--api-token-file` and only its SHA-256 is
  kept (`token.rs`). This is the door an update goes through, unchanged; the page does not touch it.
- **[F]** `kbf-ui` (merged): `html::Html` escapes every string with no raw write; every `Answer` carries `Content-Security-Policy:
  default-src 'none'; style-src 'self'; img-src 'self'; script-src 'self'; frame-ancestors 'none'` and `nosniff`; `PAGES`
  declares each page's reads and a contract test rejects an undeclared or malformed URL. It is "not mounted on a server yet".
- **[F]** No `kbf update` CLI verb exists on any line of work.

## 3. Rules the page obeys

1. **Pages read the JSON API only, by GET.** Each page is a `Page { name, reads }`; the `Reader` refuses an undeclared URL and
   the contract cannot declare a POST. **A page, a GET, never causes a write**, and this page has no form at all.
2. **Escaping by construction.** Commits, release names, refusal text, hold reasons and node ids are strings from outside and
   render through `Html`. The command the page shows (section 4) is assembled only from fields that were validated first (a
   40-hex commit, an integer generation) and is escaped like any text.
3. **CSP unchanged.** The page uses the `kbf-ui` policy as merged; this design adds no `form-action`, no cookie and no header.
4. **Built keys keep their names; new keys are additive.** Timestamps are RFC 3339 strings except where #976 already serves
   `set_at_ms` (the page formats it), sizes and durations carry units, absent means not measured. A host that never sent
   `UpdateStatus` is `availability: unsupported`, never `idle`.
5. **Instance scoping does not apply** to update data (it names no build). It is still not public: `refusal` text, staged and
   installed commits and `helper_present` are an attack map of the update path. They are served under `farm.get` (Q3), and the
   page shows those columns only where its read of the detail route is authorised (section 4). The open summary stays open.
6. A plan's "failed builds during the run" column of the controller (U4) names instances and is not shown without the
   `invocations.get` check.

## 4. What the page shows

Two layers, decided by what the page's read of each route is allowed to return, not by a login: the **open summary**
(`GET /v1/update`, `GET /v1/nodes`: desired, releases, policy, per-host `state`, `up_to_date`, `availability`) is always shown;
the **detail columns** (commits, `refusal`) are shown only if the detail route (`GET /v1/update/hosts`, `farm.get`) answers the
page's read, otherwise the columns are absent and a line says "detail needs read access". How a human's read is authorised is
ui-data Q16 as written there (a reverse proxy that injects the read header, or the open summary); this page adds no
credential. The page invents no state when a route is refused.

Farm header: the desired commit, generation and set time from `desired`; **paused** by the rule of section 2 (`policy.paused`
only if that key is present, else `desired.paused`); `policy.auto_nodes` and `policy.auto_frontend` as read-only values; the
releases list with each commit's platforms, each marked complete or not **with the component it needs (`kbf` on linux and darwin)**; **update available** when the newest complete release differs from `desired.commit`; "N of M hosts up to date"
summed by the page from the per-host `up_to_date` it already reads.

Host table (one row per node, sorted by node id, paginated by `page_size` and `page_token`): node, connected, platform, running
commit, desired commit, the host's own step state (`UpdateStatus.state`), `availability`, last error (`refusal`, escaped,
capped). The controller's richer phase is a second column once the controller exists (U4); until then a note says the page
shows wire state only and invents no phase.

**The frontend is a separate row above the table**, not one of the hosts: it updates only through the frontend switch behind
Envoy (`split-mode-updates.md` 6.3), never by the node roll, so a plan lists hosts only and says so beside the frontend row.
Its commit is the serving process's own build commit (Q7).

**Plan view (read-only, optional last page).** `GET /ui/updates/plan?commit=<c>` shows what applying release `<c>` would do,
read from `GET /v1/update/plan` (A4): the hosts that would move (from, to), the blockers (`no_complete_platform`,
`host_unsupported`, `helper_missing`, `paused`) and the farm's current generation. It stores nothing, has no form and no
button, and ends with the command to run on a server host:

```text
curl -X POST http://127.0.0.1:<api-port>/v1/update/desired \
  -H "Authorization: Bearer $(cat <api-token-file>)" -H "Content-Type: application/json" \
  -d '{"commit":"<40 hex>","expect_generation":<n>}'
```

The placeholders `<api-port>` and `<api-token-file>` are printed literally; the page knows no token and no path, and shows
none. `expect_generation` is the generation the page read, so a second operator's change makes the call 409 instead of
overwriting it. **Downgrades are allowed, and a downgrade is a rollback** (`frontend-proxy.md` section 9, R4): a commit other
than the serving process's own build commit (the frontend row's) is refused with 409 unless the body also has
`"rollback":true`, and the row records it (`desired.rollback`). The page never writes, so it posts nothing; it says this
beside the command, pointing at the frontend row's commit, and shows a desired row that is a rollback as one. The `rollback`
field says the commit is not the frontend's own, not that it is older (commits have no order): a forward upgrade to another
commit is recorded as a rollback too. A controller held back by this rule (an old child after a frontend rollback, with
a non-rollback row for a release that is not its own) logs one line and is on no route, so the page cannot show it; a host
the new child had already cordoned stays cordoned until the desired row is one that controller may drive: its own commit
offered (the host is given back) or a row that is a rollback. When the `kbf update set` verb exists (PR 6, optional) the page shows that instead. A blocker that makes the
server refuse (no complete platform) is shown as a reason above the command, and the command is omitted.

Every URL the page read is listed below it, as links.

## 5. Routes, and exactly what each amends

Built or in #976: `GET /v1/nodes`, `GET /v1/releases`, `PUT /v1/releases/...`, `GET /v1/update` (open), `POST
/v1/update/desired {commit}`, `POST /v1/update/pause`, `POST /v1/update/resume`.

Amendments, each landing in the pull request that builds it and in `pull-updates.md` 6.3 through the separate text pull
request (section 7). **Nothing here is assumed to exist.** The ids A1 to A7 keep their numbers from the earlier revision;
A3 and A7 are dropped (below).

| Id | Change | Reason | Needed by |
|---|---|---|---|
| A1 | `GET /v1/update` is `{desired, releases, policy?}`, open. The spec's `auto_update` is dropped; the flag is `policy.auto_nodes` (and `auto_frontend`) as `pull-updates.md` section 6.2 already says | #976 serves it; the spec text lags the policy row | page |
| A2 | `Offer` = `{commit, expect_generation?}`; 409 `stale_generation` on mismatch, no write | owner of #976; protects against two operators; the command the page shows carries it | CLI and API |
| A4 | `GET /v1/update/plan?commit=<c>`: the preview of hosts and blockers, a GET on the read authority, computed per call, nothing stored, `Cache-Control: no-store`. No digest and no issue time: nothing applies it | the plan view needs a read | page (optional) |
| A5 | `GET /v1/update/hosts` (paginated, `farm.get`): the full per-node `update` object. `GET /v1/nodes` carries only `update: {state, up_to_date, availability}`; commits, `refusal`, `helper_present`, `offered_generation` move to the new route | `/v1/nodes` is open; free text and commits must not be. `refusal` is capped at 512 bytes on a character boundary, control characters replaced | page |
| A6 | `pause` and `resume` documented in the spec | built in #976, absent from the spec | spec accuracy |

**Dropped with the write path.** A3 (`plan_digest` and `plan_issued_at` on `Offer`, `stale_plan`) existed so a browser form
could hold an apply to the host list its operator confirmed; with no form, the digest has no reader, and a CLI or API caller
already holds the token and the spec's call. A7 (an audit row for every `desired`, `pause` and `resume`) was for the session
door; the read-only page needs no audit and the CLI path is unchanged. Both move to the future issue (kbf#992, section 9). The core
refactor into `offer_desired` and `set_paused` behind two doors is likewise dropped: one door exists and stays as #976 has it.

**Blockers (A4).** Stable codes: `no_complete_platform` (per platform), `host_unsupported`, `helper_missing`, `paused`. An
offline host is a row, not a blocker. The plan GET is advisory: the server's own checks at apply time (a complete platform,
`expect_generation`) are the only ones that count.

## 6. Failure modes

State store down: reads fail as today and the page shows the routes that failed, not a stale value. A refused detail route: the
detail columns are absent (section 4), never filled with a guess. A host that never sent `UpdateStatus` or went silent:
`unsupported` or `unavailable`, never idle. Frontend restart: nothing is lost; the page keeps no state. Handover mid-run: the
page shows the wire state of each host as it reconnects. A pause not readable (policy row without the key and no `desired`):
the page shows "pause state unknown", never "running".

## 7. What exists, what to build, PR split and tests

| Piece | State |
|---|---|
| `kbf-ui` pages, `Reader`, `Html`, CSP, contract test | built, GET only, not mounted |
| `GET /v1/update`, `POST desired`, `pause`, `resume` | #976, open |
| `expect_generation`, `policy` in `GET /v1/update` | being added to #976 |
| `farm.get` | not built |
| `UpdateStatus` kept by the server, `GET /v1/update/hosts`, `GET /v1/update/plan` | to build (A4, A5) |
| Updates page, plan view | to build |
| Controller phases, blockers, held reasons | not started (U4, U10) |

Each PR is red first (the test is committed failing against the old code, then the change turns it green) and lists the
mutant that must turn it red again. The text pull request carries A1, A2, A4, A5 and A6 into `pull-updates.md` 6.3. All
PRs below are read-only: none adds a POST, a form, a cookie or a credential.

1. **Server keeps `UpdateStatus`; `NodeView.update`; `GET /v1/update/hosts`** (A5). Tests: staged-and-refused status is on
   hosts and only `{state, up_to_date, availability}` on `/v1/nodes`; a silent host is `unsupported` or `unavailable`, never
   idle; newest wins; a stale stream is dropped; a 100 KiB `refusal` stored as 512 bytes cut on a character boundary.
   Mutants: keep oldest; default silent to idle; accept the old stream; store uncut; `refusal` on `/v1/nodes`.
2. **`GET /v1/update/plan`** (A4; stacks on #976). Tests: the plan lists the hosts that would move and the stable blockers; a
   changed host list or blocker changes the plan; it writes nothing (counting store); it is a GET only. Mutants: store the
   plan; include `connected` or the step state in the blockers.
3. **The Updates page** (read-only). Tests: every `availability` and every `UpdateStatus` state renders; a refusal and node id
   with markup render escaped; completeness names `kbf` for linux and darwin; the frontend row is separate
   and the plan lists no frontend; **a policy row present without a `paused` key and `desired.paused` true renders paused**,
   and `policy.paused` wins when present; **update available** shows only when the newest complete release differs from
   desired; a refused detail route removes the detail columns and keeps the summary; the page declares no POST and contains no
   `<form>` (counting source). Mutants: raw-write a refusal; show detail when the route refused; read only `policy.paused` (so a
   policy row without the key shows running); declare a POST read.
4. **The plan view.** Tests: it issues only GETs; the command contains the page's generation and a validated commit, and no
   token (a planted token in the page's environment never appears in the body); a commit that is not 40 hex is a 400 and no
   command; a blocker `no_complete_platform` omits the command. Mutants: build the command from the raw query string; read a
   token file; show the command despite a blocker.
5. **Pull-updates text pull request**: edits `pull-updates.md` 6.3 for A1, A2, A4, A5 and A6 (docs only), merged before PR 1.
6. **Optional: `kbf update set|plan`** reading the token from `--api-token-file`; not required by the page.

PRs 1 to 4 are independent of the controller. PR 3 onward needs the `kbf-ui` mount (not done).

## 8. Open questions

- **Q3 Authority of the update reads.** Recommendation: `farm.get` for `/v1/update/hosts` and fields beyond the summary;
  `GET /v1/update` stays open as #976 serves it (it shows the desired commit and release list, which `GET /v1/releases` already
  shows). Until `farm.get` exists the detail routes need a read gate that does not require loopback; until then the page shows
  the open summary only (section 4).
- **Q4 Role column.** `NodeView.role` or a join on `/v1/members`; recommendation: `NodeView.role`.
- **Q7 The frontend's commit** for its separate row: build info of the serving process, a new field. Recommendation: yes.
- **Q8 Machine-readable refusal.** Free text only until a reason code exists.
- **Q9 Branch state.** `GET /v1/members` and `kbf-ui` are on arch-v2 and the update wire on pull-updates; PRs 1 and 2 need both,
  so they stack after arch-v2 is merged into pull-updates.
- **Q10 `/v1/releases` is open under the spec.** Left open, flagged.
- **Q11 Where pause lives in #976.** `policy_view` has no `paused` today; ask its owner to state it. Until then the section 2 rule.
- **Moved to the future issue (kbf#992, section 9), not open here:** whether the UI writes; the credential for a browser session; a
  trusted-proxy principal (Q5); carrying the resume generation into the API (Q6); a password-hash dependency (Q12); an Envoy
  `/ui` route with sticky routing (Q13); a hash-chained audit (Q14); a farm-wide apply lock (Q15); the shared token as login
  (Q17). The `ui-data.md` Q16 is unchanged.

## 9. The future issue and what blocks implementation

The update button and an operator sign-in are filed as a future issue, not scheduled: "kbf UI: an update button and operator
sign-in". The earlier full draft of this file (a per-operator login credential, a session door, CSRF, a TLS-only cookie, a
principal policy kind, a durable revoke-all, audit) is in the git history of this file at `15236b53`. It is not part of this
design.

Blocking the page: #976 with `expect_generation` and `policy` (and Q11's answer) blocks PR 2's stacking; the `kbf-ui` mount
blocks PR 3 onward; `farm.get` blocks serving details beyond the open summary (Q3). The controller (U4, U10) blocks phases,
held reasons and preflight blockers, none of which the first page needs. The root helper blocks real `helper_present` values
in production, not the page.
