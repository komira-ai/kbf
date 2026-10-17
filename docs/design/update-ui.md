# The update page: versions per host, update available, the update button

Status: design. Nothing in this file is built. Facts marked **[F]** were read from the code on three lines of work:
**arch-v2** (`b74678f0`: `kbf-ui`, `kbf-auth`, `GET /v1/members`, `--api-listen`, `ui-data.md`, `split-mode-updates.md`),
**pull-updates** (`6561a838`: `kbf.update.v1`, the release store, the frozen specification `pull-updates.md` 6.3, which this
file calls "the spec") and the open change **#976** (`pu/upd5-12-desired-offers`, head `f73b8c37`, the desired release and the
update routes). **[D]** is decided in another document, **[P]** is proposed here, **[Q]** is open (section 10).

**Where the amendments live.** This file amends `ui-data.md` (sections 8 and 10, Q16) and the `kbf-ui` page contract, all on
arch-v2, so this pull request stays on arch-v2 and edits `ui-data.md` itself. It amends `pull-updates.md` 6.3 only by naming
each change in section 5; the text edits to that file are a separate pull request on the pull-updates line (named in
section 9), because arch-v2 has no `pull-updates.md` to edit. No route in section 5 beyond what #976 builds is assumed: each
is an explicit amendment with its reason.

## 1. Summary

An operator sees, on one page, which commit each host runs, which release the farm wants, whether each host's update step is
done or stuck, and presses **Update to <release>**, after a read-only confirm page that shows the plan. The button writes
through the same core call the CLI and `curl` use (`POST /v1/update/desired`), with `expect_generation`, `plan_digest` and
`plan_issued_at`; the server recomputes the digest, checks the generation and the 10-minute expiry. Pause and Resume are two
more actions on the same path.

The decision this file makes, and closes **ui-data Q16** with (section 7): the browser gets **a login form that exchanges the
operator API token for a server-side session, over TLS only; the session authorises a fixed set of three POST routes under
`/ui/`; the JSON API stays cookie-blind and its gates are unchanged.** The UI process never holds the token (the server keeps
only its SHA-256, `token.rs`), so there is no token to forward and nothing to be a deputy for.

## 2. Facts that bound the design

- **[F]** The frontend receives each host's `UpdateStatus` and discards it (`worker.rs`, the `Offer(_) | UpdateStatus(_)` arm).
  No per-host update state is served.
