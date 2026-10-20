# kbf UI v1: design

Status: proposal, for approval before any code. Read-only web UI for operators and agents. Facts below were read on `feature/arch-v2`; "built" means the code there, "new" means not built. Where this document and `ui-data.md` differ on serving, access or tests, this one wins; R0 rewrites the other.

## Summary (decide from this screen)

- **Scope.** Builds, one build's failed actions with stderr, the action and its log (from the history store); nodes, leases and the queue; storage health. Read-only. Beyond the five asked areas and listed in 1.2: the Overview and Updates pages, target history pages and a composed `/v1/health`.
- **Parity.** Every page reads only declared `/v1` JSON routes and lists them as links. `GET /v1` is the route index. The UI shows no number the API lacks, and a test proves it (7.2).
- **Auth.** Behind the farm's own access: the one public port, owned by `kbf-proxy`, with a new `viewer` mTLS client-certificate role. No login page, cookie or session; GET only; the UI never holds the operator token. Cost: browsers must import a certificate (5.2, Q1).
- **Stack.** Server-rendered HTML from the built `kbf-ui` crate (typed builder, escape by construction), mounted in `kbf-server`. No script, no npm, no new dependency (6).
- **Tests.** All Rust. Unit tests over a fake `Source` built from the server's own types; parity and shape tests over a real server; a black-box suite through the real proxy with real certificates (7).
- **Size.** About 25 PRs, one concern each (8). Two are security-critical (the `viewer` role in the proxy, the in-process `Source`) and need an independent security review before merge.
- **Decisions needed.** Q1 to Q6 (9).

## 1. Goal and non-goals

### 1.1 Goal and non-goals

**Goal.** A read-only web UI on the farm's one public port. Every number on a page also comes from a documented JSON route an agent can call.

