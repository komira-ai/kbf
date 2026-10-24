# kbf-proxy: public API, module layout and U4b plan

> **Retired in part.** The installer's two-slot supervision of the proxy's frontend children
> (the swap, the takeover, the handover driver, the update API at `/v1/update/frontend` and
> the `kbf-installer update` client; section 11 and the units it names) was removed:
> `kbf-installer proxy` serves a pool of frontends that run on their own (`[[proxy.frontend]]`
> tables) and refuses the `upstream` table. The crate's two-slot machinery went with it: the
> slots and the gate (`slot`), the hold queue (`hold`), the routing of a `WaitExecution` by the
> old child's term (`term`), the append of the active child's `/metrics` (`child_metrics`),
> the CI credential of the update API (`remote`), the fenced re-dispatch and the loom models.
> Sections 5, 6 and 7 are reduced to a note on what replaced them; the pool is section 9
> ("Pool mode") and the files of `crates/kbf-proxy/src/upstreams.rs`, `pool_probe.rs` and
> `pool_admin.rs`. Other passages below that describe the supervision are history.

Status: design for review, unit U4b of `docs/design/frontend-proxy.md` (the design; section numbers below are its). Stacked on U4a (`kbf-server/src/proxy_auth.rs`, the child's side of the identity contract). Refs #1095. The crate skeleton in `crates/kbf-proxy` holds the parts that are plain values, with tests; everything marked "(U4b-n)" is not in it yet. Where this note and the design differ, this note says so. Revised after three reviews (concurrency and h2; seams for U7 and U8; a re-check of both) and support's binding needs for U7b and U8b: the Review log at the end says what each finding became.

## 1. What U4b is

`kbf-proxy` is a library; `kbf-installer proxy` (verb, U4b-3) is its only binary. It is the public port of a frontend host: one TLS terminator, per-stream routing to the active child frontend on loopback, identity headers, the hold gate, open-stream accounting, and a pluggable handler for `/v1/update/frontend*`. It supervises nothing (U7b), makes one read-only admin call to a child, `GET role`, to learn the child's term (`term`; every other admin call is U7a's supervisor's, through a trait that lives in `kbf-installer`), writes nothing under `current`, `slots/`, `status.json` or `update.json`, and has no control socket.

Reusable from the repo: `kbf-front/src/proxy/bytestream.rs` (peek, keep the first frames, fail over only before the first byte) is the pattern for the replay buffer. Nothing else in `kbf-front/src/proxy` applies: that proxy is the frontend talking to storage nodes. `kbf-tls` is not reusable as is: `Incoming` is a tonic stream, and `config.rs` builds the client verifier with `.build()` only (certificate mandatory) and ALPN `h2` only. U4b-2 adds to `kbf-tls` a second constructor, `server_config_optional_client(...)` (optional client certificate, ALPN `h2`, `http/1.1`, same ring provider, early data off); the existing path is untouched.

## 2. Module map

| Module | Holds | In skeleton |
|---|---|---|
| `route` | `Route`, `Target`, `AuthClass`, `Idle`, the closed path table | yes |
| `auth` | `decide`, `Buck2Auth`, `Token`, `Tokens`, `Refusal` | yes |
| `identity` | `Identity`, `identity_of`, `ProxySecret`, `forward_headers`, `admin_headers`, `strip_reserved` | yes |
| `replay` | `ReplayBuffer`, `Failure`, `may_retry` | yes |
| `config` | `ProxyConfig`, `Frontend`, `UpstreamTable`, `Limits`, `SLOT_MODE_REMOVED` | yes |
| `control` | `UpdateHandler`, `UpdateRequest`, `UpdateReply`, `NoUpdates` | yes |
| `tls` | `TlsReloader` (cert, key and CA as one unit, SIGHUP), the handshake | U4b-2 |
| `serve` | accept loop, hyper's h2 server (windows, concurrent streams), per-connection task, the proxy's own routes (`/healthz`, `/readyz`, `/metrics`, the update API), shutdown drain | U4b-1 |
| `core` | the state every stream shares: the members, the pool, counters, `max_streams` count, the streams in flight | U4b-1 |
| `stream` | one stream: placement on a member, dispatch, retry on another member, GOAWAY and reset mapping | U4b-1 |
| `body` | the request `Source` (replay frames), the `pump`, the response `Down`, idle `Activity`, `Lifetime` | U4b-1 |
| `pool` | upstream h2c pool per (member address, target), on h2's own client (section 18) | U4b-1 |
| `reply`, `metrics` | the proxy's own answers; counters | U4b-1 |
| `probe` | `Readiness::ask`: the `GET /readyz` of a member's operator listener (connect, I/O error, timeout and non-200 each fail the poll) | U4b-3b |
| `upstreams`, `pool_probe`, `pool_admin` | the members of the pool (up or draining, ready or not, streams open), the poll of each, and the admin API on `admin_listen` | pool |
| `limits`, `keepalive` | the h2 builders and flood limits, `STREAM_WINDOW_BYTES` and `HEADER_BYTES`; PING keepalive both ways | U4b-3a |
| `h1` | HTTP/1.1 for update, probes and `/v1/*` only | U4b-2 |
| `proxy` | `Proxy::bind`, `ProxyRunner`, `ProxyHandle` | U4b-1 |

This table is the finished crate's map; section 18 says where U4b-1 as built differs (the module `h2` of the first plan is `stream`/`body`; `probe` came back in U4b-3b). The `slot`, `hold`, `term`, `child_metrics`, `remote` and `sync` modules of the two-slot design are gone.

Dependencies of the finished crate: `hyper` (`http1`, `http2`, `server`, `client`: the pool is not `hyper-util`'s `client-legacy`, which keeps one h2 connection per host and never dials more at `MAX_CONCURRENT_STREAMS`; U4b-1 built it on h2's own client instead of hyper's `conn::http2`, section 18), `hyper-util` (`tokio`, `server-auto`), `h2` **directly** (the window, flood-limit and GOAWAY settings are not reachable through hyper's builders; h2 0.4.20 has one initial window per connection, no per-stream setter and no connection-only credit release, section 10), `tokio`, `tokio-rustls`, `rustls`, `kbf-tls`, `kbf-peer`, `futures`, `httparse`. The workspace declares neither `h2` nor `tower` today and U4b adds nothing it does not use. U4b-1 declares the hyper features it needs explicitly.

## 3. Public API

The crate's public API (signatures abridged to the load-bearing ones; docs are in the code). The two-slot gate (`ActiveSlot`, `Permit`, `HoldGuard`, `Retire`) and the hold queue (`HoldQueue`) that this section once listed were removed with the supervision; section 5 says what places a stream now.

```rust
// limits (crate-private): the ONE h2 window of every stream, both directions, and the header cap.
pub const STREAM_WINDOW_BYTES: u32;          // 256 KiB
pub const HEADER_BYTES: u64;                 // 16 KiB, outside the window

// route and auth: pure.
pub fn route_of(path: &str) -> Route;        // Child(Target) | Ui | Update | Health | Ready | Metrics | Unknown
pub const fn auth_class(r: Route) -> Option<AuthClass>;
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
pub enum Failure { Connect, RefusedStream, GoAway, Other }
pub fn may_retry(path: &str, f: Failure, b: &ReplayBuffer, attempts: u8) -> bool;

// control: the update API's handler. The built-in NoUpdates answers 503.
pub const MAX_UPDATE_BODY: usize;  pub const UPDATE_DEADLINE: Duration;   // 4096 bytes, 30 s
pub struct UpdateRequest { pub method: String, pub path: String, pub body: Vec<u8> }
pub struct UpdateReply { pub status: u16, pub headers: Vec<(HeaderName, HeaderValue)>, pub body: Vec<u8> }
impl UpdateReply { pub const fn json(status: u16, body: Vec<u8>) -> Self;
    pub fn with_header(self, n: HeaderName, v: HeaderValue) -> Self;
    pub fn payload_too_large() -> Self; pub fn timed_out() -> Self; }   // 413, 504: the proxy's own
pub trait UpdateHandler: Send + Sync {       // commands idempotent; the proxy lets a handler finish after a 504
    fn handle(&self, r: UpdateRequest) -> impl Future<Output = UpdateReply> + Send; }
// config: ProxyConfig::from_toml_str, Frontend { name, base_port }, Limits (max_streams,
//         memory_budget_mib, Limits::stream_ceiling_bytes(), worst_case_bytes(): checked against
//         the budget at start). A table with the removed `upstream` table is read and written
//         back (`ProxyConfig::check`, `UpstreamTable`), and refused at the proxy's start
//         (`ProxyConfig::check_pool`) with SLOT_MODE_REMOVED.

pub struct Proxy;  impl Proxy {
    /// Takes the systemd-activated listener (fd 3, LISTEN_FDS == 1 or an error), builds the
    /// runtime, reads or creates the secret, binds the admin socket if `admin_listen` is set,
    /// and returns without serving. A config with no `[[frontend]]` table is refused.
    pub fn bind(cfg: ProxyConfig, listener: OwnedFd) -> io::Result<(ProxyHandle, ProxyRunner)>;
}
pub struct ProxyRunner;  impl ProxyRunner {
    /// Serves on the calling thread (a synchronous `main`) until `shutdown`.
    pub fn run<U: UpdateHandler + 'static>(self, updates: U) -> io::Result<()>;
}
pub struct ProxyHandle { /* Clone; no generics */ }
impl ProxyHandle {
    pub fn runtime(&self) -> tokio::runtime::Handle;   // the host's signal handlers run here, no second runtime
    pub fn open_streams(&self) -> u64;                 // restart-on-idle; a hint
    pub fn secret_origin(&self) -> SecretOrigin;       // Created | Existing
    pub fn admin_headers(&self) -> HeaderMap;          // every call to a child admin port carries these
    pub fn reload_tls(&self) -> Result<(), TlsReloadError>; // what SIGHUP calls
    pub fn shutdown(&self);
}
```

There is no `set_ready`: readiness has one writer per member, the proxy's own poll of it. `ProxySecret::create_or_read(path) -> io::Result<(ProxySecret, SecretOrigin)>` (U4b-2) writes a temp file, fsyncs and `link`s it into place, so a crash never leaves a 0-byte file; it adds no trailing newline (the child requires exactly 32 bytes); an existing file that is not 32 bytes is replaced the same way and reported `Created`.

## 4. Threading and async model

- One multi-thread tokio runtime built inside `Proxy::bind` (workers = min(cores, 8)), reachable as `ProxyHandle::runtime()`; `kbf-installer`'s `main` stays synchronous and calls `ProxyRunner::run`. One accept task; one task per connection; hyper spawns the per-stream service futures. **Panics:** each connection task is wrapped in `FutureExt::catch_unwind`, and each stream future is wrapped too (a stream panic is a tokio `JoinError`, not a connection close, so the connection wrap does not see it). Both increment `kbf_proxy_panics_total`; a panic closes that connection or resets that stream and nothing else. The wrap is the blast-radius limit, not error handling.
- The data path takes one short lock to place a stream (the members' state, `upstreams`: a pick, a drain and a count are never apart, and the lock is never held across an await); the counters are atomics. The other locks: the TLS config (`RwLock<Arc<ServerConfig>>`, read once per handshake) and the pool's connection list (per target, held for a pick, never across an await).
- Streams read the clock only through tokio's (paused time in tests); the skeleton's pure functions take `Instant` as input.
- **Memory** is bounded by counts, and the counts are knobs. h2 0.4.20 has one initial window per connection and no per-stream setter, so every stream's window is `STREAM_WINDOW_BYTES` (256 KiB) in each direction, and the connection window is `1000 streams x window`: no set of unread bodies can exhaust it, so an update POST or `/healthz` on that connection never stalls on credit (a promise to the client; it costs memory only if data arrives, and arrival is capped by the stream counts). One stream pins at most `2 x window + replay buffer (64 KiB) + header list (16 KiB)` = 592 KiB (`Limits::stream_ceiling_bytes`); the replay term is held for a 3 hour Execute's whole life. `limits.max_streams` (default 8192; bound and dialing together; one more is refused `UNAVAILABLE` at header time) times that must fit `limits.memory_budget_mib` (default 8192, for a host of 16 GiB or more) or the config is refused at start: 4.75 GiB by default. The proxy runs with `OOMScoreAdjust=-900`, so the kernel kills the children before it, which is why the budget is checked, not hoped for. The price of one window: a single upload moves at most window/RTT (5 MB/s at 50 ms, 250 MB/s at 1 ms); parallel streams are unaffected.

## 5. Placement of a stream and its permit

This section described the per-stream binding to one of two slots, the permit on a slot's state word, the generation of a slot, the retire and the loom models of the bind; they went with the two-slot supervision.

**Rule.** A stream is placed on a member of the pool when its headers arrived (section 9, "Pool mode"), by one pick under the members' lock, which counts the stream on the member in the same step. The count is the stream's **permit** on the member (`MemberPermit`): it drops after both upstream halves are finished, reset or dropped, never at the downstream END_STREAM, so a draining member's count reaches 0 only when its last stream is over. A stream that waits for an upstream connection keeps its permit from header time. `ProxyHandle::open_streams()` is the streams between accept and the answer's head (`Pending`) plus the streams open on the members, read for `== 0`.

## 6. The hold gate

This section described the hold gate of a frontend swap: the hold flag in the gate word, the hold queue, its wakeup and expiry, the guard whose drop ended a hold into nothing, and the bypass routes. The gate and the queue were removed with the supervision. A request that no member takes (every member draining, or each that could take it has failed it) is answered at once: trailers-only `UNAVAILABLE` for gRPC, 503 with `Retry-After: 1` for the rest. Nothing waits for a member to appear.

## 7. The members' ports and the pool

**Ports.** A member's listeners are at fixed offsets from its `base_port` (`[[proxy.frontend]]`, section 9): REAPI 0, worker 1, operator 2, admin calls 3, enrollment 4, all on `127.0.0.1` (the table cannot name another host). The readiness poll is a `GET /readyz` over plain HTTP/1.1 to a member's **operator** listener, the one every frontend already answers it on, once a second; each member has its own, and it is the only writer of that member's readiness (`/readyz` of the proxy is 200 while some member is up and ready).

**Pool** (C1), per (member address, target). Prior-knowledge HTTP/2, no ALPN, built on `hyper::client::conn::http2::SendRequest`. A pick **reserves** a connection by CAS on its in-flight counter (`< advertised max`) before `send_request`, so pick and send are atomic and `h2` never queues behind one connection. Until the child's `SETTINGS` arrive the assumed max is 100. When every connection is full the stream dials: dials in flight count against the cap of 256 per (slot, target), at most 16 dial at once, and waiters wake on "stream freed or dial finished", so a release does not make thousands of tasks dial together and trip the 3 s connect timeout. A stream beyond the cap waits and keeps its permit. **Members:** a member is a process that runs on its own, so its pools are keyed by where it listens, made when first asked for and kept; a connection of a process that died is found dead by the next stream that picks it (a send on it fails, or its socket closed), is dropped, and the stream dials again. A child GOAWAY marks the connection draining (including the graceful form with `last_stream_id = 2^31-1`), is never forwarded, and a stream refused above `last_stream_id` is `Failure::GoAway`, retried. `GoAway` and `Connect` come from typed errors (`is_go_away()` with `is_remote()`, `TrySendError::take_message` returning the request unsent), never from error strings; test `goaway_below_last_stream_id_retries_the_in_flight_stream` (GOAWAY with `last_stream_id = 1` while stream 3 is in flight). A child that stops accepting connections fails new streams with `UNAVAILABLE` after 3 s; open streams are untouched. **Upstream keepalive:** PING every 30 s; an unanswered PING marks the connection suspect (no new streams) and does not close it at 20 s, since a quiesce or a Postgres takeover can stall a loopback child past that; death is the socket closing, with a 5 minute backstop (a PING unanswered for 5 minutes closes the connection, streams on it included).

## 8. TLS and identity

**Terminator.** One public listener, the activated socket (fd 3; refuse to start unless `LISTEN_FDS == 1` and `LISTEN_PID` is this process). Not tonic's `Incoming`: a raw accept loop wraps `TlsAcceptor` (`tokio-rustls`) over `kbf-tls`'s new optional-client config. ALPN `h2`, `http/1.1`; TLS 1.3 early data off; handshake, preface and request-head timeouts 10 s each. The client certificate is optional; a presented certificate that fails verification fails the handshake. After the handshake, `identity_of(leaf_der)` runs once per connection.

**Node id rule** (decision, the survey found no accessor): the node id is the certificate's one DNS name. Zero, two or more, or a name with anything but letters, digits, `-`, `_` and `.` (a wildcard, a space, a control character) make `identity_of` fail with `NodeId(n)`, and the proxy closes the connection: an identity that must be guessed is not one. The role is the one `urn:kbf:role:<name>` URI, via `kbf_peer::role_of`. Deny list: the child's own list covers `node` entries; the proxy adds none in U4b.

**Reload** (C12). SIGHUP re-reads the server certificate, key and client CA bundle as one unit and builds a new `ServerConfig`; it replaces the old one only if the whole build succeeds. On failure the old config stays, `kbf_proxy_tls_reload_failed_total` ticks, the error is logged without any file content. Only new handshakes see the new config. The skeleton's `ProxyConfig` has no reload keys: the paths are fixed at start.

**Path-class auth** (`auth::decide`; table in `route`):

| Class | Paths | Rule |
|---|---|---|
| Worker | `/kbf.worker.*`, `/kbf.map.v1.Membership/`, `/kbf.update.*`, `/kbf.enroll.v1.ColdStore/` | verified certificate with role Daemon or Storage; none: `UNAUTHENTICATED`; other role: `PERMISSION_DENIED` |
| Reapi | REAPI services, ByteStream | `none`: open; `mtls`: any verified certificate but a host updater's; `bearer`: the right token |
| Operator | other `/v1/*`, `/metrics` | operator token; a certificate with the role `updater` instead makes `POST /v1/nodes/{id}:drain`, `POST /v1/nodes/{id}:uncordon` and `GET /v1/nodes/{id}` for its own node id (`route::self_node_of`), the proxy putting the operator token in itself, and nothing else: any other call of an `updater` certificate is `PERMISSION_DENIED` whatever the class, the probes, the UI and enrollment excepted |
| Update | `/v1/update/frontend*` | operator token now (one function, swap point for OIDC #1088) |
| Probe | `/healthz`, `/readyz` | open |
| Enroll | `/kbf.enroll.v1.Enroll/` | no certificate and no token at the proxy, in every mode; the child checks the enrollment token |

Decision C8: the modes do not combine. In `bearer` mode a certificate is neither required nor sufficient, and a wrong token next to a good certificate is a refusal: one mechanism per mode, no fallback to a weaker one. In `none` mode a verified certificate is still turned into identity headers (the child decides what a daemon may do). A certificate of the wrong role is `PERMISSION_DENIED` on the worker class where the design says `UNAUTHENTICATED` for "else": the peer is known, and `kbf-peer` documents the same split.

**Bearer.** Read only from `Authorization: Bearer`, never a query string, never logged (`HeaderValue::set_sensitive`; the log layer drops the header). A per-address limit on wrong tokens (section 19) runs before any hashing; `Token` compares SHA-256 digests in constant time (`subtle`), so length does not leak.

**Closed route table.** A path matching no prefix is `Unknown`, answered by the proxy (404, or `UNIMPLEMENTED` for gRPC) and never forwarded. **Enrollment (settled in U4b-2).** No `kbf-server` listener serves `kbf.enroll.v1` today: `Enroll` and `ColdStore` exist only as the proto and test fakes (`kbf-enroll/tests/support/fake.rs`); the server's `/v1/enroll/...` routes are the operator-side token and registry API, not this service. The proto's own header says where each belongs: `Enroll` "on a listener of its own that authenticates the server only" (the caller proves itself with the enrollment token or, for `Renew`, the key the registry holds), `ColdStore` "on the worker listener (mutual TLS), storage role only". So `/kbf.enroll.v1.ColdStore/` is a **worker** path (the proxy checks a worker or storage certificate; the child's role table narrows it to storage), and `/kbf.enroll.v1.Enroll/` is a new target `Enroll` with its own auth class `Enroll`: **no certificate and no token at the proxy** in every `buck2_auth` mode (a node enrolling has no certificate; `mtls` must not lock the door it comes in by), and the child checks the token. Its port is slot base + 4 (`UpstreamTable`; the admin port is + 3), so the child's unit adds the listener without a proxy change. Until the child serves it, a call is `UNAVAILABLE` (nothing listens), not a silent `UNIMPLEMENTED`. The proto says `Enroll` and `Renew` come without a client certificate (the enroll client uses none), so a client that presents a certificate must present a valid one: a certificate that fails verification is a handshake error here as everywhere, and no `Renew` path relies on a certificate.

**Identity to the child** (`identity::forward_headers`, the exact contract of `kbf-server/src/proxy_auth.rs`): strip every inbound `x-kbf-*`; with a verified certificate set `x-kbf-node-id` and `x-kbf-role` (`PeerRole::name`) and, when the leaf names IP addresses, `x-kbf-node-ips` (sorted canonical text, comma separated, at most 16; `docs/design/frontend-proxy.md` section 3); set `x-kbf-client-addr` to the connection's address (canonical IP text, no port; the proxy's reading, never the client's); always set `x-kbf-proxy-auth` to the 64-char lower-case hex of the secret. Each header ends with exactly one value. Child answers go through `strip_reserved` before the client sees them (the fenced marker is read first). The upstream connection is loopback, so the child's loopback check holds.

**Secret** (C13). `ProxySecret` is 32 bytes. `ProxySecret::create_or_read` returns the secret and `SecretOrigin::{Created, Existing}`: it reads `/run/kbf-proxy/proxy-auth` if present (so it survives a proxy restart and adopted children keep working), else writes a temp file (0600, owner `kbf`, no trailing newline), fsyncs, and `link`s it into place, so a crash never leaves a 0-byte file the child would refuse forever. `Created` means the adopted children hold the old digest and would see every call as anonymous: the supervisor refuses to adopt then (section 7). The directory is owned by the unit (`RuntimeDirectory=kbf-proxy`, `RuntimeDirectoryPreserve=yes`, which U7b writes); U4b-2 owns the code and its test (wrong mode or owner: refuse, as the child does). Children read it by `--proxy-auth-file`; never argv or env. Admin calls carry it too (`admin_headers`).

## 9. Config schema (`installer.toml`, table `[proxy]`)

```toml
[proxy]
operator_token_file = "/etc/kbf/api-token"   # read by the proxy; reloaded on SIGHUP
client_ca_file   = "/etc/kbf/tls/ca.pem"   # verifies client certificates, whatever name they asked for
# Server certificates, one of two forms (not both).
# Single certificate, served for every name:
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file  = "/etc/kbf/tls/server.key"
# Or a list chosen by SNI (the ClientHello's server name), in [[proxy.server_cert]] tables below.
buck2_auth = "none"                  # none | bearer | mtls
buck2_token_file = "/etc/kbf/buck2.token"   # required for bearer
proxy_auth_file = "/run/kbf-proxy/proxy-auth"
# state_dir, hold_max_s and remote_operator_file belonged to the two-slot supervision and the CI
# credential of the update API: they are still accepted, so that a file written before keeps
# starting the proxy, and nothing reads them.
ui = true                            # the read-only web UI at /ui: open (no certificate, no token), GET and HEAD only; false makes /ui a 404 at the proxy
# The list form of the server certificates, in place of server_cert_file/server_key_file:
# [[proxy.server_cert]]
# names = ["reapi.example.com"]        # DNS names, each in the certificate's SANs
# cert_file = "/etc/kbf/tls/reapi.pem"
# key_file  = "/etc/kbf/tls/reapi.key"
# default = true                        # exactly one when there are two or more
# [[proxy.server_cert]]
# names = ["farm.internal.example"]
# cert_file = "/etc/kbf/tls/worker.pem"
# key_file  = "/etc/kbf/tls/worker.key"
[proxy.limits]                       # every key optional; defaults shown
pre_stream_timeout_s = 10   max_connections = 50000   max_connections_per_ip = 2000   max_streams = 8192
max_header_list_bytes = 16384   keepalive_interval_s = 30   keepalive_ack_s = 20
```

**Server certificates.** `server_cert_file` + `server_key_file` is the same as one `[[proxy.server_cert]]` with no names and `default = true`, so a config written before SNI stays valid. With a list, a ClientHello whose server name equals a `names` entry (case-insensitive, trailing dot ignored) gets that entry's certificate; no server name, or a name that is in no entry, gets the entry marked `default = true`. Two or more entries need exactly one default; an entry without `names` must be the default. At start, and at every reload, each pair is checked: the key is the certificate's, every configured name is a DNS subject alternative name of the leaf (a `*.rest` SAN covers one label), no name appears twice, and the shape rules above hold; a reload also refuses an expired leaf. Any failure refuses the whole list: a start does not proceed, and a reload keeps the previous one. Client certificates are verified against `client_ca_file` and the per-path rules apply, the same on every name.

**Pool mode.** The children are given as `[[proxy.frontend]]` tables, at least one: frontends that run on their own, which the proxy balances over, with no supervisor, no swap and no hold. The `[proxy.upstream]` table of the two-slot supervision is refused at the proxy's start (`ProxyConfig::check_pool`, which `kbf-installer proxy` calls) with a message that says the supervision was removed and what to give instead (`SLOT_MODE_REMOVED`). `ProxyConfig::check`, which `kbf-installer setup` and the release fetcher read the file with, accepts the table and writes it back, so a file written before the removal is not refused where the proxy is not started.

```toml
[proxy]
admin_listen = "127.0.0.1:18090"     # optional, loopback only, plain HTTP, no credential
[[proxy.frontend]]
name = "a"                           # 1 to 64 of letters, digits, '-' and '_'; unique
base_port = 18000                    # 1024 to 65531; the offsets of a member's listeners (REAPI 0, worker 1, operator 2, admin 3, enroll 4)
[[proxy.frontend]]
name = "b"
base_port = 18010                    # at least 5 above or below any other frontend's
```

A stream goes to the frontend with the fewest streams open among those that are up (not draining) and ready; frontends with as many are taken in turn. A frontend is ready while its last `GET /readyz` on its operator listener, asked once a second, passed; a frontend that is up but not ready is taken only when none is ready, and one that cannot be connected to is not ready until a poll passes again. A draining frontend is never taken and its open streams run to their end. A stream that fails before any answer (a refused connection, `REFUSED_STREAM`, a GOAWAY that proves it unprocessed) is placed once more on a frontend that has not failed it, if the request is whole (the rules of section 10). `/readyz` is 200 while some frontend is up and ready. With every frontend draining or failed, a call is `UNAVAILABLE`. A `WaitExecution` is not read for the term of an old child. `GET /metrics` has the series `kbf_proxy_member_up`, `kbf_proxy_member_ready`, `kbf_proxy_member_open_streams` (by `member` and `target`), `kbf_proxy_member_picked_total` and `kbf_proxy_member_failed_total` for each frontend, and no child series.

The admin API is served on `admin_listen` and on no other socket: `GET /v1/proxy/upstreams` lists `{"name", "state": "up"|"draining", "ready", "open_streams": {"reapi", "worker", "operator", "enroll"}}` for each frontend; `POST /v1/proxy/upstreams/{name}:drain` and `...:undrain` change the state, are idempotent and answer the same list; an unknown name is a 404 and changes nothing. The API asks for no credential, and a request a web page could have sent is a 403: one with an `Origin` header, or whose `Host` is not an IP literal on loopback (use the address of `admin_listen`; a DNS name that was made to point at it does not pass). `admin_listen` must not be a port of a frontend (`base_port` to `base_port + 4`). A frontend leaves the pool for an update by being drained, waited for until its `open_streams` are 0, restarted, waited for until it is ready, and undrained.

Unknown keys are refused at start. U4b-3 adds `proxy: Option<ProxyConfig>` to the installer's settings and keeps the installer's own deny-unknown rule. The operator token, which is also the update credential (C9), is read by the proxy.

## 10. Keepalive, retry, GOAWAY and resets

- **No stream deadlines.** No route timeout, no max stream duration. Idle timeout: `idle_of(path)` is `Never` for `Execution/Execute` and `Execution/WaitExecution` (matched by full method path) and 7200 s for the rest.
- **Dead peers.** Downstream PING every 30 s, ack within 20 s; a failed ack closes that connection only. Upstream PING marks a connection suspect and closes it only after 5 minutes without an answer (section 7).
- **Replay buffer** (C5): request bytes are kept from the first byte, at most 64 KB, as `Bytes` clones (the skeleton counts bytes), freed at `close()`. A 3 hour Execute keeps its buffer until the first response frame, since tonic sends headers lazily. The window closes for good at the upstream's response head, or when a frame would overflow it. `may_retry` is true only for: not ByteStream, fewer than 2 dispatches, window open **and the client's END_STREAM seen** (`replayable`), failure in `{Connect, RefusedStream, GoAway}`. An open bidirectional request (`Worker.Session`) is never re-dispatched: no remainder is spliced onto a new stream. The "fenced, nothing committed" answer of the two-slot design (a trailers-only `grpc-status: 14` with `x-kbf-fenced`) is gone: no child sends it.
- **A retry is a new placement**, not a reuse of the member: the old upstream stream is reset, the old permit dropped, and the stream is picked again with the members that failed it left out (`attempts + 1`). With no member left the stream is `UNAVAILABLE`. Two attempts at most, so no loop.
- **Resets, in the order applied.** (1) `REFUSED_STREAM` or a GOAWAY above `last_stream_id`: `may_retry` first, before any mapping. (2) `RST_STREAM(NO_ERROR)` after the child sent a complete response: not an error, forward the response. (3) A reset on a non-gRPC path (`/v1/*`, `/metrics`): 502 on h1 or `RST(INTERNAL_ERROR)` on h2, never a gRPC trailer. (4) A gRPC path, headers not yet sent: trailers-only `UNAVAILABLE`. (5) Headers sent: a trailers frame with `UNAVAILABLE` follows the data; a data frame cut mid-message is followed by the same non-OK trailer, which tonic reports as a status error, never as a short OK message (tested with a tonic client). Client RST: propagated upstream as `RST_STREAM(CANCEL)`; the permit drops after it is sent. **Child GOAWAY:** never forwarded. **The proxy's own GOAWAY:** only on its shutdown and on a connection that trips a flood limit (a misbehaving connection only); a drain sends none.
- **Flow control.** One window for every stream, 256 KiB (`STREAM_WINDOW_BYTES`), in both directions: h2 0.4.20 cannot grow one stream's window after dispatch. A stream returns credit as the other side consumes (`release_capacity`). The connection window is 1000 x window, so no combination of unread bodies stalls a probe on the connection. Adaptive BDP is off. Raising the window to buy upload speed raises `max_streams x window` in step; the memory-budget check at start enforces it.
- **h2 flood limits.** Derived from the 1000-stream cap, not left at the defaults of about 20: a Buck2 build cancel resets hundreds of streams at once, and tripping the limit would GOAWAY the connection with its other streams on it. Max pending reset streams (rapid reset) is set above 1000, the header-list cap, CONTINUATION frame count and byte caps, concurrent streams per connection 1000. Test: 1000 client cancels on one connection trip nothing; a real rapid-reset attack does.

## 11. Pre-stream limits and the verb

Handshake, h2 preface and h1 request-head timeouts: 10 s each (`pre_stream_timeout_s`); `max_connections`, `max_connections_per_ip`; per-address rate limit before any token check. **HTTP/1.1** (C4) is served only for `/v1/*`, `/healthz`, `/readyz` and `/metrics`; the request head is parsed with `httparse`, at most 16 KB and 100 headers, origin-form targets only (`CONNECT`, absolute-form and `Upgrade` are 400), and a gRPC path on h1 is 426.

**Verb.** `kbf-installer proxy [--config PATH] [--check]`: `--config` defaults to `/etc/kbf/installer.toml`; `--check` loads `installer.toml` the way a run does, through `InstallerToml::parse_tolerant` (the confirm windows, the fetch timeout, `database` and the `[proxy]` table, not only the table) and exits 0 or 2 without binding. A key the verb does not read is not checked, so `--check` (the `ExecStartPre` of `kbf-proxy.service`) does not catch a misspelt key of the rest of the file; `setup` and the root run helper, which read the file closed, refuse it. A malformed `[proxy]` table still makes the root run helper refuse `installer.toml`, since both check the one table. No other flag: everything else is the `[proxy]` table, since configuration is flags and files, not the environment. The two environment variables it reads (`LISTEN_FDS`, `LISTEN_PID`) are the platform's socket-activation markers. To add it: a `Verb::Proxy` variant (`cli.rs`), a `built_by` arm, a dispatch arm (`app.rs`), `needs_root` false. It is not a `kbf-cli` mode: `kbf-cli`'s `GROUPS` rule (every flag of every mode in one group) then does not apply, but the child's new `--await-takeover` (U2) must join a group there. Installer dependency: `kbf-installer` gains `kbf-proxy` and with it tokio, hyper and rustls in the same binary as the root helper. **Risk, with a recommendation:** the root half links a network stack it never runs. Keep the design's single verb, and add a layering test (U4b-3b) that the root helper's modules (`helper/`, `run`) and every file outside the dispatch name neither `kbf_proxy` nor the verb's module `proxy_verb`; if the maintainers prefer no shared binary, `kbf-proxy` gets its own `main` and the verb becomes `exec` of it, which is a unit-file change, not an API one.

**Open requirements the verb leaves to later units.**

- **P10 (setup) must preserve the operator's `[proxy]` table.** `InstallerToml::render` drops it today, so a setup that rewrites `installer.toml` would silently remove the proxy's configuration.
- **The verb passes `[proxy]` to `Proxy::bind` whole, so it serves TLS** (U4b-2): `server_cert_file`, `server_key_file`, `client_ca_file` and `operator_token_file` are required keys of the table, and a start without readable files exits with the bind error. Only the proxy's own poll of the child's operator listener is plain HTTP/1.1 on loopback.

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
| C9 | The proxy reads `operator_token_file` itself; update credential = the same `Token`; U8 swaps the `UpdateHandler`; the OIDC credential is `Security::authorize_call` (`remote_operator_file`), `decide` stays pure | design section 4 says "same token"; one function to replace |
| C10 | Static table is `base_port` plus fixed offsets; one readiness writer: `/readyz` is 200 when an active child's last poll (every 1 s, 3 s timeout) passed within 5 s; readiness never touches routing or open streams | design: a health failure does not touch open streams |
| C11 | `/metrics` is the proxy's own series (Prometheus text, operator token) followed by the active child's operator `/metrics` body, fetched as one bound operator stream (caller's `Authorization` and the proxy identity headers), at most 4 MiB and 2 s, with every `kbf_proxy_*` sample and `# HELP`/`# TYPE`/`# UNIT` line and any `# EOF` line of the child's body dropped; with no active child, a hold (the scrape never waits on it), an error status, an oversize body, a body that is not UTF-8 or a timeout the proxy's series are served alone and `kbf_proxy_child_metrics_failed_total` counts it | a scrape of the public port must see the child's series too, and the proxy must count its own streams; the `kbf_proxy_` prefix keeps the series apart, and the proxy is its only author, so a child cannot duplicate, contradict or terminate them |
| C12 | SIGHUP reloads cert, key and CA as one unit; a failed reload keeps the old one and counts | a half-applied reload would break handshakes |
| C13 | The proxy creates (temp + link) or reads the secret file and reports `Created`/`Existing`; the unit owns `/run/kbf-proxy/` (U7b) | the proxy restarts without the supervisor, so it cannot depend on it |
| C14 | A reset while held drops the ticket; `abort` is `resume(old)` then `release_into(old)`, cancels nothing; a dropped guard releases into nothing | the update's outcome should not cost a client its request; an abandoned hold must not guess a slot |
| C15 | U4b ports the properties of the 45 checks (section 13), not the checks; U4b owns the loom permit test | about 15 checks describe Envoy config the proxy has no analogue of |

Node id = the one DNS name (section 8) and the closed route table (section 8) are also decisions of this note.

**Design versus code.** (a) Checks 39 to 42 (a worker `tcp_proxy`, TLS passthrough) contradict design section 3 (no passthrough): they are dropped, replaced by the worker identity tests. (b) Checks 14 and 31 (7200 s, Execute idle 0) agree with section 3. (c) The design's `/v1/update/frontend*` is a prefix; the skeleton matches on a path boundary, so `/v1/update/frontendx` is an operator path. (d) The design's status JSON (`frontend-proxy.md`, step 5) lists `oldest_stream_age_s` as proxy-counted; the proxy does not provide it (a lock-free data path has no start-time per permit and no consumer asked), so the design line is stale.

## 13. Test plan: the 45 Envoy checks

Test names are `kbf-proxy` tests unless marked `it:` (in `kbf-integration-test`). "In skeleton" means the test exists now. Each row names the mutant that must go red.

| Checks | Proxy property | Test | Mutant |
|---|---|---|---|
| 1, 2, 3, 7 envoy-shape, hcm, codec, vhost | no config surface; unknown keys refused; gRPC trailers pass an h2 hop | `a_broken_table_is_refused_at_start` (in skeleton); `it: grpc_trailers_pass` | accept unknown keys |
| 4, 5, 13 max-stream-duration, request-timeout, route-timeout | no stream deadline | `no_stream_deadline_3h_paused` (a 3 h unary and a 3 h ByteStream Write) | a 60 s stream timer |
| 14, 31, 41 idle | Execute and WaitExecution never idle; 7200 s elsewhere | `only_execute_and_wait_execution_are_never_idle` (in skeleton); `idle_3h_paused_not_closed` | Execute on the 7200 s path |
| 6 filters | no buffering of a write | `bytestream_write_memory_is_constant` (1 GiB through, peak RSS bounded) | buffer the body |
| 8, 9, 11 no-catch-all, route-match, route | closed table, boundary match, every service has a route | `a_path_outside_the_table_is_never_forwarded`, `the_update_api_is_the_proxys_own_and_matches_on_a_boundary`, `every_proto_service_has_a_route` (reads the descriptors) (first two in skeleton) | prefix match without boundary; forward unknown |
| 10, 32 route-action, retry-routes | retry only before the first response, on safe failures | `only_a_safe_failure_is_retried_and_never_bytestream`, `an_upstream_response_closes_the_window` (in skeleton); `bytestream_not_retried_after_first_frame` | retry ByteStream; retry after a response byte |
| 12, 26, 37, 38 route-cluster, endpoints, instances, instance-ports | table gives every target a distinct loopback port | `slot_ports_are_fixed_offsets` (in skeleton); `every_slot_and_target_has_its_own_loopback_port` | overlapping offsets |
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
| Soak (`kbf-integration-test`, real children, U4b and U7b) | `it: soak_swap_200_streams`: the assertions of design section 12 that need only U4b (no GOAWAY, pre-flip streams OK, post-flip served by the new child, node id equals the certificate's, counters 0, old child exits after its last stream) | route per connection; retire on a 5 s timer; GOAWAY at flip; retire with no active child. The U2/U5/U7 assertions (lease, epoch, exactly-one-run, 11-minute Execute) are run by U7b |
| `x-kbf-*` is the child's contract | `it: child_accepts_proxy_headers` against the real `kbf-server` U4a gate | change a header name |

## 15. PR split

All stacked on U4a; each PR is independently reviewable and merges in order. None is merged by the author.

- **U4b-1: core routing, permits and hold, fake upstreams.** The skeleton (`slot`, `hold`, `route`, `replay`, `config`, `control`), the loom shim and the three models of section 5 (the PR does not merge without them), the pool, `serve`/`h2`/`pool` over plaintext h2c on a test listener (no TLS, no auth), per-stream binding, hold gate and expiry, GOAWAY and reset mapping, `/metrics` counters. Proof: flip with 200 streams, one connection to both slots, hold 1000, 257 streams on a 256-connection pool, the loom test. The "5000 queued streams plus unary latency" proof moves to the U4b-3 soak: the hold queue defaults to 2048 places and a connection takes 1000 streams, so 5000 held streams cannot exist as written; the soak sizes it to what the queue and the connection cap allow. Size: large; the h2 proxy core is most of it.
- **U4b-2: TLS, auth and identity.** `kbf-tls` optional-client constructor, `tls` with SIGHUP reload, `identity` wired to the handshake, `auth::decide` on every route, `x-kbf-*` strip and set, bearer rate limit, secret file create-or-read, `h1`, the enrollment-path decision. Proof: the identity and auth tests, the end-to-end fake child that records headers, `it: child_accepts_proxy_headers`. Depends on U4b-1.
- **U4b-3: keepalive, replay and retry, soak and the verb.** PING keepalive both sides, replay and fenced re-dispatch wired, h2 flood limits and their attack tests, `/readyz` polling, `kbf-installer proxy` (`Verb::Proxy`, `built_by`, dispatch, `--check`), the installer dependency and the layering test, the `kbf-integration-test` soak subset. Depends on U4b-2.

## 16. Skeleton status and what was not verified

The skeleton adds `crates/kbf-proxy`, `http` and `kbf-proxy` to the workspace, and one line `kbf-proxy 0 0` to `coverage-baseline` (a new crate goes in as `0 0`, as `kbf-coverage tighten` documents; every function in the skeleton has a test, but the coverage job has not run). It uses only dependencies already in `Cargo.lock` (`serde_json` is a dev-dependency, for the `Slot` text form); the lock entry for `kbf-proxy` was edited by hand and cargo will correct it if it differs.

**Compiled** (round 3, on the local compile lease): `cargo check -p kbf-proxy --tests`, `cargo clippy -p kbf-proxy --all-targets -- -D warnings` and `cargo test -p kbf-proxy` all passed, 60 unit tests (U4b-1 has more: section 18); the coverage job has not run. The first run of round 3 failed one test, `open_streams_never_reads_zero_while_a_stream_lives` (the two-counter `open_streams`), which is how the one-word count came about. Not verified: hyper/h2 builder names for the flood limits and keepalive (U4b-1 reads the pinned versions; the h2 0.4.20 window facts of section 10 were read from its source); which listener serves `Enroll` and `ColdStore`; that rustls can express "client certificate optional, verified if present, failure is a handshake error" with `WebPkiClientVerifier::builder(..).allow_unauthenticated()` (U4b-2 confirms against the pinned version). The loom models and the pool are U4b-1.

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

## 18. U4b-1 as built

What the first I/O PR does, where it differs from sections 2 to 15, and what it leaves. The public API of section 3 is unchanged (no "API CHANGE"); `ProxyHandle` is the partial set below until U4b-2.

- **Downstream is hyper's h2 server, not h2 directly.** Its builder sets the one 256 KiB stream window, the 1000-stream connection window, 1000 concurrent streams and the header-list cap, and its request body returns credit only as a frame is read, so a held stream (never read) returns none. The flood-limit settings (`max_pending_accept_reset_streams` and the rest) are U4b-3. `h2` stays a direct dependency for the upstream client and for typed errors.
- **Upstream is h2's own client** (`pool`), not hyper's `conn::http2`: `current_max_send_streams` is on h2's `SendRequest`. h2 assumes 100 streams until the child's SETTINGS arrive, which would reserve a burst on one connection against a limit of 2, so a new connection waits one PING round trip (the child sends SETTINGS before it answers) before it is used.
- **`Proxy::bind(cfg: ProxyConfig, listener: OwnedFd)`** builds the runtime and starts the gate holding. `ProxyHandle` has `runtime`, `slots`, `expire`, `startup_hold`, `snapshot`, `open_streams`, `shutdown`; `secret_origin`, `admin_headers` and `reload_tls` arrive with the secret and TLS code in U4b-2. `ProxySnapshot::ready` is false and `/readyz` is 503 until U4b-3's poll writes it. `LISTEN_FDS` handling belongs to the verb (U4b-3).
- **Review SHOULDs.** `Inner::ring` takes the epoch inside the doorbell's lock (model 2b). The proxy ends a startup hold nobody released through the crate-private `ActiveSlot::abandon_startup` (sequence 1, so a supervisor's later hold is never touched). (Changed in U4b-3a: a stream's `since` now lives in `run`, so a retry that is held again keeps the first bound.)
- **Retry and idle.** Idle is one `Activity` per stream, touched by either half and checked while waiting for the response head and while the response streams. Retry is wired for `Connect`, `RefusedStream` and `GoAway`: at most one more dispatch, the whole request replayed from the kept `Bytes` (a request still open, over 64 KiB or a ByteStream is never repeated), the old pump awaited before the permit drops, and a `Pending` begun before it does. `Fenced` is U4b-3. A body that was read is not sent again as headers-only: coverage found that bug in the first cut.
- **Loom** (history): the models of the slot and the gate ran under `--cfg kbf_loom`, `cargo xtask proxy-loom`. They went with the slot; the cfg, the verb and the dependency are gone.
- **Tests** run over in-memory pipes (`Connector` for the pool, `Listen` for the accept loop) under paused time. With real sockets a paused clock advances whenever every task waits on I/O, which fired the idle and hold timers in the middle of a round trip.
- **Not safe to expose yet**: there is no TLS, no auth, and `/metrics` and the update API answer anyone who connects. U4b-2 puts auth at `Service::handle`, the one place a request is classified.
- **Left for U4b-2 and U4b-3** (keepalive, fenced re-dispatch and flood limits are done in U4b-3a below): TLS, identity, auth, h1, the secret file, `kbf-tls` optional client (U4b-2); keepalive both ways, fenced re-dispatch, flood limits and attack tests, the readiness poll (done in U4b-3b), `kbf-installer proxy`, the layering test, the `kbf-integration-test` soak, the h2 preface timeout, connection-panic counting, closing a retired generation's pool at once (it is dropped when the slot's next generation is first used) (U4b-3).

### U4b-3a as built (keepalive, h2 limits, fenced re-dispatch)

The public API of section 3 is unchanged. What the unit does and where it differs from sections 10 and 11:

- **`limits.rs`** holds `h2_server_builder(&Limits)` and `h2_client_builder()`: the one 256 KiB window, the 1000 x window connection windows, adaptive BDP off, 1000 concurrent streams, the header-list cap, the 16 KiB frame size and `max_pending_accept_reset_streams` = 2 x 1000 (`PENDING_RESET_STREAMS`). h2 0.4.20 has no setter for the CONTINUATION caps: it derives them from the header-list cap and the frame size, so those two are the CONTINUATION limits. Hyper does not expose `max_concurrent_reset_streams` or `reset_stream_duration`, so they stay at h2's defaults (they bound streams the proxy itself reset, which is not a client's lever); `max_local_error_reset_streams` (hyper exposes it) stays at 1024, off the burst path. The rapid-reset limit counts resets read before the proxy accepts the stream, so it stops a burst; a slow-roll attacker whose resets are accepted promptly is not tripped (the 1000-stream cap and the per-IP connection cap bound it). Tests added with the review: a CONTINUATION flood is sent away on its own connection, an oversize header list is refused on its stream only. The upstream client takes no flood setting: the children are our own processes and push is off.
- **Which resets the limit counts.** h2 counts only streams the client reset *before the proxy accepted them* (a burst read in one go). A build cancel of 1000 streams that are already open is not counted at all and passes at h2's default of 20 too; the burst test (1000 streams opened and reset in one write) is the one that goes red at the default. The attack test sends 2500 pairs.
- **`keepalive.rs`.** Downstream: hyper's PING at `keepalive_interval_s`, ack within `keepalive_ack_s` (the timer hyper needs is set there). Upstream: a task per connection pings every 30 s; an answer later than 20 s marks the connection *suspect* (the pool puts no new stream on it, streams already on it are untouched, the late answer clears it); 300 s after the PING without an answer the connection is dropped. This is section 7's rule (suspect, 5 minute backstop), not "never closes" of the unit's brief: a loopback child that has not answered in 5 minutes is dead, and its streams would otherwise wait for ever.
- **Fenced.** `Stream::screen` reads the first response head before the window closes (`ReplayBuffer::on_first_response`). Fenced and `may_retry` allows it: the attempt returns `Failure::Fenced` (the old pump is aborted and awaited, the permit drops) and `run` closes the window and binds again. Fenced and `may_retry` refuses (second dispatch, ByteStream, a request still open or over 64 KB): the window closes and the child's own trailers-only `UNAVAILABLE` is forwarded with every `x-kbf-*` header stripped. The stream's `Pending` is kept across attempts, so the "`begin_stream` before dropping the old permit" step of section 10 is already true. A retry's `admit` takes the stream's first `since` (kept in `run`), as in section 10; test `a_retried_stream_keeps_the_first_hold_bound`. A forwarded trailers-only answer ends its stream on its HEADERS (`ProxyBody::is_end_stream` follows the h2 stream); test `a_forwarded_trailers_only_answer_ends_the_stream_on_its_headers`. An answered PING that clears *suspect* rings the pool's bell, so a stream parked at the connection cap looks again.
- **Left for U4b-2, now done there:** the TLS handshake and HTTP/1.1 head bounds read `pre_stream_timeout_s` (`Acceptor::accept`), and the per-address limit on wrong tokens runs before any hashing (`auth`). The accept path builds its h2 server from `limits::h2_server_builder` and its h1 server from `h1::builder`, so the TLS path has the windows, flood caps and downstream keepalive. Still left for U4b-3b: the h2 preface timeout (nothing bounds the time from a finished handshake to the first h2 frame beyond the keepalive). `max_connections` and `max_connections_per_ip` are enforced by `ConnLimits` in `serve.rs`.

### Review round 1 of U4b-1

Fixed, each with a test seen red under the mutant:
- **A cancelled dial leaked a dial slot** (sixteen client resets during a slow connect wedged a pool for good): the slot is a guard (`DialSlot`) that gives itself back and wakes the waiters when the stream's future is dropped. The reservation is also built before the `ready()` await. `ready()` on a fresh `SendRequest` clone resolves on its first poll (h2 queues a stream beyond the child's limit inside itself, and `poll_ready` waits only for this handle's own queued stream), so that window cannot be observed and has no test of its own.
- **A GOAWAY the h2 library raised itself** (a protocol error of the connection, which reaches the streams the child had already taken) was retried like the child's own: only a remote GOAWAY is.
- **The replay list** is capped at 64 frames and keeps no empty DATA frame. h2 already ends a connection that sends about a hundred tiny frames, so the cap sits under that bound; the bytes stay capped at 64 KiB.
- **The request pump** also ends when the response half cancels while it is parked on a full upstream window, so a child that stops reading no longer holds the permit until it dies.
- **A trailers error** after the data is a failed stream, not a silently short response (h2 cannot produce it today: the arm is shared with the data error, so there is no second path to test).
- **A child that never answers the PING** at dial is a failed dial (and counted), not a connection that runs on h2's assumed 100 streams.
- **`Proxy::bind` refuses `buck2_auth` other than `none`** (`ErrorKind::Unsupported`) until U4b-2: a configuration that asks for authentication is not served open.
- **The loom gate** (history): it counted the tests it ran and is gone with the models.
- The redundant `Pending` carried across a retry is gone: the stream's own lives for the whole run. The two 1000-stream constants are named `DOWNSTREAM_`/`UPSTREAM_CONNECTION_STREAMS`.

Known and left: `/healthz` answers 200 unconditionally and `/readyz` 503 until a poll has passed (the poll is `probe`, U4b-3b: 200 only while the gate has an active slot and a poll of that slot, in that generation, answered 200 within the last 5 s); `kbf_proxy_hold_seconds` of the frontend-proxy note is not exported yet (U4b-3); a stream is bound after its task is scheduled, not at the instant its headers arrive (microseconds; a flip inside it binds the new slot); a shutdown cuts streams that outlive the fixed 5 s drain; `ProxyHandle::expire` is `#[must_use]`; `xtask proxy-loom` sets `RUSTFLAGS`, which replaces any `[target]` rustflags of a cargo config; **`ci.yml` has no loom step, which is a merge blocker for "loom in CI"** (needs a workflow-scope token); the base branch's `kbf-server/tests/proxy_auth.rs` carries a `private address literal that `repo_lints` refuses (U4a's change).

## 19. U4b-2 as built

TLS, identity and auth. The public API of section 3 is unchanged; `ProxyHandle` now has `secret_origin`, `admin_headers` and `reload_tls`, `ProxySecret::create_or_read` and `SecretOrigin` exist, and `Proxy::bind` no longer refuses `buck2_auth` other than `none`. Where it differs from sections 2 to 15:

- **Terminator.** `kbf_tls::server_config_optional_client(cert, key, client_ca, reject_expired)` builds the rustls config (`WebPkiClientVerifier::builder_with_provider(..).allow_unauthenticated()`, confirmed on rustls 0.23.46: a presented certificate that fails is a handshake error, none is anonymous; ALPN `h2` then `http/1.1`; ring; early data 0). `tls::TlsReloader` builds a `kbf_tls::SniCerts` from the certificate list (`SniCerts::from_config`), reads the client CA, builds the config around the resolver (`server_config_optional_client_sni`), and swaps the whole `Arc<ServerConfig>` or nothing; a reload also refuses an expired leaf, the first load does not. The accept path is `Acceptor::accept` (handshake bounded by `pre_stream_timeout_s`, then `identity_of` on the leaf, then ALPN picks h2 or HTTP/1.1; no ALPN is HTTP/1.1). A verified certificate with no usable identity closes the connection (`kbf_proxy_identity_refused_total`). `Acceptor::Plain` exists only under `cfg(test)`. Graceful shutdown no longer goes through hyper-util's `GracefulShutdown` (it needs the connection before the handshake); each connection task selects on the shutdown flag and the accept loop waits for the tasks through a channel whose senders they hold.
- **Auth.** `Service::handle` classifies once: the HTTP/1.1 check, then `route_of`, then `Security::authorize`, then the hold. The bearer limit counts **wrong tokens, not calls** (a Buck2 client sends its key on every call, thousands a second): ten in a row per address, one earned back per second; an address past it is refused (`RESOURCE_EXHAUSTED`, 429) before its token is hashed. Token files follow the child's rules (regular file, this user, mode 0600 or 0400, at least 32 visible ASCII characters). The operator token and the Buck2 key are read at start; **reloading them on SIGHUP is not built** (only `reload_tls` is), so the config comment says "at start".
- **HTTP/1.1** uses hyper's HTTP/1.1 server (which parses with `httparse`) with `max_buf_size` 16 KiB, 100 headers and the pre-stream bound as the head timeout; origin-form, no `CONNECT`, no `Upgrade`; gRPC paths are 426. The request is made absolute and its connection-level headers dropped so it enters the same code as an h2 one.
- **Secret file.** Written by `link` (a creation never clobbers) or `rename` (a replacement of a file that is not 32 bytes); a file with another owner or a mode other than 0600 or 0400 is an error, not a repair, whatever its length. A second proxy racing the creation fails with `AlreadyExists` and is restarted by its unit.
- **Left for U4b-3:** the h2 preface timeout (the TLS handshake and HTTP/1.1 head bounds are in), keepalive, flood limits, fenced re-dispatch (`/readyz` and the verb are U4b-3b, section 18).

### Review of U4b-2

- **Port range.** `base_port` is at most 65521 (`MAX_BASE_PORT`): the first check allowed 65522, where slot B's enrollment port (`+10+4`) overflows `u16` on the first `Enroll` call. Test `the_largest_base_port_leaves_room_for_every_port`.
- **Secret created last.** `Proxy::bind` reads the tokens and loads TLS before `create_or_read`, which is the last step that can fail, so a start that exits has not made a secret that children could read and the supervisor then adopt (`Created` would be lost on the restart). A creation whose final directory fsync fails removes the name again for the same reason; a replacement (of a file that held no usable secret) is not undone.
- **One reload at a time.** `TlsReloader::reload` holds a lock from the read of the files to the swap, so overlapping reloads (a SIGHUP storm) cannot swap in the order they finish and bring back a CA that was rotated out.
- **Wrong-token limit, what it does not do.** It is per source address. Behind a shared address (NAT, an overlay gateway) one client sending a wrong token a second keeps the address at its burst and so blocks the operator and Buck2 keys from that address too (429). IPv6 is keyed by the full address, so a client with a /64 can rotate addresses and only multiplies the hashing it costs us, which a 32-character token makes harmless. Both are accepted for now; keying IPv6 by /64 and separate buckets per class are the fix if either is seen.
- **Enroll has no limit at the proxy and the child sees only 127.0.0.1.** The child's contract carries node id, role and secret, not the client address. **Requirement on U2:** the enrollment listener limits globally or by token, never by peer address (by peer address every enrollment shares one bucket: one attacker locks all out, or the limit never trips).
- **Enroll in the hold queue.** An anonymous `Enroll` stream waits in the hold queue like any other during a swap, and in `bearer` and `mtls` modes it is the only anonymous way in. A flood of them can take hold capacity from worker streams; a cap of its own for `Enroll` is not built.
- **No revocation.** The client verifier checks chain, validity and extended key usage and nothing else: a revoked certificate is refused only when it expires or the CA is rotated out by a reload. A resumed TLS session keeps its verified certificate for the life of one configuration; a reload swaps the session cache.
- **mTLS and the role URI.** The identity rule wins over "any verified certificate": in `buck2_auth = "mtls"` a Buck2 client certificate must have one DNS name and a role URI, or the connection is closed at the handshake.
- **Not done, with the reason.** Hand-rolled test PKIs (`tests/pki.rs`, `proxy_identity.rs`) stay: the unit tests need expired, multi-name, role-less, server-only and intermediate-issued leaves that `kbf_integration_test::pki` and the kbf-tls support do not make. The proxy does not forward the client address (U4a's contract). `ProxySecret` keeps a derived `PartialEq` and does not zeroize: it is a 32-byte value held in the process for its whole life, and nothing compares two secrets on a request path.
- **Conflict surface for U4b-3.** `serve.rs` (shutdown and drain logic replaced; `Service::call` and `handle` take the connection's `Peer`; an HTTP/1.1 builder), `core.rs` (`Core::new` takes a `Security`; `with_metrics`), `tests/kit.rs`, `bypass.rs`, `routing.rs`, `mod.rs`, and `kbf-integration-test/Cargo.toml`.

## 20. U7b as built

What the swap's ports became, and what did not. Code (removed with the supervision, see the note at the top): `crates/kbf-installer/src/proxy/{gate_real,child_real,runtime,supervisor}.rs`; units: `packaging/linux/units/kbf-proxy.{service,socket}`.

- **`Gate` grew two calls**, `clear_active` and `retire(slot) -> Retired` (the proxy's commit). The retire rule is unchanged (not the active slot, the gate's `open_streams` zero, the child's `open_counts` idle) and ends with a commit: SIGTERM goes only after `retire` answered `Closed`. The all-in-one stop does what section 7 says: stop, `clear_active`, poll `retire` until it is closed (a note, once, if it is still busy at the hold bound; no timeout), then start. `Gate::hold` still returns `()`: `ProxyGate` keeps the `HoldGuard` in a field and drops it only after `release_into` made it inert.
- **Slots are opened at their first `release_into`** (`ProxyGate::release_into` calls `open` first), not when the candidate is ready: the pool dials on bind, so nothing needs the slot earlier, a candidate that fails was never opened (`retire` says `NeverOpened`), and a retired slot returns as a new generation. `Gate::active` was not needed; `ProxyGate::active` exists for tests and the supervisor.
- **Children** are `releases/<commit>/kbf` started with `--mode=frontend --instance=<a|b> --proxy-auth-file=<file>`, each in its own process group, with the activation variables removed. Identity (pid, start time, executable) is recorded in `<state_dir>/children/<slot>.json` before `spawn` returns. Every signal goes through a pidfd opened while the identity was verified; adoption verifies before and after opening it. A child of the proxy is reaped by `try_wait`; an adopted one is watched by its pidfd and has no exit code (`-1`). macOS has no real child control: the frontend swap is Linux-only (section 2).
- **Start.** `kbf-installer proxy` adopts the recorded child when exactly one slot has a live, still-recorded process and no update is in flight (a terminal `update.json` is history). It ends the startup hold into that slot. Every other state keeps the hold and logs why: an update in flight, a corrupt `update.json`, no child, two live children.
- **Units.** `kbf-proxy.socket` holds the public port (`Accept=no`, one TCP port above 1024; 8980 is the default and a drop-in changes it). `kbf-proxy.service` runs the verb with `--config /etc/kbf/installer.toml`, `KillMode=process` (a stop signals the proxy alone, so the children keep serving and the next proxy adopts them; the unit has the default `KillMode` now, the proxy having no children), as `kbf-server`, with `OOMScoreAdjust=-900`, a preserved `RuntimeDirectory` for the proxy-auth secret, and the sandbox the frontends can live with (they inherit it). It replaces the `kbf-frontend@` instances on a host that uses it: enable one or the other. The `package_units` lint has rules for the two files in `tests/package_units/proxy.rs`.
- **Not built, and why.** `ChildAdmin` is still the fake, and its trait is unchanged from U7a: it is the shape the swap was built on, and #1162 implements it over the child's admin API (`admin_protocol` 1) in `proxy/admin.rs`, mapping `role`, `takeover` and the counts there. The proxy's `Gate` counts worker sessions and Execute streams as open streams, and the child's `open_counts` adds executions and queued work with no open stream, so the retire rule reads both and does not take the admin API's `open_streams`/`open_sessions` as the second half. `Migrator` stays open. `RootOrders` is the orders spool and `status.json` reader of the update API's unit. So the verb cannot yet run a swap, resume one that was in flight, choose between two live children, or start a first child on a host that has none. The children are started with per-slot listen flags and, as candidates, `--await-takeover`.