- **[F]** #976 serves `GET /v1/update` **open** as `{desired, releases}` (`desired` is `{generation, commit, set_at_ms,
  paused}`), `POST /v1/update/desired` with `Offer` = `deny_unknown_fields {commit}`, and `POST /v1/update/pause` and
  `/resume` (`release/desired.rs`). Writes pass `operator_gate`. Its owner says it is gaining an optional `expect_generation`
  (409 on mismatch, no write) and a `policy` object in `GET /v1/update`; **this file builds on both and says so**:
  `GET /v1/update` = `{desired, releases, policy?}`, `Offer` = `{commit, expect_generation?}`. Pause lives on the desired row
  until policy rows land; the page reads `policy.paused` when `policy` is present and `desired.paused` otherwise.
- **[F]** A platform is complete when it has its own components: linux needs `kbf`, darwin needs `kbf-daemon`
  (`ReleasePlatform::components` in #976). The page shows completeness per platform with that component named.
- **[F]** `operator_gate` (`api.rs`): loopback peer, no `Origin` header, `Authorization: Bearer` compared in constant time
  against a digest, `Content-Type: application/json`. The token is read once from `--api-token-file` and **only its SHA-256 is
  kept** (`ApiToken`, `token.rs`).
- **[F]** `kbf-auth` has `Authorizer::authorize(metadata, instance_names)` and a `Policy` with `invocations_get` and
  `blobs_get`; a missing one turns its routes off. No authenticator reads a credential, no roles exist, and `farm.get` is not
  built (`ui-data.md` Q11).
- **[F]** `kbf-ui` (merged): `html::Html` escapes every string with no raw write; every `Answer` carries `Content-Security-Policy:
  default-src 'none'; style-src 'self'; img-src 'self'; script-src 'self'; frame-ancestors 'none'` and `nosniff`; `PAGES`
  declares each page's reads and a contract test rejects an undeclared or malformed URL. It is "not mounted on a server yet".
- **[F]** The TLS read listener (`--read-listen`, `--read-tls-cert`, `--read-tls-key`, `--read-listen-insecure`) is decided
  (`ui-data.md` Q18) and **not built**: `git grep` finds those flags only in design text. Today every route is on the one
  plain-HTTP `--api-listen`.
- **[F]** No `kbf update` CLI verb exists on any line of work.

## 3. Rules the page obeys

1. **Pages read the JSON API only, by GET.** Each page is a `Page { name, reads }`; the `Reader` refuses an undeclared URL and
   the contract cannot declare a POST. **A page, a GET, never causes a write.** Writes are not reads of any page: they are
   `Action`s (section 6.3), a second declared list with its own contract test.
2. **Escaping by construction.** Commits, release names, refusal text, hold reasons, node ids and the operator's claimed name
   are strings from outside and render through `Html`. Forms are built by a `Form` type that emits `method="post"`, an
   `action` that is a fixed `/ui/...` path, and hidden fields escaped like any text; there is no way to write a raw `<form>`.
3. **CSP, no script.** Section 6.5 states the one change (adds `form-action 'self'` and `base-uri 'none'`).
4. **Built keys keep their names; new keys are additive.** Timestamps are RFC 3339 strings except where #976 already serves
   `set_at_ms` (the page formats it), sizes and durations carry units, absent means not measured. A host that never sent
   `UpdateStatus` is `availability: unsupported`, never `idle`.
5. **Instance scoping does not apply** to update data (it names no build). It is still not public: `refusal` text, staged and
   installed commits and `helper_present` are an attack map of the update path. They are served under `farm.get` (Q3), and the
   page shows them only to a viewer it has authenticated (section 4). The open summary stays open.
6. A plan's "failed builds during the run" column of the controller (U4) names instances and is not shown without the
   `invocations.get` check.

## 4. What the page shows

Viewers: **anonymous** (no session) sees the open parts only; a **session** (any role) also sees the detail table, the audit
list and the buttons its role allows. Reads of gated routes are made by the UI in-process on behalf of an authenticated viewer
only, never for an anonymous one.

Farm header: the desired commit, generation and set time from `desired`; **paused** from `policy.paused` (else
`desired.paused`) with a **Pause** or **Resume** form for a session of the `update` role; the policy fields read-only; the
releases list with each commit's platforms, each marked complete or not **with the component it needs (linux: `kbf`, darwin:
`kbf-daemon`)**; "N of M hosts up to date" summed by the page from the per-host `up_to_date` it already reads.

Host table (one row per node, sorted by node id, paginated by `page_size` and `page_token`): node, connected, platform, running
commit, desired commit, the host's own step state (`UpdateStatus.state`), `availability`, last error (`refusal`, escaped,
capped). The controller's richer phase is a second column once the controller exists (U4); until then a note says the page
shows wire state only and invents no phase.

**The frontend is a separate row above the table**, not one of the hosts: it updates only through the frontend switch behind
Envoy (`split-mode-updates.md` 6.3), never by the node roll, so the Update button's plan lists hosts only and says so beside
the frontend row. Its commit is the serving process's own build commit (Q7).

Below: the **audit list** (last 50 update actions, section 6.8) and every URL the page read, as links.

## 5. Routes, and exactly what each amends

Built or in #976: `GET /v1/nodes`, `GET /v1/releases`, `PUT /v1/releases/...`, `GET /v1/update` (open), `POST
/v1/update/desired {commit}`, `POST /v1/update/pause`, `POST /v1/update/resume`.

Amendments, each landing in the pull request that builds it and in `pull-updates.md` 6.3 through the separate text pull
request (section 9). **Nothing here is assumed to exist.**

| Id | Change | Reason |
|---|---|---|
| A1 | `GET /v1/update` is `{desired, releases, policy?}`, open. The spec's `auto_update` is dropped; the flag is `policy.auto_nodes` (and `auto_frontend`) as `pull-updates.md` section 6.2 already says | #976 serves it; the spec text lags the policy row |
| A2 | `Offer` = `{commit, expect_generation?}`; 409 `stale_generation` on mismatch, no write | owner of #976; protects against two operators |
| A3 | `Offer` gains `plan_digest` and `plan_issued_at`, **both or neither**; with them the server recomputes the plan and refuses `stale_plan`. Without them the call is the spec's, so scripts keep working | `expect_generation` catches another change of desired; it does not catch a host joining, a platform becoming incomplete or a pause since the operator looked. The operator confirms a host list, and the digest is how the server holds the apply to it. It is also why the form cannot be a bare "update to latest" |
| A4 | `GET /v1/update/plan?commit=<c>`: the preview, a GET on the read authority, computed per call, nothing stored, `Cache-Control: no-store` | confirm needs a read the page can make |
| A5 | `GET /v1/update/hosts` (paginated, `farm.get`): the full per-node `update` object. `GET /v1/nodes` carries only `update: {state, up_to_date, availability}`; commits, `refusal`, `helper_present`, `offered_generation` move to the new route | `/v1/nodes` is open; free text and commits must not be. `refusal` is capped at 512 bytes on a character boundary, control characters replaced |
| A6 | `pause` and `resume` documented in the spec | built in #976, absent from the spec |
| A7 | Every applied or refused `desired`, `pause` or `resume` call writes an audit row (section 6.8), whichever door it came through | "every update action" includes the CLI's |

There is **no `--allow-unplanned-update` flag** and no new CLI verb in this design (the previous revision proposed both): the
planned form is what the UI always sends; a script that omits the digest is already an operator with the token, as the spec
says. A `kbf update` verb is optional later work (PR 8) and not required by the page.

**`plan_digest`.** SHA-256 over `"kbf.update.plan.v1\0"` followed by canonical JSON (UTF-8, object keys sorted bytewise, no
whitespace, integers decimal, no floats) of

```text
{ "commit": <40 hex>, "generation": <current desired generation, or 0>, "issued_at": <RFC 3339, seconds, UTC>,
  "hosts": [ {"node_id": s, "from": <installed commit or null>, "to": <commit>} ... sorted by node_id ],
  "blockers": [ {"code": s, "node_id": s or null} ... sorted by (code, node_id) ] }
