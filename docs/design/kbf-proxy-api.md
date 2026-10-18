# kbf-proxy: public API, module layout and U4b plan

Status: design for review, unit U4b of `docs/design/frontend-proxy.md` (the design; section numbers below are its). Stacked on U4a (`kbf-server/src/proxy_auth.rs`, the child's side of the identity contract). Refs #1095. The crate skeleton in `crates/kbf-proxy` holds the parts that are plain values, with tests; everything marked "(U4b-n)" is not in it yet. Where this note and the design differ, this note says so. Revised after three reviews (concurrency and h2; seams for U7 and U8; a re-check of both) and support's binding needs for U7b and U8b: the Review log at the end says what each finding became.

## 1. What U4b is

`kbf-proxy` is a library; `kbf-installer proxy` (verb, U4b-3) is its only binary. It is the public port of a frontend host: one TLS terminator, per-stream routing to the active child frontend on loopback, identity headers, the hold gate, open-stream accounting, and a pluggable handler for `/v1/update/frontend*`. It supervises nothing (U7b), calls no child admin endpoint (U7a's supervisor does, through a trait that lives in `kbf-installer`), writes nothing under `current`, `slots/`, `status.json` or `update.json`, and has no control socket.

Reusable from the repo: `kbf-front/src/proxy/bytestream.rs` (peek, keep the first frames, fail over only before the first byte) is the pattern for the replay buffer. Nothing else in `kbf-front/src/proxy` applies: that proxy is the frontend talking to storage nodes. `kbf-tls` is not reusable as is: `Incoming` is a tonic stream, and `config.rs` builds the client verifier with `.build()` only (certificate mandatory) and ALPN `h2` only. U4b-2 adds to `kbf-tls` a second constructor, `server_config_optional_client(...)` (optional client certificate, ALPN `h2`, `http/1.1`, same ring provider, early data off); the existing path is untouched.

## 2. Module map

| Module | Holds | In skeleton |
|---|---|---|
| `slot` | `Slot`, `ActiveSlot` (gate, permits, generations), `Permit`, `HoldGuard`, `Pending`, `Bind`, `Retire` | yes |
| `hold` | `HoldLimits`, `HoldQueue` (admit, expire), `Ticket`, `Wake` | yes |
| `route` | `Route`, `Target`, `AuthClass`, `Idle`, the closed path table | yes |
| `auth` | `decide`, `Buck2Auth`, `Token`, `Tokens`, `Refusal` | yes |
| `identity` | `Identity`, `identity_of`, `ProxySecret`, `forward_headers`, `admin_headers`, `strip_reserved` | yes |
| `replay` | `ReplayBuffer`, `Failure`, `may_retry`, `is_fenced`, the fenced contract | yes |
| `config` | `ProxyConfig`, `UpstreamTable`, `Limits` | yes |
| `control` | `UpdateHandler`, `UpdateRequest`, `UpdateReply`, `ProxySnapshot`, `SlotStats`, `NoUpdates` | yes |
| `tls` | `TlsReloader` (cert, key and CA as one unit, SIGHUP), the handshake | U4b-2 |
| `serve` | accept loop, per-connection task, `catch_unwind`, limits, rate limiter | U4b-1 |
| `h2` | downstream h2 server, per-stream dispatch, windows, GOAWAY and reset mapping | U4b-1 |
| `pool` | upstream h2c pool per (slot, generation, target), custom over hyper's `conn::http2` | U4b-1 |
| `h1` | HTTP/1.1 for update, probes and `/v1/*` only | U4b-2 |
| `probe` | `/healthz`, `/readyz`, `/metrics` | U4b-1 (metrics), U4b-3 |
| `lib` | `Proxy::bind`, `ProxyRunner`, `ProxyHandle` | U4b-1 |

Dependencies of the finished crate: `hyper` (`http1`, `http2`, `server`, `client`: the pool is built on `hyper::client::conn::http2::SendRequest`, not on `hyper-util`'s `client-legacy`, which keeps one h2 connection per host and never dials more at `MAX_CONCURRENT_STREAMS`), `hyper-util` (`tokio`, `server-auto`), `h2` **directly** (the window, flood-limit and GOAWAY settings are not reachable through hyper's builders; h2 0.4.20 has one initial window per connection, no per-stream setter and no connection-only credit release, section 10), `tokio` (the skeleton already uses its `sync` feature: the hold doorbell is a `watch`), `tokio-rustls`, `rustls`, `kbf-tls`, `kbf-peer`, `futures`, `httparse`; `loom` as a dev-dependency. The workspace declares neither `h2` nor `tower` today and U4b adds nothing it does not use. U4b-1 declares the hyper features it needs explicitly.

## 3. Public API

The skeleton compiles the first block (signatures abridged to the load-bearing ones; docs are in the code). The second block is planned (U4b-1 to U4b-3). U7b and U8b code against both. Support's six needs map to it in the table after the blocks.

```rust
// slot: the binding rule. Slot word: bit 63 CLOSED, bit 62 RETIRED, rest = open permits.
// The gate is ONE AtomicU64: active slot, hold flag, the hold's bound (ms), a hold sequence. Every change is one CAS.
pub enum Slot { A, B }                       // text form "a" / "b": name(), Display, FromStr, serde
pub struct ActiveSlot { /* Arc<..> */ }      // Clone; the shared handle
impl ActiveSlot {
    pub fn new() -> Self;                    // both slots closed, nothing active, not holding
    pub fn starting(hold_max: Duration) -> (Self, HoldGuard); // holding from the first instant (adoption)
    pub fn open(&self, slot: Slot) -> u64;   // child ready: bindable; returns the generation (an open slot keeps its own)
    pub fn adopt(&self, active: Slot, other: Option<Slot>);   // open both, set_active; hold untouched
    pub fn set_active(&self, slot: Slot) -> Result<(), SwapError>; // Closed slot refused; hold untouched
    pub fn clear_active(&self);              // no active slot; a hold in force stays, with its bound
    pub fn hold(&self, hold_max: Duration) -> Result<HoldGuard, AlreadyHolding>; // Err changes nothing
    pub fn release_into(&self, slot: Slot) -> Result<(), SwapError>;  // set active AND end the hold, one CAS
                                             // no hold: only sets active. Closed slot: Err, hold stays
    pub fn active(&self) -> Option<Slot>;  pub fn holding(&self) -> bool;
    pub fn epoch(&self) -> u64;              // moves each time a hold ends, whichever way
    pub fn subscribe(&self) -> watch::Receiver<u64>;  // the doorbell: the epoch, rung by `Inner::ring`
    pub fn abandoned_holds(&self) -> u64;    // holds ended by a guard's drop (kbf_proxy_hold_abandoned_total)
    pub fn bind(&self) -> Bind;   // Bound(Permit) | Held { epoch, hold_max } | NoActive
    pub fn begin_stream(&self) -> Pending;   // counted from accept; a retry begins one BEFORE dropping its old permit
    pub fn open_streams(&self) -> u64;       // pending first, then both slots' permits: a hint, never 0 while one lives
    pub fn open_permits(&self, slot: Slot) -> u64;  pub fn generation(&self, slot: Slot) -> u64;
    pub fn can_retire(&self, slot: Slot) -> bool;   // read-only probe: open, not active, 0 permits
    pub fn retire(&self, slot: Slot) -> Retire;     // the commit: CAS 0 -> CLOSED|RETIRED
}
pub enum Retire { Closed, AlreadyClosed, NeverOpened, Current, Busy(u64) } // signal a child ONLY on Closed (this call)
pub struct HoldGuard;   // no methods. Drop while its hold is in force = release into NOTHING: no active slot,
                        // held and later streams get UNAVAILABLE, abandoned_holds + 1. After release_into: inert.
pub struct Permit;      impl Permit { pub const fn slot(&self) -> Slot; pub const fn generation(&self) -> u64; }

// hold
pub const ALL_IN_ONE_HOLD: Duration;         // 300 s; a frontend swap uses ProxyConfig::frontend_hold()
pub const STREAM_WINDOW_BYTES: u32;          // 256 KiB: the ONE h2 window of every stream, both directions
pub struct HoldLimits { pub queue_streams: usize, pub window_bytes: u32 }   // Default: 2048, STREAM_WINDOW_BYTES
impl HoldLimits { pub const fn worst_case_bytes(&self) -> u64; }            // queue * (window + 16 KiB)
pub struct HoldQueue;  impl HoldQueue {
    pub fn new(l: HoldLimits) -> Self;
    pub fn admit(&self, now: Instant, hold_max: Duration, since: Option<Instant>) -> Result<Ticket, QueueFull>;
    pub fn expire(&self, now: Instant) -> usize;     // the sweep: fail streams past their bound, count, return how many
    pub fn held(&self) -> usize;  pub fn expired_total(&self) -> u64; }   // kbf_proxy_hold_expired_total
impl Ticket { pub fn wake(&self, now: Instant, released: bool) -> Wake;  // Wait | Bind | Expire: one CAS, shared with expire
    pub fn since(&self) -> Instant;  pub fn deadline(&self) -> Instant; } // pass since() back to admit when held again

// route and auth: pure.
pub fn route_of(path: &str) -> Route;        // Child(Target) | Update | Health | Ready | Metrics | Unknown
pub const fn auth_class(r: Route) -> Option<AuthClass>;
pub const fn bypasses_hold(r: Route) -> bool; // true for everything that does not go to a backend
pub fn idle_of(path: &str) -> Idle;          // Never for Execute and WaitExecution, else 7200 s
pub fn decide(c: AuthClass, m: Buck2Auth, t: &Tokens, p: Presented<'_>) -> Result<(), Refusal>;

// identity: what the child sees.
pub fn identity_of(der: &[u8]) -> Result<Identity, IdentityError>; // role URI + the ONE plain DNS name
pub fn forward_headers(h: &mut HeaderMap, id: Option<&Identity>, s: &ProxySecret)
    -> Result<(), IdentityError>;            // strip x-kbf-*, then set node-id, role, proxy-auth
pub fn admin_headers(s: &ProxySecret) -> Result<HeaderMap, IdentityError>; // secret, no identity
pub fn strip_reserved(h: &mut HeaderMap);    // on every child answer

// replay
pub struct ReplayBuffer;  // NOT Copy. push(n) -> bool, end_request(), window_open(), replayable(), close()
impl ReplayBuffer { pub fn on_first_response(&mut self, h: &HeaderMap, end_stream: bool) -> bool; } // fenced? read BEFORE close
pub enum Failure { Connect, RefusedStream, GoAway, Fenced, Other }
pub fn may_retry(path: &str, f: Failure, b: &ReplayBuffer, attempts: u8) -> bool;
pub fn is_fenced(h: &HeaderMap, end_stream: bool) -> bool;

// control: the update API. (ChildControl is NOT here: U7a defines it in kbf-installer.)
pub const MAX_UPDATE_BODY: usize;  pub const UPDATE_DEADLINE: Duration;   // 4096 bytes, 30 s
pub struct UpdateRequest { pub method: String, pub path: String, pub body: Vec<u8> }
pub struct UpdateReply { pub status: u16, pub headers: Vec<(HeaderName, HeaderValue)>, pub body: Vec<u8> }
impl UpdateReply { pub const fn json(status: u16, body: Vec<u8>) -> Self;
    pub fn with_header(self, n: HeaderName, v: HeaderValue) -> Self;
    pub fn payload_too_large() -> Self; pub fn timed_out() -> Self; }   // 413, 504: the proxy's own
pub trait UpdateHandler: Send + Sync {       // commands idempotent; the proxy lets a handler finish after a 504
    fn handle(&self, r: UpdateRequest) -> impl Future<Output = UpdateReply> + Send; }
pub struct ProxySnapshot { pub active: Option<Slot>, pub holding: bool, pub held: usize,
    pub per_slot: [SlotStats; 2], pub ready: bool, pub holds_abandoned: u64 }   // stats(slot)
pub struct SlotStats { pub open_streams: u64 }
// config: ProxyConfig::from_toml_str, ProxyConfig::frontend_hold() -> Duration,
//         UpstreamTable::{port, admin_port}, Limits (max_streams, memory_budget_mib,
//         Limits::stream_ceiling_bytes(), worst_case_bytes(): checked against the budget at start).
```

Planned, not in the skeleton:

```rust
pub struct Proxy;  impl Proxy {
    /// Takes the systemd-activated listener (fd 3, LISTEN_FDS == 1 or an error), builds the
    /// runtime, reads or creates the secret, and returns without serving. The gate starts
    /// holding (`ActiveSlot::starting(cfg.frontend_hold())`).
    pub fn bind(cfg: ProxyConfig, listener: OwnedFd) -> io::Result<(ProxyHandle, ProxyRunner)>;
}
pub struct ProxyRunner;  impl ProxyRunner {
    /// Serves on the calling thread (a synchronous `main`) until `shutdown`. `updates` is
    /// built from the handle first, so U8a's actor can hold one. A startup hold the
    /// supervisor has not released within `hold_max_s` of this call is dropped by the proxy.
    pub fn run<U: UpdateHandler + 'static>(self, updates: U) -> io::Result<()>;
}
pub struct ProxyHandle { /* Clone; no generics */ }
impl ProxyHandle {
    pub fn runtime(&self) -> tokio::runtime::Handle;   // U7a's async actor runs here, no second runtime
    pub fn slots(&self) -> &ActiveSlot;                // the gate: hold, release_into, active, clear_active, open, adopt, retire
    pub fn expire(&self) -> usize;                     // HoldQueue::expire(now): the swap calls it on each poll
    pub fn startup_hold(&self) -> Option<HoldGuard>;   // take once; release_into ends adoption
    pub fn snapshot(&self) -> ProxySnapshot;           // GET /v1/update/frontend; a hint
    pub fn open_streams(&self) -> u64;                 // restart-on-idle (U7b); a hint
    pub fn secret_origin(&self) -> SecretOrigin;       // Created | Existing
    pub fn admin_headers(&self) -> HeaderMap;          // every call to a child admin port carries these
    pub fn reload_tls(&self) -> Result<(), TlsReloadError>; // what SIGHUP calls
    pub fn shutdown(&self);                            // also what U7a's actor calls if it exits or panics
}
```

| Support's need (U7b/U8b) | API | Exactly as asked? |
|---|---|---|
| 1. `hold(max)` holds only backend streams; update API, probes, metrics bypass; 15 s / 300 s | `slots().hold(max)`; `bypasses_hold` is true for every route that is not `Child(_)`; `frontend_hold()` 20 s default (config `hold_max_s = 15` gives 15), `ALL_IN_ONE_HOLD` 300 s | yes; the config default stays 20 s, set 15 to match |
| 2. `release_into(slot)`: set active and release every held stream, atomically; none held: only sets active; adopted-held and old-slot rollback | `slots().release_into(slot)`: one CAS on the gate word, rings the doorbell only if a hold ended; works on any hold (guard or `starting`), into any open slot | yes; a closed slot is refused and the hold stays |
| 3. `expire()` fails streams past `hold_max`, counts, returns the count | `ProxyHandle::expire()` over `HoldQueue::expire`; the stream's own deadline stays as the guarantee; each stream is failed and counted once by whichever acts first | yes; both kept, see section 6 |
| 4. read the active slot as an Option | `slots().active() -> Option<Slot>` | yes |
| 5. `clear_active` for both children dead | `slots().clear_active()`; keeps the hold; then `open`, `release_into` | yes |
| 6. dropping the hold guard releases into nothing | `HoldGuard::drop`: active cleared, held streams `UNAVAILABLE`, counted; inert after `release_into` | yes; also clears the active slot, so later streams fail too (the alternative, keep the active slot, would bind to a quiesced child) |

There is no `set_ready`: readiness has one writer, the proxy's own poll. There is no `open_sessions`. `ProxySecret::create_or_read(path) -> io::Result<(ProxySecret, SecretOrigin)>` (U4b-2) writes a temp file, fsyncs and `link`s it into place, so a crash never leaves a 0-byte file; it adds no trailing newline (the child requires exactly 32 bytes); an existing file that is not 32 bytes is replaced the same way and reported `Created`.

## 4. Threading and async model

- One multi-thread tokio runtime built inside `Proxy::bind` (workers = min(cores, 8)), reachable as `ProxyHandle::runtime()`; `kbf-installer`'s `main` stays synchronous and calls `ProxyRunner::run`. One accept task; one task per connection; hyper spawns the per-stream service futures. **Panics:** each connection task is wrapped in `FutureExt::catch_unwind`, and each stream future is wrapped too (a stream panic is a tokio `JoinError`, not a connection close, so the connection wrap does not see it). Both increment `kbf_proxy_panics_total`; a panic closes that connection or resets that stream and nothing else. The wrap is the blast-radius limit, not error handling.
- The bound data path takes no lock: `ActiveSlot`, the permits and the counters are atomics. The locks: the TLS config (`RwLock<Arc<ServerConfig>>`, read once per handshake), the pool's connection list (per target, held for a pick, never across an await), and the hold registry's mutex (`HoldQueue`: admit, ticket drop and the `expire` sweep), which only a held stream or the supervisor takes.
- **One writer for control.** `set_active`, `clear_active`, `hold`, `release_into`, `open`, `adopt` and `retire` are called by one task (U7a's actor; design section 4). The proxy calls no `ChildControl` method, so the proxy type has no `ChildControl` parameter. If that actor exits or panics it calls `ProxyHandle::shutdown()` (fail-stop): a proxy that keeps serving with a dead supervisor answers every update call 504 and holds nothing, so systemd restarts it and adoption runs.
- Streams read the clock only through a `Clock` (paused time in tests); the skeleton's pure functions take `Instant` as input.
- **Memory** is bounded by counts, and the counts are knobs. h2 0.4.20 has one initial window per connection and no per-stream setter, so every stream's window is `STREAM_WINDOW_BYTES` (256 KiB) in each direction, held or bound, and the connection window is `1000 streams x window`: no set of held bodies can exhaust it, so an update POST or `/healthz` on that connection never stalls on credit (a promise to the client; it costs memory only if data arrives, and arrival is capped by the stream counts). One stream pins at most `2 x window + replay buffer (64 KiB) + header list (16 KiB)` = 592 KiB (`Limits::stream_ceiling_bytes`); the replay term is held for a 3 hour Execute's whole life. `limits.max_streams` (default 8192; bound, held and dialing together; one more is refused `UNAVAILABLE` at header time) times that must fit `limits.memory_budget_mib` (default 8192, for a host of 16 GiB or more) or the config is refused at start: 4.75 GiB by default. The hold queue is a subset: 2048 held x (256 KiB + 16 KiB) = 544 MiB. The proxy runs with `OOMScoreAdjust=-900`, so the kernel kills the children before it, which is why the budget is checked, not hoped for. The price of one window: a single upload moves at most window/RTT (5 MB/s at 50 ms, 250 MB/s at 1 ms); parallel streams are unaffected.

## 5. Per-stream binding and the permit

**Rule.** A stream binds to the slot active when its headers arrived. The bind is one atomic step on the slot word, not a load followed by an increment, because that pair races a retire.

**Mechanism.** Each slot is one `AtomicU64`: bit 63 `CLOSED`, bit 62 `RETIRED` (closed by a retire, as opposed to never opened), the rest count permits. `bind` loads the gate (active slot and hold flag), then `fetch_update`s the slot word: increment unless `CLOSED`. A closed slot sends it back to reload the gate; an unchanged gate on a closed slot is `NoActive` (never a spin). `retire(slot)` refuses `Current`, otherwise `compare_exchange(0, CLOSED|RETIRED)`: a stream that incremented first makes it fail (`Busy(n)`), one that comes later sees `CLOSED`. `open` of a closed or retired slot starts a new **generation**; slots are reused (A, B, A), so `open` must work on a retired slot.

**What the permit guarantees.** Only "the slot was not closed when the permit was taken". It does not mean the stream dispatches before a hold: bind reads the gate, then increments another word, and a pool-queued stream dispatches long after it bound. A stream can bind to A, the supervisor can hold and quiesce A, and the stream can dispatch to a child that answers fenced. That is expected, not a bug; the safety net is the fenced answer and a retry that goes back through `bind` (section 10). I did not add a re-check of the hold flag after the permit: it narrows the window without closing it and cannot be tested deterministically.

**Permit lifetime.** The permit is owned by the future that owns the upstream request and response pumps. It drops after both upstream halves are finished, reset or dropped, never at the downstream END_STREAM: a client-streaming request still open when the response completes keeps `open_permits` at 1 until the upstream RST is sent, or `retire` would return `Closed` and SIGTERM would hit a child still serving it. A queued stream (waiting for an upstream connection) holds its permit from header time. `Pending` counts a stream from accept; the permit is taken before the `Pending` drops, a retry calls `begin_stream()` before it drops its old permit, and `open_streams()` is one word counting live `Pending`s and permits (a stream holding both counts twice, so read it for `== 0`). It never reads 0 while a stream lives, between accept and bind, across a retry, or for a bypass stream (update API, probes, h1: a `Pending` for their life). Test: `open_streams_never_reads_zero_while_a_stream_lives`; it failed against the first version, a sum of the slot words and a separate pending count, which either read order misses one hand-over of.

**Retire.** `can_retire(old)` is the read-only probe a supervisor polls during bake; `retire(old)` is the commit. It returns `Closed` only on the call that closed the slot, `AlreadyClosed` for a repeat (a re-run after an actor restart), so a supervisor signals a child only on `Closed`. It is necessary but not sufficient: the supervisor also needs the child's own `open_streams == 0 && open_sessions == 0` (design section 5 step 10). `NeverOpened` is not proof a child is gone (a proxy restart before `adopt`). No timeout, no kill. A stuck stream stays visible. A child that was stopped needs no bake: its loopback sockets are gone, every upstream stream ends, and `retire` is polled until `Closed`. **Respawn rule:** `open` on an open slot is the same child and returns the same generation; to run a new child in a slot the caller stops the old one, `clear_active()`s if it is active, polls `retire` to `Closed`, then `open`s (a new generation, so the dead process's pool is never reused); `reopen` is not provided. I rejected making `open` fail on a retired slot (it would forbid A, B, A); a stale `open` is the single writer's discipline.

**Loom test (U4b-1; a hard requirement of that PR, not optional).** U4b-1 adds a private `sync` shim through which `slot.rs` takes its atomics (`loom::sync::atomic` under `cfg(loom)`; the skeleton imports `std::sync::atomic`). Model 1: thread 1 calls `bind` once; thread 2 calls `open(B)`, `set_active(B)`, `retire(A)`; thread 3 calls `hold`, then `release_into` or drops the guard. Invariants over every interleaving: a `Permit` for A exists only if `retire` returned `Busy`; if `retire` returned `Closed`, thread 1 got `Bound(B)`, `Held` or `NoActive`, never `Bound(A)`; no underflow; after the permit drops `retire` returns `Closed`. Model 2 (the lost wakeup): a binder samples the epoch inside `bind`, gets `Held`, and `release` lands before the binder parks; the binder's wait condition (`epoch != sampled`) must already be true, for every interleaving (the `watch` doorbell is rung by the same function after the epoch moves, and the waiter re-reads the epoch on wake; the skeleton tests the drop-on-panic ring). Model 3: a stream binds to A, `hold` and `retire`-attempt run, and the stream dispatches after: the permit keeps `retire` at `Busy`. The skeleton's racing test is the stress version (it retires in a loop and binds concurrently); the loom models are the gate.

**Mutants.** `bind` as load-then-`fetch_add`: model 1 and the racing test fail. `retire` that ignores the count: `streams_stay_on_the_slot_they_bound_to` fails. A `Notify` woken once instead of the epoch: model 2 fails. A permit dropped at response end: `open_permits_stays_1_until_upstream_rst` (U4b-1) fails.

## 6. The hold gate

Design section 5 step 6.1 and 5a. `hold(hold_max)` sets the hold flag, the bound and a sequence number in the same word as the active slot, in one compare-and-swap, so a stream either sees the hold with its bound (`Bind::Held`) or binds before it, and a rejected `hold` changes nothing. Only a stream going to a backend is held: `bypasses_hold` is true for every route that is not `Child(_)` (the update API, probes, metrics, and a path outside the table). A held stream is accepted, its headers are read (so the route is known and bypass works), nothing is opened upstream, and no credit is returned.

- **Wakeup (no lost release).** A stream calls `bind()`; on `Held { epoch, hold_max }` it admits a `Ticket` (`HoldQueue::admit(now, hold_max, since)`), then waits for "release epoch differs from `epoch` or the deadline". The epoch was sampled before the gate read; every end of a hold (`release_into`, the guard's drop, a panic's drop) goes through one function, `Inner::ring`, after the gate changed: it moves the atomic epoch, then writes the `watch<u64>` doorbell (`ActiveSlot::subscribe`) that lives inside `Inner`. The waiter parks on `wait_for(*e != sampled)`, which checks the current value first, and re-reads `epoch()` on every wake (the atomic is the truth, the channel only wakes the task). On wake the stream calls `bind()` again. Arrival order is **not** guaranteed: tasks wake in any order and a stream that arrives after the release can bind ahead of the queue.
- **One winner.** The stream decides in its own task, once, with `Ticket::wake(now, released)`: a seen release binds even if the deadline has passed too; an expiry answers trailers-only `UNAVAILABLE` and the task ends, so it can never also bind.
- **Ending a hold** (round 3, support's needs 2 and 6). `hold(hold_max)` takes the bound: `ProxyConfig::frontend_hold()` (`hold_max_s`, default 20 s: the 3 + 10 + 2 s budget plus a 5 s margin; set 15 for the 15 s support asked for) or `ALL_IN_ONE_HOLD` (300 s). It returns a `HoldGuard`. `release_into(slot)` ends the hold into `slot` in one CAS (held streams bind into it; with nothing held it only sets the active slot) and makes the guard inert. **Dropping the guard first ends the hold into nothing**: no slot is active, held and later streams get `UNAVAILABLE`, `kbf_proxy_hold_abandoned_total` ticks and the gate stays closed until a supervisor sets a slot. This **reverses round 2**, which released a dropped guard to the active slot; support (the consumer, U7b) asked for it, and it is their call: a dropped guard means the supervisor died mid-swap, and the active slot may be a quiesced child. A cancel path must `resume(old)` then `release_into(old)` before its guard drops. I rejected a second dead-man timer: a hung actor is U7a's watchdog, and each stream still expires at its bound.
- **Buffer** (C3, re-derived in round 3): h2 0.4.20 gives one initial window per connection (`initial_window_size`, SETTINGS) and one connection window; `FlowControl::release_capacity` returns connection and stream credit together (`recv.rs:465`) and refuses more than was received. So a held stream's window is the same `STREAM_WINDOW_BYTES` (256 KiB) as a bound one, the header list is capped at 16 KiB outside it, and a held stream **withholds all credit**: no `release_capacity` until it binds. The connection window is `1000 x window`, so held bodies cannot starve a bypass request on the connection (section 4). At most 2048 held streams (544 MiB worst case); the 2049th gets `UNAVAILABLE` at once and `kbf_proxy_hold_rejected_total` ticks.
- **Expiry** (support's need 3). Two mechanisms, one count. Each held stream has its own deadline (`since + hold_max`, from its first hold), and its task answers trailers-only `UNAVAILABLE` when it passes: this is the guarantee, with no one polling. `HoldQueue::expire(now)` (`ProxyHandle::expire()`) is the explicit sweep the swap calls on each poll: it fails every held stream past its bound now, counts them in `kbf_proxy_hold_expired_total` and returns how many it failed. Both decide with one compare-and-swap on the ticket (waiting to bound or to expired), so each stream is counted once, a swept stream stays failed even if a release follows, and an expired stream is never bound. Expiry does not release the gate or abort the update.
- **Held streams are not bound** (C2): no permit, counted in `held` and in `Pending`; `old.open_streams` excludes them.
- **Bypass:** `bypasses_hold` routes never queue. On h1 the proxy stops reading further requests on a held connection and at `hold_max` answers 503 with `Retry-After`.
- **Client reset while held:** the ticket drops. **`abort`:** the supervisor calls `resume` on the old child, then `release_into(old)`; held streams bind to the old slot. Crash: the hold is lost, accepted (design section 6).
- **Mutant "a window with no active child":** `set_active` and `release_into` refuse a closed slot and `bind` on no active slot is `NoActive`. After a stop with no live replacement the supervisor calls `clear_active()`, so a release answers `NoActive` at once instead of binding streams to a dead child for the 3 s connect timeout.

## 7. The upstream table, the supervisor's flows and the pool

**Static in U4b** (C10). `[proxy.upstream] base_port = N`. Slot A listens on `N + 0..3`, slot B on `N + 10..13`; offsets: REAPI 0, worker 1, operator 2, admin 3. All `127.0.0.1`; the table cannot name another host. Admin calls carry `ProxyHandle::admin_headers()` (the secret, no identity). U4a's gate covers the REAPI, worker and operator listeners; the admin listener is U2's and unbuilt, so **requirement on U2:** it refuses every request whose trust is not `Proxied` (test `it: admin_port_refuses_an_unproxied_caller`), and until then no claim is made that the admin port is closed to local processes. The 1 s readiness poll is a `GET` to the active child's admin port with `admin_headers` (the path is U2's). `UpstreamTable::port` and `admin_port` compute the ports.

**Frontend swap (U7a's supervisor, all through `ProxyHandle`).**
1. Start the candidate in the idle slot, wait ready, `slots().open(new)`.
2. `let hold = slots().hold(cfg.frontend_hold())?`; `control.quiesce(old)`; `control.takeover(new)`.
3. Success: `release_into(new)`. Failure: kill the candidate, **`retire(new)`** (a slot left open with no child would accept a later `set_active`), `control.resume(old)`, `release_into(old)`. Either way the guard is then inert; `ProxyHandle::expire()` is called on each poll while held.
4. `control.stand_down(old)`; poll `can_retire(old)` and the child's counts through bake; then `retire(old)` once, and on `Closed` SIGTERM.

**All-in-one swap (stop, then start; 7b).** 1. `hold(ALL_IN_ONE_HOLD)`. 2. Stop the old daemon, `clear_active()` at once, then poll `retire(old)` until `Closed` (its sockets are gone and the permits drain; a slot still `Busy` after the hold bound is reported stuck and never reopened). 3. Start the new one in the idle slot, wait ready, `open(new)`, `release_into(new)`. **New start fails:** `open(old)` returns a **new** generation because step 2 retired the slot (without the retire it would return the dead process's), then `release_into(old)`; **both dead:** start the recorded-active child fresh, `open`, `release_into`. Crash points: before the stop, the old daemon still serves and a cancelled supervisor calls `resume(old)` then `release_into(old)`; if its guard drops first the hold ends into nothing (streams `UNAVAILABLE`, `/readyz` 503, counted) until a slot is set; a proxy crash loses the hold (accepted). Tests: `all_in_one_failed_start_restarts_old_with_new_generation`, `both_children_dead_then_a_fresh_start_is_released_into`.

**Adoption after a proxy restart.** `Proxy::bind` starts the gate holding; the supervisor takes `startup_hold()`, checks pidfd, `/proc/<pid>/exe` and `role` on both children, calls `adopt(active, other)` and `release_into(active)` (or into the other slot, when the recorded update says rollback; the guard is not needed for that). `hold_max` bounds each stream's wait; a startup hold not released within `hold_max_s` of `run` is dropped by the proxy, so a supervisor that never arrives is `UNAVAILABLE` and `/readyz` 503, not a silent hold. If `secret_origin()` is `Created`, the adopted children hold a different secret and would read every call as anonymous: the supervisor refuses to adopt and restarts them.

**Readiness** has one writer (C10): the proxy polls the active child every 1 s, and `/readyz` is 200 only with an active child and a poll that passed within 5 s. The supervisor cannot set it.

**Pool** (C1), per (slot, generation, target). Prior-knowledge HTTP/2, no ALPN, built on `hyper::client::conn::http2::SendRequest`. A pick **reserves** a connection by CAS on its in-flight counter (`< advertised max`) before `send_request`, so pick and send are atomic and `h2` never queues behind one connection. Until the child's `SETTINGS` arrive the assumed max is 100. When every connection is full the stream dials: dials in flight count against the cap of 256 per (slot, target), at most 16 dial at once, and waiters wake on "stream freed or dial finished", so a release does not make thousands of tasks dial together and trip the 3 s connect timeout. A stream beyond the cap waits and keeps its permit. **Generation:** `retire` and a killed candidate close every pool connection of that (slot, generation); a reused slot opens a new generation, so the first stream after a restart never meets a dead connection. A child GOAWAY marks the connection draining (including the graceful form with `last_stream_id = 2^31-1`), is never forwarded, and a stream refused above `last_stream_id` is `Failure::GoAway`, retried. `GoAway` and `Connect` come from typed errors (`is_go_away()` with `is_remote()`, `TrySendError::take_message` returning the request unsent), never from error strings; test `goaway_below_last_stream_id_retries_the_in_flight_stream` (GOAWAY with `last_stream_id = 1` while stream 3 is in flight). A child that stops accepting connections fails new streams with `UNAVAILABLE` after 3 s; open streams are untouched. **Upstream keepalive:** PING every 30 s; an unanswered PING marks the connection suspect (no new streams) and never closes it, since a quiesce or a Postgres takeover can stall a loopback child past 20 s; death is the socket closing, with a 5 minute backstop.

## 8. TLS and identity

**Terminator.** One public listener, the activated socket (fd 3; refuse to start unless `LISTEN_FDS == 1` and `LISTEN_PID` is this process). Not tonic's `Incoming`: a raw accept loop wraps `TlsAcceptor` (`tokio-rustls`) over `kbf-tls`'s new optional-client config. ALPN `h2`, `http/1.1`; TLS 1.3 early data off; handshake, preface and request-head timeouts 10 s each. The client certificate is optional; a presented certificate that fails verification fails the handshake. After the handshake, `identity_of(leaf_der)` runs once per connection.

**Node id rule** (decision, the survey found no accessor): the node id is the certificate's one DNS name. Zero, two or more, or a name with anything but letters, digits, `-`, `_` and `.` (a wildcard, a space, a control character) make `identity_of` fail with `NodeId(n)`, and the proxy closes the connection: an identity that must be guessed is not one. The role is the one `urn:kbf:role:<name>` URI, via `kbf_peer::role_of`. Deny list: the child's own list covers `node` entries; the proxy adds none in U4b.

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

**Secret** (C13). `ProxySecret` is 32 bytes. `ProxySecret::create_or_read` returns the secret and `SecretOrigin::{Created, Existing}`: it reads `/run/kbf-proxy/proxy-auth` if present (so it survives a proxy restart and adopted children keep working), else writes a temp file (0600, owner `kbf`, no trailing newline), fsyncs, and `link`s it into place, so a crash never leaves a 0-byte file the child would refuse forever. `Created` means the adopted children hold the old digest and would see every call as anonymous: the supervisor refuses to adopt then (section 7). The directory is owned by the unit (`RuntimeDirectory=kbf-proxy`, `RuntimeDirectoryPreserve=yes`, which U7b writes); U4b-2 owns the code and its test (wrong mode or owner: refuse, as the child does). Children read it by `--proxy-auth-file`; never argv or env. Admin calls carry it too (`admin_headers`).

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
state_dir = "/var/lib/kbf-proxy"     # update.json and the swap state machine are U7a's
hold_max_s = 20                      # a frontend swap's hold; all-in-one is fixed at 300
[proxy.upstream]
base_port = 18000
[proxy.limits]                       # every key optional; defaults shown
pre_stream_timeout_s = 10   max_connections = 50000   max_connections_per_ip = 2000   max_streams = 8192
max_header_list_bytes = 16384   keepalive_interval_s = 30   keepalive_ack_s = 20
```

Unknown keys are refused at start. U4b-3 adds `proxy: Option<ProxyConfig>` to the installer's settings and keeps the installer's own deny-unknown rule. The operator token and update credential (C9) are read by the proxy; U8 injects a different `UpdateHandler`, not a different token source.

## 10. Keepalive, retry, GOAWAY and resets

- **No stream deadlines.** No route timeout, no max stream duration. Idle timeout: `idle_of(path)` is `Never` for `Execution/Execute` and `Execution/WaitExecution` (matched by full method path) and 7200 s for the rest.
- **Dead peers.** Downstream PING every 30 s, ack within 20 s; a failed ack closes that connection only. Upstream PING never closes (section 7).
- **Replay buffer** (C5): one buffer serves hold and retry. Request bytes are kept from the first byte, at most 64 KB, as `Bytes` clones (the skeleton counts bytes), freed at `close()`. A 3 hour Execute keeps its buffer until the first response frame, since tonic sends headers lazily. The window closes for good at an upstream response head that is not fenced, or when a frame would overflow it. `may_retry` is true only for: not ByteStream, fewer than 2 dispatches, window open **and the client's END_STREAM seen** (`replayable`), failure in `{Connect, RefusedStream, GoAway, Fenced}`. An open bidirectional request (`Worker.Session`) is never re-dispatched: no remainder is spliced onto a new stream.
- **Order at the first response head** (`on_first_response`): evaluate `is_fenced` on it first. Fenced: the window stays open, `may_retry` decides, and the caller `close()`s after the re-dispatch is issued or refused. Not fenced: close, then forward. Closing first would make a fenced answer unretriable.
- **Fenced-not-committed** (C6): a trailers-only answer with `grpc-status: 14` and exactly one `x-kbf-fenced: nothing-committed`. Any other shape (a body, two markers, another status) is not fenced. If `may_retry` refuses, the client gets the same `UNAVAILABLE` with the marker stripped, which a gRPC client retries by itself; correct because the marker means nothing committed.
- **A retry is a new bind**, not a reuse of the slot: reset the old upstream stream, call `begin_stream()` (so the stream is never in no count), drop the old permit, call `bind()` with `attempts + 1`. If a hold is in force the stream takes a queue place like any other (same `queue_streams` bound, a fresh ticket with the stream's first `since`, so chained holds cannot extend its wait past one bound) and goes to the slot the hold ends into; else to the active slot. Two attempts at most, so no loop.
- **Resets, in the order applied.** (1) `REFUSED_STREAM` or a GOAWAY above `last_stream_id`: `may_retry` first, before any mapping. (2) `RST_STREAM(NO_ERROR)` after the child sent a complete response: not an error, forward the response. (3) A reset on a non-gRPC path (`/v1/*`, `/metrics`): 502 on h1 or `RST(INTERNAL_ERROR)` on h2, never a gRPC trailer. (4) A gRPC path, headers not yet sent: trailers-only `UNAVAILABLE`. (5) Headers sent: a trailers frame with `UNAVAILABLE` follows the data; a data frame cut mid-message is followed by the same non-OK trailer, which tonic reports as a status error, never as a short OK message (tested with a tonic client). Client RST: propagated upstream as `RST_STREAM(CANCEL)`; the permit drops after it is sent. **Child GOAWAY:** never forwarded. **The proxy's own GOAWAY:** only on its shutdown and on a connection that trips a flood limit (a misbehaving connection only); a hold, a flip and a retire send none.
- **Flow control.** One window for every stream, 256 KiB (`STREAM_WINDOW_BYTES`), in both directions, held or bound: h2 0.4.20 cannot grow one stream's window after dispatch. A held stream returns no credit; a bound stream returns credit as the other side consumes (`release_capacity`). The connection window is 1000 x window, so no combination of held bodies stalls a bypass request. Adaptive BDP is off. Raising the window to buy upload speed raises `max_streams x window` in step; the memory-budget check at start enforces it.
- **h2 flood limits.** Derived from the 1000-stream cap, not left at the defaults of about 20: a Buck2 build cancel resets hundreds of streams at once, and tripping the limit would GOAWAY the connection with pre-flip streams on it. Max pending reset streams (rapid reset) is set above 1000, the header-list cap, CONTINUATION frame count and byte caps, concurrent streams per connection 1000. Test: 1000 client cancels on one connection trip nothing; a real rapid-reset attack does.

## 11. Pre-stream limits and the verb

Handshake, h2 preface and h1 request-head timeouts: 10 s each (`pre_stream_timeout_s`); `max_connections`, `max_connections_per_ip`; per-address rate limit before any token check. **HTTP/1.1** (C4) is served only for `/v1/*`, `/healthz`, `/readyz` and `/metrics`; the request head is parsed with `httparse`, at most 16 KB and 100 headers, origin-form targets only (`CONNECT`, absolute-form and `Upgrade` are 400), and a gRPC path on h1 is 426. The hold bypass is decided from the same parse.

**Verb.** `kbf-installer proxy [--config PATH] [--check]`: `--config` defaults to `/etc/kbf/installer.toml`; `--check` loads and validates the table (`ProxyConfig::from_toml_str`) and exits 0 or 2 without binding. No other flag: everything else is the `[proxy]` table, since configuration is flags and files, not the environment. The two environment variables it reads (`LISTEN_FDS`, `LISTEN_PID`) are the platform's socket-activation markers. To add it: a `Verb::Proxy` variant (`cli.rs`), a `built_by` arm, a dispatch arm (`app.rs`), `needs_root` false. It is not a `kbf-cli` mode: `kbf-cli`'s `GROUPS` rule (every flag of every mode in one group) then does not apply, but the child's new `--await-takeover` (U2) must join a group there. Installer dependency: `kbf-installer` gains `kbf-proxy` and with it tokio, hyper and rustls in the same binary as the root helper. **Risk, with a recommendation:** the root half links a network stack it never runs. Keep the design's single verb, and add a layering test (U4b-3) that the root helper's modules (`helper/`, `run`) name nothing from `kbf_proxy`; if the CEO prefers no shared binary, `kbf-proxy` gets its own `main` and the verb becomes `exec` of it, which is a unit-file change, not an API one.

## 12. Decisions the design left open

| # | Choice | Reason |
|---|---|---|
| C1 | h2c prior-knowledge pool per (slot, generation, target), up to 256 connections each, CAS reserve at pick, 16 dials at once, dial before queue; queued stream keeps its permit | the child's per-connection stream limit would otherwise queue 5000 streams behind long ones; a reused slot must not meet old connections |
| C2 | Held streams are not bound; bind at release in one step with the slot load | no slot yet; a permit on the old slot would block its retire |
| C3 | One 256 KiB window per stream (h2 has no per-stream setter), all credit withheld while held, connection window 1000 x window; 2048 held streams max; `max_streams` x 592 KiB checked against `memory_budget_mib` at start; overflow is `UNAVAILABLE` at once | bounds held memory to 544 MiB and total memory by a budget under `OOMScoreAdjust=-900`, using only what h2 can express |
| C4 | h1 only for `/v1/*` and probes; head <= 16 KB, 100 headers, origin-form; gRPC on h1 is 426 | h1 cannot multiplex, so holding it means stop reading |
| C5 | One buffer for hold and retry; fenced check first, then close; whole request only | one rule, no state to keep in step |
| C6 | Fenced = trailers-only, `grpc-status: 14`, one `x-kbf-fenced: nothing-committed` | a client that sees it retries correctly; strict shape cannot be forged by a body |
| C7 | Idle is chosen by full method path of Execute and WaitExecution | generic routing needs no per-stream state |
| C8 | Modes do not combine; wrong-role certificate is `PERMISSION_DENIED`; wrong token never falls back to a certificate | no downgrade to a weaker mechanism |
| C9 | The proxy reads `operator_token_file` itself; update credential = the same `Token`; U8 swaps the `UpdateHandler`, `decide` is the OIDC seam | design section 4 says "same token"; one function to replace |
| C10 | Static table is `base_port` plus fixed offsets; one readiness writer: `/readyz` is 200 when an active child's last poll (every 1 s, 3 s timeout) passed within 5 s; readiness never touches routing or open streams | design: a health failure does not touch open streams |
| C11 | `/metrics` is served by the proxy about itself, Prometheus text, operator token; the child's metrics are not forwarded | the series are different and the proxy must count its own streams |
| C12 | SIGHUP reloads cert, key and CA as one unit; a failed reload keeps the old one and counts | a half-applied reload would break handshakes |
| C13 | The proxy creates (temp + link) or reads the secret file and reports `Created`/`Existing`; the unit owns `/run/kbf-proxy/` (U7b) | the proxy restarts without the supervisor, so it cannot depend on it |
| C14 | A reset while held drops the ticket; `abort` is `resume(old)` then `release_into(old)`, cancels nothing; a dropped guard releases into nothing | the update's outcome should not cost a client its request; an abandoned hold must not guess a slot |
| C15 | U4b ports the properties of the 45 checks (section 13), not the checks; U4b owns the loom permit test | about 15 checks describe Envoy config the proxy has no analogue of |

Node id = the one DNS name (section 8) and the closed route table (section 8) are also decisions of this note.

**Design versus code.** (a) Checks 39 to 42 (a worker `tcp_proxy`, TLS passthrough) contradict design section 3 (no passthrough): they are dropped, replaced by the worker identity tests. (b) Checks 14 and 31 (7200 s, Execute idle 0) agree with section 3. (c) The design's `/v1/update/frontend*` is a prefix; the skeleton matches on a path boundary, so `/v1/update/frontendx` is an operator path. (d) The design's status JSON (`frontend-proxy.md`, step 5) lists `oldest_stream_age_s` as proxy-counted; the proxy does not provide it (a lock-free data path has no start-time per permit and no consumer asked), so the design line is stale.

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
| 34, 36 breakers, upstream-streams | open more connections rather than queue; honour the child's stream limit | `it: five_thousand_bound_long_lived_streams_unary_latency` | queue behind one connection |
| 35 upstream-keepalive | PING 30 s / 20 s both sides | `keepalive_closes_dead_peer_only` | close a live idle stream |
| 39, 40 worker-listener, worker-tls | n/a (dropped, section 12): replaced | `worker_session_carries_certificate_identity` | pass the certificate's role unchecked |
| 43 admin-loopback | no admin listener | `proxy_binds_only_the_activated_socket` | open an admin port |
| 44, 45 parent-shutdown, drain-time | n/a (Envoy hot restart); the proxy exposes the count restart-on-idle needs | `open_streams_counts_both_slots_and_the_pending` (in skeleton) | count one slot |

## 14. Test plan: the U4b proofs

| Proof (design section 12) | Test | Mutant that must go red |
|---|---|---|
| 200 streams on A, flip to B, every A stream completes, no GOAWAY or RST | `flip_200_streams_complete_on_a` (fake upstreams, one h2 connection; `streams_stay_on_the_slot_they_bound_to` in skeleton) | GOAWAY on flip |
| One connection carries streams to both slots | `one_connection_binds_per_stream` | per-connection routing (bind once at connect) |
| Idle 3 h under paused time not closed | `idle_3h_paused_not_closed` | idle timer on Execute |
| ByteStream not retried after the first frame | `bytestream_not_retried_after_first_frame` | retry ByteStream |
| 1000 held streams buffer at most the window and expire to `UNAVAILABLE` | `hold_1000_streams_bounded_and_expire`; `held_stream_credit_stops_at_window` (feed a held stream: no `release_capacity`, no credit past 256 KiB); `held_bodies_do_not_starve_a_bypass_request` (1000 held full-window bodies plus an update POST on one connection: the POST completes); `a_thousand_held_streams_expire_and_the_worst_case_is_window_plus_headers`, `the_sweep_fails_streams_past_the_bound_counts_them_and_returns_the_count` (in skeleton) | no window cap; no expiry; connection window left at the default; credit released while held |
| Trusting an inbound `x-kbf-node-id` | `inbound_identity_headers_never_reach_the_child` (in skeleton), plus a fake child that records headers end to end | skip the strip |
| A window with no active child | `a_closed_slot_cannot_become_active_or_bind` (in skeleton); `it` supervisor test | `set_active` without the closed check |
| Retire races a bind; a release races a held stream | loom models 1 to 3 (section 5); `a_racing_flip_and_retire_never_put_a_permit_on_a_closed_slot`, `a_release_after_held_is_never_missed` (in skeleton); `open_permits_stays_1_until_upstream_rst`; `retire_a_restart_a_first_stream_succeeds` | load-then-increment bind; `Notify` woken once; permit dropped at response end; pool kept across retire |
| Hold end and supervisor flows | `release_into_*`, `a_rejected_hold_does_not_rewrite_the_bound_in_force`, `a_dropped_guard_releases_into_nothing_and_is_counted`, `a_guard_dropped_on_a_panic_still_rings_the_doorbell`, `all_in_one_failed_start_restarts_old_with_new_generation`, `open_streams_never_reads_zero_while_a_stream_lives` (all in skeleton) | store the bound before the CAS; drop releases to the active slot; no retire in 7b; two-counter `open_streams` |
| Fenced re-dispatch | `fenced_trailers_only_redispatched`, `fenced_with_closed_window_reaches_client_as_unavailable`, `a_stream_bound_pre_hold_dispatched_post_quiesce_then_fenced_lands_on_b`, `an_open_bidirectional_request_is_not_redispatched`; `a_fenced_first_response_is_read_before_the_window_closes` (in skeleton) | re-dispatch with a body or two markers; close before the fenced check; retry onto the stale slot |
| Soak (`kbf-it`, real children, U4b and U7b) | `it: soak_swap_200_streams`: the assertions of design section 12 that need only U4b (no GOAWAY, pre-flip streams OK, post-flip served by the new child, node id equals the certificate's, counters 0, old child exits after its last stream) | route per connection; retire on a 5 s timer; GOAWAY at flip; retire with no active child. The U2/U5/U7 assertions (lease, epoch, exactly-one-run, 11-minute Execute) are run by U7b |
| `x-kbf-*` is the child's contract | `it: child_accepts_proxy_headers` against the real `kbf-server` U4a gate | change a header name |

## 15. PR split

All stacked on U4a; each PR is independently reviewable and merges in order. None is merged by the author.

- **U4b-1: core routing, permits and hold, fake upstreams.** The skeleton (`slot`, `hold`, `route`, `replay`, `config`, `control`), the loom shim and the three models of section 5 (the PR does not merge without them), the pool, `serve`/`h2`/`pool` over plaintext h2c on a test listener (no TLS, no auth), per-stream binding, hold gate and expiry, GOAWAY and reset mapping, `/metrics` counters. Proof: flip with 200 streams, one connection to both slots, hold 1000, 5000 queued streams plus unary latency, the loom test. Size: large; the h2 proxy core is most of it.
- **U4b-2: TLS, auth and identity.** `kbf-tls` optional-client constructor, `tls` with SIGHUP reload, `identity` wired to the handshake, `auth::decide` on every route, `x-kbf-*` strip and set, bearer rate limit, secret file create-or-read, `h1`, the enrollment-path decision. Proof: the identity and auth tests, the end-to-end fake child that records headers, `it: child_accepts_proxy_headers`. Depends on U4b-1.
- **U4b-3: keepalive, replay and retry, soak and the verb.** PING keepalive both sides, replay and fenced re-dispatch wired, h2 flood limits and their attack tests, `/readyz` polling, `kbf-installer proxy` (`Verb::Proxy`, `built_by`, dispatch, `--check`), the installer dependency and the layering test, the `kbf-it` soak subset. Depends on U4b-2.

## 16. Skeleton status and what was not verified

The skeleton adds `crates/kbf-proxy`, `http` and `kbf-proxy` to the workspace, and one line `kbf-proxy 0 0` to `coverage-baseline` (a new crate goes in as `0 0`, as `kbf-coverage tighten` documents; every function in the skeleton has a test, but the coverage job has not run). It uses only dependencies already in `Cargo.lock` (`serde_json` is a dev-dependency, for the `Slot` text form); the lock entry for `kbf-proxy` was edited by hand and cargo will correct it if it differs.

**Compiled** (round 3, on the local compile lease): `cargo check -p kbf-proxy --tests`, `cargo clippy -p kbf-proxy --all-targets -- -D warnings` and `cargo test -p kbf-proxy` all passed, 60 unit tests; the coverage job has not run. The first run of round 3 failed one test, `open_streams_never_reads_zero_while_a_stream_lives` (the two-counter `open_streams`), which is how the one-word count came about. Not verified: hyper/h2 builder names for the flood limits and keepalive (U4b-1 reads the pinned versions; the h2 0.4.20 window facts of section 10 were read from its source); which listener serves `Enroll` and `ColdStore`; that rustls can express "client certificate optional, verified if present, failure is a handshake error" with `WebPkiClientVerifier::builder(..).allow_unauthenticated()` (U4b-2 confirms against the pinned version). The loom models and the pool are U4b-1.

## 17. Review log

Critique 1 is concurrency and h2 (C1), critique 2 is seams for U7 and U8 (C2).

| Finding | Outcome |
|---|---|
| C1 M1 lost wakeup after `Held` | Fixed: `Bind::Held { epoch, hold_max }`, `epoch()`, release clears the flag then bumps the epoch; `watch` in U4b-1; loom model 2; test `a_release_after_held_is_never_missed` |
| C1 M2 permit dropped at stream end | Fixed in the note (section 5 lifetime rule, `Permit` doc); test `open_permits_stays_1_until_upstream_rst` is U4b-1 (no I/O in the skeleton) |
| C1 M3 close before the fenced check | Fixed: `ReplayBuffer::on_first_response`, order stated (section 10); tests for a fenced and an ordinary first response |
| C1 M4 redispatch of an open request | Fixed: `end_request`, `replayable` needs it; `Bytes` ownership and lifetime stated; `an_open_request_is_never_redispatched` |
| C1 M5 retry not through the gate | Fixed: retry = new bind, same queue and bound, fresh ticket (section 10); test `a_retry_rebinds_through_the_gate_and_waits_for_the_new_child` |
| C1 M6 "no third state" false | Fixed: claim replaced by "a permit implies not closed" (section 5, `slot` docs); loom model 3; the optional post-permit re-check is rejected (narrows only, untestable) |
| C1 M7 pool unbuildable, dial races | Fixed in the note: custom pool on `conn::http2`, CAS reserve, 16 dials, dials count against the cap, assumed max 100 before `SETTINGS`; U4b-1 builds it |
| C1 M8 pool survives slot reuse | Fixed: `open` returns a generation, `Permit::generation`, pool keyed by (slot, generation), purged on retire; `a_reopened_slot_gets_a_new_generation_and_its_permits_carry_it` |
| C1 M9 GOAWAY half specified | Fixed: `Failure::GoAway`, retried like `RefusedStream`; draining on the first GOAWAY; idle-connection write error is `GoAway`/`Connect` |
| C1 S1 upstream PING closes | Fixed (section 7): marks suspect, closes only after 5 minutes |
| C1 S2 held streams starve the connection window | Fixed in the note (connection credit returned, `h2` direct); test `held_bodies_do_not_starve_a_bypass_request` is U4b-1. Superseded by round 3 R1 M1: no connection-only release exists in h2 0.4.20 |
| C1 S3 memory bound is a hold bound | Fixed: `Limits::max_streams`, 160 MiB figure, tautology test replaced by an exact worst-case assertion; the held-credit test is U4b-1. Superseded by round 3 R1 M1 (256 KiB window, 592 KiB per stream) |
| C1 S4 64 KB window throttles uploads | Fixed: 64 KB only while held or queued, 1 MiB after dispatch (`BOUND_WINDOW_BYTES`), `h2` direct now. Superseded by round 3 R1 M1: h2 cannot raise one stream's window |
| C1 S5 reset mapping | Fixed: ordered decision table (section 10); tonic-client test is U4b-1 |
| C1 S6 `open_streams` gaps | Fixed: `Pending`, `begin_stream`, permit taken before the pending drops; `open_streams_counts_both_slots_and_the_pending` |
| C1 S7 flood limits GOAWAY a Buck2 cancel | Fixed in the note: derived from 1000 streams, GOAWAY exception stated, 1000-cancel test U4b-1 |
| C1 S8 failed takeover leaves an open slot | Fixed: `retire(candidate)` in the failure arm; `Retire::NeverOpened`; necessary-not-sufficient stated |
| C1 S9 weak racing test | Fixed: retire loops until `Closed` while binding; flag read before each bind; loom is a hard U4b-1 requirement |
| C1 N1 arrival order | Fixed: claim dropped, "not guaranteed" stated |
| C1 N2 `hold_max` margin | Fixed: default 20 s |
| C1 N3 panics per stream | Fixed: stream futures wrapped, `JoinError`s counted |
| C1 N4 test name vs bound | Fixed: renamed `five_thousand_bound_long_lived_streams_unary_latency` |
| C1 N5 single winner | Fixed: `Ticket::wake`, decided in the stream's task, release wins |
| C2 M1 `start` cannot host U7a/U8a | Fixed: `Proxy::bind` then `ProxyRunner::run(updates)`, `ProxyHandle::runtime()`, doc sentence fixed |
| C2 M2 `ChildControl` ownership | Fixed: trait, `ChildRole` and `ControlError` removed from the crate; U7a defines them in `kbf-installer`; no generic on `Proxy` |
| C2 M3 hold set forever | Fixed: `HoldGuard` releases on drop; the dead-man timer is rejected (hung actor is U7a's watchdog) |
| C2 M4 all-in-one policy | Fixed: `hold(hold_max)`, `ALL_IN_ONE_HOLD`, `clear_active`, section 7 flow 7b with crash points |
| C2 M5 adoption starts open | Fixed: `ActiveSlot::starting`, `adopt`, `startup_hold`; bounded by `hold_max` |
| C2 S1 regenerated secret | Fixed in the note: `SecretOrigin`, adopt refused on `Created`, temp + link, no newline; the file code is U4b-2 |
| C2 S2 admin port auth | Fixed: `identity::admin_headers`, `ProxyHandle::admin_headers`; test `an_admin_call_carries_the_secret_and_no_identity` |
| C2 S3 readiness two writers | Fixed: `set_ready` dropped, poll is the only writer |
| C2 S4 status too thin | Fixed: `ProxySnapshot` and `ProxyHandle::snapshot`; a hint, `retire` authoritative |
| C2 S5 `update.json` owner | Fixed: U7a owns it and the state machine, U8a asks it through a trait in `kbf-installer` (config comment, `UpdateHandler` doc) |
| C2 S6 reply too narrow | Fixed: `UpdateReply::headers`, 413 and 504 from the proxy, the proxy rate-limits; tests |
| C2 S7 `Slot` text form | Fixed: `name`, `Display`, `FromStr`, serde, lowercase only |
| C2 S8 `retire` as a probe | Partly: `can_retire` added, `NeverOpened` added; "`open` fails on a retired slot" rejected because slots are reused (A, B, A) |
| C2 nits | Fixed: DNS-character check in `identity_of`; layering test and route boundary endorsed, unchanged; skeleton uncompiled is stated above |

### Round 3

Re-check 1 is R1 (concurrency and h2), re-check 2 is R2 (seams), SU is support's binding needs for U7b and U8b (the consumer's contract).

| Finding | Outcome |
|---|---|
| R1 M1 window design needs h2 behaviour 0.4.20 lacks | Fixed: confirmed in `recv.rs:465` and `share.rs:532`; Option B taken: one 256 KiB window per stream, all credit withheld while held, connection window 1000 x window; `BOUND_WINDOW_BYTES` and per-stream growth removed; sections 4, 6, 10, `worst_case_bytes` and the two tests rewritten |
| R1 M2 rejected `hold()` rewrote the bound in force | Fixed: the bound lives in the gate word and is written by the same CAS that sets the hold; test `a_rejected_hold_does_not_rewrite_the_bound_in_force` (300 s then a rejected 20 s) |
| R2 M1 all-in-one never retired the stopped slot | Fixed: flow 7b is stop, `clear_active`, poll `retire` to `Closed`, then restart; `open` rule stated, no `reopen`; test `all_in_one_failed_start_restarts_old_with_new_generation` (shows `open` without a retire returns the dead generation) |
| R1 S1, R2 S2 `watch` mirror had no owner | Fixed: `Inner::ring` moves the epoch then writes the doorbell, called by `release_into` and the guard's drop; `subscribe()`; test `a_guard_dropped_on_a_panic_still_rings_the_doorbell`; tokio `sync` added to the crate |
| R1 S2 `open_streams` could read 0 | Fixed, and the proposed read order was not enough: either order of a two-counter sum misses one hand-over (accept to bind, or a retry), and the stress test failed on it. One `live` word counts `Pending`s and permits (read for `== 0`); a retry begins its `Pending` first |
| R1 S3 memory omitted the replay buffer; default not a bound | Fixed: 592 KiB per stream; `memory_budget_mib` (default 8192) checked against `max_streams` at start; config tests |
| R1 S4 `Closed` meant two things | Fixed: `Retire::AlreadyClosed`; a supervisor signals only on `Closed` |
| R1 S5, R2 S7 `oldest_stream_age` had no source | Fixed by dropping the field (no consumer asked); the design's status line is noted stale |
| R1 N1, R2 nit `ReplayBuffer` is `Copy` | Fixed: `Copy` removed |
| R1 N2, R2 S3 a dropped guard looks like a release | Fixed: counted as abandoned (`abandoned_holds`, `ProxySnapshot::holds_abandoned`); behaviour changed per SU 6 |
| R1 N3, R2 nit a held-again stream got a fresh deadline | Fixed: `admit(.., since)` keeps the first hold's start; test `a_stream_held_again_keeps_its_first_deadline` |
| R1 N4 GOAWAY and connect from error strings | Fixed in the note: typed errors, test `goaway_below_last_stream_id_retries_the_in_flight_stream` (U4b-1) |
| R2 S4 a dead supervisor | Fixed in the note: the actor's exit or panic calls `shutdown()`; an unreleased startup hold is dropped by `run` after `hold_max_s` |
| R2 S5 a 504 drops the handler future | Fixed: the handler runs in the proxy's own task and finishes; commands idempotent (`UpdateHandler` doc) |
| R2 S6 admin-port claim rests on unbuilt code | Fixed: claim removed; requirement and `it:` test recorded for U2 |
| R2 nits | `sync` shim marked U4b-1; non-32-byte secret file regenerated; `decide` principal and a query string on `UpdateRequest` deferred to U8 / OIDC #1088 (additive); not done |
| SU 1 hold only backend streams; 15 s / 300 s | Done: `bypasses_hold` is now true for every non-`Child` route (an unknown path was held before); `hold_max_s = 15` gives 15 s, default stays 20 s |
| SU 2 `release_into(slot)` | Added: one CAS, `ActiveSlot::release_into`; works with or without a guard, on `starting`, into the old slot |
| SU 3 `expire()` against per-stream deadlines | Both kept: own deadline is the guarantee, `HoldQueue::expire` / `ProxyHandle::expire` the explicit sweep; one CAS makes each stream fail and count once |
| SU 4 read of the active slot, SU 5 `clear_active` | Already present; mapped in section 3 |
| SU 6 a dropped guard releases into nothing | Done, **reversing round 2** ("HoldGuard Drop = release"). It is the consumer's call (support asked); the drop also clears the active slot, so later streams fail until a slot is set, because keeping the active slot would bind them to a possibly quiesced child. Supervisor cancel path: `resume(old)`, `release_into(old)` first |
| Round 3 housekeeping | Slot tests moved to `src/slot/tests.rs` (`slot.rs` passed 1000 lines); note kept under 450 lines |
