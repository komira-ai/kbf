# The update page: versions per host, update available, the update button

Status: design. Nothing in this file is built. Facts marked **[F]** were read from the code on three lines of work:
**arch-v2** (`b74678f0`: `kbf-ui`, `kbf-auth`, `GET /v1/members`, `--api-listen`, `ui-data.md`, `split-mode-updates.md`),
**pull-updates** (`6561a838`: `kbf.update.v1`, the release store, the frozen specification `pull-updates.md` 6.3, which this
file calls "the spec") and the open change **#976** (`pu/upd5-12-desired-offers`, head `6935f7e6`, the desired release and the
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

The decision this file makes, and closes **ui-data Q16** with (section 7): the browser gets **a login form that checks a
per-operator password from a login file (`--ui-login-file`, one entry per operator: name, password hash, role) and makes a
server-side session, over TLS only; the session authorises a fixed set of POST routes under `/ui/`; the JSON API stays
cookie-blind and its gates are unchanged.** The operator API token is **not** a browser credential and never reaches a browser:
the server keeps only its SHA-256 (`token.rs`), the UI has no token to forward, and a stolen browser credential is not a bearer
credential (each is revoked alone).

## 2. Facts that bound the design

- **[F]** The frontend receives each host's `UpdateStatus` and discards it (`worker.rs`, the `Offer(_) | UpdateStatus(_)` arm).
  No per-host update state is served.
