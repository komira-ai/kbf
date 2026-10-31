# One host updater with per-service strategies, and a generic coordinator

Status: DRAFT, for decision. No code. Facts were read at `origin/main` 80436afe7; a `path:line` is a line of that file at that commit. Where a statement is an inference or a claim about software we do not own, it says so.

Labels: **[F]** read in the code at the commit above, **[I]** inference, **[D]** a design choice made here, **[V]** to verify before building on it.

Sections 4.1 (tierstore) and 4.2 (js-agent) are completed by their owners. Section 4.1 below holds the updater-side mechanics of the port forward in full; the tierstore owner adds a summary of the service-side contract and links the deep dive (`docs/design/tierstore-handover.md`).

## 0. The proposal in one page

Today every host runs one agent per host (`kbf-updater agent`, unprivileged) beside one root helper per product (`kbf-host-install --serve`), and a coordinator that already keeps a desired version per service. The mechanism is nearly generic; the knowledge of what to do to a particular product is not: the A/B in the agent is one fixed sequence (fetch, drain the node, switch, health, confirm or revert, undrain), and the helper installs a component whose name is the literal `kbf`.

This document proposes:

1. **One updater per host**, the existing agent, generalised. Services **register** with it when they start (name, version, health endpoint, strategy name, strategy parameters). A registration claims; it grants nothing. What a service may have done to it (its units, its release channel and signer, its ports) comes from a root-owned file, as the helper's channel allow-list does today.
2. **A strategy per service**, behind one interface. The updater does what is the same for every service (fetch, verify the sha256 and the attestation, stage, persist progress, check health, report, roll back) and calls the strategy for what differs: how a new version takes over from the running one. Three strategies are named: `node-drain-restart` (today's kbf-daemon behaviour), `port-forward` (tierstore) and `lease-drain-restart` (js-agent).
3. **One root executor per host** doing the root half for every strategy, with a closed set of verbs and no free-form command. It is the existing helper made service-generic.
4. **A coordinator generic over services**: it already is, nearly (`crates/kbf-coordinator/src/decide.rs` branches on no service name). What is left is the kbf-shaped desired state and roles, no canary, no automatic rollback, and health that only the agent reports. Section 5 lists each.

**This replaces the earlier simple-updater rule.** The earlier direction was that no update logic lives inside a service and that the updater stays a simple program that fetches, verifies, replaces binaries and restarts units. This design keeps the first half (a service never contains update logic, it only offers the small contract in section 4) and drops the second: the updater is now the one place update logic lives, and the strategies are where it is organised. That is a deliberate growth in the updater's size, paid for by moving the sequencing out of services, out of workflows and out of held scripts.

## 1. Today

**Coordinator** (`crates/kbf-coordinator`). It keeps one desired state per service name (`desired.rs:239`, `DesiredStore`; the file is replaced whole by temporary file, fsync, rename and directory fsync, `desired.rs:1-14`). A service is data: a name, the roles it runs on with `max_updating`, `min_up`, `never_last` and per-role `after`, and `after` between services (`services.rs:1-20`, `:90` for the built-in `kbf`). `decide` is a pure function of services, desired states, the fleet built from heartbeats, the host and the time (`decide.rs:1-34`); it orders a host to update only if seven rules hold: the coordinator has been up for `QUIET_START` (30 s, `fleet.rs:30`); the host is not updating and no other service updates on it or has an order outstanding for it (**one update per host at a time**, `decide.rs:105`, `is_busy_besides`); the host is behind; it did not fail this version; the role has room (`max_updating` slots, counted per service); the role and service `after` conditions hold; enough other hosts of the role are up (`min_up`). A host silent for `DOWN_AFTER` (45 s, `fleet.rs:27`) is down. An order the host has not reported holds its slot for `ORDER_AGE` (120 s, `fleet.rs:36`). The state is rebuilt from heartbeats after a restart; outstanding orders are forgotten (`docs/design/updates.md`, section 6). Convergence is `converged`, `blocked` or `in_progress`, computed from heartbeats alone (`converge.rs:1-30`); `blocked` means a live host failed an update to the desired version and is behind it. A rollback is an operator (or CI) setting an older desired version (`docs/design/updates.md`, section 3, "Rollback: set X older").

**Wire.** A heartbeat carries platform, version, health, last update result and `updating`, all for `kbf`, plus `services: Option<Vec<ServiceBeat>>` with the same four facts per other service (`wire.rs:57-100`); `kbf` is never listed in `services` (`wire.rs:138`, `:154`). A reply is `nothing` or `update to X`; for another service it names the service, in a shape only an updater that sent `services` is sent (`wire.rs:235`, `:254`). Node id and role come from the host's certificate, not the heartbeat (`docs/design/updates.md`, section 2).

**Agent** (`crates/kbf-updater/src/agent`). Unprivileged: `User=kbf-updater`, empty `CapabilityBoundingSet=`, `NoNewPrivileges=yes`, address families unix, inet and inet6 (`packaging/host-agent/linux/kbf-updater-agent.service:32-43`). Two threads: a heartbeat thread, and an update thread that runs one update at a time (`agent/run.rs:1-20`). Extra services are configured **statically** by flag: `--service NAME,helper-socket=PATH[,channel=C][,drain=node|none][,ready-url=URL]` (`agent/flags.rs:52-57`, `ServiceConfig` at `:186`), each with its own helper socket; the agent refuses a service whose helper socket is its own (`flags.rs:324`). The update is one sequence (`agent/update.rs:1-34`, `run_inner` at `:89`): health precondition, `query` the helper, resume a trial the helper has pending (`:135`), install (fetch and verify, nothing of the node changes), drain the node at the frontend (cordon, wait for leases; `:253`), switch on trial, health (`:162`), confirm or revert, undrain only if healthy; a node that was out of placement before is never undrained. Failure to switch puts the node back (`back_on_a`, `:211`). The health clock starts after the switch returns; `--health-timeout-secs` defaults to 300 and is agent-wide (`flags.rs`, doc at `:60`). A service with `ready-url` is healthy when `GET path` on a loopback address answers 2xx; that "says nothing of which version answered" (`agent/service_health.rs:1-6`).

**Root helper** (`crates/kbf-host-install`, `--serve`). Root on purpose (`packaging/host-agent/linux/kbf-host-local.service:21-26`), with a unix socket of mode 0600 owned by the one account `kbf-updater` as the whole access control (`local/serve.rs:1-30`; the unit passes `--socket-user kbf-updater`, `kbf-host-local.service:26`). Five requests: `install`, `switch`, `confirm`, `revert`, `query` (`local/mod.rs:1-30`). It owns the revert: a release on trial gets a pending marker with a deadline on disk, and `Ab::tick` puts the old release back when the deadline passes unconfirmed, including the first tick after a host restart (`local/core.rs:173`; `local/mod.rs`, "This program owns the revert"). One connection at a time. Every request but `query` and `revert` is audited. It verifies the release itself: the pinned `gh attestation verify`, repository, signer workflow, hosted runners, full commit and allowed branch, from root-owned files (`crates/kbf-installer/src/attest.rs:1-17`), with a list of accepted signers (`attest.rs:136-137`, loop at `:251`). **What is kbf-specific in it:** the component name `"kbf"` and the staged file `kbf.tar.gz` (`local/linux.rs:369`, `:379`, `:387`); the restart timeout and the active wait, 120 s each, constants (`crates/kbf-host-install/src/plan.rs:88`, `:94`); the confirm window, 600 s by default and accepted only from 120 to 1800 (`local/serve.rs:58`, `:175`); the restart units named by `--restart-unit` on its command line (`kbf-host-local.service:26`). A second product therefore needs a second helper process with its own socket today (`docs/deploy/kbf-updater.md:347`).

**tierstore.** `crates/tierstore/src/server/handover.rs` is **not** a process hand-over: it is the hand-over of a put's body from the request to the backend call over a bounded queue (`handover.rs:1-14`). Nothing in the server passes a listening socket. The drain (`server/mod.rs:164-172`, `serve_from` at `:596`) keeps accepting through `--drain-lb-wait` (10 s default, `:410`), then sets `REFUSING` (`:614`), answers every new request 503 on connections it still holds (`:748-749`), waits for requests in flight, then closes connections (`finish`, `:644-647`). The local cache removes `.stage/` at start (`server/local_cache/mod.rs:289`, `layout::clear_stage`) and has no lock today (`local_cache/mod.rs:1-8` says it has "no journal, no lease and no reconciler"), which is why two processes cannot share a cache directory today. Nothing updates tierstore: it is not a managed service of the host updater.

## 2. The registry

### 2.1 What a registration is

A service registers once per start, with these fields. Every one is a claim to be checked, none is authority.

| field | meaning | checked against |
|---|---|---|
| `name` | service name, same grammar as today (`wire::is_service_name`, at most 64 bytes) | must be in the root-owned services file |
| `version` | the version text the service says it runs | compared with the version the executor recorded when it staged the release; a mismatch is reported as unhealthy, never believed |
| `health` | a loopback address and path answering 2xx when ready | must be loopback; port within the range the services file grants this name |
| `strategy` | the strategy name | must equal the services file's strategy for this name |
| `params` | the strategy's parameters (for `port-forward`: public port, private ports, ready path, drain signal) | a closed schema per strategy; values bounded by the services file; unknown keys refused |

### 2.2 Socket or file drop: socket, with the capabilities in a root-owned file

**Option A, a unix socket.** The updater binds `/run/kbf-updater/register.sock`. The service connects on start, sends one JSON line, and holds the connection or reconnects. Authentication is `SO_PEERCRED` on the connection (uid, gid, pid as the kernel recorded them), which the repository already uses in exactly this way for a local signing socket (`crates/kbf-signer/src/server.rs:44-53`, "without `SO_PEERCRED` no peer can be told from another: none is admitted"). The uid must be the one the root-owned services file names for that service. Advantages **[I]**: the updater learns that a service is *running* (the connection), learns of a stop (the close), and can ask the service a question over the same connection; it needs no filesystem the service can write; stale registrations vanish with the connection. Costs: the service needs a small client, and the updater must be up when the service starts (the client retries; the updater's `Restart=always` makes the gap short).

**Option B, a file drop.** The service (or its `ExecStartPre=`) writes a file into a directory the updater watches. Advantages: no runtime dependency in either direction; survives an updater restart trivially. Costs: the directory must be writable by the service accounts, so any of them can write any name unless the directory is per-service (`/run/kbf-updater/registrations/<name>/`, owned by that account); a directory watch has its own limits and failure modes (inotify watch limits, missed events on a full queue); a file does not say the service is still alive; atomic replacement and torn files are each a failure to handle.

**Pick A.** The deciding reasons: peer credentials are a proof the file path cannot give, the connection is a liveness signal, and the repository has a tested pattern for the check. The capability side is still a root-owned file (B's good idea): `/etc/kbf/updater/services.d/<name>.toml`, owned by root and not writable by the registrant, naming the uid, the release channel and signer (repository and workflow, the way the helper's channel allow-list does), the unit names or template, the port ranges, the strategy and its parameter bounds. A service cannot register a name that file does not list, cannot name a unit, and cannot widen a bound. A package or an operator installs the file; "registers when it starts" means the *running* facts (version, health address, strategy parameters inside the bounds), not the grant.

### 2.3 A service that is not running, or never registered

- **Declared, never registered.** The services file lists it, no connection has come. The updater reports it to the coordinator as `absent` (a new health value; today a heartbeat has only `healthy` and `unhealthy`, `wire.rs:29-35`). The coordinator does not count an absent service toward `min_up` and does not order it, unless the service's definition says `install_when_absent` (so a new host can be given a service that has not run yet). This replaces the static `--service` flags: a flag becomes a synthetic registration read from the services file at start, so a host that never gets a registering client still behaves as today.
- **Registered, now not running.** The connection closed. The registration stays for the stop bound in the file (`registration_ttl`, a config value); the service is reported unhealthy with `stopped`. A strategy is told the service is stopped (`ServiceState::Stopped`); `port-forward` and `node-drain-restart` can both update a stopped service (install, start), `lease-drain-restart` has nothing to drain.
- **Registered twice.** A second registration of the same name from the same uid replaces the first (a restarted process). A second from another uid is refused and logged.

### 2.4 Persistence across an updater restart

The accepted registration is written to the updater's state directory (mode 0700, as today `StateDirectory=kbf-updater`) by temporary file, fsync, rename, directory fsync, the pattern of `desired.rs`. On start the updater loads it, marks every service `unconfirmed` and reports it as its last known state until the service reconnects (the clients reconnect on their own). A registration that does not reconnect within `registration_ttl` becomes `stopped`. The file is never trusted for authority: the capabilities are re-read from the services file on every start.

## 3. The generic updater

### 3.1 The pipeline

For one update order of one service, in this order, each step only if the previous one succeeded, and each step recorded in the journal (3.4) before it begins:

1. **Precondition.** The host is healthy enough to update (today `health.refusal`, `agent/update.rs`), the service is declared, no other update runs on the host (the coordinator also enforces it, `decide.rs:105`; the updater enforces it again because it is the one that would be hurt).
2. **Fetch and verify** (executor verb `stage`). Download the release the version names from the channel the services file names; check the sha256 against the signed sums and the attestation (repository, signer workflow, issuer, hosted runners, commit, branch) on the host, with the pinned `gh` and trusted root; lay it out beside the running release. Nothing of the running service changes. The updater never takes a URL or a path from the coordinator's reply: the version is the only input.
3. **Plan** (strategy, pure). The strategy reads the staged release and the registration and returns the steps and their bounds, or refuses (for example: the host's firewall guard is missing, section 4.1).
4. **Prepare** (strategy). Make the new version ready beside the old one without a change a client can see: for `node-drain-restart`, drain the node; for `port-forward`, start the next instance.
5. **Switch** (strategy). The one step clients can see.
6. **Health gate** (updater, generic). The strategy names the observations; the updater repeats them until they pass or the strategy's health bound passes. Pass means: the registered health answers, the reported version equals the staged version, and the strategy's extra checks pass.
7. **Commit** (strategy). After this the previous version cannot be put back by an `abort`; a later problem is a new update to an older version. Each strategy declares its commit point.
8. **Finish** (strategy): undrain, retire the old instance. Report in the next heartbeat (`UpdateResult`, `wire.rs:48`).

On a failure before the commit point the updater calls `abort` with the phase reached, which puts the previous version back and reports `Failed`; on a failure after it, the update reports `Failed` and leaves the host as it is for the coordinator to act on (section 5.4).

### 3.2 What is generic and what is the strategy

Generic, owned by the updater and the executor: the order and the journal; fetch, sha256, attestation, staging; one-update-per-host; bounds as config (every wait has a configured upper bound and a configured name, none is a literal in a sequence); the health gate loop; reporting; the revert deadline and the dead-man timer (3.4); the audit log.

Strategy, owned by the per-service code: `plan`, `prepare`, `switch`, `commit`, `finish`, `abort` and `recover`, plus the declaration of its extra health checks and its commit point.

### 3.3 The strategy interface

A sketch in Rust, as prose for the contract; names are not final.

```rust
pub trait Strategy {
    /// Pure. Refuses with a reason, or returns the steps with their bounds.
    fn plan(&self, cx: &Cx, from: &Release, to: &Release) -> Result<Plan, Refusal>;
    /// Reversible. After this the new version exists beside the old and no client sees it.
    fn prepare(&self, cx: &mut Cx, plan: &Plan) -> Result<(), StepError>;
    /// The one client-visible step. Must be idempotent: running it again after
    /// a crash leaves the same state.
    fn switch(&self, cx: &mut Cx, plan: &Plan) -> Result<(), StepError>;
    /// Extra observations for the health gate (the generic ones always run).
    fn checks(&self, cx: &Cx, plan: &Plan) -> Vec<Check>;
    /// The last step an `abort` can undo. After it, only a new update undoes it.
    fn commit(&self, cx: &mut Cx, plan: &Plan) -> Result<(), StepError>;
    fn finish(&self, cx: &mut Cx, plan: &Plan) -> Result<(), StepError>;
    /// Undo from any phase before the commit point. Idempotent; never fails
    /// for a phase it has not reached.
    fn abort(&self, cx: &mut Cx, reached: Phase) -> Result<(), StepError>;
    /// After a crash: given the journal's phase and what the executor says is
    /// true, go forward, or abort. Never both.
    fn recover(&self, cx: &mut Cx, journal: &Journal, observed: &Observed) -> Resume;
}
```

Guarantees the updater gives a strategy: it calls `switch` only after `prepare` returned and wrote its journal line; it never calls two strategies at once on a host; it calls `abort` with the *journal's* phase, so a strategy that crashed in `switch` sees `Phase::Switching`, not `Prepared`; every bound the strategy reads is a config value, never a literal; it never gives a strategy a shell, a path outside the service's staged directory, or a unit name that the services file does not list. Guarantees the strategy gives the updater: every step is idempotent; `abort` before the commit point restores the previous behaviour; a step reports failure rather than hanging (it has a bound from `plan`).

### 3.4 Privileges: who runs `nft`, `systemctl` and the file replacement

**Keep the split.** The updater holds the farm client certificate, talks to the network and parses a coordinator's reply; it stays `User=kbf-updater` with an empty capability set as today (`kbf-updater-agent.service:32-43`). Making the whole updater root would put a network parser and a certificate in root. The root half is the **executor**, the helper made service-generic. It takes a closed set of verbs from the updater over the same kind of socket (mode 0600, one allowed account, `local/serve.rs:1-30`):

| verb | what it does as root |
|---|---|
| `stage <service> <version>` | fetch, verify (sha256, attestation), lay out; today's `install` |
| `slot-set <service> <slot> <release>` | point a slot's link at a staged release |
| `unit <service> start\|stop\|restart <slot>` | `systemctl` on a unit the services file lists, for a slot that exists |
| `forward-apply <service> <port>` | generate the nftables file from a fixed template with validated integers and apply it with `nft -f` (4.1) |
| `forward-show <service>` | report the rule state and the guard state |
| `trial`, `confirm`, `revert` | today's `switch`, `confirm`, `revert`, with the pending marker and the dead-man deadline |
| `query` | what is staged, running, on trial, and the recorded forward state |

Never a verb that takes a command line, a path or nft text. The nftables file is generated by the executor from the services file's integers; the registrant's `params` are only ever numbers and identifiers that were bounded and checked. The audit log is kept as today: every verb but `query` and `revert` is audited and not run if the log cannot be opened (`local/mod.rs`).

The executor generalises the helper: one process per host for all services, configured by the same root-owned `services.d` files that give the updater its capabilities, so the component name, staged file name, probe, restart timeout, active wait and confirm window are per-service values (the literals at `local/linux.rs:369-387`, `plan.rs:88-94` and `serve.rs:58`). For `kbf` the defaults stay byte-for-byte what they are, with the existing 120 to 1800 s cap on the confirm window; a service file may lift the cap for that service under a load-time invariant (the confirm window must be at least restart timeout plus active wait plus the health bound plus a margin, so a slow healthy start is not reverted).

### 3.5 Persistence and recovery

Two journals, one authority.

- The **executor's record** (root, `/var/lib/kbf-updater/executor/`, rename-and-fsync) is **authoritative for what is true on the host**: which release is staged, which release is in which slot, which slot is active, the recorded forward port, the pending trial with its deadline. The kernel's nftables state is *derived* from it.
- The **updater's journal** (unprivileged state directory) is authoritative only for **what the updater was doing for which order**: the service, the target version, the phase last begun. A line is written before a step begins ("begin switch") and after it ends ("done switch").

After a crash of the updater: on start it reads its journal and calls `query`. If the journal says a step began and not ended, the strategy's `recover` is given both and decides: go forward (the observed state is on the new side of the step) or abort (it is not). The executor's record wins any disagreement.

After a crash of the executor, or a host restart: the executor starts first (`Before=` the service units; the services have no `[Install]`, the executor is what starts the active slot, as the helper is today the only restarter), loads its record, **reconciles the kernel to it** (re-applies the forward file for the recorded active port, re-installs the guard table), then ticks. The first tick does what today's does after a restart: a trial whose deadline passed while the host was down is reverted at once (`local/core.rs:173`). The reconcile also runs on every tick, so a `flush ruleset` by a firewall service that reloads is corrected within one tick (a config interval).

After a crash of a service: its strategy is not involved; systemd restarts it per the unit; the updater reports unhealthy until it re-registers. A crash of the *new* instance during `port-forward` before the switch is `abort` (stop it); after the switch and before the commit it is `abort` too (swap the rule back, 4.1); after the commit it is the coordinator's rollback.

## 4. Strategies

### 4.1 tierstore: port-forward hand-over

Written by the tierstore owner for the service side; the mechanics of the forward are here because they are updater-side.

**Service side.** The full contract, with code references, failure cases and tests, is in
[tierstore-handover.md](tierstore-handover.md).

Registers: name `tierstore`, strategy id `port-forward-handover`, health `GET /readyz` on the instance's
private port (exists), version from a new `tierstore_build_info` gauge (today only the start line and
`--version` carry it, and that is the crate version, not a commit).

What v-next offers, and what is new in tierstore:
- **Read-only start** (`--read-only-cache`, new). It serves hits from the cache and sends misses and writes to the
  upstream. It must not clear `.stage/`, delete sidecars, chmod, drop entries on a read error, or stamp file times;
  each of those happens on today's start or read path.
- **The cache lock** (new): `<cache>/.lock`, flock, exclusive, non-blocking, held by the read-write instance.
  The kernel frees it when the process dies.
- **`promote`** (new): SIGUSR1, not an HTTP route. Once the rule points at an instance, every route on its port is
  reachable by any client. Promote takes the lock, cleans `.stage/`, rescans the index, and enables fills and
  eviction. On a storage node it also starts the reconciler. The ack is a cache-mode gauge.
- **The drain** (changed): `Connection: close` on every answer from drain start. v-current keeps accepting through
  the settle wait (`--drain-lb-wait`, never 0 here). At the end of the settle, idle connections close, and a request
  already on the wire is served, not answered 503. Today's drain answers 503 on kept connections.

Storage node: each slot has its own proxy id (`tierstore-a` and `tierstore-b`), so two processes never write one
journal. v-next does not run the reconciler until it is promoted; v-current releases the lease on exit. A `dir:`
hot tier cannot have two lease holders, so it keeps the plain restart. A restarted holder can reproduce an older
lease body and tag (kbf#2108); a per-process nonce fixes that before the hand-over relies on the lease.

Metrics: the directory gauges (`cache_bytes`, `cache_objects`, `cache_fs_free_bytes`) double-count while both
instances run, and the fill and evict families stay 0 on v-next. No family names a slot or mode today; the build
and mode gauges are new. Needed from the mechanics below: established flows stay on v-current, and the private
ports are loopback-only (the guard closes the `route_localnet` trap).

**Shape [D].** Clients always call the public port, here 9300. No process listens on it. Each tierstore instance listens on its own private port, 9301 or 9302, alternating. One nftables redirect per family sends public-port connections to the current instance. An update starts v-next on the idle private port, waits for its `/readyz`, swaps the rule in one transaction, lets v-current drain on the flows conntrack still maps to it, and then asks v-next to take over the cache (the **promote** verb described in the tierstore owner's section). Rollback before the commit point is swapping the rule back.

Mapped onto the pipeline of 3.1:

| pipeline step | port-forward |
|---|---|
| plan | refuse if the guard table is missing or stale; refuse if the conntrack established timeout is not longer than the service's idle bound (4.1.3); choose the idle slot (the port not in the recorded forward) |
| prepare | `slot-set` the idle slot to the staged release, `unit start`; wait for the instance's `/readyz` 200 **on its private port** (this includes its start-up self-test: it does not bind before the self-test passes) |
| switch | `forward-apply <idle port>` |
| health gate | probe through the **public** port from the node (now answered by v-next) as well as the private one; the version the instance reports must equal the staged version |
| commit | the moment v-current is told to drain and its settle bound passes (it has closed its listener): from then on a swap back would send new connections to a closed port |
| finish | wait for v-current to exit (its drain bound), call the instance's `promote`, record the new active slot, retire the old slot |
| abort (before commit) | `forward-apply <old port>`, `unit stop <idle slot>` |

Between `switch` and `commit` the updater observes v-next through the public path for the strategy's health bound, **before** it tells v-current to drain. That keeps the revert alive while the evidence about v-next is gathered. The cost is a longer window in which both instances run; the cache is not yet v-next's during it, so the window has no extra write hazard.

#### 4.1.1 The ruleset

One `inet` table per service for the redirect and one for the guard. The `inet` family handles IPv4 and IPv6 in one rule; NAT in an `inet` chain needs Linux 5.2 or later (nftables wiki, "Performing Network Address Translation (NAT)": stateful NAT in the inet family "since Linux kernel 5.2") **[V on the oldest supported kernel]**. For an older kernel the same rules are generated twice, once for `ip` and once for `ip6`.

```
table inet kbf_forward_tierstore {
    chain pre {
        type nat hook prerouting priority dstnat; policy accept;
        fib daddr type local tcp dport 9300 redirect to :9301
    }
    chain out {
        type nat hook output priority -100; policy accept;
        fib daddr type local tcp dport 9300 redirect to :9301
    }
}

table inet kbf_guard_tierstore {
    chain in {
        type filter hook input priority filter; policy accept;
        iifname "lo" tcp dport { 9301, 9302 } accept
        tcp dport { 9301, 9302 } ct status dnat ct original proto-dst 9300 accept
        tcp dport { 9301, 9302 } drop
    }
}
```

The nft syntax above is the intended text; the first rule-generation test loads it with `nft -c -f` in a namespace to pin it **[V]**. Four points about it:

- **`fib daddr type local` on both rules.** Without it, `tcp dport 9300` in the output chain also redirects the host's own outgoing connections to port 9300 of *other* hosts (every node that calls another node's tierstore would be sent to itself), and in prerouting it would redirect forwarded traffic if the host routes. The match restricts the rule to addresses that are the host's own.
- **Redirect, not DNAT.** `redirect` rewrites the destination to the local machine. Per the iptables REDIRECT description, which nftables follows **[F: documentation; the kernel behaviour is not re-read here]**: for packets arriving from outside the destination becomes the *primary address of the incoming interface*; for locally generated packets it becomes 127.0.0.1 (::1 for IPv6). So the private listener has to accept connections on **both** the node's address and loopback. The simple way is to listen on the wildcard address for the family (`0.0.0.0:9301`, and `[::]:9301` with `IPV6_V6ONLY` set explicitly so the result does not depend on a sysctl). The alternative, `dnat to 127.0.0.1:9301` with the listener bound to loopback only, does not work for packets arriving from outside unless `net.ipv4.conf.<interface>.route_localnet=1` is set (the kernel documentation: "Do not consider loopback addresses as martian source or destination while routing. This enables the use of 127/8 for local routing purposes", default false). That sysctl makes the host accept packets to 127/8 from the wire, which is a known attack surface for services that trust loopback, and it applies to every service on the interface. **Choose redirect plus wildcard listeners plus the guard (Question 6).** The price is that the private port is open on all interfaces unless the guard is loaded; the guard is therefore part of `plan` (refuse to start v-next without it) and of the executor's reconcile.
- **A trap in the guard.** After the redirect the packet reaches the *input* hook with destination port 9301, not 9300. So a host firewall that allows 9300 from some networks stops matching: remote callers are dropped by the host's own policy even though the redirect worked. The guard's second rule accepts the redirected flows (`ct status dnat`, original destination port 9300) in the guard table; another table's drop or default-drop policy is **not** overridden by an accept here, so the operator's firewall must match `ct original proto-dst 9300` instead of `dport 9300`. State this in the service's packaging notes,.
- **The third rule drops** anything else to the private ports: a remote caller that addresses 9301 directly reaches no listener-bypassing path. Loopback sources are accepted (the first rule), so the updater can probe v-next directly. Any local process can reach the private port directly too; that gives it nothing it lacks on the public port, because the service authenticates every request itself.

#### 4.1.2 The atomic swap

The swap is one file given to one `nft -f`, containing

```
add table inet kbf_forward_tierstore
flush table inet kbf_forward_tierstore
add chain inet kbf_forward_tierstore pre { type nat hook prerouting priority dstnat; policy accept; }
add chain inet kbf_forward_tierstore out { type nat hook output priority -100; policy accept; }
add rule inet kbf_forward_tierstore pre fib daddr type local tcp dport 9300 redirect to :9302
add rule inet kbf_forward_tierstore out fib daddr type local tcp dport 9300 redirect to :9302
```

The same file installs the rules the first time (`add table` is a no-op when it exists), applies a swap and applies a rollback; only the port differs. The wiki ("Atomic rule replacement") says `nft -f` reads the whole file, builds the new objects beside the existing configuration and swaps "in one single transaction", so "there is no moment when the firewall is partially configured"; it documents `flush table` at the top of a file for exactly this use. A packet is evaluated against the old ruleset or the new, never a mix and never an empty chain. The guard table is separate and is not in the file, so it is never flushed by a swap.

What clients see during a swap: nothing. The redirect runs only for the first packet of a connection (wiki: "No rule lookup happens for follow up packets in the flow"); a SYN that arrives just before the swap is mapped to v-current, a SYN just after is mapped to v-next, and neither is dropped or refused by the swap. Not claimed: that the swap is free of every race; 4.1.3 names the one that matters.

#### 4.1.3 Conntrack

A flow's NAT mapping is stored in its conntrack entry when the first packet creates it, and every later packet of the flow uses that entry, whatever the rules now say. So connections established before the swap keep going to v-current, which is what the hand-over needs; connections made after go to v-next.

- **The handshake race.** A connection whose SYN was mapped to v-current just before the swap is still completing its handshake, or sits in v-current's accept queue, when the swap happens. If v-current closed its listener at that moment, the queued connection would be reset. So v-current must **keep accepting for a settle period after the swap** and only then stop. That is what `--drain-lb-wait` already does on main: it serves everything and accepts through the wait, then stops accepting (`server/mod.rs:596-615`). Its meaning changes from "time for a load balancer to notice" to "time for connections already mapped to me to arrive"; it is a config value, never zero here. A retransmitted SYN keeps its mapping (the entry exists), so the settle bound covers the client's SYN retransmissions (1, 3 and 7 seconds in a default Linux client) **[I]**. Choose the value in the services file; the default of 10 s covers the three.
- **Idle flows and conntrack timeouts.** The established-TCP timeout is `nf_conntrack_tcp_timeout_established`, default 432000 s (5 days) per the kernel documentation. A keep-alive connection that is idle longer than the entry's timeout loses its entry. When the client then sends on it, the packet is not a SYN, but with `nf_conntrack_tcp_loose` enabled (the default) conntrack picks it up as a new entry, and the NAT hook runs for it: it is mapped by the **current** rule, which after a swap is v-next. v-next has no such connection and answers with a reset. Before the swap the same mid-stream pickup would have been mapped to the same port as the socket and worked. So the hand-over adds one failure mode, for exactly one kind of flow: one that idled past the host's conntrack timeout and is reused after the swap. Two bounds close it: the service closes idle keep-alive connections itself after its idle bound (`header_timeout`, 60 s default, `server/mod.rs:346`, `:409`), far below five days; and the plan refuses the update if `nf_conntrack_tcp_timeout_established` read from `/proc` is not longer than the service's idle bound plus a margin. Both are checks of configuration, not of timing. If an operator lowers the timeout below the idle bound the plan says so and the update does not start.
- **Draining flows.** While v-current drains, its established flows keep their entries and are served. A flow whose entry expires during the drain, if any, falls under the previous paragraph and gets a reset on its next packet; the drain closes idle connections at the settle's end, so the exposed set is empty by construction once the drain has done that.
- **Other failures to name.** `conntrack -F` (a flush) drops all mappings: later packets on established flows are picked up as new entries and mapped by the current rule, so a flush during the drain resets the flows still on v-current. A full table (`nf_conntrack_max`) drops new connections with a kernel log line, with or without this design. The executor does not run `conntrack` itself.

#### 4.1.4 On-node callers

Local callers (the same host's other services, a sidecar, a container on a bridge) are covered by two paths: a process on the host connecting to 9300 goes through the **output** chain (its destination is local, so `fib daddr type local` matches), mapped to 127.0.0.1:9301; a container on a bridge arrives at **prerouting** like a remote caller. Both rules carry the same port. A process that connects to the node's own LAN address uses the output chain as well. Loopback traffic needs loopback to be up in the namespace the rules live in (an obvious point that a test namespace forgets).

#### 4.1.5 Readiness and metrics on private ports

The probes (`/healthz`, `/readyz`) and `/metricz` are served by the same listener as the data path (the S3 listener), so they exist on each private port. The updater probes v-next directly on `127.0.0.1:<idle port>` (accepted by the guard's first rule) and probes the public port from the host to see who answers it. A remote scraper or a load balancer that addresses 9300 sees whichever instance is current; it never sees the other. A scraper that wants both instances scrapes the node's two private ports locally (a node-local scraper, or a registration parameter that publishes the two addresses); the instance label is the service's proxy id, which must differ between the two instances (the unit comments already require "one of its own" per process over the same tiers, `packaging/tierstore/tierstore.service`). Counters reset per process, as they do at any restart.

#### 4.1.6 What the strategy still needs from the service

Taken from the tierstore owner's section, listed here only so the updater side is complete: a read-only start mode; the `promote` verb; a drain that closes idle connections at the end of the settle and answers requests that arrive meanwhile instead of 503. The current drain answers 503 on connections it still holds from the moment it stops accepting until in-flight work is done (`server/mod.rs:614`, `:748-749`), and that would turn every keep-alive client of v-current into a failure after the swap; this is the main change the service needs for the hand-over to be zero-failure, and it is specified there.

### 4.2 js-agent: lease-drain-restart

Written by the js-agent owner. Facts are from the jobscheduler repository: **[F]** is merged code on its main (a path or PR is given), **[F-open]** is an open pull request (jobscheduler#509 the farm unit, #493 the image pull and the store lock, #516 the `crun` runtime wiring), **[new]** is work this design adds, **[D]** a design choice, **[V]** to verify before building on it. Where the interface of 3.3 does not fit, the text says so.

**The shape [D].** The strategy drains, then stops the agent, swaps the release, starts it, and waits to see it join the lease server again. The agent is never killed first: until the keeper exists, a `crun` container can outlive an agent that dies without cleaning up (a claim of the jobscheduler owner, **[V]**: the runtime's doc says `crun run` dies with its parent, and the sweep still kills a stray cgroup, `crates/js-agent/src/crun/mod.rs` in jobscheduler#508). No lease is handed over: a job either finishes inside the drain, or is stopped at its end and the lease server runs it again from the rerun budget **[F]** (same file).

**What exists.**
- *The agent's own drain [F]* (`crates/js-agent/src/lib.rs`, `cli.rs`): SIGTERM or SIGINT sends a heartbeat with the phase `draining` at once, takes no new job, lets running jobs finish for `--drain-wait` (default the stop grace, at most 3600 s), stops the rest with `--stop-grace` (default 10 s, at most 3600 s), and keeps heartbeating until the server has acknowledged every result, then exits 0. A second signal ends it at once.
- *The unit [F-open #509]* (`packaging/js-agent/js-agent-crun.service`): `Delegate=yes`, `DelegateSubgroup=supervisor` (agent in `<unit>/supervisor`, lease cgroups in `<unit>/actions`), `KillMode=mixed`, `--drain-wait 3600 --stop-grace 120`, `TimeoutStopSec=3900`, `Restart=always`, `RestartSec=30`. A host that lacks what `crun` needs makes the agent exit 2 with every missing piece named (`crun_host.rs`).
- *The sweep [F]* (jobscheduler#508): `CrunRuntime::open` removes every lease whose sidecar (`<scratch>/oci-labels/*.json`) names this agent id: kills what runs in the lease cgroup, deletes the container, the cgroup, the directories, then the sidecar. It leaves another agent's and malformed sidecars alone and reports them; a lease it cannot remove keeps its sidecar for the next start. The swept runs are lost, not adopted.
- *Taint and owed checks [F]* (`lifecycle/mod.rs`): a marker under `--state-root` taints the host; an `owed-*` record means a job's checks did not finish, and a restart or SIGTERM inside that window taints the next start. A tainted agent refuses every start (`host-tainted`) and nothing clears it: "the host is reimaged".
- *Locks [F-open #493, #516]*: agents take the image store's lock shared; the start-time stage sweep runs only if `LOCK_EX|LOCK_NB` succeeds; #516 adds a per-scratch-root claim, so a second `crun` agent on the same roots is refused.
- *The server's view [F]*: the hello carries `agent_version` (`js-agent-wire/src/hello.rs`); `GET /v1/hosts` lists hosts with it (`js-lease-server/src/hosts.rs`), and kbf already has a typed client for it (`kbf-lease-client`). A second hello for a live host id is a 409 `host-live`, retried with backoff.
- *Cordon [F]*: `js-sched` has `Cordon`, `Drain(deadline)` and `Uncordon` as operator actions on a worker; a drain never kills a lease and pauses at its deadline (`js-sched/src/cordon.rs`, `js-server-core/src/core.rs`); the rows persist in the store (`js-store/src/cordon.rs`). I found no HTTP route or CLI for them in the lease server or `js-cli` **[V]**. `kbf cluster drain <node>` (`kbf-cli/src/cluster.rs`) is the kbf frontend's node drain; whether it reaches a js-agent host is **[V]**.
- *`js-agent --version` [F]*: clap prints the package version. The line `runtimes: docker crun` is **[new]** (not in #509 or main as read), and the runtime in the hello is komira's pending addition (`contract.rs`) **[new]**.

**Registration.** The agent has no loopback health listener and no registration client **[F]** (no listen or health flag in `cli.rs`). The services file gives the host a synthetic registration (2.3): name, unit, uid, strategy `lease-drain-restart`, and no `health` address. Health is learned from the lease server instead (checks, below) **[new]**.

**Mapped onto 3.1 and 3.3.**

| step | lease-drain-restart |
|---|---|
| plan | pure. Refuse if the staged release is not an agent of this service, the services file's bounds are inconsistent (below), or the host is tainted (the journal or `GET /v1/hosts`). Returns the three bounds. |
| prepare | `stage` has fetched and verified the release (3.1 step 2). The executor then runs `<new> --version` as the service's user; the line must name the runtimes the services file requires (`crun`) **[new]**. Then **cordon**: no new leases **[new]**; the first choice is the lease server's `Cordon`/`Drain` for this host if a route exists, else nothing (the agent's SIGTERM already announces `draining`). Then wait up to `drain_wait` for `GET /v1/hosts` to show no lease on the host. The new version cannot run beside the old (one agent per host id and scratch root), so "beside" here means staged and checked, not started. |
| switch | `unit stop` (SIGTERM, then the agent's drain of at most `drain_wait + stop_grace`, bounded by `TimeoutStopSec`; the unit has `KillMode=mixed`, so only after that is the cgroup killed), `slot-set` to the new release, `unit start`. The three are idempotent given the executor's record. Done as the executor's `trial` (3.4), whose restart is the drain, so the dead-man deadline reverts the host by itself if the updater is gone. |
| checks | all of: a hello after the start time carrying the staged `agent_version` and `runtime=crun`; the sweep ran clean (no foreign or malformed sidecar, no lease kept: nothing reports the `SweepReport` outside the log today, **[new]**); no taint and no `owed-*` record. Bounded by `health_bound`. |
| commit | the executor's `confirm`, issued only after every check has passed once and the host has been seen `ready` in `GET /v1/hosts` for `bake` consecutive polls. From that moment the old release cannot be put back by `abort`; a bad release is then a new update. Cordon is lifted by `finish` (`Uncordon`), and only if this update placed it. |
| abort | before `confirm`: `revert` (old release back, `unit start`), then uncordon if this update cordoned. Runs stopped during the switch are not brought back; the server reruns them. Abort during `prepare` is only uncordon. |
| recover | below. |

**Recovery after a crash [D].** The executor's record wins (3.5). From the journal phase and `query`:
- *Updater died during the wait in `prepare`:* nothing but the cordon was changed. If the cordon is ours and the drain bound has passed, abort (uncordon); else resume the wait.
- *Updater died during `switch`:* if the pending trial exists, go forward from the health step; the unit's state and the slot say which of stop, slot-set, start were done; each is re-run only if not. If no trial is recorded, abort.
- *Agent crashed mid-drain, or the host rebooted:* systemd restarts it (`Restart=always`). The new process sweeps the previous run's leases (the sidecar ledger), which are lost and rerun, and starts clean. A crash inside the cleanup window leaves an `owed-*` record, so it starts tainted: **the updater stops, reports `Failed` with the taint, and pages; it never clears a taint or an owed record, and never retries** (no clear flow exists **[F]**).
- *Trial deadline passes while draining:* the confirm window must exceed `stop_timeout` plus start and checks (below), or the executor reverts a healthy drain.

**Bounds, all flags of the services file [D].** None is a literal in the strategy. Defaults match the unit.

| name | default | meaning |
|---|---|---|
| `drain_wait_secs` | 3600 | most time to wait for leases to end; equals the unit's `--drain-wait` |
| `stop_grace_secs` | 120 | the unit's `--stop-grace` |
| `stop_timeout_secs` | 3900 | the unit's `TimeoutStopSec` |
| `health_bound_secs` | to set | most time for the checks after start (new work: no default exists; the agent's `RestartSec` is 30 s) |
| `bake_polls` | to set | consecutive ready observations before `confirm` |
| `confirm_window_secs` | at least `drain_wait + stop_grace + 30 + health_bound` plus a margin | lifted above the 1800 s cap of 3.4 for this service |

The plan refuses if the services file's `drain_wait + stop_grace` exceeds `stop_timeout`, or the unit's own flags (read from the staged unit) differ from the file's; otherwise `systemd` would kill a drain the updater is still waiting on.

**New work [new], in one list.** (1) `<new> --version` prints the runtimes. (2) The runtime in the hello (komira, `contract.rs`). (3) The updater reads `GET /v1/hosts` for its own host id, with a read credential. (4) An operator cordon route on the lease server, or accept that SIGTERM's `draining` is the cordon. (5) Reporting the sweep result outside the log. (6) The synthetic registration without a health address. (7) The bounds above in the services file.

**Open points.** (a) *The keeper:* until it lands, a stop that reaches `TimeoutStopSec` or a cgroup kill is the only way a container leaves; the strategy never kills the agent first, and a kill is the unit's. (b) *Lease hand-off:* none, by design; a stopped job reruns, so a job that cannot rerun must not be on a host that is updated. (c) *Cgroup layout across versions:* `supervisor` and `actions` are names two versions must agree on; a release that changes them cannot be adopted by the sweep of the old one. Plan should compare the staged unit's layout with the running one and refuse a change **[new]**.

### 4.3 kbf-daemon: node-drain-restart (today's behaviour)

This strategy is `agent/update.rs:89-250` moved behind the interface of 3.3 and run by the executor's generic verbs, with no change of behaviour:

- `plan`: the host health precondition (`health.refusal`); the node's cordon state before the update (a node already out of placement is never undrained).
- `prepare`: drain the node at the frontend (cordon, wait until it holds no lease, however long that takes unless `--drain-deadline-secs` is set; a deadline that passes stops the update with the node left drained and nothing installed over running leases). The cordon mark is kept on disk (`--cordon-dir`).
- `switch`: the executor's `trial` verb: pending marker with deadline first, links move, units restart.
- `checks`: the node's health through the frontend (`node_health`), or a service's ready URL; both bounded by `--health-timeout-secs`.
- `commit`: `confirm`.
- `finish`: undrain, only if healthy and only if the updater's own drain placed the cordon.
- `abort`: `revert`; the executor does it by itself at the deadline whatever the updater is doing.
- `recover`: the helper's pending marker means a trial is on; finish it from the health step; do not fetch, drain or switch a second time (`agent/update.rs:135`).

A frontend that runs as a container and a host on the `kbf` package (`deb`, `rpm` layouts) are variants of the same strategy with another backend under the executor, as they are today (`local/mod.rs`: `frontend`, `package`, `linux`, `mac_backend`).

## 5. The generic coordinator

### 5.1 Desired version per service; host sets

The coordinator already stores one desired state per service (`desired.rs:239`) and defines services as data (`services.rs`). A host's membership in a service is its heartbeat's list of services and its role, from the certificate. Proposal: a **host set** is a named selector over certificate roles and labels in the service definition, so a service defines where it runs without a built-in enum. Today the roles are the four of `HostRole` (`kbf-types/src/host_role.rs:14`, `ALL` at `:35`), and a tierstore host must be called `storage`.

### 5.2 One host at a time across all services

Already true per host: rule 2 (`decide.rs:105`) orders nothing to a host that has any service updating or an order outstanding. Whether it should also be true **across hosts** (global) is Question 3.

### 5.3 Canary, health gate, rollback; how health is learned

- **Canary.** None in the coordinator today. A one-host canary can be modelled as a separate service with `min_up = 0`, which is a workaround (without `min_up = 0` a lone host is never ordered, `decide.rs:116-120`). Proposal: a first-class `canary` in the service definition: a count or a host selector, ordered first; the rest wait for it.
- **Health gate.** The coordinator never probes: the agent reports `Health` and `UpdateResult` (`wire.rs:29-52`), and a failed update shows as `last_update.outcome == Failed` for the desired version, which keeps the host's slot and makes the service `blocked` (`converge.rs:1-14`). Proposal: keep that, and add the gate as a rule on the *reports*: after a host (or the canary) updates, it must report `healthy` and the desired version for `bake` consecutive heartbeats (a config count) before the next host or the rest of the set is ordered. The coordinator has no network path to a host's loopback and should not get one; the updater's strategy runs the real checks (4.1, 3.1 step 6) and reports the result.
- **Rollback.** Today a human or CI sets an older version. Proposal: the coordinator remembers `last_good` per service (the last version for which the set converged and passed the gate) and, when a service is `blocked` or a gate fails and the service's definition says `rollback = "automatic"`, sets desired to `last_good` and marks the failed version `bad` (it is not ordered again until another version is set). CI can still set a version by hand, which clears the mark. The hosts then roll back through the same strategy as any update.

### 5.4 Where today's coordinator is kbf-specific

| where | what | change |
|---|---|---|
| `services.rs:90` | `kbf` is built in and cannot be redefined | make it a definition in the same file format, shipped as a default |
| `desired.rs:36`, `:86`, `:108` | `MaxUpdating` and `MinUp` are structs with the four kbf roles as fields | per-role limits move into the service definition; the desired state carries a version and optional overrides keyed by role name |
| `converge.rs:15-16` | converged needs `min_up` storage and frontend hosts up | read `min_up` per role from the definition |
| `wire.rs:57-100`, `:138`, `:154` | `kbf` is the top-level heartbeat; others are in `services`; `kbf` may not appear in `services` | new heartbeat version lists every service including `kbf`; the old shape is read for as long as old agents exist |
| `wire.rs:235`, `:254` | two reply shapes for `kbf` and for other services | one shape with a service field; the old reply still sent to an old agent |
| `kbf-types/src/host_role.rs:14` | roles are an enum of four | roles are strings read from the certificate and from the definition |
| `decide.rs:1-34` doc, `services.rs:3-8` | the code branches on no service name (read); only the prose names `kbf` | no code change |
| no code | no canary, no `last_good`, no automatic rollback, no `absent` health | 5.3 |

## 6. Where the generic code lives

**Option one: kbf's existing crates, generalised in place.** The crates exist (`kbf-updater`, `kbf-coordinator`, `kbf-host-install`, `kbf-installer`), their tests pin today's behaviour, and the release pipeline already signs and attests them: hosts verify kbf's artifacts workflow as the signer (`crates/kbf-installer/src/attest.rs:1-17`), with a list of accepted signers (`attest.rs:136-137`) that is the existing mechanism for a second signer. Cost: the generic parts carry the name and the compile times of a repository that is mostly something else.

**Option two: new crates, `host-updater` and `fleet-coordinator`, in kbf or in a repository of their own.** In kbf this is renaming plus a boundary lint; in its own repository it needs its own release pipeline, its own signer identity on every host (a second trust anchor, a decision of its own), and its own cross-repository testing against the services.

**The dependency question.** tierstore is moving to a repository of its own, and its strategy must not make the updater depend on tierstore or tierstore on the updater. The `port-forward` strategy does not need either: it is **parameterised, generic code in the updater** (public port, private ports, ready path, drain signal, family set) and works for any TCP service that offers the same four things. What is tierstore-specific is a **contract** the service implements: private-port listening, a read-only start mode, a `promote` verb, a drain that closes idle connections. The contract is a document and a golden test, in the updater's repository; the service implements it with no code from the updater. The same holds for the registration protocol: it is one JSON line over a unix socket, small enough that a service reimplements the client (an optional crate may follow). So: **updater depends on no service; services depend on no updater crate; the contract is the interface** (Question 5).

**Recommendation: option one now, with the boundary drawn as option two would.** Generalise in kbf; keep the generic code in crates whose names and public items mention no product; add a lint that the generic crates name no service; move to their own repository only when a second owner needs to change them. A repository split is cheap later if the contract is the only interface, and expensive early because of the trust anchor on every host.

## 7. The interim per-service deploy workflow

Until the pieces above exist, a service that is not `kbf` is rolled by an interim per-service deploy workflow (kept in kbf, off unless a repository variable is set, in its own concurrency group). What it does now: after a release is published, it sets that release as the desired version of the service with the CI token (an allow-list entry that names only that service, `docs/design/updates.md`, section 5.1), polls the convergence of that service (`GET /v1/convergence?service=S`, `docs/design/updates.md`, section 3), runs a check against the hosts, and on a failed check sets the desired version back to the version recorded before the roll. A stall or a cancelled job is "incomplete", not a failure of the release, because the coordinator goes on rolling by itself once the version is set.

How it maps onto the generic pieces later:

| the workflow does | later done by |
|---|---|
| sets desired, polls convergence | unchanged; the workflow becomes "set desired and read the result" |
| runs a check on the hosts | the strategy's `checks` and the coordinator's health gate (5.3) |
| rolls back by setting the recorded version | the coordinator's `last_good` and automatic rollback (5.3) |
| a one-host canary service with `min_up = 0` | the first-class canary (5.3) |
| restart timeout, active wait and confirm window as per-service config of the helper | the same values, in the executor's `services.d` file (3.4) |
| records the attempt | the coordinator's `bad` mark |

Nothing in the workflow is thrown away without its replacement being in place first; the workflow is deleted by the last PR of section 8.

## 8. Migration order

Each step is one PR, independently shippable, with its tests written first and seen to fail.

1. **Characterisation.** Tests that pin today's behaviour: a kbf-only golden for `decide` (every decision of `decide_tests.rs` unchanged), a golden for the helper's timeouts and window, and a golden for the agent's `run_inner` call order against the existing fake ports (`fakes.rs`). Red proof: each golden fails against a deliberately altered constant.
2. **Strategy extraction.** Move `run_inner`, `trial`, `resume` and `back_on_a` (`agent/update.rs`) behind the `Strategy` trait as `node-drain-restart`. No behaviour change; the existing update tests pass unmodified. Red: a test that a strategy whose `switch` fails is aborted at the journal's phase.
3. **Registry and the services file.** The socket, `SO_PEERCRED` check, the `services.d` file, persistence, `absent` health. The static `--service` flags become synthetic registrations. Red: a registration from the wrong uid is refused; a name not in the file is refused; a parameter outside the bound is refused; a registration survives an updater restart.
4. **Executor generalisation.** Component name, staged file name, restart timeout, active wait and confirm window per service, the load-time invariant, one process serving every service; the existing `kbf` helper unit is an instance with defaults. Red: a configuration violating the invariant is refused; a slow healthy start under the lifted cap is not reverted (logical time).
5. **Coordinator generalisation.** Roles and limits from the definition; shared role slots across services; `canary`, `bake`, `last_good` and automatic rollback; the heartbeat that lists every service. Red: with only `kbf` defined, decisions are unchanged; a failed host keeps the shared slot; a rolled-back version is not ordered again.
6. **Port-forward mechanism.** Pure rule generation (the two tables), `forward-apply` and `forward-show`, reconcile at start and on tick. Red: see 9.
7. **tierstore service changes** (in the tierstore repository or crate): the drain, read-only mode, `promote`, two-instance packaging (a unit template whose instance is the private port, a distinct proxy id per instance). Specified in the tierstore owner's section.
8. **`port-forward` strategy and the hand-over test** (section 9).
9. **js-agent strategy**, from its owner.
10. **Retire the interim workflow** (section 7) and the static `--service` flags.

## 9. Tests

The rules for all of them: **no test asserts on or depends on elapsed time**; tests are driven by handshakes (a barrier counted in completed requests, an exit status, a start line, an answer), and hang guards live in `.config/nextest.toml`. **Red proofs come first**: each test is written, seen to fail against the wrong behaviour, and only then is the behaviour written.

**The hand-over test, with clients hammering.** A harness starts two real tierstore processes over fake stores on private ports 9301 and 9302, in a network namespace with the real rules loaded, and N client tasks looping: a signed create-only put of a unique key, a get of it, a ranged get, each over keep-alive connections and some over fresh ones. The sequence is driven by counts, not durations: (1) every client has completed at least one request answered by v-current (identified by a response header carrying the proxy id); (2) the harness runs the swap; (3) every client has completed at least one request answered by v-next; (4) the harness tells v-current to drain and waits for its exit status; (5) every client completes at least one more request; (6) the clients stop and the test asserts **zero failed requests**. The clients run continuously across steps 2 to 5, so the swap lands at an arbitrary point in their traffic; the assertion does not depend on where. The client policy is stated and fixed: a request that fails on a **reused** connection before any response byte is retried once on a new connection, as an HTTP client does (the keep-alive close race, Question 8); the test counts retries and asserts the count does not exceed the number of closed idle connections, so a retry does not hide a reset. Red proofs: (a) swap to a port nothing listens on, and the test must fail with refused connections; (b) the same sequence with v-current closing its listener at the swap instead of after the settle must fail with reset connections; (c) the current 503-on-reuse drain must fail with 503s on reused connections; (d) lower the conntrack established timeout below the idle bound in the namespace and the plan must refuse, and a forced run must fail with a reset on a reused idle connection.

**Rule generation.** Unit tests, no namespace: the generator's output for a port pair is a golden file; ports outside the bounds, equal ports and reserved ports are refused; the swap file differs between `9301` and `9302` only in the two port numbers (a mutation test: change a rule's family or hook and the golden fails); the guard accepts only `lo` sources and redirected flows. A `nft -c -f` check of the golden in a namespace pins the syntax **[V]**.

**Registry.** Wrong uid refused; unlisted name refused; out-of-bound parameter refused; second registration from the same uid replaces; from another uid refused; persistence across a restart; `absent` when never registered; `stopped` after the close and the TTL (logical time). Each with its red proof.

**Strategy fakes.** The `Strategy` trait gets a fake that fails at each phase; the updater's tests assert that `abort` is called with the journal's phase, that a crash between the journal line and the step is resumed by `recover` exactly once, and that nothing after the commit point is aborted. The executor is faked at the verb boundary, as the agent's helper is today (`fakes.rs`).

**Coordinator over fake hosts.** The existing `decide_tests.rs` shape with several services: shared slots, canary first, bake count, gate failure, `last_good` rollback, `bad` mark, a coordinator restart mid-roll. The kbf-only golden must be unchanged.

**Can CI run nftables in a namespace?** The machinery needs `CAP_NET_ADMIN` in the namespace the rules are loaded in, and loopback brought up there. `unshare -rn` (a user namespace mapping the caller to root plus a new network namespace) gives that without host root **if the runner allows unprivileged user namespaces**. I could not run it here, so this is **[V]**: Ubuntu 24.04 can restrict unprivileged user namespaces by an AppArmor policy (`kernel.apparmor_restrict_unprivileged_userns`), and a hosted runner may or may not have it set. The repository already has a root path on hosted Linux runners: `cargo xtask sudo-test` runs a test as root under passwordless sudo (`.github/workflows/host-install-linux.yml:47-51`). So: the tests that need only one namespace (the output chain, the guard from loopback, the swap, conntrack behaviour on loopback) run under `unshare -rn` if it works, else under `sudo-test`; the tests of the **prerouting** path need two namespaces joined by a veth pair (a remote caller), which `unshare -rn` alone cannot build, so they run under `sudo-test` with `ip netns`. The first PR of the series (step 6) is a probe job that prints which of the two works and fails red if neither does; nothing relies on the answer before then. Whether `nft` is on the runner image is also **[V]**; the job installs the package if not.

## 10. Questions

Each with a recommendation. A decision goes to the owner of this design.

- **Question 1: registry transport.** A unix socket with `SO_PEERCRED`, or a file drop. *Recommend the socket, with capabilities in a root-owned file (2.2).*
- **Question 2: privilege.** One root updater, or the unprivileged updater plus a root executor. *Recommend the split of 3.4: it keeps the certificate and the network parser out of root, and generalises the helper that exists.*
- **Question 3: serialisation.** Per host only (today's rule 2), per host plus shared role slots across services (so at most one host of a role is down for any service), or global (one host in the whole fleet at a time). *Recommend per host plus shared role slots: global makes the update time of a fleet the sum of all services, and one stuck host would stop every service, while shared slots keep two services from taking two hosts of one role down together.*
- **Question 4: who owns automatic rollback.** The coordinator (`last_good`, `bad` mark), or each deploy workflow. *Recommend the coordinator: it works with no CI in the loop, and a workflow that is cancelled or lost cannot leave a half-rolled fleet.*
- **Question 5: where the code lives and who depends on whom.** *Recommend option one, in kbf, with product-free crates, a name lint, and the contract (registration protocol, strategy parameters, hand-over contract) as the only interface to services (section 6).*
- **Question 6: private listeners.** Wildcard listeners plus a guard with redirect, or loopback listeners plus DNAT and `route_localnet`. *Recommend wildcard plus guard: `route_localnet` widens the host's acceptance of loopback addresses for every service. Accept the cost that a missing guard exposes the private port, and make the guard a plan precondition and part of the reconcile.*
- **Question 7: one drain or two.** The service's drain changes (close idle connections when it stops accepting, no 503, `Connection: close` once draining) either replace the current drain or sit behind a flag. *Recommend replacing it unless a deployment relies on 503-on-reuse; a second drain mode is a parallel path that will rot. The tierstore owner decides, since a load-balancer deployment is theirs to know.*
- **Question 8: the keep-alive close race.** Closing an idle connection at the end of the settle can race with a request already on the wire; the client sees a close with no response. *Recommend closing at once and relying on the client retry policy of the test and of real HTTP clients for idempotent requests, and stating the residual in the service's documentation; the alternative (close an idle connection only with the next answer) leaves idle connections holding the drain.*
- **Question 9: first conversion of an existing host.** A host that listens on 9300 today. *Recommend treating the legacy listener as the first v-current: install the rules (they shadow the old listener for new connections), start v-next on 9301, and the first update is itself zero-downtime. Verify in the namespace test that established flows to the old listener are untouched.*
- **Question 10: the service-side registration client.** Reimplemented by each service, or an optional crate. *Recommend the protocol be one JSON line and documented, with a small optional crate later; tierstore's standalone repository must not take a kbf dependency.*
- **Question 11: canary and bake in the coordinator definition.** First-class fields, or keep the one-host-service workaround. *Recommend first-class (5.3): the workaround needs `min_up = 0` and a second service name per product.*
- **Question 12: absent services.** Order an install on a host where a declared service never registered, or leave it. *Recommend off by default (`install_when_absent = false`), on per service when a host is being provisioned.*
