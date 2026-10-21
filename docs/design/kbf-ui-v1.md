# kbf UI v1: design

Status: approved with the CEO rulings below (amended 2026-10-21 from the proposal). Read-only web UI for operators and agents. Built marks were re-read against `main` on 2026-10-21, after every page and route merged, the Log page and the history-off page included: "built" means the code on `main`, "follow-up" means not built. The built and follow-up lists are in "Built on main" below; the operator guide is [deploy/kbf-ui.md](../deploy/kbf-ui.md). Where this document and `ui-data.md` differ on serving, access or tests, this one wins (`ui-data.md` carries the matching amendments).

## Built on main

Built: all thirteen pages (Overview, Builds, Build, Action, Log, Targets, Target, Nodes, Node, Leases, Queue, Storage, Updates) in the `kbf-ui` crate; the "History is off on this farm" page; the mount in `kbf-server` (`ui_mount.rs`, `--ui` default on, GET and HEAD, the seven security headers, the stylesheet at `/ui/static/kbf.css`); the read limiter (`read_limit.rs`); the proxy `/ui` route with open class and the `[proxy] ui` setting, default on; the routes `/v1/invocations/{id}/failures`, `/v1/leases`, `/v1/queue`, `GET /v1/nodes/{id}`, `/v1/health`, and `/v1/storage` with the seven write-tier fields; the black-box suite through the real proxy (`kbf-integration-test`, `ui_through_proxy`); the operator guide (D1).

Differences from the page table in 3, as built:

- The **Log page** (`/ui/runs/{run_id}/logs/{stream}`) shows the first 1 MiB of a log, read with one ranged request of `/v1/runs/{id}/logs/{stream}`, as escaped text: control characters and bidi marks are made inert. It says when the log is cut and links the attachment download of the whole log. The Action page shows a failed action's log tails and links each attempt's logs. The log's end (a suffix-range read) is a follow-up.
- **History off.** When the history routes are not mounted, a page that reads them (Targets, Target, the Log page) answers `200` with a "History is off on this farm" page that names `--history-objects=<URI>` (which needs `--state-store`). A record the history does not hold is still a `404`.
- Target and Targets read `/v1/grid` (and `/v1/tests`, `/v1/shapes/{key}`), not `/v1/shapes/{key}/runs`: a strip is one cell per **build**, the last 16, each a link to its build.
- Overview reads `/v1/health` and `/v1/invocations`, not `/readyz`. Node reads `/v1/nodes/{id}` only: its leases are the ids in that answer, and it shows no recent failed runs. Storage reads `/v1/storage` only: no cache or cold-store section, and `/v1/members` and `/v1/object-stores` are not read. Build has no slowest-actions list and no banner for `restored` or `gap_from` (it shows `rows capped` in the state line).
- Refresh is opt-in on Overview, Nodes and Node only (`?refresh=` 5, 10, 30 or 60); no page refreshes by default, and Leases, Queue and Storage have no refresh.

Follow-up, not built: the log's end (a suffix-range read, so a long log can be read from its tail); HSTS and the security headers on the proxy's own pages (the 503 handover page); the public-name SNI certificate setting; the 25 percent read-budget share and a ranged-log cap; the server start time on the restart notice (the notice itself is built); the `ui-data.md` and `ARCHITECTURE.md` text listed under R0.

## CEO rulings (2026-10-20)

These amend the proposal. Where an older sentence below contradicts them, the ruling wins; sentences that were dropped are marked "dropped".

- **The UI is open.** No auth for now: no viewer certificate, no viewer role, no allowlist, no deny-list, no certificate verb. Write and operator routes keep their existing auth unchanged (the operator token); the REAPI and worker gates are unchanged.
- **Read-only**, server-rendered HTML, no script, no npm (as designed).
- **On by default.** `--ui` defaults on, and so does the installer's proxy config `ui`, so anyone can open it in a browser.
- **Everything unredacted** (logs, failed-command arguments). No raw-Command gate; a one-line note on the Logs page says so.
- **Storage nodes report health through the heartbeat.**
- **TestGrid is cut.** Per-target history is kept: its purpose is to see a target's recent runs and spot flakiness at a glance.
- **Postgres is an operator-provided runtime dependency.** kbf does not own its backups; nothing here designs one.
- **Dropped design units, one line each:**
  - R2, `farm.get` enforcement and proxy-role authenticator: dropped; the UI reads in-process, with no per-instance filter.
  - R2, raw-Command gate: dropped; everything is unredacted.
  - R4a, `viewer` role in `kbf-peer`, `kbf-ca` and preflight: dropped; no viewer exists.
  - R4b, `ViewerRead` class and the viewer allowlist: dropped; `/v1` GET gates are unchanged.
  - R4b, viewer deny-list reload: dropped; there is no credential to revoke.
  - R4d, viewer-certificate installer verb: dropped; only the `ui` default-on setting remains (in R4).
  - R1, route table, `GET /v1` and access oracle: not in the v1 unit list (4.3).

## Summary (decide from this screen)