- **[F]** #976 serves `GET /v1/update` **open** as `{desired, releases}` (`desired` is `{generation, commit, set_at_ms,
  paused}`), `POST /v1/update/desired` with `Offer` = `deny_unknown_fields {commit}`, and `POST /v1/update/pause` and
  `/resume` (`release/desired.rs`). Writes pass `operator_gate`. Its owner says it is gaining an optional `expect_generation`
  (409 on mismatch, no write) and a `policy` object in `GET /v1/update`; **this file builds on both and says so**:
  `GET /v1/update` = `{desired, releases, policy?}`, `Offer` = `{commit, expect_generation?}`. **Where pause lives.** At #976's head `policy_view` has no `paused` field: pause is `desired.paused`, and `policy` is null until a
  policy row exists. The page's rule: **use `policy.paused` only if that key is present, else `desired.paused`**, so a policy row
  without the key never reads as "running". A wrong "running" is a safety error (the operator presses Update or never presses
  Resume). #976 is asked to say where pause lives (Q11); this file changes its reading only after it does.
- **[F]** A platform is complete when it has its own components: linux needs `kbf`, darwin needs `kbf-daemon`
  (`ReleasePlatform::components` in #976). The page shows completeness per platform with that component named.
- **[F]** `operator_gate` (`api.rs`): loopback peer, no `Origin` header, `Authorization: Bearer` compared in constant time
  against a digest, `Content-Type: application/json`. The token is read once from `--api-token-file` and **only its SHA-256 is
  kept** (`ApiToken`, `token.rs`).
- **[F]** `kbf-auth` has `Authorizer::authorize(metadata, instance_names)` and a `Policy` with `invocations_get` and
  `blobs_get`; a missing one turns its routes off. A policy is built from `allow`, `deny`, `any` and `all` only: **no policy kind
  reads a principal or a role**, so today an authorizer for a key is allow-everyone or deny-everyone. No authenticator reads a
  credential, and `farm.get` is not built (`ui-data.md` Q11). `Policy::allow_all()` and `from_json`'s default fill unset keys
  with `allow`, which section 6.7 must not let reach the new key.
- **[F]** `api.rs` `gate` requires a loopback peer, so today a leaked operator token is useless to a remote attacker. Envoy
  today (`kbf-deploy` `envoy/render.rs`) renders only the REAPI gRPC route: no HTTP or `/ui` route exists.
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
2. **Escaping by construction.** Commits, release names, refusal text, hold reasons, node ids and the operator's name
   are strings from outside and render through `Html`. Forms are built by a `Form` type that emits `method="post"`, an
   `action` that is a fixed `/ui/...` path, and hidden fields escaped like any text; there is no way to write a raw `<form>`.
3. **CSP, no script.** Section 6.5 states the change (`script-src 'none'`, adds `form-action 'self'` and `base-uri 'none'`).
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

Farm header: the desired commit, generation and set time from `desired`; **paused** by the rule of section 2 (`policy.paused`
only if that key is present, else `desired.paused`) with a **Pause** or **Resume** form for a session of the `update` role; the policy fields read-only; the
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
| A7 | Every `desired`, `pause` or `resume` call that reaches the core, applied or refused, writes an audit row (section 6.8), whichever door it came through. A refusal by the bearer gate before the core (loopback, `Origin`, token, content type) writes none | "every update action" includes the CLI's; a row per gate refusal would let a local process flood the store |

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
| session (new) | a browser form POST to `/ui/updates/...` on the TLS read listener | section 6.4: TLS listener, Host, Origin, session, role, CSRF, rate limit | the session's principal: the operator name from the login file |

The session door is **not** a client of the bearer door. The UI does not call `/v1/update/desired` over loopback with a token:
it cannot (only the digest is kept), and reading the token file into the UI process would make it a token holder whose every
bug is a write to the farm. The session door calls the core in-process, with a fixed call set. This is the answer to the
loopback invariant of `api.rs`: that invariant stays true of the bearer door; the session door is a second, narrower
admission with its own invariant, tested in section 9. The `api.rs` module doc is amended in PR 3 to say two doors exist.

**Why this is not a token-holding confused deputy.** (1) The UI holds no token and no way to make one. (2) The set of things a
session can cause is three core calls chosen by the route, not by any request field (no URL, method or path comes from the
client). (3) Each call is bound to the session's own CSRF value and to the plan (6.4), so a page the attacker controls cannot
drive it. (4) The session is made by an operator who proved a password from the login file (not the bearer token) and is checked against
the authorizer, by principal, on every write (6.7). (5) Every write is audited with the session (6.8). (6) The cookie is accepted on exactly these routes
and is ignored by every `/v1` route, so a cookie makes no JSON API call, GET or POST.

### 6.2 Login: what authenticates an operator in a browser

The operator API token is **not** the browser credential. A shared token typed into a form would (a) void the loopback defence
(`gate` makes a leaked token useless to a remote attacker today; token plus reach to the TLS listener would be a full remote
write), (b) pass through autofill, password managers and look-alike pages, (c) give the audit no verified identity and no
roles, and (d) tie the browser's revocation to the script's. Options and the choice:

| Option | Token stays out of browsers | Built today | Identity of the actor |
|---|---|---|---|
| (1) Authenticating proxy principal (ui-data Q16 proxy supplies a verified identity) | yes | no: needs `--trusted-proxy` and an `Authenticator` that reads it | real, per person |
| **(2) Per-operator login file `--ui-login-file` [P, v1]** | yes | no: new, small | real, per operator: the name in the file |
| (3) Shared operator token typed into a form | no | yes | none; rejected above |
| (4) UI forwards the bearer token over loopback | n/a | no: only the digest is kept | the token; rejected (the UI would hold the token) |

Chosen: **(2) for v1**, with (1) as a second authenticator on the same session interface once a trusted proxy exists (Q5).

**The login file.** One entry per operator: `name` (`[A-Za-z0-9._-]`, at most 64 bytes), `password_hash` (an argon2id PHC string;
the loader refuses parameters below 19 MiB memory and 2 iterations), `role` (`update` or `view`). It is a mode `0600` file
owned by the server's user, read at start and on `SIGHUP`, and refused if it is group or world readable, the same rule as
`token.rs`. A small `kbf` subcommand reads a password from stdin (never argv) and prints an entry. A session re-reads the
entry on use: a removed operator or a changed role takes effect on the next request. The name is the principal given to the
authorizer (6.7) and the audit's `principal`, so the audit's "who" is verified and the page no longer asks for a typed name.
An unknown name is verified against a dummy hash, so time does not tell names apart. A new hashing dependency needs the usual
vetting (Q12).

`GET /ui/login` shows a form (name; password as `type=password`). `POST /ui/login` is the only place a password appears: the
body is capped at 1 KiB, the password is verified, then dropped; never logged, never in a URL (POST only; a GET carrying a
credential is ignored and not logged), never echoed, never in a later page. The login passes the same TLS, Host, Origin and
Sec-Fetch-Site checks as an action. At most 2 hash verifications run at once.

If an owner insists on the shared token for a first release, the doc's claim must change: the loopback defence would no longer
cover writes, and only the rate limits, the 1-hour absolute lifetime and the TLS listener would stand in for it. This design
does not offer that.

### 6.3 The session

- **Id**: 256 random bits from the OS generator. **Cookie**: `__Host-kbf-session=<id>; Secure; HttpOnly; SameSite=Strict;
  Path=/` and no `Domain` (the `__Host-` prefix makes a browser refuse any cookie without `Secure` and `Path=/`, or with a
  `Domain`). `Path=/` means the browser also sends it to `/v1`; `/v1` ignores it.
- **Server side**: an in-memory table **per frontend process** keyed by the SHA-256 of the id, holding principal, role, peer
  address at login (6.9 says what that address is), created and last-used times, a random per-session CSRF secret, the
  **epoch** read at login (below), the outstanding one-time nonces of 6.11, and the last four plans shown (for 6.10). At most
  64 live sessions; the oldest idle is evicted first.
- **Lifetime**: idle timeout 15 minutes, absolute 1 hour, whichever is first, on the server's clock at every use; the cookie's
  `Max-Age` is advisory.
- **Revocation, durable across frontends.** A state-store row `ui/sessions_epoch` (a counter). Every session stores the epoch it
  was made under and **every use compares it** (a point read); a mismatch refuses the session. `POST
  /v1/ui/sessions/revoke-all` on the bearer door raises the epoch by CAS, so revoking on any frontend revokes on all of them,
  and the audit records it (6.8). `POST /ui/logout` (CSRF-checked) deletes that session's entry and clears the cookie. A
  restart empties the table (sessions are not persisted: a persisted session is a second credential store); a frontend handover
  also ends sessions. The epoch row and the audit survive. If the epoch cannot be read, sessions are refused (fail closed).
- A session is **not bound to an address** (operators move networks); it is bound to the listener it was made on.
- **Sessions are per frontend**: a POST that reaches a frontend other than the one that made the session redirects to login.
  6.6 says how a browser reaches one named listener, so this is a fact of the topology and not a surprise.

### 6.4 Admission of a session POST, in order

Routes: `POST /ui/login`, `POST /ui/logout`, `POST /ui/updates/apply`, `POST /ui/updates/pause`, `POST /ui/updates/resume`.
Everything else under `/ui` is GET. A request failing a step stops there with the step's status. Steps 1 to 3 (and a missing or expired session, step 4) write
**no audit row**, only a counter, so an unauthenticated peer cannot grow the store (a login attempt is the one pre-session
request that is audited, under the cap of 6.8); from step 5 on the request is authenticated
and a refusal is audited (6.8):

1. **TLS listener and Host.** The write path is on only when the listener it arrived on terminates TLS (a startup fact, not a header
   such as `X-Forwarded-Proto`). On a listener started with `--read-listen-insecure` the forms are not rendered, the routes
   answer 404 `write path off: this listener has no TLS`, and the page says so. A plain-HTTP request to a TLS listener gets
   no page (ui-data Q18). `Host` must equal the host of `--ui-origin`, else **421** (this holds for every `/ui` request, GET
   included, so a DNS-rebinding page cannot read session pages).
2. **Origin and fetch metadata.** `Origin` must be present and equal the configured `--ui-origin` (a flag naming
   `https://host[:port]`, never derived from `Host`). If `Sec-Fetch-Site` is present it must be `same-origin`. Absent `Origin`
   is refused (a browser form POST always sends it; the page sets `Referrer-Policy: same-origin`, not `no-referrer`, because a
   `no-referrer` page makes the browser send `Origin: null`).
