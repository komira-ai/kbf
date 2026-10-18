# kbf-proxy: public API, module layout and U4b plan

Status: design for review, unit U4b of `docs/design/frontend-proxy.md` (the design; section numbers below are its). Stacked on U4a (`kbf-server/src/proxy_auth.rs`, the child's side of the identity contract). Refs #1095. The crate skeleton in `crates/kbf-proxy` holds the parts that are plain values, with tests; everything marked "(U4b-n)" is not in it yet. Where this note and the design differ, this note says so.

## 1. What U4b is

`kbf-proxy` is a library; `kbf-installer proxy` (verb, U4b-3) is its only binary. It is the public port of a frontend host: one TLS terminator, per-stream routing to the active child frontend on loopback, identity headers, the hold gate, open-stream accounting, and a pluggable handler for `/v1/update/frontend*`. It supervises nothing (U7b), writes nothing under `current`, `slots/` or `status.json`, and has no control socket.

Reusable from the repo: `kbf-front/src/proxy/bytestream.rs` (peek, keep the first frames, fail over only before the first byte) is the pattern for the replay buffer. Nothing else in `kbf-front/src/proxy` applies: that proxy is the frontend talking to storage nodes. `kbf-tls` is not reusable as is: `Incoming` is a tonic stream, and `config.rs` builds the client verifier with `.build()` only (certificate mandatory) and ALPN `h2` only. U4b-2 adds to `kbf-tls` a second constructor, `server_config_optional_client(...)` (optional client certificate, ALPN `h2`, `http/1.1`, same ring provider, early data off); the existing path is untouched.

## 2. Module map

| Module | Holds | In skeleton |
|---|---|---|
| `slot` | `Slot`, `ActiveSlot` (gate and permits), `Permit`, `Bind`, `Retire` | yes |
| `hold` | `HoldPolicy`, `HoldQueue`, `Ticket` | yes |
| `route` | `Route`, `Target`, `AuthClass`, `Idle`, the closed path table | yes |
| `auth` | `decide`, `Buck2Auth`, `Token`, `Tokens`, `Refusal` | yes |
| `identity` | `Identity`, `identity_of`, `ProxySecret`, `forward_headers`, `strip_reserved` | yes |
| `replay` | `ReplayBuffer`, `may_retry`, `is_fenced`, the fenced contract | yes |
| `config` | `ProxyConfig`, `UpstreamTable`, `Limits` | yes |
| `control` | `ChildControl`, `UpdateHandler`, `SlotStats`, `NoUpdates` | yes |
| `tls` | `TlsReloader` (cert, key and CA as one unit, SIGHUP), the handshake | U4b-2 |
| `serve` | accept loop, per-connection task, `catch_unwind`, limits, rate limiter | U4b-1 |
| `h2` | downstream h2 server, per-stream dispatch, windows, GOAWAY and reset mapping | U4b-1 |
| `pool` | upstream h2c pool per (slot, target) | U4b-1 |
| `h1` | HTTP/1.1 for update, probes and `/v1/*` only | U4b-2 |
| `probe` | `/healthz`, `/readyz`, `/metrics` | U4b-1 (metrics), U4b-3 |
| `lib` | `Proxy::start`, `ProxyHandle` | U4b-1 |

Dependencies of the finished crate: `hyper` (`http1`, `http2`, `server`, `client`), `hyper-util` (`tokio`, `server-auto`, `client-legacy`), `tokio`, `tokio-rustls`, `rustls`, `kbf-tls`, `kbf-peer`, `futures`, `httparse`; `h2` only if the h2 limits below are not reachable through hyper's builders (U4b-1 checks and says). The workspace declares neither `h2` nor `tower` today and U4b adds nothing it does not use. The lockfile does not show features, so U4b-1 declares the hyper features it needs explicitly.

## 3. Public API

The skeleton compiles these (signatures abridged to the load-bearing ones; docs are in the code).

```rust
// slot: the binding rule. One atomic word per slot: top bit CLOSED, rest = open permits.
pub enum Slot { A, B }                       // fixed: ports are fixed offsets (section 9)
pub struct ActiveSlot { /* Arc<..> */ }      // Clone; the shared handle
impl ActiveSlot {
    pub fn new() -> Self;                    // both slots closed, nothing active
    pub fn open(&self, slot: Slot);          // child ready: bindable
    pub fn set_active(&self, slot: Slot) -> Result<(), SwapError>; // Closed slot refused
    pub fn hold(&self);  pub fn release(&self);  pub fn holding(&self) -> bool;
    pub fn bind(&self) -> Bind;              // Bound(Permit) | Held | NoActive
    pub fn open_permits(&self, slot: Slot) -> u64;
    pub fn retire(&self, slot: Slot) -> Retire; // Closed | Current | Busy(n); CAS 0 -> CLOSED
    pub fn active(&self) -> Option<Slot>;
}
pub struct Permit { /* Drop = fetch_sub */ }  impl Permit { pub const fn slot(&self) -> Slot }

// hold
pub struct HoldPolicy { pub hold_max: Duration, pub queue_streams: usize, pub window_bytes: u32 }
impl HoldPolicy { pub const fn frontend() -> Self; pub const fn all_in_one() -> Self; }
pub struct HoldQueue;  impl HoldQueue {
    pub fn new(p: HoldPolicy) -> Self;
    pub fn admit(&self, now: Instant) -> Result<Ticket, QueueFull>; // Ticket::{deadline, expired}
    pub fn held(&self) -> usize; }

// route and auth: pure.
pub fn route_of(path: &str) -> Route;        // Child(Target) | Update | Health | Ready | Metrics | Unknown
pub const fn auth_class(r: Route) -> Option<AuthClass>;
pub const fn bypasses_hold(r: Route) -> bool;
pub fn idle_of(path: &str) -> Idle;          // Never for Execute and WaitExecution, else 7200 s
pub fn decide(c: AuthClass, m: Buck2Auth, t: &Tokens, p: Presented<'_>) -> Result<(), Refusal>;

// identity: what the child sees.
pub fn identity_of(der: &[u8]) -> Result<Identity, IdentityError>; // role URI + the ONE DNS name
pub fn forward_headers(h: &mut HeaderMap, id: Option<&Identity>, s: &ProxySecret)
    -> Result<(), IdentityError>;            // strip x-kbf-*, then set node-id, role, proxy-auth
pub fn strip_reserved(h: &mut HeaderMap);    // on every child answer

// replay
pub struct ReplayBuffer;  // push(n) -> bool, close(), replayable()
pub fn may_retry(path: &str, f: Failure, b: &ReplayBuffer, attempts: u8) -> bool;
pub fn is_fenced(h: &HeaderMap, end_stream: bool) -> bool;

// control: implemented by U7 / U8.
pub trait ChildControl: Send + Sync {        // frozen admin_protocol
    fn takeover(&self, s: Slot) -> impl Future<Output = Result<(), ControlError>> + Send;
    fn quiesce(&self, s: Slot) -> impl Future<..>;  fn stand_down(..);  fn resume(..);
    fn role(&self, s: Slot) -> impl Future<Output = Result<ChildRole, ControlError>> + Send; }
pub trait UpdateHandler: Send + Sync {
    fn handle(&self, r: UpdateRequest) -> impl Future<Output = UpdateReply> + Send; }
```

Planned, not in the skeleton (U4b-1 and later):

```rust
pub struct Proxy<C: ChildControl, U: UpdateHandler> { .. }
impl<C: ChildControl, U: UpdateHandler> Proxy<C, U> {
    /// Takes the systemd-activated listener (fd 3, LISTEN_FDS == 1 or an error), builds
    /// the runtime, starts the accept loop. Returns when `shutdown` resolves.
    pub fn start(cfg: ProxyConfig, listener: OwnedFd, control: C, updates: U) -> io::Result<ProxyHandle>;
}
pub struct ProxyHandle { /* Clone */ }
impl ProxyHandle {
    pub fn slots(&self) -> &ActiveSlot;                // U7's supervisor: open, set_active, hold, release, retire
    pub fn stats(&self, s: Slot) -> SlotStats;         // open_streams, oldest_stream_age: GET status
    pub fn open_streams(&self) -> u64;                 // both slots + held: restart-on-idle (U7b)
    pub fn set_ready(&self, s: Slot, ready: bool);     // /readyz input (section 9)
    pub fn reload_tls(&self) -> Result<(), TlsReloadError>; // what SIGHUP calls
    pub fn shutdown(&self);
}
```

## 4. Threading and async model

- One multi-thread tokio runtime built inside `Proxy::start` (workers = min(cores, 8)); `kbf-installer`'s `main` stays synchronous. One accept task; one task per connection; one task per stream inside it. Each connection task is wrapped in `FutureExt::catch_unwind`: a panic closes that connection, increments `kbf_proxy_panics_total`, and nothing else. Panics are a bug; the wrap is the blast-radius limit, not error handling.
- The data path takes no lock: `ActiveSlot`, `HoldQueue` and the counters are atomics. The only locks guard the TLS config (`RwLock<Arc<ServerConfig>>`, read once per handshake) and the pool's connection list (per target, held for a pick, never across an await).
- **One writer for control.** `set_active`, `hold`, `release`, `open` and `retire` are called by one task (U7's supervisor). `retire` documents that a concurrent `set_active` to the same slot is unsupported; U7a's actor is the single caller (design section 4: one state machine, one actor).
- Streams read the clock only through a `Clock` (paused time in tests); the skeleton's pure functions take `Instant` as input.
- Memory is bounded by construction: per stream 64 KB window up to the child, header list 16 KB, queue 2048 held streams (128 MiB worst case), connection caps, and the upstream pool cap. The proxy runs with `OOMScoreAdjust=-900`, so these bounds are what stop it eating the host.

## 5. Per-stream binding and the permit

**Rule.** A stream binds to the slot active when its headers arrived. The bind is one atomic step, not a load followed by an increment, because that pair races a retire: a stream could load slot A, the supervisor could flip and retire A (count 0), and the stream then take a permit on a closed slot.

**Mechanism.** Each slot is one `AtomicU64`: bit 63 is `CLOSED`, the rest count permits. `bind` loads the gate (active slot and hold flag), then `fetch_update`s the slot word: increment unless `CLOSED`. A closed slot sends it back to reload the gate; an unchanged gate on a closed slot is `NoActive` (never a spin). `retire(slot)` refuses `Current`, otherwise does `compare_exchange(0, CLOSED)`. So a stream that incremented first makes the retire fail (`Busy(n)`), and a stream that comes later sees `CLOSED`. "Retire needs all permits of the old slot and not current" is that CAS, taken together with the gate check; retire never closes a slot a stream holds.

**Retire waits** by being polled: the supervisor calls `retire(old)` when the child reports `open_streams == 0 && open_sessions == 0` and on each status tick until it returns `Closed`; `Busy(n)` is the `old.open_streams` of the status. No timeout, no kill (design section 5 step 10). A permit drops when its stream ends for any reason (end, client RST, upstream reset, connection loss), so a stuck stream stays visible, never silently forgotten.

**Queued streams count as bound.** The permit is taken at header time, before pool checkout. A stream waiting for an upstream connection holds its permit, so retire cannot close a slot a queued stream is waiting on.

**Loom test (U4b-1, owner: U4b).** `slot.rs` takes its atomics from a private `sync` shim (`std::sync::atomic`, or `loom::sync::atomic` under `cfg(loom)`; `loom` is a dev-dependency declared in the workspace by that PR). Model: thread 1 calls `bind` once; thread 2 calls `open(B)`, `set_active(B)`, `retire(A)`. Invariants over every interleaving: a `Permit` for A exists only if `retire` returned `Busy`; if `retire` returned `Closed`, thread 1 got `Bound(B)` or `NoActive`, never `Bound(A)`; the A counter never underflows; after thread 1 drops its permit and `retire` is repeated, it returns `Closed`. Second model: two binders and a `hold`/`release` pair; a binder that sees `Held` never holds a permit. The skeleton's racing-threads test is the stress version of the same invariant, and the real stress test (200 streams, 5000 queued) is in section 14.

**Mutants.** `bind` as load-then-`fetch_add` (no `CLOSED` check): the loom model and the racing test must fail. `retire` that ignores the count: `streams_stay_on_the_slot_they_bound_to` fails.

## 6. The hold gate

Design section 5 step 6.1 and 5a. `hold()` sets the hold flag in the same word as the active slot, so a stream either sees the flag (`Bind::Held`) or binds before it; there is no third state. A held stream is accepted, its headers are read (so the route is known and bypass works), nothing is opened upstream, and no window beyond the initial one is granted. `release()` clears the flag; the caller then calls `bind()` for each held stream, in arrival order, into whichever slot is active (the new one after a swap, the old one after an abort or a failed takeover).

- **Buffer** (decision C3): the window is 64 KB and covers headers plus data. The header list cap (16 KB) sits inside it. Held streams are bounded to 2048 (`HoldPolicy::queue_streams`), so a hold buffers at most 128 MiB. The 2049th stream gets `UNAVAILABLE` at once and `kbf_proxy_hold_rejected_total` ticks.
- **Expiry:** each `Ticket` carries `deadline = admit time + hold_max` (15 s for a frontend swap, 300 s all-in-one; `hold_max_s` sets the first). At the deadline the stream gets a trailers-only `UNAVAILABLE`, the ticket drops, `kbf_proxy_hold_expired_total` ticks. Expiry does not release the gate or abort the update.
- **Held streams are not bound** (decision C2). They hold no permit and count in `kbf_proxy_held_streams`; the bind happens at release, in one step with the slot load, exactly as for an unheld stream. Reason: a held stream has no slot, and a permit on the old slot would block its retire for nothing. Consequence: the status `old.open_streams` does not include held streams; `HoldQueue::held()` is reported separately as `held`.
- **Bypass:** `bypasses_hold` (update API, `/healthz`, `/readyz`, `/metrics`) never queues. On HTTP/1.1 the proxy stops reading further requests on a held connection and at `hold_max` answers 503 with `Retry-After`. h1 serves only paths that are not gRPC (section 11), so no gRPC call sits on an h1 connection.
- **Client reset while held:** the stream task ends, the ticket drops, `held` falls; nothing was sent upstream. **`abort`:** the supervisor calls `resume` on the old child, then `release()`; held streams bind to the old slot and none is cancelled. Crash: the hold is lost, accepted (design section 6).
- **Mutant "a window with no active child":** `set_active` refuses a closed slot (`a_closed_slot_cannot_become_active_or_bind`), and `bind` on no active slot is `NoActive`, not a permit on a stale slot. Remove the check and both tests fail.

## 7. The upstream table and the supervisor's API

**Static in U4b** (decision C10). `[proxy.upstream] base_port = N`. Slot A listens on `N + 0..3`, slot B on `N + 10..13`; offsets: REAPI 0, worker 1, operator 2, admin 3 (the admin calls of `ChildControl`). All are `127.0.0.1`; the table cannot name another host, which is what the Envoy upstream-TLS checks bought (the hop is loopback). `UpstreamTable::port` and `admin_port` compute them; both slots exist from the start, so "adding a slot" is `open(slot)`.

**What U7's supervisor does**, all through `ProxyHandle`:

1. start the candidate in the idle slot, wait ready, `set_ready(slot, true)`, `slots().open(slot)`;
2. `slots().hold()`; `control.quiesce(old)`; `control.takeover(new)`;
3. success: `slots().set_active(new)` then `slots().release()`; failure: kill candidate, `control.resume(old)`, `slots().release()`;
4. `control.stand_down(old)`; poll `slots().retire(old)` until `Closed`, then SIGTERM.

`set_active` before `release` means held streams bind once, to the new child. A swap to a slot that was never opened or already retired is an error, not a window with no child.

**Pool** (decision C1). One h2c pool per (slot, target), prior-knowledge HTTP/2, no ALPN. A pick takes the connection with the fewest open streams; if every connection is at the child's advertised `MAX_CONCURRENT_STREAMS` it dials another, up to 256 per (slot, target); beyond that the stream waits (and still counts as bound, section 5). A child GOAWAY marks that connection draining (no new streams, existing finish) and is never forwarded. A child that stops accepting connections fails new streams with `UNAVAILABLE` after a 3 s connect timeout; open streams are untouched. Upstream keepalive: PING every 30 s, ack within 20 s, same as downstream.

## 8. TLS and identity

**Terminator.** One public listener, the activated socket (fd 3; refuse to start unless `LISTEN_FDS == 1` and `LISTEN_PID` is this process). Not tonic's `Incoming`: a raw accept loop wraps `TlsAcceptor` (`tokio-rustls`) over `kbf-tls`'s new optional-client config. ALPN `h2`, `http/1.1`; TLS 1.3 early data off; handshake, preface and request-head timeouts 10 s each. The client certificate is optional; a presented certificate that fails verification fails the handshake. After the handshake, `identity_of(leaf_der)` runs once per connection.

**Node id rule** (decision, the survey found no accessor): the node id is the certificate's one DNS name. Zero, two or more, or a wildcard make `identity_of` fail with `NodeId(n)`, and the proxy closes the connection: an identity that must be guessed is not one. The role is the one `urn:kbf:role:<name>` URI, via `kbf_peer::role_of`. Deny list: the child's own list covers `node` entries; the proxy adds none in U4b.

**Reload** (C12). SIGHUP re-reads the server certificate, key and client CA bundle as one unit and builds a new `ServerConfig`; it replaces the old one only if the whole build succeeds. On failure the old config stays, `kbf_proxy_tls_reload_failed_total` ticks, the error is logged without any file content. Only new handshakes see the new config. The skeleton's `ProxyConfig` has no reload keys: the paths are fixed at start.

**Path-class auth** (`auth::decide`; table in `route`):

| Class | Paths | Rule |
|---|---|---|
| Worker | `/kbf.worker.*`, `/kbf.map.v1.Membership/`, `/kbf.update.*` | verified certificate with role Daemon or Storage; none: `UNAUTHENTICATED`; other role: `PERMISSION_DENIED` |
| Reapi | REAPI services, ByteStream | `none`: open; `mtls`: any verified certificate; `bearer`: the right token |
| Operator | other `/v1/*`, `/metrics` | operator token |
| Update | `/v1/update/frontend*` | operator token now (one function, swap point for OIDC #1088) |
| Probe | `/healthz`, `/readyz` | open |

Decision C8: the modes do not combine. In `bearer` mode a certificate is neither required nor sufficient, and a wrong token next to a good certificate is a refusal: one mechanism per mode, no fallback to a weaker one. In `none` mode a verified certificate is still turned into identity headers (the child decides what a daemon may do). A certificate of the wrong role is `PERMISSION_DENIED` on the worker class where the design says `UNAUTHENTICATED` for "else": the peer is known, and `kbf-peer` documents the same split.

**Bearer.** Read only from `Authorization: Bearer`, never a query string, never logged (`HeaderValue::set_sensitive`; the log layer drops the header). A per-address token bucket runs before any hashing; `Token` compares SHA-256 digests in constant time (`subtle`), so length does not leak.

**Closed route table.** A path matching no prefix is `Unknown`, answered by the proxy (404, or `UNIMPLEMENTED` for gRPC) and never forwarded. **Open question, flagged:** `/kbf.enroll.v1.Enroll/*` is called before a node has a certificate and is not on the worker listener that `serve.rs` builds; I did not find which listener serves it. The skeleton refuses it (`Unknown`), which breaks enrollment through the proxy. U4b-2 must settle the listener and add a class that takes no certificate (the child checks the enrollment token). Same for `ColdStore`.

**Identity to the child** (`identity::forward_headers`, the exact contract of `kbf-server/src/proxy_auth.rs`): strip every inbound `x-kbf-*`; with a verified certificate set `x-kbf-node-id` and `x-kbf-role` (`PeerRole::name`); always set `x-kbf-proxy-auth` to the 64-char lower-case hex of the secret. Each header ends with exactly one value. Child answers go through `strip_reserved` before the client sees them (the fenced marker is read first). The upstream connection is loopback, so the child's loopback check holds.

**Secret** (C13). `ProxySecret` is 32 bytes. The proxy creates `/run/kbf-proxy/proxy-auth` (`O_EXCL`, 0600, owner `kbf`) if absent and reads it if present, so it survives a proxy restart and adopted children keep working. The directory is owned by the unit (`RuntimeDirectory=kbf-proxy`, `RuntimeDirectoryPreserve=yes`, which U7b writes); U4b-2 owns the create-or-read code and its test (wrong mode or owner: refuse, as the child does for its copy). Children read it by `--proxy-auth-file`; never argv or env.

## 9. Config schema (`installer.toml`, table `[proxy]`)

```toml
[proxy]
operator_token_file = "/etc/kbf/operator.token"   # read by the proxy; reloaded on SIGHUP
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file  = "/etc/kbf/tls/server.key"
client_ca_file   = "/etc/kbf/tls/ca.pem"
buck2_auth = "none"                  # none | bearer | mtls
buck2_token_file = "/etc/kbf/buck2.token"   # required for bearer
proxy_auth_file = "/run/kbf-proxy/proxy-auth"
state_dir = "/var/lib/kbf-proxy"     # update.json is U8b's
hold_max_s = 15
[proxy.upstream]
base_port = 18000
[proxy.limits]                       # every key optional; defaults shown
pre_stream_timeout_s = 10   max_connections = 50000   max_connections_per_ip = 2000
max_header_list_bytes = 16384   keepalive_interval_s = 30   keepalive_ack_s = 20
```

Unknown keys are refused at start. U4b-3 adds `proxy: Option<ProxyConfig>` to the installer's settings and keeps the installer's own deny-unknown rule. The operator token and update credential (C9) are read by the proxy; U8 injects a different `UpdateHandler`, not a different token source.

## 10. Keepalive, retry, GOAWAY and resets

- **No stream deadlines.** No route timeout, no max stream duration. Idle timeout: `idle_of(path)` is `Never` for `Execution/Execute` and `Execution/WaitExecution` (matched by full method path: the table is per path, so a generic route needs no state) and 7200 s for the rest.
- **Dead peers.** Downstream PING every 30 s, ack within 20 s; a failed ack closes that connection only.
- **Replay buffer** (C5): one buffer serves hold and retry (`replay`). Request bytes are kept from the first byte, 64 KB. The window closes for good at the first upstream response headers or when a frame would overflow it. `may_retry` is true only for: not ByteStream, fewer than 2 dispatches, window open, failure in `{Connect, RefusedStream, Fenced}`.
- **Fenced-not-committed** (C6, the contract the child pins in U2): a trailers-only answer (END_STREAM on the headers frame) with `grpc-status: 14` and exactly one `x-kbf-fenced: nothing-committed`. The proxy re-dispatches the buffered request to the active slot if `may_retry` allows. If the window is closed, the client gets the same `UNAVAILABLE` with the marker stripped, which a gRPC client retries by itself; that is correct because the marker means nothing committed. Any other shape (a body, two markers, another status) is not fenced.
- **Resets.** Client RST: propagated upstream as RST_STREAM(CANCEL); the permit drops. Upstream reset mid-stream: the client gets a gRPC `UNAVAILABLE` trailer, not a bare RST (headers already sent) or a trailers-only `UNAVAILABLE` (not yet sent). **Child GOAWAY:** never forwarded; the proxy never sends GOAWAY to a client except on its own shutdown (a hold, a flip and a retire send none).
- **Flow control.** Per stream, windows capped at 64 KB to the client and 64 KB to the child; per connection 16 MiB. The proxy forwards window updates only as the other side consumes, so memory per stream is the window.
- **h2 flood limits.** Max pending reset streams (rapid reset), header-list cap, CONTINUATION frame count and byte caps, concurrent streams per connection 1000 (an advertised SETTING), each a hyper/`h2` builder setting asserted by a test that attacks the fake proxy.

## 11. Pre-stream limits and the verb

Handshake, h2 preface and h1 request-head timeouts: 10 s each (`pre_stream_timeout_s`); `max_connections`, `max_connections_per_ip`; per-address rate limit before any token check. **HTTP/1.1** (C4) is served only for `/v1/*`, `/healthz`, `/readyz` and `/metrics`; the request head is parsed with `httparse`, at most 16 KB and 100 headers, origin-form targets only (`CONNECT`, absolute-form and `Upgrade` are 400), and a gRPC path on h1 is 426. The hold bypass is decided from the same parse.

**Verb.** `kbf-installer proxy [--config PATH] [--check]`: `--config` defaults to `/etc/kbf/installer.toml`; `--check` loads and validates the table (`ProxyConfig::from_toml_str`) and exits 0 or 2 without binding. No other flag: everything else is the `[proxy]` table, since configuration is flags and files, not the environment. The two environment variables it reads (`LISTEN_FDS`, `LISTEN_PID`) are the platform's socket-activation markers. To add it: a `Verb::Proxy` variant (`cli.rs`), a `built_by` arm, a dispatch arm (`app.rs`), `needs_root` false. It is not a `kbf-cli` mode: `kbf-cli`'s `GROUPS` rule (every flag of every mode in one group) then does not apply, but the child's new `--await-takeover` (U2) must join a group there. Installer dependency: `kbf-installer` gains `kbf-proxy` and with it tokio, hyper and rustls in the same binary as the root helper. **Risk, with a recommendation:** the root half links a network stack it never runs. Keep the design's single verb, and add a layering test (U4b-3) that the root helper's modules (`helper/`, `run`) name nothing from `kbf_proxy`; if the CEO prefers no shared binary, `kbf-proxy` gets its own `main` and the verb becomes `exec` of it, which is a unit-file change, not an API one.

## 12. Decisions the design left open

| # | Choice | Reason |
|---|---|---|
| C1 | h2c prior-knowledge pool per (slot, target), up to 256 connections each, fewest-streams pick, dial before queue; queued stream keeps its permit | the child's per-connection stream limit would otherwise queue 5000 streams behind long ones |
| C2 | Held streams are not bound; bind at release in one step with the slot load | no slot yet; a permit on the old slot would block its retire |
| C3 | 64 KB window covers headers + data; 2048 held streams max; overflow is `UNAVAILABLE` at once | bounds memory to 128 MiB under `OOMScoreAdjust=-900` |
| C4 | h1 only for `/v1/*` and probes; head <= 16 KB, 100 headers, origin-form; gRPC on h1 is 426 | h1 cannot multiplex, so holding it means stop reading |
| C5 | One buffer for hold and retry; window closes at the first upstream response or overflow | one rule, no state to keep in step |
| C6 | Fenced = trailers-only, `grpc-status: 14`, one `x-kbf-fenced: nothing-committed` | a client that sees it retries correctly; strict shape cannot be forged by a body |
| C7 | Idle is chosen by full method path of Execute and WaitExecution | generic routing needs no per-stream state |
| C8 | Modes do not combine; wrong-role certificate is `PERMISSION_DENIED`; wrong token never falls back to a certificate | no downgrade to a weaker mechanism |
| C9 | The proxy reads `operator_token_file` itself; update credential = the same `Token`; U8 swaps the `UpdateHandler`, `decide` is the OIDC seam | design section 4 says "same token"; one function to replace |
| C10 | Static table is `base_port` plus fixed offsets; `/readyz` is 200 when the active child's last poll (every 1 s, 3 s timeout) passed within 5 s; readiness never touches routing or open streams | design: a health failure does not touch open streams |
| C11 | `/metrics` is served by the proxy about itself, Prometheus text, operator token; the child's metrics are not forwarded | the series are different and the proxy must count its own streams |
| C12 | SIGHUP reloads cert, key and CA as one unit; a failed reload keeps the old one and counts | a half-applied reload would break handshakes |
| C13 | The proxy creates or reads the secret file; the unit owns `/run/kbf-proxy/` (U7b) | the proxy restarts without the supervisor, so it cannot depend on it |
| C14 | A reset while held drops the ticket; `abort` releases to the old slot, cancels nothing | the update's outcome should not cost a client its request |
| C15 | U4b ports the properties of the 45 checks (section 13), not the checks; U4b owns the loom permit test | about 15 checks describe Envoy config the proxy has no analogue of |

Node id = the one DNS name (section 8) and the closed route table (section 8) are also decisions of this note.

**Design versus code.** (a) Checks 39 to 42 (a worker `tcp_proxy`, TLS passthrough) contradict design section 3 (no passthrough): they are dropped, replaced by the worker identity tests. (b) Checks 14 and 31 (7200 s, Execute idle 0) agree with section 3. (c) The design's `/v1/update/frontend*` is a prefix; the skeleton matches on a path boundary, so `/v1/update/frontendx` is an operator path.

## 13. Test plan: the 45 Envoy checks

Test names are `kbf-proxy` tests unless marked `it:` (in `kbf-it`). "In skeleton" means the test exists now. Each row names the mutant that must go red.

| Checks | Proxy property | Test | Mutant |
|---|---|---|---|
| 1, 2, 3, 7 envoy-shape, hcm, codec, vhost | no config surface; unknown keys refused; gRPC trailers pass an h2 hop | `a_broken_table_is_refused_at_start` (in skeleton); `it: grpc_trailers_pass` | accept unknown keys |
| 4, 5, 13 max-stream-duration, request-timeout, route-timeout | no stream deadline | `no_stream_deadline_3h_paused` (a 3 h unary and a 3 h ByteStream Write) | a 60 s stream timer |
| 14, 31, 41 idle | Execute and WaitExecution never idle; 7200 s elsewhere | `only_execute_and_wait_execution_are_never_idle` (in skeleton); `idle_3h_paused_not_closed` | Execute on the 7200 s path |
| 6 filters | no buffering of a write | `bytestream_write_memory_is_constant` (1 GiB through, peak RSS bounded) | buffer the body |
| 8, 9, 11 no-catch-all, route-match, route | closed table, boundary match, every service has a route | `a_path_outside_the_table_is_never_forwarded`, `the_update_api_is_the_proxys_own_and_matches_on_a_boundary`, `every_proto_service_has_a_route` (reads the descriptors) (first two in skeleton) | prefix match without boundary; forward unknown |
| 10, 32 route-action, retry-routes | retry only before the first response, on safe failures | `only_a_safe_failure_is_retried_and_never_bytestream`, `an_upstream_response_closes_the_window` (in skeleton); `bytestream_not_retried_after_first_frame` | retry ByteStream; retry after a response byte |
| 12, 26, 37, 38 route-cluster, endpoints, instances, instance-ports | table gives every target a distinct loopback port | `slot_ports_are_fixed_offsets` (in skeleton); `upstream_table_ports_distinct` | overlapping offsets |
| 15, 16, 18, 42 health-check* | `/readyz` is 200 only with an active ready child | `readyz_needs_active_ready_child` | 200 with no child |
| 19, 33 panic-threshold, health-keeps-streams | an unhealthy child still gets the stream bound to it; open streams untouched | `child_health_failure_leaves_open_streams` | close streams on health failure |
| 17, 21 to 25, 27 upstream TLS, CA, ALPN, SNI, SAN | n/a: the hop is loopback h2c; the property it bought is "only loopback" | `upstream_dials_loopback_only` | a table entry with a remote host |
| 20 upstream-h2 | prior-knowledge h2c | `upstream_is_h2c` | HTTP/1.1 upstream |
| 28, 29 listeners, reuse-port | one activated socket | `refuses_without_exactly_one_listen_fd` | bind its own port |
| 30 listener-tls | TLS only; ALPN h2 and http/1.1 | `plaintext_refused`, `alpn_h2_and_http11_only` | allow plaintext |
| 34, 36 breakers, upstream-streams | open more connections rather than queue; honour the child's stream limit | `it: five_thousand_held_streams_unary_latency` | queue behind one connection |
| 35 upstream-keepalive | PING 30 s / 20 s both sides | `keepalive_closes_dead_peer_only` | close a live idle stream |
| 39, 40 worker-listener, worker-tls | n/a (dropped, section 12): replaced | `worker_session_carries_certificate_identity` | pass the certificate's role unchecked |
| 43 admin-loopback | no admin listener | `proxy_binds_only_the_activated_socket` | open an admin port |
| 44, 45 parent-shutdown, drain-time | n/a (Envoy hot restart); the proxy exposes the count restart-on-idle needs | `open_streams_counts_both_slots_and_held` | count one slot |

## 14. Test plan: the U4b proofs

| Proof (design section 12) | Test | Mutant that must go red |
|---|---|---|
| 200 streams on A, flip to B, every A stream completes, no GOAWAY or RST | `flip_200_streams_complete_on_a` (fake upstreams, one h2 connection; `streams_stay_on_the_slot_they_bound_to` in skeleton) | GOAWAY on flip |
| One connection carries streams to both slots | `one_connection_binds_per_stream` | per-connection routing (bind once at connect) |
| Idle 3 h under paused time not closed | `idle_3h_paused_not_closed` | idle timer on Execute |
| ByteStream not retried after the first frame | `bytestream_not_retried_after_first_frame` | retry ByteStream |
| 1000 held streams buffer at most the window and expire to `UNAVAILABLE` | `hold_1000_streams_bounded_and_expire` (`a_thousand_held_streams_buffer_at_most_the_window_each_and_expire` for the bound, in skeleton) | no window cap; no expiry |
| Trusting an inbound `x-kbf-node-id` | `inbound_identity_headers_never_reach_the_child` (in skeleton), plus a fake child that records headers end to end | skip the strip |
| A window with no active child | `a_closed_slot_cannot_become_active_or_bind` (in skeleton); `it` supervisor test | `set_active` without the closed check |
| Retire races a bind | `loom: bind_vs_flip_retire`; `a_racing_flip_and_retire_never_leave_a_permit_on_a_closed_slot` (in skeleton) | load-then-increment bind |
| Fenced re-dispatch | `fenced_trailers_only_redispatched`, `fenced_with_closed_window_reaches_client_as_unavailable` | re-dispatch with a body or two markers |
| Soak (`kbf-it`, real children, U4b and U7b) | `it: soak_swap_200_streams`: the assertions of design section 12 that need only U4b (no GOAWAY, pre-flip streams OK, post-flip served by the new child, node id equals the certificate's, counters 0, old child exits after its last stream) | route per connection; retire on a 5 s timer; GOAWAY at flip; retire with no active child. The U2/U5/U7 assertions (lease, epoch, exactly-one-run, 11-minute Execute) are run by U7b |
| `x-kbf-*` is the child's contract | `it: child_accepts_proxy_headers` against the real `kbf-server` U4a gate | change a header name |

## 15. PR split

All stacked on U4a; each PR is independently reviewable and merges in order. None is merged by the author.

- **U4b-1: core routing, permits and hold, fake upstreams.** The skeleton (`slot`, `hold`, `route`, `replay`, `config`, `control`), the loom shim and model, `serve`/`h2`/`pool` over plaintext h2c on a test listener (no TLS, no auth), per-stream binding, hold gate and expiry, GOAWAY and reset mapping, `/metrics` counters. Proof: flip with 200 streams, one connection to both slots, hold 1000, 5000 queued streams plus unary latency, the loom test. Size: large; the h2 proxy core is most of it.
- **U4b-2: TLS, auth and identity.** `kbf-tls` optional-client constructor, `tls` with SIGHUP reload, `identity` wired to the handshake, `auth::decide` on every route, `x-kbf-*` strip and set, bearer rate limit, secret file create-or-read, `h1`, the enrollment-path decision. Proof: the identity and auth tests, the end-to-end fake child that records headers, `it: child_accepts_proxy_headers`. Depends on U4b-1.
- **U4b-3: keepalive, replay and retry, soak and the verb.** PING keepalive both sides, replay and fenced re-dispatch wired, h2 flood limits and their attack tests, `/readyz` polling, `kbf-installer proxy` (`Verb::Proxy`, `built_by`, dispatch, `--check`), the installer dependency and the layering test, the `kbf-it` soak subset. Depends on U4b-2.

## 16. Skeleton status and what was not verified

The skeleton adds `crates/kbf-proxy`, `http` and `kbf-proxy` to the workspace, and one line `kbf-proxy 0 0` to `coverage-baseline` (a new crate goes in as `0 0`, as `kbf-coverage tighten` documents; every function in the skeleton is exercised by a test, but the coverage job has not run). It uses only dependencies already in `Cargo.lock`; the lock entry for `kbf-proxy` was written by hand and cargo will correct it if it differs.

Not done unless stated in the branch's commit message: `cargo check` and `clippy` were held back by the local disk gate (free space under 300 GB); this note is written from reading `kbf-peer`, `kbf-server/src/proxy_auth.rs` and the design, not from running them. Not verified: hyper/h2 builder names for the flood limits and keepalive (U4b-1 reads the pinned versions); which listener serves `Enroll` and `ColdStore`; that rustls can express "client certificate optional, verified if present, failure is a handshake error" with `WebPkiClientVerifier::builder(..).allow_unauthenticated()` (rustls 0.23 has `allow_unauthenticated()` on the verifier builder, but `kbf-tls` does not use it today; U4b-2 confirms against the pinned version).