```

Stable blocker codes: `no_complete_platform` (per platform), `host_unsupported`, `helper_missing`, `paused`. Excluded because they
change on a reconnect or a status tick: `connected`, step state, `refusal`, `offered_generation`, queue and lease counts. An
offline host is a row, not a blocker. **At apply time** the server recomputes with the request's `plan_issued_at` and refuses
`stale_plan` unless the digest equals, `expect_generation` equals the current generation, and the plan is not older than 10
minutes nor more than 5 seconds in the future by the server's clock. The 409 body names which of the four reasons
(`digest`, `generation`, `expired`, `future`) so the page can say why.

## 6. The write path

### 6.1 Two doors, one core

Today's gate protects the bearer token: the module doc of `api.rs` gives why writes are loopback-only, Origin-free and
JSON-only. This design does not loosen it. The three handlers (`set_desired`, `pause`, `resume`) become thin wrappers over
**core functions** (`offer_desired`, `set_paused`) that take parsed arguments and an `Actor` and do the checks (commit
parse, completeness, generation, digest, expiry), the audit and the row write. There are two doors in front of the core:

| Door | Reached by | Admission | Actor |
|---|---|---|---|
| bearer (unchanged) | `curl`, CLI, scripts, on the host | `operator_gate`: loopback, no `Origin`, bearer, JSON | `bearer` |
| session (new) | a browser form POST to `/ui/updates/...` on the TLS read listener | section 6.4: TLS listener, Origin, session, role, CSRF, rate limit | the session's principal |

The session door is **not** a client of the bearer door. The UI does not call `/v1/update/desired` over loopback with a token:
it cannot (only the digest is kept), and reading the token file into the UI process would make it a token holder whose every
bug is a write to the farm. The session door calls the core in-process, with a fixed call set. This is the answer to the
loopback invariant of `api.rs`: that invariant stays true of the bearer door; the session door is a second, narrower
admission with its own invariant, tested in section 9. The `api.rs` module doc is amended in PR 3 to say two doors exist.

**Why this is not a token-holding confused deputy.** (1) The UI holds no token and no way to make one. (2) The set of things a
session can cause is three core calls chosen by the route, not by any request field (no URL, method or path comes from the
client). (3) Each call is bound to the session's own CSRF value and to the plan (6.4), so a page the attacker controls cannot
drive it. (4) The session is made by someone who proved they hold the operator token and is checked against the authorizer on
every write (6.7). (5) Every write is audited with the session (6.8). (6) The cookie is accepted on exactly these routes
and is ignored by every `/v1` route, so a cookie makes no JSON API call, GET or POST.

### 6.2 Login: what authenticates an operator in a browser

Options, and the choice:

| Option | Keeps token out of the browser | Built today | Identity of the actor |
|---|---|---|---|
| **(1) Authenticating proxy principal** (the reverse proxy of ui-data Q16 v1 supplies a verified identity) | yes | no: needs a trusted-proxy flag and an `Authenticator` that reads it | real, per person |
| **(2) Login form: the operator token for a server-side session [P, v1]** | no, the operator types it once, over TLS, to the server | yes: `ApiToken::admits` | the session plus an unverified name the operator types |
| (3) UI forwards the bearer token over loopback | n/a | no: only the digest is kept; re-reading the file makes the UI a token holder | the token |

Chosen: **(2) for v1**, with **(1) as a second authenticator on the same session interface** once a trusted proxy is
configured (Q5). Reasons: (2) needs only the credential that exists and works with no proxy; (1) is the better end state
because it gives each person an identity and keeps the token out of every browser, but it is not buildable today and a
design that waits for it ships no button. (3) is refused above. Because the token is shared, the audit's "who" under (2) is a
session and a typed name, said plainly in the page ("name as typed, not verified"); real identity needs (1).

`GET /ui/login` shows a form (token as `type=password`, `autocomplete=off`; a name field up to 64 characters). `POST
/ui/login` is the only place the token appears: the body is capped at 1 KiB, the token is compared by `ApiToken::admits`
(constant time on digests), then dropped; it is never logged, never put in a URL (the form is POST only; a GET carrying
`token=` is ignored and not logged), never echoed, and never in a later page, so the page has nothing to leak and no script
could read it (the CSP allows none). The login passes the same TLS, Origin and Sec-Fetch-Site checks as an action.

### 6.3 The session

- **Id**: 256 random bits from the OS generator. **Cookie**: `__Host-kbf-session=<id>; Secure; HttpOnly; SameSite=Strict;
  Path=/` and no `Domain` (the `__Host-` prefix makes a browser refuse any cookie without `Secure` and `Path=/`, or with a
  `Domain`). `Path=/` means the browser also sends it to `/v1`; `/v1` ignores it.
- **Server side**: an in-memory table keyed by the SHA-256 of the id, holding principal, claimed name, role, peer address
  at login, created and last-used times, a random per-session CSRF secret, and the last four plans the session was shown
  (for the stale-plan diff, 6.10). At most 64 live sessions; the oldest idle is evicted first.
- **Lifetime**: idle timeout 15 minutes, absolute 1 hour, whichever is first. Both are enforced on the server's clock at every
  use; the cookie's own `Max-Age` is advisory. A browser that keeps the cookie past either is refused.
- **Revocation**: `POST /ui/logout` (CSRF-checked) deletes the entry and clears the cookie. `POST /v1/ui/sessions/revoke-all`
  on the bearer door deletes every session (the operator's panic button, from the host). A server restart empties the table:
  sessions are not persisted, because a persisted session would be a second credential store. A frontend handover also ends
  sessions; the operator logs in again. The audit and the desired row are in the state store and survive.
- A session is **not bound to an address** (operators move networks); it is bound to the listener it was made on.

### 6.4 Admission of a session POST, in order

Routes: `POST /ui/login`, `POST /ui/logout`, `POST /ui/updates/apply`, `POST /ui/updates/pause`, `POST /ui/updates/resume`.
Everything else under `/ui` is GET. A request failing a step stops there with the step's status and writes no row (an
unauthenticated request must not write to the store):

1. **TLS listener.** The write path is on only when the listener it arrived on terminates TLS (a startup fact, not a header
   such as `X-Forwarded-Proto`). On a listener started with `--read-listen-insecure` the forms are not rendered, the routes
   answer 404 `write path off: this listener has no TLS`, and the page says so. A plain-HTTP request to a TLS listener gets
   no page (ui-data Q18).
2. **Origin and fetch metadata.** `Origin` must be present and equal the configured `--ui-origin` (a flag naming
   `https://host[:port]`, never derived from `Host`). If `Sec-Fetch-Site` is present it must be `same-origin`. Absent `Origin`
   is refused (a browser form POST always sends it; the page sets `Referrer-Policy: same-origin`, not `no-referrer`, because a
   `no-referrer` page makes the browser send `Origin: null`).