3. **Shape.** `Content-Type: application/x-www-form-urlencoded` exactly, body at most 4 KiB, each field once, no unknown field,
   hex fields validated by length and alphabet before any use.
4. **Session**: cookie present, entry found, within idle and absolute lifetime (else 303 to `/ui/login`).
5. **Role**: the session's role is `update` and the session's principal (the operator name) passes the authorizer for `update`
   (6.7), else 403.
6. **CSRF**: the hidden `csrf` field equals `HMAC-SHA256(session_secret, "kbf.ui.csrf.v1\0" action "\0" bound)`, compared
   in constant time. `bound` for `apply` is `commit "\0" expect_generation "\0" plan_digest "\0" plan_issued_at`; for
   `pause` and `resume`, the one-time nonce issued with the form (6.11); for `logout`, empty. The token is therefore bound to the
   session (secret), to the action and to the plan it confirms: a token from one confirm page does not work on another plan,
   another action or another session, and editing a hidden field breaks it.
7. **Rate limit** (6.9).
8. **Audit and core call** (6.8, 6.1).
9. Answer **303 See Other** to `GET /ui/updates?audit=<row id>`; the page shows the result from the audit row, never from
   text in the URL (a link cannot put words on the page). A refresh does not repost.

### 6.5 The CSP and the headers