- **Scope.** Builds, one build's failed actions with stderr, the action and its log (from the history store); nodes, leases and the queue; storage health; a target's recent runs. Read-only. Beyond the five asked areas and listed in 1.2: the Overview and Updates pages, target history pages and a composed `/v1/health`.
- **Parity.** Every page reads only declared `/v1` JSON routes and lists them as links. The UI shows no number the API lacks, and a test proves it (7.2).
- **Auth.** None for `/ui` (open, GET only, on by default). It is served on the proxy's one public port. The UI never holds the operator token and has no write path; every other route keeps its existing auth (5).
- **Stack.** Server-rendered HTML from the built `kbf-ui` crate (typed builder, escape by construction), mounted in `kbf-server` on the operator API, so it needs `--api-listen` (6). No script, no npm, no new dependency.
- **Tests.** All Rust. Unit tests over a fake `Source` built from the server's own types; parity and shape tests over a real server; a black-box suite through the real proxy (7).
- **Size.** About 20 PRs, one concern each (8). The in-process `Source` (R5) and the proxy `/ui` route (R4) need an independent security review before merge: they must not widen access to anything but the declared reads.
- **Decisions.** Q1 to Q6 are answered (9).

## 1. Goal and non-goals

### 1.1 Goal and non-goals

**Goal.** A read-only web UI on the farm's one public port, open to anyone who can reach it. Every number on a page also comes from a documented JSON route an agent can call.