**Non-goals for v1:** write actions (cordon, drain, update button, retry; kbf#992); SSO or OIDC; login pages, sessions, cookies; multi-tenancy (7 of `ui-data.md` stands: one farm is one trust domain). **Tenancy posture, used throughout:** per-instance gating is defence in depth, not a tenant boundary; any viewer who learns a digest can read the blob; ids outside the caller's instance scope return 404. `/v1/blobs/{digest}` is authorized by the viewer role plus the route allowlist (5.2), with no per-instance check; charts and series; storage map and handoff views; a cloud-hosted UI; browser-driven tests (7.5).

### 1.2 Beyond the assignment: approve or drop

- **Overview** (beyond the five asked areas; in scope, proposed): the one-screen answer to "is the farm healthy". It adds no data of its own, it reads `/readyz`, `/v1/health` and the build list. Droppable: P7.
- **Updates** (beyond the five asked areas; in scope, built): the page already exists in `kbf-ui` and reads built routes only, so keeping it costs nothing and dropping it is a deletion. Unchanged by v1.
- **Targets and Target pages** (shape and test history, flaky proof). Reason: agents debugging a flaky test need it, and the routes are built. Droppable: last page units (P8).
- **`GET /v1/health`** (composed summary). Reason: the Overview would otherwise count nodes by state in the UI, which parity forbids. Alternative: the Overview shows only `/readyz`, `/v1/queue` and `/v1/nodes` as-is.
- Cut from the earlier draft: the Grid page, the proxy-update row, `/llms.txt`.

## 2. Users and the parity rule

- **Operators:** is the farm healthy, which build is slow or failing, which node is sick, is storage safe.
- **Agents debugging builds** (first-class): why did this action fail, what ran where, what is the command, the log, the history.

**Parity rule, enforced by test.**
- A page reads only the routes it declares (built: `Page { name, reads }`; the `Reader` refuses any other URL before sending it).
- Each page lists the URLs it read as links.
- Every API-derived value on a page is tagged with the JSON pointer it came from (7.2). The UI computes no sum, age or percentage the API lacks.
- Prometheus text is never parsed by the UI. A metric that matters gets a JSON field.
- Agents get `GET /v1` (N1). The failure bundle (N2) has the same authority gating as the page, because the page reads it.

| The UI shows | Route that carries it (agents call the same) |
|---|---|
| Build list, build summary, banners | `/v1/invocations`, `/v1/invocations/{id}` |
| Failed actions, attempts, stderr tail | `/v1/invocations/{id}/failures` (N2) |
| One action, its attempts | `/v1/invocations/{id}/actions/{digest}`, `/v1/runs/{id}` |
| A full log | `/v1/runs/{id}/logs/{stream}`, `/v1/blobs/{digest}` |
| Shape and test history | `/v1/shapes`, `/v1/shapes/{key}`, `/v1/shapes/{key}/runs`, `/v1/tests` |
| Nodes, node detail, a node's failures | `/v1/nodes`, `/v1/nodes/{id}` (N5), `/v1/runs?node=` |
| Leases, queue | `/v1/leases` (N3), `/v1/queue` (N4) |
| Storage health | `/v1/members`, `/v1/storage` (N7), `/v1/object-stores` |
| Overview | `/readyz`, `/v1/health` (N6) |
| Updates (built) | `/v1/update`, `/v1/nodes`, `/v1/update/hosts` |

## 3. Pages

Freshness: **static** (loaded once) or **refresh N** (`?refresh=N`, 3.1). All paths are under `/ui`. Marks: **built** (in `kbf-ui` today, four pages), **new**, **moved**.

| Page | Path | Shows | Reads | Freshness | Mark |
|---|---|---|---|---|---|
| Overview | `/ui` | Version, ready and failing checks, nodes by state, queue depth, lease count, under-replicated blobs, update state, last 10 builds | `/readyz`, `/v1/health`, `/v1/invocations?page_size=10` | refresh 30 | new, **moves** the built id lookup |
| Builds | `/ui/builds` | Recent invocations; filters instance, since, until, state; id lookup (GET form) | `/v1/invocations` | static | the built `/ui` page moves here |
| Build | `/ui/builds/{id}` | Summary, failed actions first, slowest, banners for `restored`, `rows_capped`, `gap_from`, `complete:false` | `/v1/invocations/{id}`, `.../actions`, `.../failures` | refresh while `open` | built, extended |
| Action | `/ui/builds/{id}/actions/{digest}` | Attempts, node, timings, exit, reason, wait reason, command arguments and environment names for failed runs, log tails | `.../actions/{digest}`, `/v1/runs/{id}` | static | built, extended |
| Log | `/ui/runs/{run_id}/logs/{stream}` | Whole log as escaped text, ranged, link to the raw route | `/v1/runs/{id}/logs/{stream}`, `/v1/blobs/{digest}` | static | new |
| Targets | `/ui/targets` | Shapes and tests, filter by status | `/v1/shapes`, `/v1/tests` | static | new (1.2) |
| Target | `/ui/targets/{key}` | Last 16 runs, flaky proof, server-derived label | `/v1/shapes/{key}`, `/v1/shapes/{key}/runs` | static | new (1.2) |
| Nodes | `/ui/nodes` | Connected, placement, phase, software, update state, `needs_attention`, lease count, booked | `/v1/nodes` | refresh 10 | new |
| Node | `/ui/nodes/{id}` | Capacity, booked, free, leases, recent failed runs (today only) | `/v1/nodes/{id}`, `/v1/leases?node=`, `/v1/runs?node=` | refresh 10 | new |
| Leases | `/ui/leases` | Lease, node, age, phase, operation | `/v1/leases` | refresh 10 | new |
| Queue | `/ui/queue` | Depth, oldest age, per platform and QoS, why nothing can run it | `/v1/queue` | refresh 10 | new |
| Storage | `/ui/storage` | Members, debts, open writes, under-replicated blobs, flush backlog, cache, lost blobs, cold store | `/v1/members`, `/v1/storage`, `/v1/object-stores` | refresh 30 | new |
| Updates | `/ui/updates` | Desired release, releases, host update state | `/v1/update`, `/v1/nodes`, `/v1/update/hosts` | static | built, unchanged |

The built `/ui` is the Builds id lookup and `BUILDS` reads only `/v1/invocations/{id}`. Moving it changes that `Page` declaration and its tests (P1). `PAGES` has four entries and no mount exists.

### 3.1 Rules for every page

- **Absent means not measured.** Show "not measured" or "unknown", never zero. A null under-replicated count is "unknown".
- **Honest limits.** `/v1/runs` and `/v1/actions/{digest}/runs` are today's index only: show `partial` and `covers_from`.
- **Restart notice.** `/v1/nodes`, leases and queue are memory-only: after a restart or swap they are empty until daemons reconnect. These pages show the server start time and a banner, so an empty farm is not read as an outage.
- **Target labels.** The label shown is server-derived. A client-supplied `target_id`, commit or branch is shown apart, labelled "reported by client (hint)". `{key}` in `/ui/targets/{key}` is the server's shape key and only a lookup; every row is filtered by the caller's instance authority. Nothing is keyed on a query value.
- **Query values are validated server-side.** `refresh` is one of 5, 10, 30, 60 (anything else is 400, not clamped silently). Filters are an enum or a bounded string. Unknown and duplicate parameters are 400.
- **Refresh.** `<meta http-equiv="refresh" content="N">` with only the integer, never a `url=` part; the target is the page's own URL with its filters and `page_token` kept (tested). On a 503 or 429 page the interval is at least `Retry-After` plus jitter, so tabs do not stampede a new frontend after a swap. Default off, except Overview (30).
- **Lists show ids and shapes, not commands.** The failures view shows arguments and environment names (N2, Q5). Environment values and the raw Command blob stay behind `commands.getRaw`, a gate that is planned and not built (`ui-data.md` Q19): R2 builds it before `/v1/blobs` is opened to viewers.
- **Errors.** Rendered from the stable error code only, escaped. `WWW-Authenticate` is never passed on (it would make a browser prompt for credentials on the farm's origin). `Retry-After` is parsed as an integer or date and re-emitted normalised.

## 4. Data sources and the read API

### 4.1 Built

Build pages, history routes, nodes, members, storage, frontend drain, update, update hosts, releases, object-stores, blobs (`inline=text` to 4 MiB), `/healthz`, `/readyz`. Page-size limits differ per route and the UI uses each route's own: the invocation routes allow 1 to 1000 (`MAX_PAGE_SIZE`), the cross-build routes 1 to 200.

History is on only when the farm runs `--history-objects=<URI>`, which needs `--state-store` (Postgres). Left out, nothing is recorded and the history pages answer "history is off" naming the flag. `--history-objects` accepts `s3://...` or `file:///dir`, and `file://` with a state store is accepted today (the directory is owned by one process): single frontend only. A multi-frontend on-prem farm uses an S3-compatible store.

### 4.2 New and changed routes (fields in Appendix A)

All GET, JSON, `Cache-Control: no-store`. Durations carry unit suffixes. Links are root-relative. Built field names are never renamed. Unknown or duplicate query parameters answer 400. `page_token` is route and filter bound.

- **N1 `GET /v1`**: route index with `gate` per route. Built from the router's own table (4.3).
- **N2 `GET /v1/invocations/{id}/failures`**: specified in `invocations-api.md` 4.7. This document adopts that shape unchanged; R0 only sets its page-size cap (1 to 100, default 20).
- **N3 `GET /v1/leases`**, **N4 `GET /v1/queue`**: new scheduler views.
- **N5 `GET /v1/nodes/{id}`**: new GET on the existing `/v1/nodes/{target}` route (today `post(act)`, whose `{target}` takes `:cordon`, `:drain`, `:uncordon` suffixes). The GET must refuse an id with a suffix.
- **N6 `GET /v1/health`**, **N7 `GET /v1/storage` extended**.
- Nothing else: no series, events or `/v1/storage/map` (those wait).

### 4.3 The route table is the registration

Axum cannot list its own routes. So the server builds its router from one table (`method, path, query, gate, summary`) and `GET /v1` serves that table. A route registered outside the table fails a test that diffs the table against `Router` behaviour by probing each entry. Off routes (flag off, policy absent) are listed with the reason. The `gate` column is also the test oracle for access (5.5).

### 4.4 Limits the UI shows, not hides

- Build restore: at most 50,000 records, then `rows_capped` and `complete:false`.
- `gap_from` and `restored` on invocations. A `404 no_record` carries `records_since` (built in `http_invocations.rs`), and the page says "no record held; records are held since X".
- Unsupported filters answer 400 (`needs_report`, `unsupported_filter`); the UI does not offer them (no commit, branch, tool or target-text filter).
- Read budget: one shared 1 GiB budget. A 503 or 429 renders as a page with the wait, never retried silently.

## 5. Auth and serving

### 5.1 Where it is served

- `kbf-ui` becomes a normal dependency of `kbf-server` (a dev-dependency today) and is mounted on the operator router (`--api-listen`, loopback) as `GET /ui/*`, enabled by `--ui` (off by default). No new port, no TLS listener in `kbf-server`.
- `kbf-proxy` gets `route_of` prefix `/ui` -> `Route::Child(Target::Operator)` with a new `AuthClass::Ui`, and `/ui/*` is added to the HTTP/1.1 allowlist. The public port stays the only public listener, in all-in-one as well.
- **In-process Source and identity (confused deputy).** The proxy strips inbound `x-kbf-*` headers and stamps identity only with its secret (built). The UI handler reads the child's already-verified `Identity` for the request. If there is none (a local process, or a DNS-rebinding page hitting `--api-listen` directly) the answer is 401. There is no implicit loopback or operator identity for `/ui` or any gated route. The `Source` dispatches to the inner router with that `Identity` placed in a request extension whose type is private to the mount, never with headers, so a client cannot forge it and the page can never call with more authority than the caller has. `Host` is validated against the configured public names.
- `ui-data.md` loses `--read-listen`, its TLS flags and `--read-listen-insecure` (R0).

### 5.2 How a browser authenticates

**Choice: mTLS client certificate with a new `viewer` role.**
- The proxy already asks for an optional client certificate and stamps `x-kbf-role` from a verified SAN (`urn:kbf:role:<name>`). `PeerRole` is a closed enum (`Frontend`, `Storage`, `Daemon`) with `ALL`, `name` and `from_name`; `viewer` is a new variant, which touches `kbf-peer`, `kbf-ca` (issuance) and the preflight certificate checks in `kbf-cli`.
- **The SAN is the only discriminator**, because the farm CA also issues daemon, storage and frontend certificates. A viewer leaf has EKU clientAuth only and exactly one role SAN; a certificate with two roles is refused. Every gRPC listener role table refuses `viewer`.
- **The REAPI class must refuse it.** Today `decide` for `AuthClass::Reapi` in `mtls` mode admits any verified identity whatever its role, so adding `viewer` alone would let a viewer certificate call Execute and read and write the CAS. R4a changes `Reapi` to refuse `viewer`, with a named test.
- **Class changes.** Today `AuthClass::Operator` demands the operator bearer token for every `/v1` GET. v1 adds a class `ViewerRead` (viewer certificate or operator token) for an allowlist of GET routes, and `Ui`. Writes, `/metrics` and `Update` stay operator-token only; a viewer certificate never authorizes a write (the child's write gates still require loopback, no `Origin`, bearer and JSON). The proxy never injects the operator token.
- **Which `/v1` GETs a viewer may read: an allowlist, not "all except".** Several read-looking routes are not safe: every enroll route, GET included (`/v1/enroll/tokens`, `/v1/enroll/nodes`), `/v1/object-stores/connections`, `/audit`, `/bindings/{role}`, and `/v1/admin/role`. The allowlist is the routes the pages and agents need (section 2 table, plus `GET /v1`, `/v1/releases`, `/v1/frontend/drain`). The N1 `gate` value is the source of truth and a new or unclassified route defaults to operator-only. R2 verifies and records here how `/v1/blobs`, `/v1/runs`, logs and `/v1/update/hosts` are gated today (the last needs `farm.get`; the invocation and blob routes are off unless their policy slots are named).
- **Child side.** `farm.get` exists only as the policy slot `Policy::farm_get` (`farm.getAuthorizer`), consumed today by `update_hosts.rs` alone; `/v1/nodes` and `/v1/storage` are open reads. R2 adds an authenticator that maps the proxy-stamped role to a principal and enforces `farm.get` on the new routes. The invocations authority is asked per instance as for REAPI, as defence in depth (1.1). `/v1/blobs/{digest}` is authorized by the viewer role plus the allowlist only; a digest is a read capability.
- **Binding.** The per-instance allow-list is keyed in server policy to the certificate's SAN name, never to anything the client sends.
- **Lifetime and revocation.** The farm has no CRL or OCSP. Viewer leaves are valid 90 days at most, and the proxy reads a deny-list (serial or SAN name) from a file it reloads without a handover. A leaked PKCS#12 grants read of unredacted logs until then (risk, section 10).
- **Installer verb** (new; `kbf-installer` has `proxy` verbs but none that issues a certificate): issues a viewer certificate (PKCS#12 for browsers, PEM for agents). It generates the passphrase, never takes it on argv, writes the file `0600`, and never logs a key. A bootstrap verb an admin runs; the proxy holds no standing credential.
- **Browser-trusted server name.** The public name's TLS certificate must be one browsers trust: the operator supplies it, added to the proxy's SNI list (the existing mechanism; the farm-CA certificate stays for the internal name). The farm CA is never imported into browsers as a trust anchor for the public name. TLS minimum 1.2. A request with no client certificate gets a static 401 page. R4d names the installer flag.

**Why.** It reuses the access the farm has (CA, SAN roles, proxy), adds no state, no login page, no password, and is the credential agents use for `/v1`, so parity is automatic. GET-only means no cookie and no CSRF proof.

**Cost, plainly.** Humans import a PKCS#12 file into the browser. That is what "behind the farm's own access" costs. The cheapest fallback is to forward the loopback operator listener over SSH; it is not designed here (the UI refuses unstamped identity, 5.1), so it would need a loopback-trust flag. Not recommended for v1.

**Rejected:** the proxy injecting the operator token after login (gives every UI user write power); a login page and cookie at the proxy (state, rate limit, and `ui-data.md` Q16 blocks cookies until a POST is proven cookie-proof); the operator token typed into a browser (cannot be sent on navigation); open on loopback only (no remote operators); SSO or OIDC (deferred; seam is `auth::decide`).

### 5.3 Read-only enforcement and browser defences

- **Read-only.** The `/ui` router accepts GET and HEAD; anything else is 405 (a test through the proxy covers POST, OPTIONS, HEAD). No form posts: the id lookup uses GET. A page-contract test refuses `method=post` and any non-GET link.
- **CSRF.** No mutating route is reachable by a viewer, so there is nothing to forge. No `Access-Control-Allow-*` header is ever sent. `Sec-Fetch-Site: cross-site` with a `Sec-Fetch-Mode` other than `navigate` is refused at the proxy for `/ui` and the viewer-read routes. Every answer, JSON included, carries `Cross-Origin-Resource-Policy: same-origin` and `Cross-Origin-Opener-Policy: same-origin`. We do not rely on browser certificate behaviour.
- **Headers on every answer** (the built `kbf-ui` sets only CSP and `nosniff`; the rest are new): `Content-Security-Policy: default-src 'none'; style-src 'self'; img-src 'self'; script-src 'none'; base-uri 'none'; form-action 'self'; object-src 'none'; frame-ancestors 'none'` (the built policy says `script-src 'self'`; v1 has no script, so it becomes `'none'`), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer` (ids and tokens sit in URLs), `Permissions-Policy` denying every feature, and `Cache-Control: no-store` on HTML as well as JSON (pages carry logs). The stylesheet is `immutable`. The proxy sets HSTS.
- **XSS.** `Html` escapes every string and has no raw write. Logs are untrusted: escaped text in `<pre>` only. Every emitted href is root-relative, with ids and `page_token` percent-encoded (asserted). Raw blob, log and flog routes answer `Content-Type: text/plain` or `application/octet-stream`, `Content-Disposition: attachment`, `nosniff` and a sandbox CSP; `inline=text` is always `text/plain`. Invalid UTF-8 is replaced, and a 16 KiB tail cut inside a code point is dropped.
- **Redaction is presentation, not a boundary.** Tails and logs are unredacted. Any viewer certificate can read every log of the instances it is allowed (Q5, risk).
- **Heartbeat data is untrusted.** Storage-node numbers in N7 are bounded and type-checked server-side; a negative or absurd value is dropped to null.
- **Raw routes cannot be reached as pages**: attachment disposition plus `frame-ancestors 'none'` and CORP.

### 5.4 Handover and all-in-one

- During a frontend swap the proxy holds a request up to `hold_max` (20 s frontend, 300 s all-in-one), then answers 503 with `Retry-After`. That answer is a static page produced by the proxy with the same security headers; the UI crate cannot produce it when the child is down. Held requests are capped in number and per peer, and each is re-stamped with its own verified identity for the new child, never a stale or elevated one (tests).
- Pages rely on the history store, not child memory. In-memory data (nodes, leases, cordons, recorder records) is empty after a swap until daemons reconnect (3.1 banner).
- Proxy update status is `Route::Update`, served by the proxy with the operator token, outside the operator router: the in-process `Source` cannot reach it. v1 has no proxy-update row.

### 5.5 Access oracle

R1 adds a table-driven test over every registered route and every caller (no certificate, daemon, viewer, operator token) asserting the expected status from the `gate` column. A route not in the table, or with no gate, is operator-only. This catches a read route that was never gated, which a "viewer is not operator" mutant alone would not.

## 6. Tech stack

**Server-rendered HTML, no SPA, no script.**
- Hermetic: no npm, bundler or lockfile. `kbf-ui` has five small dependencies and no async runtime. No new dependency for v1, tests included (7.2).
- The built crate already has escape by construction, the CSP and the declared-reads contract.
- Every page is a plain GET, so agents can fetch it.

Details:
- Typed `html::Html` builder; no template engine. Each page builds a typed model first, then renders it.
- One embedded stylesheet (`include_str!`) at `/ui/static/kbf.css` with a content hash in the URL. No fonts, images, CDN or `style` attributes; status marks are inline SVG with classes. Light and dark by `prefers-color-scheme`, from CSS custom properties.
- Accessibility: one `h1`, landmarks, `th scope` and `caption`, a `label` on every input, `lang` and `title`, no positive `tabindex`, status never by colour alone.
- Tables: stable column order, plain-text numbers. Times are UTC with the raw Unix ms in a `data-ms` attribute. "Next" links carry `page_token`.
- One module per page, each under 1000 lines.

## 7. Testing

All tests are Rust. No shell, Python or JavaScript. No browser runs in v1, so there is no browser fetch, container or download in the build.

### 7.1 Layer 1: `kbf-ui` unit tests, fake `Source`

- **The fake is typed.** Fixtures are built from the server's own serde types (shared through an `api-types` module that both the server and `kbf-ui` tests use), or checked against recorded real responses, so a field rename breaks Layer 1. Today `kbf-ui` parses its own shapes in `api`; R5 adds a test that every fixture round-trips through the server type.
- **Contract:** every page reads only its declared routes. Catches a hidden route (breaks agent parity).
- **Escape, generated:** set every string field of every fixture to a payload corpus and assert no raw `<` from data reaches any page. The field list is generated from the types, so a new field cannot skip it. Corpus: `<script>`, attribute breakout (`"><img onerror=...>`), `javascript:` and `data:` hrefs, CRLF, NUL, ANSI escapes, bidi overrides.
- **Honesty, table driven:** rows of (JSON flag or status, banner): `partial`, `complete:false`, `rows_capped`, `gap_from`, `restored`, null-is-unknown, `no_record` with `records_since`, history off with the flag name, 400 (unknown parameter), 404 for an id outside the caller's instance scope, 503 and 429 with `Retry-After`, restart notice. Catches silent zeros.
- **Static page rules:** no inline script, `style` attribute, event handler or external URL on any page or in the CSS; single `h1`, landmarks, `th scope`, labels, `lang`, `title`; every href root-relative; no non-GET link or form. The CSP header is compared with the page's real resource use.
- **Contrast** of the CSS token pairs, light and dark. This checks tokens, not rendered output. Accepted residual risk: layout, rendered contrast, h2 client-certificate negotiation and meta-refresh behaviour are not checked by any gating test.
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
| each new route N1 to N7 | shape against this document, unit suffixes, 400 on unknown or duplicate parameter |

- **Parity method.** The page tags every API-derived value with `data-src="<route>#<json pointer>"`. The test (a) resolves each tag in the JSON the page read and compares the formatted value; (b) walks every other text node containing a digit and requires it to be on an allowlist of static labels, so an untagged number (a UI-computed sum, age or percentage) turns it red. The extractor is a small tokenizer over the page's own output, safe because all text is escaped. No `scraper` crate: a tokenizer over our own escaped output is smaller than a parser to vet and adds no licence review.
- **Secret sentinel.** Plant a sentinel in argv, environment values, stderr and, for object stores, a connection credential, then assert it is absent from every page and JSON a viewer can reach, except where this document says it is shown (argv and logs, Q5). `/v1/object-stores`, `/v1/update`, `/v1/update/hosts` and `/v1/storage` use a field allowlist, so no endpoint, URI credential or key can be returned.
- **Authority (defence in depth, not a tenant boundary; 1.1).** A viewer without the invocations authority for an instance gets 404 for that instance's leases, invocations and ids, and the fields N3 to N5 mark as authority-gated are omitted, on page and JSON. This does not prove isolation: a digest learned elsewhere still reads its blob through `/v1/blobs/{digest}`, which needs only the viewer role and the allowlist. A planted defect where the `Source` is called with a stronger identity than the caller's must turn a named test red.
- **Mount.** Direct loopback GET with no identity: 401. Forged `x-kbf-role`: ignored or refused. `--ui` off: 404 and `GET /v1` says why.

### 7.3 Layer 3: black-box through the real proxy

Reuses `kbf-it`'s `proxy_support` (proxy at library level, real `kbf` frontend children from this test binary, test PKI, `reqwest`). It needs no service unless history is under test: then Postgres is the CI service container pinned by digest (`KBF_TEST_POSTGRES_URL`), history objects are a `file://` directory (single frontend; accepted today), and the test is `#[ignore]`d like the other Postgres tests. Linux hosted CI only; macOS developers run Layers 1 and 2 and the service-free part of Layer 3. Budget: 5 minutes. Ports are ephemeral, PKI validity is long, and every wait is a poll with a deadline, never a sleep.

**Minimum for R-T1 (the gate):** no certificate refused; `daemon` and `storage` certificates refused on `/ui` and the viewer routes; a `viewer` certificate refused on every write and on the REAPI class even with a bearer; a viewer certificate refused on daemon and storage ports; `/ui` 404 with `--ui` off; no token, proxy secret or `x-kbf-*` reflected in a page; h1 and h2 both served; `Sec-Fetch-Site: cross-site` refused; HTML uploaded as a blob comes back as an attachment (the headers are built in R2b; this checks them through the proxy); a deny-listed and an expired viewer certificate refused (the deny-list ships in R4b as a security mitigation); POST, OPTIONS and HEAD behave (405, 405, served).

**Later, as pages land:** a real build with one failing action, its stderr tail on the Build page, the node, lease and queue pages; the swap test (`hold_max` shortened by a test flag, never waited in real time; a held viewer request is served with the viewer identity); the held-request cap. Each page unit adds a fetch of its page through the proxy, so no page merges with only Layers 1 and 2.

**Mutants** (planted locally, never pushed): wrong auth class on `/ui`, `viewer` mapped to operator, `Reapi` admits `viewer`, drop one `declare`, remove one escape, drop one banner, widen the CSP, pass `WWW-Authenticate`, `Source` with elevated identity, drop one gate row. Each PR body records the patch name and the named test that went red.

### 7.4 CI placement

Layers 1 and 2 run in the normal PR check. Layer 3 runs in the PR check for a change to `kbf-ui`, `kbf-proxy`, `kbf-server`, `kbf-installer`, `kbf-it` or the API types, with a cheap smoke subset (one `viewer` allowed, one refused, `--ui` off) on every PR; the full suite always runs in the beta pipeline. Postgres is the metadata store everywhere history is used; the SQLite phases in `ui-data.md` are rewritten to Postgres only (R0).

### 7.5 No browser tests in v1

Pages have no script, so a browser adds little over Layers 1 to 3. Revisit with charts. Not part of this design.

## 8. Rollout

Behind `--ui` and `ui = true` in the installer's proxy config, both off by default. One concern per PR; each is mergeable alone with its own proof.

| Unit | Content | Proof |
|---|---|---|
| R0 | Docs: rewrite `ui-data.md` serving, auth and SQLite text to this design; document `phase` in `api.md`; correct the `file://` text; page list in `ARCHITECTURE.md` | Review; each built or planned mark checked against code |
| R1 | Route table, `GET /v1`, the access oracle test (5.5) | Table diff test; off routes carry a reason |
| R2 | `farm.get` enforcement, proxy-role authenticator, raw-Command blob gate (Q19), record of today's route gates | Authority tests; blob of a recorded Command refused without `commands.getRaw` |
| R2b | Raw-route headers (5.3) on the built `/v1/blobs`, `/v1/runs/{id}/logs` and flog routes: `text/plain` or `application/octet-stream`, `Content-Disposition: attachment`, `nosniff`, sandbox CSP; `inline=text` always `text/plain`. Prerequisite of R4b | Test `raw_routes_serve_attachment_text_plain` in `kbf-server` (uploaded HTML blob, log and flog: headers asserted; mutant: drop `attachment`) |
| R3 | Per-caller read budget (25 percent, 4 concurrent, 429), path-parameter validation, ranged-log cap. Prerequisite of R4b and of Q4 | Budget starvation test; bound tests |
| R4a | `viewer` in `kbf-peer`, `kbf-ca`, preflight; `Reapi` and every role table refuse it. **Security-critical, independent security review** | Named tests; mutants `viewer`->operator and Reapi admits viewer |
| R4b | Proxy: `ViewerRead` class over the allowlist, `Ui` class, `/ui` route, h1 allowlist, deny-list reload. **Security-critical, independent review** | Route and class unit tests; refused on write and operator routes |
| R4c | Proxy: `Sec-Fetch-Site`, held-request caps, static handover page, security headers | Unit tests |
| R4d | Installer: viewer certificate verb, public-name SNI certificate setting | Verb test (0600, no argv passphrase, 90-day validity) |
| R5 | Server: mount `kbf-ui`, in-process `Source`, `--ui`, GET-only router, `Host` check, headers, CSS route. **Security-critical, independent review** | Layer 2 mount tests; R-T1 smoke |
| R-T1 | Black-box minimum (7.3) | Layer 3 green; mutants red |
| R6 | `/v1/invocations/{id}/failures` | Shape, failed-run fixture, escape |
| R7 | `Scheduler::snapshot()` and `/v1/leases` | `kbf-sim` first; authority test |
| R8 | `/v1/queue` | Shape; per platform and QoS |
| R9 | `/v1/nodes/{id}`, `lease_count` and `booked` on `/v1/nodes` | Shape; suffix refused |
| R10 | Heartbeat fields (`kbf-proto`), `/v1/storage` extension | Null-is-unknown; values recover after reconnect |
| R11 | `/v1/health` | Same values as `/readyz` and the source routes |
| P1 | Builds and Build (the move off `/ui`) | Layers 1, 2 and a proxy fetch |
| P2 | Action and Log | same |
| P3 | Nodes and Node | same |
| P4 | Leases | same |
| P5 | Queue | same |
| P6 | Storage | same |
| P7 | Overview, refresh option | same |
| P8 | Targets and Target (1.2, droppable) | same |
| D1 | Operator guide: certificates, SNI, `--ui` | Followed on a clean cell |

Order: R0; R1, R2, R2b, R3 in parallel (R2b before R4b); R4a then R4b, R4c; R4d in parallel; R5 after R2 and R4b; R-T1 after R5; routes R6 to R11 any time after R1 and R2; each page unit after its routes and R5. v1 is done when D1 is merged and the `--ui` default is decided (Q4).

## 9. Open questions

Each has a recommendation, the alternative, and the cost of being wrong.

- **Q1. Browser credential.** Recommended: mTLS client certificate with a `viewer` role (5.2). Alternative: a proxy login cookie, or SSH-forwarding the loopback listener. Cost of being wrong: certificate import friction for every human operator (the cookie path costs a session store and a proven cookie-proof POST later). Revisit after one release of use.
- **Q2. What a viewer certificate may read.** Recommended: only the allowlisted `/v1` GET routes (5.2), so agents use the same credential and parity holds. Alternative: viewer opens `/ui` only and agents keep the operator token. Cost of being wrong: too wide leaks enroll or object-store detail (the allowlist prevents it); too narrow breaks parity and agents keep holding the write token.
- **Q3. Storage-node health.** Recommended: fold into the heartbeat and `/v1/storage` (a `kbf-proto` field addition; no new storage listener, one JSON source). Alternative: the frontend scrapes storage nodes' metrics ports. Cost of being wrong: a wire change that must stay compatible across a mixed-version farm, against a new listener on every storage node.
- **Q4. Default of `--ui`.** Recommended: off in v1; on after R3 and R-T1 are merged and one release has run in beta. Alternative: on at once. Cost of being wrong: on early exposes unredacted logs to every viewer before the read budget exists.
- **Q5. Unredacted logs and command arguments.** Recommended: ship with a visible warning, show the failed command's arguments and environment names (as `invocations-api.md` 4.7 specifies; agents need the command to reproduce), keep environment values and the raw Command behind `commands.getRaw`. Alternative: hide arguments unless `commands.getRaw`, and add a separate `logs` authority so overview-only viewers cannot read logs. Cost of being wrong: a secret passed as `--token=...` is readable by every viewer certificate. Redaction is a separate decision.
- **Q6. Beyond the assignment (1.2).** Recommended: keep Targets and Target and `/v1/health`; Grid, the proxy-update row and `/llms.txt` are cut. Alternative: drop both. Cost of being wrong: the Overview computes counts in the UI (breaks parity) or agents debugging flaky tests lose the history pages.

## 10. Risks

Each security item needs an independent security review before R4a, R4b and R5 merge.

- **The `viewer` role touches the proxy.** A mapping mistake widens access, and the REAPI class admits any verified identity today. Mitigation: R4a and R5.5 tests, mutants named in 7.3.
- **Confused deputy in the in-process `Source`.** Mitigation: private extension type, 401 without identity, escalation test.
- **Certificate revocation.** No CRL. Mitigation: 90-day leaves, reloadable deny-list, tested.
- **Unredacted logs on screen.** Any viewer reads every log of its authorised instances; owner: the CEO decides Q5. Mitigation: warning, read budget, no-store.
- **Tenancy is not a boundary.** Depth, per-platform counts and `lease_count` are visible to every viewer; ids, digests and instance names are gated per instance as defence in depth, out-of-scope ids return 404, and any viewer who learns a digest can read the blob. Do not host mutually untrusting tenants on one farm (1.1).
- **Scheduler snapshot cost.** A copy under the scheduler lock could slow scheduling. Mitigation: bounded copy, page cap, farm benchmark in R7. The by-platform and per-lease age and operation data need new scheduler fields, not only a snapshot of `waiting`, `avoided`, `queued_len`, `leases_held`, `queued`, `leases_on`.
- **Read budget.** One shared 1 GiB budget lets a viewer starve agents. Mitigation: R3 lands before any viewer access (R4b depends on it).
- **Empty farm after a swap.** Mitigation: the start-time banner. Moving node and lease state into the state store is a later design.
- **Partial history.** Today-index routes are partial and day-close does not run. Mitigation: show `covers_from`, promise nothing older.
- **Doc and code drift.** `ui-data.md` lists built routes as new. Mitigation: R0 first; `GET /v1` comes from the router's table.
- **Test cost.** Layer 3 is slow. Mitigation: smoke on every PR, full suite in beta.
- **Viewer-read through `/v1/blobs`.** Until R2's Command gate lands, a recorded Command (with environment values) is readable by digest. R2 is a prerequisite of R4b.

## Appendix A: new route fields

Shared: every list carries `next_page` (root-relative) and takes `page_token`, `page_size` (1 to 200, default 100, unless stated).

**N1 `GET /v1`.** `{server{version,commit}, routes[{method,path,query[],gate,summary,status}], limits{...}}`. `gate` is `open`, `read` (viewer or operator), `farm`, `invocations` or `operator`. `status` is `on` or `off` with a reason. The gate is `read` for the index itself; it discloses nothing to an unauthenticated peer.

**N2 failures.** As `invocations-api.md` 4.7: `{invocation{...,links{failures,...}}, failures[{action, href, target, mnemonic, outcome, failure, attempts[{lease, worker, queued, granted, started, ended, end, run_id, href}], command{arguments, environment_names, working_directory, platform, timeout_micros, output_paths}, stderr, stdout{digest, bytes, available, tail, tail_bytes, truncated, href}, outputs, usage}], next_page_token, next_page}`. Tails are 16 KiB. `links` gains `failures` (not in `views.rs` today). `page_size` 1 to 100, default 20 (set in R0).

**N3 `GET /v1/leases?node=`.** `{term, items[{lease, node, phase, age_ms, operation, since_ms, invocation?, digest?, instance?}], next_page}`. The last three are present only when the caller holds the invocations authority for the instance, omitted (not null) otherwise (defence in depth, not a tenant boundary; 1.1); an operation id that encodes an invocation or digest is treated as one. `lease` is `term.seq`, not stable across a restart; `term` says so. Source: a new public `Scheduler::snapshot()`.

**N4 `GET /v1/queue`.** `{depth, oldest_age_ms, by_platform[{platform, qos, depth, oldest_age_ms}], waiting[{reason, count, example_digest?}], avoided[]}`. `example_digest` follows the same rule (defence in depth; the digest, once known, is a read capability). `GET /v1/frontend/drain` keeps its three fields.

**N5 `GET /v1/nodes/{id}`.** All fields of the list entry plus `capacity`, `booked`, `free`, `leases[]` (ids) and `recent_failures{count, run_ids[], partial, covers_from}` (run ids follow the authority rule). 404 `{"error":"no_node"}`. `/v1/nodes` entries gain `lease_count` and `booked`.

**N6 `GET /v1/health`.** `{ready, failing[{check,reason}], nodes{serving,cordoned,draining,drained,quarantined,disconnected}, queue{depth,oldest_age_ms}, leases, storage{members_active,members_total,under_replicated_blobs}, update{state}, history{enabled,dropped_total}}`. Built from in-process values; replaces nothing; `/readyz` stays the probe. The TLS leaf expiry is not included: the proxy owns the leaf and the child has no source for it.

**N7 `/v1/storage` extended.** Per node: `unflushed_bytes`, `unflushed_blocks`, `oldest_unflushed_ms`, `budget_bytes`, `used_bytes`, `lost_blobs`, `flush_failures`, `flush_conflicts`. All nullable, null means unknown. None is in the storage-health source today (it has under-replicated blobs, debts, open writes); each is a new heartbeat field in `kbf-proto` and clamped on receipt.