3. **Shape.** `Content-Type: application/x-www-form-urlencoded` exactly, body at most 4 KiB, each field once, no unknown field,
   hex fields validated by length and alphabet before any use.
4. **Session**: cookie present, entry found, within idle and absolute lifetime (else 303 to `/ui/login`).
5. **Role**: the session's principal passes the authorizer for `update` (6.7), else 403.
6. **CSRF**: the hidden `csrf` field equals `HMAC-SHA256(session_secret, "kbf.ui.csrf.v1\0" action "\0" bound)`, compared
   in constant time. `bound` for `apply` is `commit "\0" expect_generation "\0" plan_digest "\0" plan_issued_at`; for
   `pause`, the current generation; for `resume`, the generation shown; for `logout`, empty. The token is therefore bound to the
   session (secret), to the action and to the plan it confirms: a token from one confirm page does not work on another plan,
   another action or another session, and editing a hidden field breaks it.
7. **Rate limit** (6.9).
8. **Audit and core call** (6.8, 6.1).
9. Answer **303 See Other** to `GET /ui/updates?audit=<row id>`; the page shows the result from the audit row, never from
   text in the URL (a link cannot put words on the page). A refresh does not repost.

### 6.5 The CSP and the headers

The current CSP has no `form-action`, and `form-action` does not fall back to `default-src`: with `default-src 'none'` a form
can still submit to any origin. The change, applied to **every** `Answer` (so no page's policy varies):

```text
Content-Security-Policy: default-src 'none'; style-src 'self'; img-src 'self'; script-src 'self'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'
```

Added: `form-action 'self'` (a form posts only to this origin) and `base-uri 'none'` (an injected `<base>` cannot redirect the
form's relative action). No script is added; `script-src 'self'` stays as it is today and serves no file. Also on every page:
`Referrer-Policy: same-origin`, and on every session page, login and action answer: `Cache-Control: no-store` (the token
and the CSRF value must not be in a shared cache or the back-forward list). `frame-ancestors 'none'` already blocks a page
that frames the form.

### 6.6 TLS

The write path needs the TLS read listener: `Secure` cookies are never sent to plain HTTP, and a token typed into a form must
not cross a network in clear. That listener (U1 of `ui-data.md`) is built first (PR 1 of section 9). Behind a load balancer
that terminates TLS, the balancer-to-frontend hop is the TLS hop (`ui-data.md` section 8), and the listener the UI is mounted
on terminates it. `--ui-origin` is required to enable forms; without it the page is read-only and says why.

### 6.7 Roles

A session carries a role, checked on every write by the existing `kbf-auth` machinery rather than a new check: a new policy
key `update.setAuthorizer` (an `Authorizer`, like `invocations_get`) is asked with the session's `AuthenticationMetadata` and
the single fixed name `farm/update` (the authorizer API is per instance name; the farm-wide update has none, so instance
scoping does not apply and this name is a constant, not a request field). **A missing key turns the write path off**, as
`invocations.getAuthorizer` does for its routes, so a farm with no policy file has no UI write. Roles in v1 are `update`
(Update, Pause, Resume) and none: a session whose principal the authorizer refuses can log in and see the page and cannot
write. Logging in with the operator token yields the principal `operator`, which the policy decides on. Read-only
humans have no session today (read tokens are not built, ui-data section 8); they reach the pages through the proxy and see
the open summary. Skip host, Accept failing builds and Policy edits stay out of scope and need the `admin` role of
`split-mode-updates.md`.

### 6.8 Audit

The core writes one row per action, **before** it acts (`outcome: pending`) and updates it after (`ok`, `refused` with the
reason code, or `error`). Key `update/audit/<13-digit unix ms>/<8 random hex>` in the state store, so it is durable where the
desired row is and shared by frontends. Fields: `id`, `at` (RFC 3339), `action` (`update`, `pause`, `resume`), `door`
(`bearer` or `session`), `principal`, `claimed_name` (escaped on display, flagged unverified), `session` (first 8 hex of the
session id's SHA-256, never the id), `peer`, `commit`, `from_commit`, `to_commit`, `expect_generation`, `generation_after`,
`plan_digest`, `outcome`, `reason`. A request that failed before step 8 of 6.4 writes no row (counted in a metric and logged).

- **Fail closed**: if the pending row cannot be written, the action is refused 503 `audit store unavailable`; no desired
  write happens without a record of intent. Reads still work.
- A crash between pending and result leaves a `pending` row; the page shows it as "outcome unknown: check the desired
  row". A repeat is safe: the same commit already desired makes no new generation (#976), and the stale generation is
  reported (6.10).
- The page shows the last 50 rows, newest first, by a reverse range scan (`scan_range`, ui-data Q20). Retention 400 days, the
  longest class of ui-data 5.3; rows are small and trimmed by the same sweep.

### 6.9 Rate limits

Login: 5 failures per peer address per minute, then 429 with `Retry-After`; 30 failures a minute across all peers (a shared
budget, so a spray cannot be spread over addresses to help itself). Actions: 6 per session per minute and 20 farm-wide per
minute; one apply in flight at a time (a second answers 409 `busy`). A rejected login writes a log line and a counter, not a
row.

### 6.10 Stale plan and generation conflict

On 409 the page does not show a bare error. It says which of `digest`, `generation`, `expired`, `future`, and shows what
changed: for `generation`, the audit row that moved desired (who, when, to which commit), and if desired already equals the
submitted commit, "already applied, nothing to do"; for `digest`, a diff between the plan the session was shown (kept in the
session, 6.3) and a fresh one, as hosts added or removed and blockers appeared or cleared; for `expired`, the age. Every
variant offers one link, **Re-plan** (`GET /ui/updates/confirm?commit=<c>`), which issues a new digest and form. The refused
attempt is in the audit list.

### 6.11 Pause and Resume

`Pause` is one form POST from the page's header (no confirm page: pausing only stops new work, and it passes every check of
6.4). `Resume` goes through a confirm page that shows the current plan for the desired commit, because resuming restarts a run;
its CSRF value is bound to the generation shown, and the session door refuses `stale_generation` if desired moved. #976's
`resume` takes no generation, so this check is made by the core before the call (the amendment to carry it into the API is
Q6).

### 6.12 Failure modes

State store down: reads of rows fail as today; writes refuse (audit first). Frontend restart: sessions gone, re-login, nothing
lost. Handover mid-apply: the pending row and the CAS make a retry safe (6.8). Clock skew: expiry uses the server's clock only;
`plan_issued_at` more than 5 seconds ahead is `future`. Listener lost TLS material: the listener refuses to start (ui-data Q18),
so there is no plain-HTTP fallback that would show a form.

## 7. Resolution of ui-data Q16 (this pull request changes `ui-data.md`)

Replaces the text of `ui-data.md` section 8 ("How a human authenticates") and the item 16 of section 10:

> **How a human authenticates [D] (Q16).** Reads: a reverse proxy that injects the bearer header, or the open summary, as
> before. **Writes from a browser: a login form that exchanges the operator API token for a server-side session, on the TLS
> read listener only.** The session cookie (`__Host-kbf-session`, `Secure`, `HttpOnly`, `SameSite=Strict`, idle 15 minutes,
> absolute 1 hour) is accepted on the `/ui` routes and ignored by every `/v1` route, so no cookie makes a JSON API call. A POST
> is proven cookie-proof when it passes, in order: a TLS listener, an `Origin` equal to `--ui-origin` and `Sec-Fetch-Site:
> same-origin`, the session, the authorizer, a CSRF value bound to the session, the action and the plan, and a rate limit; each
> has a red test with a planted mutant (`update-ui.md` section 9). The operator write gate of `api.rs` is unchanged and remains
> the only way a script writes. An authenticating proxy principal is a second authenticator on the same session
> interface, not built.

And replaces the two bullets that say the UI is read-only: *"The UI's pages are read-only. The only writes a page offers are the
update actions of `update-ui.md` (Update, Pause, Resume); drain, cordon and purge stay CLI and API."*, and the `UI pages` bullet's
CSP, adding `form-action 'self'; base-uri 'none'` and `Referrer-Policy: same-origin`. Summary line 9 says "read-only pages
plus one declared write family".

## 8. What exists and what to build

| Piece | State |
|---|---|
| `kbf-ui` pages, `Reader`, `Html`, CSP, contract test | built, GET only, not mounted |
| `GET /v1/update`, `POST desired`, `pause`, `resume` | #976, open |
| `expect_generation`, `policy` in `GET /v1/update` | being added to #976 |
| TLS read listener | decided, not built |
| `farm.get`, `update.setAuthorizer` | not built |
| `UpdateStatus` kept by the server, `GET /v1/update/hosts`, plan, digest | to build (A3 to A5) |
| Session table, CSRF, Origin check, `Form`, `Action` contract, audit rows, rate limits | to build |
| Updates page and confirm page | to build |
| Controller phases, blockers, held reasons | not started (U4, U10) |

## 9. PR split and tests

Each PR is red first (the test is committed failing against the old code, then the change turns it green) and lists the
mutant that must turn it red again. The text pull request on pull-updates carries A1 to A7 into `pull-updates.md` 6.3 and is
merged before PR 3.

1. **TLS read listener** (`ui-data.md` U1, owned there). Tests: `--read-listen` without TLS files or the insecure flag exits
   with a usage error; a plain-HTTP request to the TLS listener gets no page. Mutant: serve plain HTTP on fallback.
2. **Server keeps `UpdateStatus`; `NodeView.update`; `GET /v1/update/hosts`** (A5). Tests: staged-and-refused status is on
   hosts and only `{state, up_to_date, availability}` on `/v1/nodes`; a silent host is `unsupported` or `unavailable`, never
   idle; newest wins; a stale stream is dropped; a 100 KiB `refusal` stored as 512 bytes cut on a character boundary.
   Mutants: keep oldest; default silent to idle; accept the old stream; store uncut; `refusal` on `/v1/nodes`.
3. **Core, plan and digest** (A2 to A4, A6, A7; stacks on #976): the handlers become wrappers of `offer_desired` and
   `set_paused`; `GET /v1/update/plan`; the audit rows. Tests: stale generation is 409 `stale_generation`; a stale digest, an
   expired plan, a plan from the future and an edited `issued_at` are 409 `stale_plan` with the reason named; a reconnecting
   host or a step-state change leaves the digest unchanged, a changed host list or blocker changes it; `plan_digest` without
   `plan_issued_at` is 400; the plan GET writes nothing (counting store); **an audit row exists for an applied, a refused and
   a CLI-door update, pending before the write and final after**; an unwritable audit store refuses the update and the desired
   row is unchanged; the bearer door still refuses non-loopback, `Origin`, no token, non-JSON. Mutants: take the generation
   every call; ignore the digest; digest covers `connected`; skip expiry; write the row after the apply; swallow the audit error.
4. **Session, CSRF, audit page, `Form` and `Action` contract** (no button yet). Tests, each with its mutant:
   - login with a wrong token is refused and is rate limited at the sixth try (mutant: no limit; limit per address only);
   - the token appears in no response body, header or log line (mutant: echo it in an error);
   - the cookie has exactly `__Host-kbf-session`, `Secure`, `HttpOnly`, `SameSite=Strict`, `Path=/`, no `Domain`
     (mutant: drop `SameSite`);
   - an idle-expired and an absolute-expired session are refused on the server's clock even if the cookie is sent
     (mutant: trust `Max-Age`); a logged-out and a revoke-all session are refused (mutant: logout clears only the cookie);
   - **missing CSRF refused; a CSRF token from another session, another action or another plan refused; an edited hidden
     field refused** (mutants: no check; token not bound to the plan; token not bound to the session);
   - **cross-origin POST refused; absent `Origin` refused; `Sec-Fetch-Site: cross-site` refused** (mutants: compare `Host`; accept
     absent Origin);
   - **POST on a listener without TLS refused and the form not rendered** (mutant: trust `X-Forwarded-Proto`);
   - **a session whose principal the authorizer refuses cannot write** and a missing `update.setAuthorizer` turns the write path off
     (mutants: skip the authorizer; default allow);
   - a cookie sent to a `/v1` GET or POST changes nothing (mutant: accept the cookie on `/v1`);
   - the CSP string is exact including `form-action 'self'; base-uri 'none'`; every `Answer` has it (mutant: drop `form-action`);
   - the contract test fails if a `Page` declares a POST read, if an `Action` path is outside `/ui/`, or if a `Form` has an absolute
     `action` (mutants: each);
   - the audit list renders a claimed name of `<script>` escaped.
5. **The Updates page and the confirm page** (read-only). Tests: every `availability` and every `UpdateStatus` state renders;
   a refusal and node id with markup render escaped; completeness names `kbf` for linux and `kbf-daemon` for darwin; the
   frontend row is separate and the plan lists no frontend; `paused` shown from `policy.paused`, else `desired.paused`; an
   anonymous viewer sees no detail columns; the confirm page issues only GETs (counting source). Mutants: raw-write a
   refusal; show detail to anonymous; route the plan through a POST; read `desired.paused` when `policy` exists.
6. **The Update, Pause and Resume buttons.** Tests: the Update POST reaches the core with the four fields from the hidden
   form; **stale digest refused and the page names why and links Re-plan**; a generation conflict shows the audit row that
   moved desired; an applied update shows `ok` in the audit list from the row; a blocker disables the button with its
   reason; Resume refuses a moved generation; one apply in flight. Mutants: drop `plan_digest` from the form; show the error
   from the URL text; allow a second concurrent apply.
7. **Pull-updates text pull request**: edits `pull-updates.md` 6.3 for A1 to A7 (docs only).
8. **Optional: `kbf update set|plan`** reading the token from `--api-token-file`; not required by the page.

PRs 2 to 5 are independent of the controller. PR 4 onward needs the `kbf-ui` mount (not done).

## 10. Open questions

- **Q1 (closed here)** UI writes: yes, session door, section 6.
- **Q2 (closed here)** The credential for a session: the operator API token, exchanged once; the proxy principal later.
- **Q3 Authority of the update reads.** Recommendation: `farm.get` for `/v1/update/hosts` and `/plan` and fields beyond the summary;
  `GET /v1/update` stays open as #976 serves it (it shows the desired commit and release list, which `GET /v1/releases` already
  shows). Until `farm.get` exists those routes need a bearer-token read gate that does not require loopback, or the page reaches
  them in-process for an authenticated viewer only (section 4).
- **Q4 Role column.** `NodeView.role` or a join on `/v1/members`; recommendation: `NodeView.role`.
- **Q5 Proxy principal.** When a trusted proxy exists, add it as a second authenticator on the session interface; needs a
  `--trusted-proxy` flag. Gives per-person audit identity. Recommendation: do it right after PR 6.
- **Q6 Carry the resume generation into the API.** Recommendation: add optional `expect_generation` to `resume` too.
- **Q7 The frontend's commit** for its separate row: build info of the serving process, a new field. Recommendation: yes.
- **Q8 Machine-readable refusal.** Free text only until a reason code exists.
- **Q9 Branch state.** `GET /v1/members` and `kbf-ui` are on arch-v2 and the update wire on pull-updates; PRs 2 and 3 need both,
  so they stack after arch-v2 is merged into pull-updates.
- **Q10 `/v1/releases` is open under the spec.** Left open, flagged.

## 11. What blocks implementation

The TLS read listener blocks every form (PR 1). #976 with `expect_generation` and `policy` blocks PR 3. The `kbf-ui` mount blocks
PR 4 onward. `farm.get` blocks serving details to anyone but a session (Q3). The controller (U4, U10) blocks phases, held
reasons and preflight blockers, none of which the first page needs. The root helper blocks real `helper_present` values in
production, not the page.