**Non-goals for v1:** write actions (cordon, drain, update button, retry; kbf#992); authentication of any kind (SSO, OIDC, certificates, login pages, sessions, cookies); redaction; multi-tenancy (7 of `ui-data.md` stands: one farm is one trust domain, and the UI shows every instance); the TestGrid; charts and series; storage map and handoff views; a cloud-hosted UI; browser-driven tests (7.5). **Exposure posture:** network reachability of the public port is the only boundary; whoever can load `/ui` can read every build, log and failed-command argument on the farm. DNS rebinding can let a victim's browser read an internal-only `/ui`; the ruling accepts this (no `Host` allowlist, no `Sec-Fetch-Site` refusal).

### 1.2 Beyond the assignment

- **Overview** (in scope): the one-screen answer to "is the farm healthy". It adds no data of its own, it reads `/readyz`, `/v1/health` and the build list.
- **Updates** (in scope, built): the page already exists in `kbf-ui` and reads built routes only. Unchanged by v1.
- **Targets and Target pages** (kept, re-scoped by the CEO): a target's recent runs as a pass/fail strip, to spot flakiness fast (P8). The routes are built.
- **`GET /v1/health`** (composed summary): without it the Overview would count nodes by state in the UI, which parity forbids.
- Cut: the Grid page (TestGrid), the proxy-update row, `/llms.txt`.

## 2. Users and the parity rule

- **Operators:** is the farm healthy, which build is slow or failing, which node is sick, is storage safe.
- **Agents debugging builds** (first-class): why did this action fail, what ran where, what is the command, the log, the history.

**Parity rule, enforced by test.**
- A page reads only the routes it declares (built: `Page { name, reads }`; the `Reader` refuses any other URL before sending it).
- Each page lists the URLs it read as links.
- Every API-derived value on a page is tagged with the JSON pointer it came from (7.2). The UI computes no sum, age, percentage or flake rate the API lacks.
- Prometheus text is never parsed by the UI. A metric that matters gets a JSON field.
- An agent calls the same routes over HTTP with the route's existing auth (the operator token where one is needed); the UI's open access does not change any `/v1` route's gate.

| The UI shows | Route that carries it (agents call the same) |
|---|---|
| Build list, build summary, banners | `/v1/invocations`, `/v1/invocations/{id}` |
| Failed actions, attempts, stderr tail | `/v1/invocations/{id}/failures` (N2) |
| One action, its attempts | `/v1/invocations/{id}/actions/{digest}`, `/v1/runs/{id}` |
| A full log | `/v1/runs/{id}/logs/{stream}`, `/v1/blobs/{digest}` |
| A target's runs, flakiness strip | `/v1/shapes`, `/v1/shapes/{key}`, `/v1/shapes/{key}/runs`, `/v1/tests` |
| Nodes, node detail, a node's failures | `/v1/nodes`, `/v1/nodes/{id}` (N5), `/v1/runs?node=` |
| Leases, queue | `/v1/leases` (N3), `/v1/queue` (N4) |
| Storage health | `/v1/members`, `/v1/storage` (N7), `/v1/object-stores` |
| Overview | `/readyz`, `/v1/health` (N6) |
| Updates (built) | `/v1/update`, `/v1/nodes`, `/v1/update/hosts` |

## 3. Pages

Freshness: **static** (loaded once) or **refresh N** (`?refresh=N`, 3.1). All paths are under `/ui`. Marks: **built** (on `main`, in `kbf-ui`; all thirteen pages). The Freshness column is the design's; the Mark column says what was built.

| Page | Path | Shows | Reads | Freshness | Mark |
|---|---|---|---|---|---|
| Overview | `/ui` | Version, ready and failing checks, nodes by state, queue depth, lease count, under-replicated blobs, last 10 builds. Update state is not in `/v1/health`; the page links `/ui/updates` for it | `/readyz`, `/v1/health`, `/v1/invocations?page_size=10` | refresh 30 | **built**; reads `/v1/health` and `/v1/invocations`, not `/readyz`; refresh is opt-in, off by default |
| Builds | `/ui/builds` | Recent invocations; filters instance, since, until, state; id lookup (GET form) | `/v1/invocations` | static | **built** (the id lookup moved here); filters are state and instance, not since and until |
| Build | `/ui/builds/{id}` | Summary, failed actions first, slowest, banners for `restored`, `rows_capped`, `gap_from`, `complete:false` | `/v1/invocations/{id}`, `.../actions`, `.../failures` | static | **built**; no slowest list, no `restored` or `gap_from` banner, no refresh |
| Action | `/ui/builds/{id}/actions/{digest}` | Attempts, node, timings, exit, reason, wait reason, command arguments and environment names for failed runs, log tails | `.../actions/{digest}`, `/failures`, `/v1/blobs/{digest}` | static | **built** (does not read `/v1/runs/{id}`) |
| Log | `/ui/runs/{run_id}/logs/{stream}` | The first 1 MiB of a log as escaped text (controls and bidi marks inert), a note when the log is cut, link to the attachment download, one-line "unredacted" note | `/v1/runs/{id}/logs/{stream}` (one ranged read) | static | **built**; reads the log route only, not `/v1/blobs/{digest}`; no tail (suffix range) yet; the Action page links each attempt's logs |
| Targets | `/ui/targets` | Shapes and tests, filter by status, each with a strip of its latest outcomes | `/v1/tests`, `/v1/grid` | static | **built** (1.2); needs `--history-objects` |
| Target | `/ui/targets/{key}` | A target's recent runs (last 16) as a pass/fail strip, oldest left, each cell a link to its run; the server's label | `/v1/shapes/{key}`, `/v1/grid` | static | **built** (1.2); the strip is the last builds, not `/runs` |
| Nodes | `/ui/nodes` | Connected, placement, phase, software, update state, `needs_attention`, lease count, booked | `/v1/nodes` | opt-in refresh | **built** |
| Node | `/ui/nodes/{id}` | Capacity, booked, free, leases, recent failed runs (today only) | `/v1/nodes/{id}` | opt-in refresh | **built**; no recent failed runs |
| Leases | `/ui/leases` | Lease, node, age, phase, operation | `/v1/leases` | static | **built**; `?node=` filter |
| Queue | `/ui/queue` | Depth, oldest age, per platform and QoS, why nothing can run it | `/v1/queue` | static | **built** |
| Storage | `/ui/storage` | Members, debts, open writes, under-replicated blobs, flush backlog (7 heartbeat fields, N7), cache, cold store (no lost-blob count: not built) | `/v1/storage` | static | **built**; no cache or cold-store section, `/v1/members` and `/v1/object-stores` are not read |
| Updates | `/ui/updates` | Desired release, releases, host update state | `/v1/update`, `/v1/nodes`, `/v1/update/hosts` | static | **built** |

The id lookup that was once `/ui` is now on Builds (`BUILDS` declares `/v1/invocations` and `/v1/invocations/{id}`). The `kbf-ui` pages are served at `/ui` through the in-process `Source` (R5, #1373); the router in `kbf_ui::respond` is the list of pages (re-derive the marks against it before relying on one).

**The target strip (P8).** Built as one cell per build (the last 16) from `/v1/grid`, newest on the right, so a target that alternates pass and fail is visible without reading a number. A cell is a link to its build with a glyph and a text label naming the outcome (`pass`, `fail`, and any other outcome the route carries, shown as itself); status is never by colour alone. There is no SVG and no read of `/v1/shapes/{key}/runs`. The UI draws what the route returns and computes no flake rate; any "flaky" label is the server's. On the Targets list each row carries the same strip for its latest outcomes.

### 3.1 Rules for every page

- **Absent means not measured.** Show "not measured" or "unknown", never zero. A null under-replicated count is "unknown".
- **Honest limits.** `/v1/runs` and `/v1/actions/{digest}/runs` are today's index only: show `partial` and `covers_from`. The target strip shows how many runs the route returned, so a short strip is not read as a quiet target.
- **Restart notice.** `/v1/nodes`, leases and queue are memory-only: after a restart or swap they are empty until daemons reconnect. These pages show a banner, so an empty farm is not read as an outage (showing the server start time is a follow-up).
- **Target labels.** The label shown is server-derived. A client-supplied `target_id`, commit or branch is shown apart, labelled "reported by client (hint)". `{key}` in `/ui/targets/{key}` is the server's shape key and only a lookup. Nothing is keyed on a query value.
- **Query values are validated server-side.** `refresh` is one of 5, 10, 30, 60 (anything else is 400, not clamped silently). Filters are an enum or a bounded string. Unknown and duplicate parameters are 400.
- **Refresh.** `<meta http-equiv="refresh" content="N">` with only the integer, never a `url=` part; the target is the page's own URL with its filters and `page_token` kept (tested). Built: a page whose read failed does not refresh at all, so tabs do not stampede a new frontend after a swap (the `Retry-After`-plus-jitter interval is not built). Refresh is off by default on every page, including Overview, and offered on Overview, Nodes and Node only.
- **Unredacted.** Lists show ids and shapes, not commands. The failures view shows arguments and environment names (N2), the Action page the log tails, all as recorded. No gate. The Log page carries the one-line "unredacted" note, and the operator guide says it too.
- **Errors.** Rendered from the stable error code only, escaped. `WWW-Authenticate` is never passed on (it would make a browser prompt for credentials on the farm's origin). `Retry-After` is parsed as an integer or date and re-emitted normalised.

## 4. Data sources and the read API

### 4.1 Built

Build pages, history routes, nodes, members, storage, frontend drain, update, update hosts, releases, object-stores, blobs (`inline=text` to 4 MiB), `/healthz`, `/readyz`. Page-size limits differ per route and the UI uses each route's own: the invocation routes allow 1 to 1000 (`MAX_PAGE_SIZE`), the cross-build routes 1 to 200.

History is on only when the farm runs `--history-objects=<URI>`, which needs `--state-store` (Postgres). Left out, nothing is recorded and the history routes (`/v1/tests`, `/v1/grid`, the run routes) are not mounted, so they answer `404`. The pages that read them (Targets, Target, the Log page) turn that `404` into a `200` "History is off on this farm" page that names `--history-objects=<URI>` and `--state-store`; a record the history does not hold (`no_record`, `no_shape`, `no_run`, `no_log`) is still a `404`. `--history-objects` accepts `s3://...` or `file:///dir`, and `file://` with a state store is accepted today (the directory is owned by one process): single frontend only. A multi-frontend on-prem farm uses an S3-compatible store. **Postgres is an operator-provided runtime dependency**: its provisioning, backup and restore belong to the operator, and this design adds none.

### 4.2 New and changed routes (fields in Appendix A)

All GET, JSON, `Cache-Control: no-store`. Durations carry unit suffixes. Links are root-relative. Built field names are never renamed. Unknown or duplicate query parameters answer 400. `page_token` is route and filter bound.

- **N2 `GET /v1/invocations/{id}/failures`**: built (#1377); the route table in `docs/api.md` ("The failures view") is the authority, and Appendix A lists the fields the built route adds to `invocations-api.md` 4.7.
- **N3 `GET /v1/leases`**, **N4 `GET /v1/queue`**: new scheduler views.
- **N5 `GET /v1/nodes/{id}`**: new GET on the existing `/v1/nodes/{target}` route (today `post(act)`, whose `{target}` takes `:cordon`, `:drain`, `:uncordon` suffixes). The GET must refuse an id with a suffix.
- **N6 `GET /v1/health`**, **N7 `GET /v1/storage` extended**.
- Nothing else: no series, events or `/v1/storage/map` (those wait). N1 (`GET /v1`, a route index) is not in the v1 unit list.

### 4.3 No route table in v1

The proposal's single route table, `GET /v1` index and per-route access oracle (R1) existed to gate viewer reads. With no viewer gate there is nothing for it to prove, and it is dropped from v1 (a later unit if agents want a route index). Access is tested where it matters: the `/ui` mount serves GET and HEAD only and reaches only declared routes, and existing write and operator routes still refuse a request without the operator token (7.3).

### 4.4 Limits the UI shows, not hides

- Build restore: at most 50,000 records, then `rows_capped` and `complete:false`.
- `gap_from` and `restored` on invocations. A `404 no_record` carries `records_since` (built in `http_invocations.rs`), and the page says "no record held; records are held since X".
- Unsupported filters answer 400 (`needs_report`, `unsupported_filter`); the UI does not offer them (no commit, branch, tool or target-text filter).
- Read budget: the blob route's one shared 1 GiB budget stays; on top of it each client has at most 4 reads open (5.3). A 503 or 429 renders as a page with the wait, never retried silently.

## 5. Serving (no auth)

### 5.1 Where it is served

- `kbf-ui` becomes a normal dependency of `kbf-server` (a dev-dependency today) and is mounted on the operator router (`--api-listen`, loopback) as `GET /ui/*`, enabled by `--ui`, **on by default** (`--ui=false` turns it off, and `/ui` is then a 404 at the child). With `--api-listen` off there is nothing to mount on. No new port, no TLS listener in `kbf-server`.
- `kbf-proxy` (#1368) gets a new `Route::Ui` for the prefix `/ui`, with the open class `AuthClass::Ui` that admits any caller. It is forwarded to the operator listener (`proxy_child(Target::Operator)`) and is held during a swap like any child route (`bypasses_hold` is false). GET and HEAD only: any other method is 405 with `Allow`. A path that is not plain segments (`..`, `.`, an empty segment, an encoded dot, slash or backslash) is `Route::Unknown`, a 404. The proxy config `ui = false` makes `/ui` a 404 at the proxy; it is a separate switch from the server's `--ui=false` (404 at the child). Every other class keeps its decision unchanged: `Operator`, `Update`, `Reapi`. The public port stays the only public listener, in all-in-one as well. The proxy never injects the operator token.
- **In-process `Source`.** The UI handler does not call out over HTTP. `InProcess` (#1373) calls a second router over the same state, with an all-allow policy (`open_policy()`: `invocations_get`, `blobs_get` and `farm_get` allow), and sends only GET. Its requests carry `ConnectInfo(127.0.0.1:0)` and `Trust::Unverified`; there is no extension marker, and no header can forge the open policy because the open-policy router is not served on any socket: it is reachable only through `InProcess`. The network routes keep their policy gates. The UI carries no caller identity and no instance filter: it shows every instance. Consequence: the UI shows blobs and failures that the network route refuses when `--reapi-auth-policy` names no authorizer for them. Parity is therefore stated as: the UI shows nothing an agent with the token could not ask for, under the same policy.
- **Direct loopback.** A local process reaching `--api-listen` can load `/ui` too; the same page is served. No `Host` allowlist (nothing is gated by it).
- **Browser-trusted server name.** Browsers must trust the public name's TLS certificate: the operator supplies one in the proxy's SNI list (existing mechanism; the farm-CA certificate stays for the internal name). TLS minimum 1.2. The proxy's optional client-certificate request stays; `/ui` needs none.
- `ui-data.md` loses `--read-listen`, its TLS flags and `--read-listen-insecure` (done in this change).

### 5.2 What stays as it is

REAPI, worker, storage and enroll gates, the operator token on write and operator routes, `/metrics`, and `Route::Update` do not change. No new role, certificate, SAN, installer verb, deny-list or class is added. Opening `/ui` does not open `/v1/*` over the proxy: an agent keeps calling `/v1` with the token the route needs today.

### 5.3 Read-only enforcement and browser defences

- **Read-only.** The `/ui` router accepts GET and HEAD; anything else is 405 (a test through the proxy covers POST, OPTIONS, HEAD). No form posts: the id lookup uses GET. A page-contract test refuses `method=post` and any non-GET link.
- **Read limiter** (built in #1373, `read_limit.rs`). Each client has at most 4 reads open at once; the 5th is `429` with `Retry-After: 1`, answered before its handler runs. A slot is held until the answer's body is sent or dropped (a blob streams after its handler returns). A client is the node id the proxy named, else the client address; the proxy forwards the client address to the child (`x-kbf-client-addr`, built) so browsers are limited one by one. `/ui` and the `/v1` read routes have separate limits, so a page cannot starve the API or the reverse. The `/v1` read routes (blobs, invocations, runs, `update/hosts`) now answer the same 429, a behaviour change for agents. Path checks: a path over 2048 bytes or a query over 4096 is `414`; a segment over 256 bytes, or one that decodes to a control character, is `400`. The 25 percent budget share and the ranged-log cap are not built.
- **No cookies, no credentials.** The UI sets no cookie and reads none, so there is nothing to forge. No `Access-Control-Allow-*` header is ever sent.
- **Headers on every `/ui` answer, errors and 405 included** (built in #1373 and #1420, `ui_mount.rs`): `Content-Security-Policy: default-src 'none'; style-src 'self'; img-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'` (`kbf_ui::CONTENT_SECURITY_POLICY`; no script of any kind, `default-src` covers `script-src`), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` (ids sit in URLs) and `Cache-Control: no-store` (pages carry logs). `Cross-Origin-Resource-Policy: same-origin`, `Cross-Origin-Opener-Policy: same-origin` and a `Permissions-Policy` denying the powerful features are set the same way (`ui_mount.rs`). **Not built, follow-up unit:** HSTS on the proxy (TLS ends there), and the security headers on the proxy's own pages (the 503 handover page); #1368 adds no headers.
- **XSS.** `Html` escapes every string and has no raw write. Logs are untrusted: escaped text in `<pre>` only. Every emitted href is root-relative, with ids and `page_token` percent-encoded (asserted). Raw blob, log and flog routes answer `Content-Type: text/plain` or `application/octet-stream`, `Content-Disposition: attachment`, `nosniff` and a sandbox CSP; `inline=text` is always `text/plain`. Invalid UTF-8 is replaced, and a 16 KiB tail cut inside a code point is dropped. This matters more with an open UI: an uploaded HTML blob must never render on the farm's origin.
- **Unredacted is a ruling, not an accident.** Tails and logs are shown as recorded (Q5).
- **Heartbeat data is untrusted.** Storage-node numbers in N7 are bounded and type-checked server-side; a negative or absurd value is dropped to null.

### 5.4 Handover and all-in-one

- During a frontend swap the proxy holds a request up to `hold_max` (20 s frontend, 300 s all-in-one), then answers 503 with `Retry-After`. That answer is a static page produced by the proxy; the UI crate cannot produce it when the child is down. Giving it the security headers is a follow-up unit (not built). Held requests keep the caps they have today.
- Pages rely on the history store, not child memory. In-memory data (nodes, leases, cordons, recorder records) is empty after a swap until daemons reconnect (3.1 banner).
- Proxy update status is `Route::Update`, served by the proxy outside the operator router: the in-process `Source` cannot reach it. v1 has no proxy-update row.

## 6. Tech stack

**Server-rendered HTML, no SPA, no script.**
- Hermetic: no npm, bundler or lockfile. `kbf-ui` has five small dependencies and no async runtime. No new dependency for v1, tests included (7.2).
- The built crate already has escape by construction, the CSP and the declared-reads contract.
- Every page is a plain GET, so agents can fetch it.

Details:
- Typed `html::Html` builder; no template engine. Each page builds a typed model first, then renders it.
- One embedded stylesheet (`include_str!`) at `/ui/static/kbf.css` with a content hash in the URL. No fonts, images, CDN or `style` attributes; status marks and the target strip are inline SVG with classes. Light and dark by `prefers-color-scheme`, from CSS custom properties.
- Accessibility: one `h1`, landmarks, `th scope` and `caption`, a `label` on every input, `lang` and `title`, no positive `tabindex`, status never by colour alone.
- Tables: stable column order, plain-text numbers. Times are UTC with the raw Unix ms in a `data-ms` attribute. "Next" links carry `page_token`.
- One module per page, each under 1000 lines.

## 7. Testing

All tests are Rust. No shell, Python or JavaScript. No browser runs in v1, so there is no browser fetch, container or download in the build.

### 7.1 Layer 1: `kbf-ui` unit tests, fake `Source`

- **The fake is typed.** Fixtures are built from the server's own serde types (shared through an `api-types` module that both the server and `kbf-ui` tests use), or checked against recorded real responses, so a field rename breaks Layer 1. Today `kbf-ui` parses its own shapes in `api`; R5 adds a test that every fixture round-trips through the server type.
- **Contract:** every page reads only its declared routes. Catches a hidden route (breaks agent parity).
- **Escape, generated:** set every string field of every fixture to a payload corpus and assert no raw `<` from data reaches any page. The field list is generated from the types, so a new field cannot skip it. Corpus: `<script>`, attribute breakout (`"><img onerror=...>`), `javascript:` and `data:` hrefs, CRLF, NUL, ANSI escapes, bidi overrides.
- **Honesty, table driven:** rows of (JSON flag or status, banner): `partial`, `complete:false`, `rows_capped`, `gap_from`, `restored`, null-is-unknown, `no_record` with `records_since`, history off with the flag name, 400 (unknown parameter), 503 and 429 with `Retry-After`, restart notice, a short target strip. Catches silent zeros.
- **Target strip:** a fixture of alternating pass and fail renders alternating cells in run order with text labels; an unknown outcome renders as itself, not as pass.
- **Static page rules:** no inline script, `style` attribute, event handler or external URL on any page or in the CSS; single `h1`, landmarks, `th scope`, labels, `lang`, `title`; every href root-relative; no non-GET link or form. The CSP header is compared with the page's real resource use.
- **Contrast** of the CSS token pairs, light and dark. This checks tokens, not rendered output. Accepted residual risk: layout, rendered contrast and meta-refresh behaviour are not checked by any gating test.
- **Edge content:** 4 MiB inline log, ranged reads, binary and invalid UTF-8, a very long command line, page size at 1, the maximum and maximum plus 1, paging token reused after a filter change (400).
- **Determinism:** an injectable clock in the `Source` (ages, start-time banner, refresh); lease ids are never asserted literally.

### 7.2 Layer 2: parity and shape tests over a real server (`kbf-server`, TCP, as `tests/ui_pages.rs` does)

Page to route to contract test, every built route included:

| Route | Contract test |
|---|---|
| `/v1/invocations`, `/{id}`, `/actions`, `/actions/{digest}` | shape (field names, units, nullables), 400 on unknown parameter, paging token refused on another filter |
| `/v1/runs`, `/v1/runs/{id}`, logs, `/v1/blobs/{digest}` | shape, `partial` and `covers_from`, range bounds, raw-route headers |
| `/v1/shapes*`, `/v1/tests` | shape, status filter enum |
| `/v1/nodes`, `/v1/members`, `/v1/object-stores`, `/v1/update*` | shape; secret-field allowlist (below) |
| each new route N2 to N7 | shape against this document, unit suffixes, 400 on unknown or duplicate parameter |

- **Parity method.** The page tags every API-derived value with `data-src="<route>#<json pointer>"`. The test (a) resolves each tag in the JSON the page read and compares the formatted value; (b) walks every other text node containing a digit and requires it to be on an allowlist of static labels, so an untagged number (a UI-computed sum, age or percentage) turns it red. The extractor is a small tokenizer over the page's own output, safe because all text is escaped. No `scraper` crate.
- **Secret sentinel (infrastructure secrets only).** Everything the farm recorded is shown, so a sentinel in argv or stderr is expected on the page. Plant a sentinel in what the UI must never show: an object-store connection credential, the operator token, the proxy secret, an `x-kbf-*` header value; assert it is absent from every page and JSON the UI reads. `/v1/object-stores`, `/v1/update`, `/v1/update/hosts` and `/v1/storage` use a field allowlist, so no endpoint, URI credential or key can be returned.
- **Mount.** `--ui` on by default: a server started with no flag serves `/ui`; `--ui=false`: 404. A write attempt through the mount (POST, any method but GET and HEAD): 405. A request through the mount for an undeclared route: refused. The open-policy router is reachable only through `InProcess`, which sends GET; forged `x-kbf-*` headers are ignored.

### 7.3 Layer 3: black-box through the real proxy

Reuses `kbf-integration-test`'s `proxy_support` (proxy at library level, real `kbf` frontend children from this test binary, test PKI, `reqwest`). It needs no service unless history is under test: then Postgres is the CI service container pinned by digest (`KBF_TEST_POSTGRES_URL`), history objects are a `file://` directory (single frontend; accepted today), and the test is `#[ignore]`d like the other Postgres tests. Linux hosted CI only; macOS developers run Layers 1 and 2 and the service-free part of Layer 3. Budget: 5 minutes. Ports are ephemeral, PKI validity is long, and every wait is a poll with a deadline, never a sleep.

**Minimum for R-T1 (the gate):** `/ui` served with no client certificate and no token, by default (no `--ui` flag); `/ui` 404 with `--ui=false`; h1 and h2 both served; POST, OPTIONS and HEAD behave (405, 405, served); the built security headers of 5.3 present on a page (the proxy's 503 page and HSTS join when their unit lands); no token, proxy secret or `x-kbf-*` reflected in a page; the existing gates unchanged through the proxy: a write route and an operator `/v1` route without the operator token are refused, a `daemon` or `storage` certificate cannot use the REAPI class beyond what it could before; HTML uploaded as a blob comes back as an attachment (headers built in R2b, checked here through the proxy).

**Later, as pages land:** a real build with one failing action, its stderr tail on the Build page, the node, lease and queue pages, a target strip over two builds of one flaky target; the swap test (`hold_max` shortened by a test flag, never waited in real time). Each page unit adds a fetch of its page through the proxy, so no page merges with only Layers 1 and 2.

**Mutants** (planted locally, never pushed): `/ui` mapped to a stricter or looser class than designed, `/ui` admitting a POST, the open-policy router served on a socket or reached by a non-GET request, drop one `declare`, remove one escape, drop one banner, widen the CSP, pass `WWW-Authenticate`, `ui` default flipped to off, drop `attachment` on a raw route, strip cells out of run order. Each PR body records the patch name and the named test that went red.

### 7.4 CI placement

Layers 1 and 2 run in the normal PR check. Layer 3 runs in the PR check for a change to `kbf-ui`, `kbf-proxy`, `kbf-server`, `kbf-installer`, `kbf-integration-test` or the API types, with a cheap smoke subset (`/ui` open by default, `--ui=false` 404, write route refused) on every PR; the full suite always runs in the beta pipeline. Postgres is the metadata store everywhere history is used; the SQLite phases in `ui-data.md` are rewritten to Postgres only (R0, with the `ui-data.md` follow-up).

### 7.5 No browser tests in v1

Pages have no script, so a browser adds little over Layers 1 to 3. Revisit with charts. Not part of this design.

## 8. Rollout

On by default: `--ui` defaults on and the installer's proxy config `ui` defaults on (set `ui = false` to turn it off). R3 (the read limiter) ships inside #1373 together with R5, and the R2b raw-route header code already existed on `feature/arch-v2`, so neither is a separate gate before R4. One concern per PR; each is mergeable alone with its own proof.

| Unit | Content | Proof |
|---|---|---|
| R0 | Docs: this amendment; `ui-data.md` serving and auth text rewritten to it. Still open as follow-ups: SQLite text to Postgres only, `phase` in `api.md`, the `file://` text, page list in `ARCHITECTURE.md` | Review; each built or planned mark checked against code |
| R2b | Raw-route headers (5.3) on the built `/v1/blobs`, `/v1/runs/{id}/logs` and flog routes: `text/plain` or `application/octet-stream`, `Content-Disposition: attachment`, `nosniff`, sandbox CSP; `inline=text` always `text/plain`. Prerequisite of R4 | Test `raw_routes_serve_attachment_text_plain` in `kbf-server` (uploaded HTML blob, log and flog: headers asserted; mutant: drop `attachment`) |
| R3 | Read limiter (#1373): 4 concurrent reads per client, 429 `Retry-After: 1`, separate `/ui` and `/v1` limits, path validation (414, 400). Not built: 25 percent budget share, ranged-log cap | Budget starvation test; bound tests |
| R4 | Proxy (#1368): `Route::Ui` with open `AuthClass::Ui`, GET and HEAD, `ui` proxy-config setting default on. Not built: security headers on proxy pages (503 handover page), HSTS, the public-name SNI certificate setting. **Independent security review** | Route and class unit tests; every other class unchanged; installer default-on test; mutant: `/ui` admits POST |
| R5 | Server (#1373): mount `kbf-ui`, `InProcess` `Source` over an open-policy router, `--ui` default on, GET-only router, CSP, nosniff, Referrer-Policy and no-store headers (CORP, COOP and `Permissions-Policy` followed in #1420), CSS route. **Independent security review** | Layer 2 mount tests; R-T1 smoke |
| R6 | `/v1/invocations/{id}/failures` | Shape, failed-run fixture, escape |
| R7 | `Scheduler::snapshot()` and `/v1/leases` | `kbf-sim` first; shape |
| R8 | `/v1/queue` | Shape; per platform and QoS |
| R9 | `/v1/nodes/{id}`, `lease_count` and `booked` on `/v1/nodes` | Shape; suffix refused |
| R10 | Storage health through the heartbeat: fields (`kbf-proto`), `/v1/storage` extension | Null-is-unknown; values recover after reconnect; mixed-version compatibility |
| R11 | `/v1/health` | Same values as `/readyz` and the source routes |
| P1 | Builds and Build (the move off `/ui`) | Layers 1, 2 and a proxy fetch |
| P2 | Action and Log (both built; the log's tail by suffix range is a follow-up) | same |
| P3 | Nodes and Node | same |
| P4 | Leases | same |
| P5 | Queue | same |
| P6 | Storage | same |
| P7 | Overview, refresh option | same |
| P8 | Targets and Target: a target's recent runs as a pass/fail strip to spot flakiness | same, plus the strip tests (7.1) |
| R-T1 | Black-box minimum (7.3), through the real proxy | Layer 3 green; mutants red |
| D1 | Operator guide ([deploy/kbf-ui.md](../deploy/kbf-ui.md), built): what the UI is, reaching it, turning it off (`[proxy] ui = false`, `--ui=false`), the open and unredacted exposure, the read bound, the pages. Not in it: the public-name certificate (not built) and Postgres as an operator-provided dependency | Claims checked against `main`; not yet followed on a clean cell |

Dropped, not rolled out: R1, R2, R4a, R4d, and the viewer parts of R4b and R4c (Sec-Fetch-Site refusal; the held-request caps stay as built).

Order: R4 and R5 (with R3) in either order; R-T1 after R5; routes R6 to R11 any time; each page unit after its routes and R5. v1 is done when D1 is merged.

## 9. Questions, answered

- **Q1. Browser credential.** Ruled: none. The UI is open; no certificate import, no login. Revisit when the farm is exposed beyond a trusted network.
- **Q2. What a viewer may read.** Ruled: moot, there is no viewer. `/ui` reads the declared routes in-process; `/v1` over HTTP keeps its existing gates and agents use the token those routes already need.
- **Q3. Storage-node health.** Ruled: through the heartbeat and `/v1/storage` (a `kbf-proto` field addition, no new listener, one JSON source). The wire change must stay compatible across a mixed-version farm (R10).
- **Q4. Default of `--ui`.** Ruled: on by default, in the server and in the installer's proxy config.
- **Q5. Unredacted logs and command arguments.** Ruled: everything unredacted, with a one-line note; no raw-Command gate, no `logs` authority.
- **Q6. Beyond the assignment (1.2).** Ruled: keep Targets and Target (re-scoped to the flakiness strip) and `/v1/health`; the Grid is cut, as are the proxy-update row and `/llms.txt`.

## 10. Risks

Independent security review before R4 and R5 merge: the check is that the new open surface reaches nothing beyond the declared GET reads.

- **Open and unredacted by ruling.** Anyone who can reach the public port reads every build, log and failed-command argument, and a secret passed as `--token=...` is readable. Mitigation: the one-line note, the operator guide (D1), and the operator's network boundary. Owner: the CEO accepted this.
- **The in-process `Source` widens a read path.** The open-policy router would be a hole if it were served on a socket or reached with a write. Mitigation: it is reachable only through `InProcess`, which sends GET. The UI also shows blobs and failures the network route refuses when `--reapi-auth-policy` names no authorizer for them.
- **Open surface and the read budget.** One shared 1 GiB budget lets any page load starve agents. Mitigation: the per-client read limiter (R3, in #1373).
- **Uploaded content rendered on the farm's origin.** Mitigation: the raw-route headers (R2b), built before the mount.
- **Tenancy is not a boundary.** The UI shows every instance; do not host mutually untrusting tenants on one farm (1.1).
- **Scheduler snapshot cost.** A copy under the scheduler lock could slow scheduling. Mitigation: bounded copy, page cap, farm benchmark in R7. The by-platform and per-lease age and operation data need new scheduler fields, not only a snapshot of `waiting`, `avoided`, `queued_len`, `leases_held`, `queued`, `leases_on`.
- **Empty farm after a swap.** Mitigation: the start-time banner. Moving node and lease state into the state store is a later design.
- **Partial history.** Today-index routes are partial and day-close does not run. Mitigation: show `covers_from`, promise nothing older; the target strip shows its run count.
- **Doc and code drift.** `ui-data.md` lists built routes as new. Mitigation: its serving text now defers to this document; the remaining follow-ups are in R0.
- **Test cost.** Layer 3 is slow. Mitigation: smoke on every PR, full suite in beta.

## Appendix A: new route fields

Shared: every list carries `next_page` (root-relative) and takes `page_token`, `page_size` (1 to 200, default 100, unless stated).

**N2 failures.** As `invocations-api.md` 4.7: `{invocation{...,links{failures,...}}, failures[{action, href, target, mnemonic, outcome, failure, attempts[{lease, worker, queued, granted, started, ended, end, run_id, href}], command{arguments, environment_names, working_directory, platform, timeout_micros, output_paths}, stderr, stdout{digest, bytes, available, tail, tail_bytes, truncated, href}, outputs, usage}], next_page_token, next_page}`. Tails are 16 KiB. `links` gains `failures`. Built (#1377) beyond this shape: `instance`, `failure{class, reason}` (null when the last attempt did not end in a failure), `command_available`, `command.truncated`, and `tail_omitted` on a log object with no tail (`not_held`, `too_large`, `unreadable`, `blobs_off`). Over HTTP the command and tails are read only when the policy names `blobs.getAuthorizer`; otherwise `tail_omitted` is `blobs_off`. `page_size` 1 to 100, default 20. The authority is `docs/api.md`.

**N3 `GET /v1/leases?node=`** (#1371; authority `docs/api-scheduler.md`). `{term, items[{lease, node, phase, age_ms, since_ms, granted_unix_ms, started_unix_ms, operation, instance, digest, platform, qos}], next_page}`. `term` is a string (a 64-bit number a JSON reader would round). `lease` is `term.seq`, not stable across a restart. There is no `invocation` field. `page_size` 1 to 200; an unknown or repeated parameter is 400; `node` filters (a node the scheduler does not know has none: empty list, 200).

**N4 `GET /v1/queue`** (#1371). No query. `{depth, oldest_age_ms, by_platform[{platform, qos, depth, oldest_age_ms}], waiting[{reason, count, example_operation, example_instance, example_digest}], avoided[{operation, instance, digest, nodes[]}], avoided_total}`. `avoided` is at most 100 entries, `avoided_total` counts all. `oldest_age_ms` is null when nothing waits. `GET /v1/frontend/drain` keeps its three fields.

**N5 `GET /v1/nodes/{id}`** (#1377). All fields of the list entry plus `capacity{cpu_millis, memory_bytes, gpus, vms}`, `free` (`capacity` less `booked`, floored at 0) and `leases[]` (`term.seq` strings); `lease_count` and `booked` are also on the entries of `/v1/nodes`. There is no `recent_failures`: a node's recent failed runs are `GET /v1/runs?node=`. 404 `{"error":"no_node"}` for an unregistered node, for an id ending in `:cordon`, `:drain` or `:uncordon`, and for any `/v1/nodes/{id}/...` sub-path.

**N6 `GET /v1/health`** (#1377). `{ready, failing[{check,reason}], nodes{total,serving,cordoned,draining,drained,quarantined,disconnected}, queue{depth,oldest_age_ms}, leases, storage{members_active,members_total,under_replicated_blobs}}`. `storage` is null with `storage_unavailable` when the membership cannot be read. `queue.depth` and `queue.oldest_age_ms` come from one scheduler snapshot, the one `/v1/queue` shows (`oldest_age_ms` is null only when nothing waits). There is no `update{state}` and no `history{...}`: read `/v1/update` for update state. Built from in-process values; `/readyz` stays the probe. The TLS leaf expiry is not included: the proxy owns the leaf and the child has no source for it.

**N7 `/v1/storage` extended** (#1370). Per node, seven fields: `unflushed_bytes`, `unflushed_blocks`, `oldest_unflushed_ms`, `budget_bytes`, `used_bytes`, `flush_failures`, `flush_conflicts`. Each is a heartbeat field 17 to 23 in `kbf-proto`, an optional `uint64`. Null means unknown: a stale report, no heartbeat since registration, no write tier, an older build, or a value above 2^53 - 1. There is no `lost_blobs` (not built).