The current CSP has no `form-action`, and `form-action` does not fall back to `default-src`: with `default-src 'none'` a form
can still submit to any origin. The change, applied to **every** `Answer` (so no page's policy varies), also takes script off
the table, since no script is served and a write button now exists:

```text
Content-Security-Policy: default-src 'none'; style-src 'self'; img-src 'self'; script-src 'none'; frame-ancestors 'none'; form-action 'self'; base-uri 'none'
```

Added: `form-action 'self'`, `base-uri 'none'` (an injected `<base>` cannot redirect the form's relative action) and
`script-src 'none'` (replaces `'self'`; if HTML injection ever existed, no same-origin text could run as a script with the
session). The exact-string CSP test changes with it. Also on every page: `Referrer-Policy: same-origin` and `X-Frame-Options:
DENY` (older browsers ignore `frame-ancestors`); on every session page, login and action answer: `Cache-Control: no-store` (a
CSRF value must not be in a shared cache or the back-forward list). **No response compression on session pages**: the CSRF value
sits next to attacker-influenced strings (refusal text, node ids), the BREACH shape. The value is also a deterministic HMAC per
(session, action, bound), so the rule is a rule and not a mask; masking per response is the alternative if compression is ever
wanted. `Host` is checked on every `/ui` request (6.4 step 1). A test loads each same-origin route that exists (blob, metrics,
`/v1` JSON) as a script and as a document and requires that neither runs (the blob route's `sandbox`, `nosniff` and
octet-stream headers are what make it so; the test holds them).

### 6.6 TLS and how a browser reaches the listener

The write path needs the TLS read listener: `Secure` cookies are never sent to plain HTTP, and a password must not cross a
network in clear. That listener (U1 of `ui-data.md`) is built first (PR 1). `--ui-origin` is required to enable forms; without
it the page is read-only and says why.

**Topology, stated.** Nothing today routes a browser to a frontend: Envoy renders only the REAPI gRPC route. v1 therefore is:
**a browser connects to one named frontend's TLS read listener, and `--ui-origin` is that name.** A balancer in front is allowed
only as a TCP pass-through to that listener, or as an HTTPS route to `/ui` with **sticky routing and re-encrypted upstream
TLS** (the balancer-to-frontend hop is the TLS hop, `ui-data.md` section 8); that Envoy route is not built and is a later pull
request (Q13). Because sessions, CSRF secrets, rate counters and the apply latch live in one process (6.3, 6.9), a topology with
several frontends and no stickiness would make every POST land on a process that has no session.

**The read listener mounts no `/v1` write route.** `POST /v1/update/*` and every other write live only on the loopback bearer
door, so a cookie-bearing request to the TLS listener cannot reach it. A reverse proxy on the host that makes remote callers
look loopback would defeat the loopback gate; the v1 proxy of ui-data Q16 (the one that injects the bearer header for reads) is
therefore configured for `GET` and `HEAD` on read paths only, and the doc says so there.

### 6.7 Roles and the authorizer

Two checks, both required: the login file's `role` (`update` shows the buttons) and the authorizer, asked on **every write**
through the existing `kbf-auth` machinery: a new policy key `update.setAuthorizer` (an `Authorizer`, like `invocations_get`),
asked with the session's `AuthenticationMetadata` (principal = the operator name) and the fixed name `farm/update` (a constant,
not a request field).

**v1 needs a principal-matching policy kind, and builds it first.** Today a policy is `allow`, `deny`, `any` or `all`, none of
which reads a principal, so the authorizer could only be on for all operators or off for all, and "a session the authorizer
refuses cannot write" would be true only by turning writes off for everyone. **PR 3a (prerequisite)** adds a `principal` policy
kind (`{"principal": ["alice", "bob"]}`, matching the principal of the metadata) and its tests. The test for the write path
uses **two principals in one policy, one allowed and one refused**: the allowed one writes, the refused one logs in, sees the
page and gets 403, in the same process. A test that uses `deny` proves nothing about role separation and is not accepted.

**A missing key turns the write path off**, as `invocations.getAuthorizer` does for its routes. `Policy::allow_all()` and
`from_json`'s default (`Authorizers::allow_all()`) fill unset keys with `allow`; the new key is **excluded from that fill**, and
a test builds a server with no policy file, with `Policy::allow_all()` and with a policy that omits the key, and requires the
write path off in each. If PR 3a does not land, v1 says plainly that it has exactly two states, write path on (the policy names
`update: allow`) or off, and ships with no claim of per-operator authorisation; the audit's verified name is still there.

Read-only humans have no session today (read tokens are not built, ui-data section 8); they reach the pages through the proxy
and see the open summary. Skip host, Accept failing builds and Policy edits stay out of scope and need the `admin` role of
`split-mode-updates.md`.

### 6.8 Audit

The core writes one row per update action, **before** it acts (`outcome: pending`) and updates it after (`ok`, `refused` with
the reason code, or `error`). Key `update/audit/<13-digit unix ms>/<8 random hex>` in the state store, so it is durable where
the desired row is and shared by frontends. Fields: `id`, `at` (RFC 3339), `action`, `door` (`bearer` or `session`),
`principal`, `session` (first 8 hex of the session id's SHA-256, never the id), `peer`, `commit`, `from_commit`, `to_commit`,
`expect_generation`, `generation_after`, `plan_digest`, `outcome`, `reason`.

**What is audited.** The session door writes a row for: login ok, login refused and login rate-limited (the typed name only if
it fits `[A-Za-z0-9._-]` and 64 bytes, else the literal `invalid`; control characters never reach a log or a row), logout,
revoke-all, and every authenticated refusal (CSRF, role, stale plan or generation); and the core writes a row for every applied
or refused update, pause and resume on either door. **A refusal by the bearer gate itself** (loopback, `Origin`, token, content
type) writes **no row**, only a counter: loopback is reachable by build actions on server hosts (the module doc of `api.rs`
says so), and a row per refusal would let an unauthenticated local process flood the store. Login refusal rows are capped at
20 a minute per frontend; the excess is counted and one `suppressed: n` row is written per minute, so a spray cannot grow the
store without bound.

- **Fail closed**: if the pending row cannot be written, the action is refused 503 `audit store unavailable`; no desired
  write happens without a record of intent. Reads still work. Login and logout fail closed too (no session without a row).
- A crash between pending and result leaves a `pending` row; the page shows it as "outcome unknown: check the desired
  row". A repeat is safe: the same commit already desired makes no new generation (#976), and the stale generation is
  reported (6.10).
- **Append-only.** No API route deletes under `update/audit/`; only the retention sweep does, and it runs in the server
  process, never in a frontend. Retention 400 days, the longest class of ui-data 5.3; rows are small. The sweep's owner is the
  server's existing retention task (ui-data 5.3), extended with this prefix.
- **Tamper evidence is not in v1** (Q14): a hash chain would need a CAS head row on every write, a serialised hot row on a path
  that must work during an incident, and two frontends writing concurrently would contend on it. Append-only by API plus
  state-store access control is the v1 boundary, said plainly.
- The page shows the last 50 rows, newest first, by a reverse range scan (`scan_range`, ui-data Q20), every name and reason
  escaped through `Html`.

### 6.9 Rate limits

All counters here are **per frontend process**; this doc claims nothing farm-wide for them. The farm-wide guard on an apply is the
CAS on `expect_generation` in the core (a second apply on a stale generation is 409 whichever frontend it reached), and the
per-process "one apply in flight" latch is courtesy only (a second answers 409 `busy`), not a guarantee.

**Client address.** Behind any proxy the peer is the proxy, so a per-peer limit would be a global limit of five a minute and
the audit `peer` would identify nothing. The login limit is keyed on the **client address only when `--trusted-proxy` names the
proxy** (the rightmost `X-Forwarded-For` entry not in that list; the flag is pulled forward from Q5 for this); without it the key
is the peer and the behaviour is documented, and `X-Forwarded-For` is never read, because trusting it unconfigured would let an
attacker choose their own key. The audit `peer` is the same address.

**Login.** 5 failures per client address per minute, then 429 with `Retry-After`. There is **no global hard block**, because one
would let an unauthenticated spray keep every operator out of login during an incident when Pause is wanted. Instead failures
raise a **global slow-down**: each failed answer is delayed by an amount that doubles with the failures of the last minute, to
a cap of 10 seconds, and the delay applies to the failed attempt's own answer, not to other attempts' admission. A correct
credential is verified by the 2 verification slots (6.2) with at most 16 attempts waiting; a spray therefore delays a login by a
bounded wait and does not deny it, and a full queue answers 429. The escape hatch if a spray still bites: the bearer door on
the host pauses.

**Actions.** 6 per session per minute and 20 per frontend per minute. A rejected login writes a counter and, within the cap of
6.8, a row.

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
the session door refuses `stale_generation` if desired moved since the confirm page, a check made by the core before the call
because #976's `resume` takes no generation (carrying it into the API is Q6).

**Replay.** Pause does not change the generation, so a value bound to the generation would be replayable for the whole session
(a replayed Resume restarts a run). The CSRF value of `pause` and `resume` is therefore bound to a **server-issued one-time
nonce**, issued when the form is rendered, kept in the session (at most 8 outstanding, oldest dropped) and **consumed on use**.
A second POST of the same form is refused and audited. An Update form's value binds the plan (and `expect_generation`, which
goes stale after the first apply), so it is bounded the same way.

**What makes `plan_issued_at` trustworthy.** It is client-supplied and the digest is not a MAC (anyone can compute it), so the
10-minute expiry is enforced only because the CSRF value, which only a session can obtain from the page, covers `issued_at`. The
bearer door has no such protection: a script holding the token can forge `plan_issued_at`, and the design treats a script as an
operator already (section 5).

### 6.12 Failure modes

State store down: reads of rows fail as today; writes and logins refuse (audit first, epoch unreadable). Frontend restart:
sessions gone, re-login, nothing lost. Handover mid-apply: the pending row and the CAS make a retry safe (6.8). Clock skew: expiry uses the server's clock only;
`plan_issued_at` more than 5 seconds ahead is `future`. Listener lost TLS material: the listener refuses to start (ui-data Q18),
so there is no plain-HTTP fallback that would show a form.

## 7. Resolution of ui-data Q16 (this pull request changes `ui-data.md`)

Replaces the text of `ui-data.md` section 8 ("How a human authenticates") and the item 16 of section 10:

> **How a human authenticates [D] (Q16).** Reads: a reverse proxy that injects the bearer header, **configured for `GET` and
> `HEAD` on read paths only** (a proxy that injects the bearer header on a write would make every remote caller a loopback
> writer), or the open summary, as before. **Writes from a browser: a login form that checks a per-operator password from a
> login file (`--ui-login-file`) and makes a server-side session, on the TLS read listener only.** The operator API token is
> never a browser credential. The session cookie (`__Host-kbf-session`, `Secure`, `HttpOnly`, `SameSite=Strict`, idle 15
> minutes, absolute 1 hour, revocable on every frontend by a durable epoch) is accepted on the `/ui` routes and ignored by
> every `/v1` route, so no cookie makes a JSON API call; the read listener mounts no `/v1` write route. A POST is proven
> cookie-proof when it passes, in order: a TLS listener and a `Host` equal to `--ui-origin`'s, an `Origin` equal to
> `--ui-origin` and `Sec-Fetch-Site: same-origin`, the session, the authorizer by principal, a CSRF value bound to the session,
> the action and the plan (a one-time nonce for pause and resume), and a rate limit; each has a red test with a planted mutant
> (`update-ui.md` section 9). The operator write gate of `api.rs` is unchanged and remains the only way a script writes. An
> authenticating proxy principal is a second authenticator on the same session interface, not built.

And replaces the two bullets that say the UI is read-only: *"The UI's pages are read-only. The only writes a page offers are the
update actions of `update-ui.md` (Update, Pause, Resume); drain, cordon and purge stay CLI and API."*, and the `UI pages` bullet's
CSP, with `script-src 'none'; ... form-action 'self'; base-uri 'none'`, `Referrer-Policy: same-origin` and `X-Frame-Options:
DENY`. Summary line 9 says "read-only pages plus one declared write family".

## 8. What exists and what to build

| Piece | State |
|---|---|
| `kbf-ui` pages, `Reader`, `Html`, CSP, contract test | built, GET only, not mounted |
| `GET /v1/update`, `POST desired`, `pause`, `resume` | #976, open |
| `expect_generation`, `policy` in `GET /v1/update` | being added to #976 |
| TLS read listener | decided, not built |
| `farm.get`, `update.setAuthorizer`, the `principal` policy kind | not built |
| `--ui-login-file`, login hashing, `ui/sessions_epoch`, `--trusted-proxy`, `--ui-origin` | not built |
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
3a. **A `principal` policy kind in `kbf-auth`** (prerequisite of the role claim in 6.7), with the two-principal test.
3. **Core, plan and digest** (A2 to A4, A6, A7; stacks on #976): the handlers become wrappers of `offer_desired` and
   `set_paused`; `GET /v1/update/plan`; the audit rows. Tests: stale generation is 409 `stale_generation`; a stale digest, an
   expired plan, a plan from the future and an edited `issued_at` are 409 `stale_plan` with the reason named; a reconnecting
   host or a step-state change leaves the digest unchanged, a changed host list or blocker changes it; `plan_digest` without
   `plan_issued_at` is 400; the plan GET writes nothing (counting store); **an audit row exists for an applied, a refused and
   a CLI-door update, pending before the write and final after**; an unwritable audit store refuses the update and the desired
   row is unchanged; the bearer door still refuses non-loopback, `Origin`, no token, non-JSON. Mutants: take the generation
   every call; ignore the digest; digest covers `connected`; skip expiry; write the row after the apply; swallow the audit error.
4. **Login file, session, epoch, CSRF, audit page, `Form` and `Action` contract** (no button yet). Tests, each with its mutant:
   - login with a wrong password is refused and is rate limited at the sixth try from one client address (mutants: no limit;
     limit keyed on the proxy peer; trust `X-Forwarded-For` without `--trusted-proxy`); a spray from many addresses delays but
     does not deny a correct login (mutant: global hard block); the **operator API token is refused as a login password**
     (mutant: accept it); an unknown name and a wrong password take equal time within a bound (mutant: skip the dummy hash);
   - a login file readable by group or world, or with weak hash parameters, is refused at start (mutant: skip the mode check);
   - the password appears in no response body, header, audit row or log line, and a name with control characters is logged as
     `invalid` (mutants: echo it in an error; log the raw name);
   - the cookie has exactly `__Host-kbf-session`, `Secure`, `HttpOnly`, `SameSite=Strict`, `Path=/`, no `Domain`
     (mutant: drop `SameSite`);
   - an idle-expired and an absolute-expired session are refused on the server's clock even if the cookie is sent
     (mutant: trust `Max-Age`); a logged-out session is refused (mutant: logout clears only the cookie); **revoke-all on one frontend refuses a session held
     on a second frontend sharing the state store** (mutant: revoke only the local table); an unreadable epoch refuses the
     session;
   - **missing CSRF refused; a CSRF token from another session, another action or another plan refused; an edited hidden
     field refused** (mutants: no check; token not bound to the plan; token not bound to the session);
   - **cross-origin POST refused; absent `Origin` refused; `Sec-Fetch-Site: cross-site` refused** (mutants: compare `Host`; accept
     absent Origin);
   - **POST on a listener without TLS refused and the form not rendered** (mutant: trust `X-Forwarded-Proto`);
   - **a `Host` other than `--ui-origin`'s is 421 on a GET and a POST** (mutant: skip the check);
   - **two principals in one policy, one allowed and one refused: the allowed writes, the refused gets 403** (needs PR 3a;
     mutants: skip the authorizer; match the role field only); a missing `update.setAuthorizer` turns the write path off with no
     policy file, with `Policy::allow_all()` and with a policy that omits the key (mutants: default allow; fill the key from
     `allow_all`);
   - audit rows exist for login ok, login refused, logout, revoke-all, a CSRF refusal and a role refusal, and **none for a
     bearer-gate refusal** (counter only); login refusal rows stop at the cap and one `suppressed` row follows (mutants: skip a
     row; write a row per bearer refusal; no cap);
   - a replayed Pause or Resume form after one success is refused (mutant: bind the value to the generation);
   - each same-origin route that exists (blob, metrics, `/v1` JSON) cannot be loaded as a script or a document (mutant: drop
     `nosniff`); a session page has no `Content-Encoding` (mutant: enable compression);
   - a cookie sent to a `/v1` GET or POST changes nothing (mutant: accept the cookie on `/v1`);
   - the CSP string is exact including `script-src 'none'; ... form-action 'self'; base-uri 'none'`, and `X-Frame-Options: DENY`;
     every `Answer` has it (mutants: drop `form-action`; restore `script-src 'self'`);
   - the read listener mounts no `/v1` write route: a cookie-bearing `POST /v1/update/desired` to it is 404 (mutant: mount the
     write router on it);
   - the contract test fails if a `Page` declares a POST read, if an `Action` path is outside `/ui/`, or if a `Form` has an absolute
     `action` (mutants: each);
   - the audit list renders a principal or a refusal of `<script>` escaped.
5. **The Updates page and the confirm page** (read-only). Tests: every `availability` and every `UpdateStatus` state renders;
   a refusal and node id with markup render escaped; completeness names `kbf` for linux and `kbf-daemon` for darwin; the
   frontend row is separate and the plan lists no frontend; **a policy row present without a `paused` key and `desired.paused` true renders paused**, and `policy.paused` wins when present; an
   anonymous viewer sees no detail columns; the confirm page issues only GETs (counting source). Mutants: raw-write a
   refusal; show detail to anonymous; route the plan through a POST; read only `policy.paused` (so a policy row without the key shows running).
6. **The Update, Pause and Resume buttons.** Tests: the Update POST reaches the core with the four fields from the hidden
   form; **stale digest refused and the page names why and links Re-plan**; a generation conflict shows the audit row that
   moved desired; an applied update shows `ok` in the audit list from the row; a blocker disables the button with its
   reason; Resume refuses a moved generation; a second apply on a stale generation is 409 whichever frontend served it (the CAS, not the latch). Mutants: drop `plan_digest` from the form; show the error
   from the URL text; allow a second concurrent apply.
7. **Pull-updates text pull request**: edits `pull-updates.md` 6.3 for A1 to A7 (docs only).
8. **Optional: `kbf update set|plan`** reading the token from `--api-token-file`; not required by the page.

PRs 2 to 5 are independent of the controller. PR 4 onward needs the `kbf-ui` mount (not done).

## 10. Open questions

- **Q1 (closed here)** UI writes: yes, session door, section 6.
- **Q2 (closed here)** The credential for a session: a per-operator password from `--ui-login-file`, not the bearer token (6.2);
  the proxy principal later.
- **Q3 Authority of the update reads.** Recommendation: `farm.get` for `/v1/update/hosts` and `/plan` and fields beyond the summary;
  `GET /v1/update` stays open as #976 serves it (it shows the desired commit and release list, which `GET /v1/releases` already
  shows). Until `farm.get` exists those routes need a bearer-token read gate that does not require loopback, or the page reaches
  them in-process for an authenticated viewer only (section 4).
- **Q4 Role column.** `NodeView.role` or a join on `/v1/members`; recommendation: `NodeView.role`.
- **Q5 Proxy principal.** When a trusted proxy exists, add it as a second authenticator on the session interface. The
  `--trusted-proxy` flag itself is pulled forward into PR 4 for the login limit's client address (6.9); the authenticator is not.
  Recommendation: the authenticator right after PR 6.
- **Q6 Carry the resume generation into the API.** Recommendation: add optional `expect_generation` to `resume` too.
- **Q7 The frontend's commit** for its separate row: build info of the serving process, a new field. Recommendation: yes.
- **Q8 Machine-readable refusal.** Free text only until a reason code exists.
- **Q9 Branch state.** `GET /v1/members` and `kbf-ui` are on arch-v2 and the update wire on pull-updates; PRs 2 and 3 need both,
  so they stack after arch-v2 is merged into pull-updates.
- **Q10 `/v1/releases` is open under the spec.** Left open, flagged.
- **Q11 Where pause lives in #976.** `policy_view` has no `paused` today; ask its owner to state it. Until then the section 2 rule.
- **Q12 A password-hash dependency** (argon2id) needs vetting and a pinned version; the alternative, a hash of a long random
  password (no stretching), is refused because operators choose their own.
- **Q13 An Envoy `/ui` route** with sticky routing and re-encrypted upstream TLS, so a browser can reach a farm of frontends
  through one name. Not built; v1 names one frontend (6.6).
- **Q14 Rejected: a hash-chained audit.** It serialises every write on a CAS head row and contends across frontends (6.8);
  append-only by API and store access control is the v1 boundary. Revisit with a signed export.
- **Q15 Rejected: a farm-wide apply lock row.** The CAS on `expect_generation` already makes a second apply fail on any
  frontend; a lock adds a lease to expire during a crash. The per-process latch is courtesy only (6.9).
- **Q17 Rejected: keeping the shared token as the v1 login.** It voids the loopback defence for writes, passes the token through
  browsers and gives no identity or separate revocation (6.2).

## 11. What blocks implementation

The TLS read listener blocks every form (PR 1). #976 with `expect_generation` and `policy` (and Q11's answer) blocks PR 3; the `principal` policy kind (3a) blocks the role claim. The `kbf-ui` mount blocks
PR 4 onward. `farm.get` blocks serving details to anyone but a session (Q3). The controller (U4, U10) blocks phases, held
reasons and preflight blockers, none of which the first page needs. The root helper blocks real `helper_present` values in
production, not the page.
