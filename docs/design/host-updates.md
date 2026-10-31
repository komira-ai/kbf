# One host updater with per-service strategies, and a generic coordinator

Status: DRAFT, for decision. No code. Facts were read at `origin/main` 80436afe7; a `path:line` is a line of that file at that commit. Where a statement is an inference or a claim about software we do not own, it says so.

Labels: **[F]** read in the code at the commit above, **[I]** inference, **[D]** a design choice made here, **[V]** to verify before building on it.

Sections 4.1 (tierstore) and 4.2 (js-agent) are completed by their owners. Section 4.1 below holds the updater-side mechanics of the port forward in full; the tierstore owner adds a summary of the service-side contract and links the deep dive (`docs/design/tierstore-handover.md`).

## Changes in rev 2

Rev 2 answers an independent review of rev 1 (head 1863b7d77). What changed, by where:

1. **Strategy interface (3.3, 3.1).** Every step now carries a reversibility (`Invisible`, `Visible`, `Irreversible`) and a bound; a strategy declares whether old and new can run together (`overlap`). Commit is the **first irreversible step**, journalled before it starts. `abort` is defined before commit; after commit there is no abort, only `restore_previous`, which is a repair that cannot give back what the irreversible step lost. The tables for kbf-daemon (from code), tierstore and js-agent are in 3.3. This moves the commit point of tierstore to the stop of v-current and the commit point of js-agent to the stop of the agent.
2. **Root executor concurrency (3.4).** Today's helper serves one connection at a time (`crates/kbf-host-install/src/local/serve.rs:14-20`). The executor serves each connection on its own thread, takes long verbs as jobs (start, then poll), and runs the dead-man tick and the reconcile on a thread of their own. The 120 s restart constant becomes a per-service value. Red test: a long `unit stop` must not delay a `revert`.
3. **Forward recovery (3.5, 4.1.2).** `forward-apply` is a trial: record the intent first, then apply, then confirm; an unconfirmed record is reverted to the previous port by the executor at its deadline and at reconcile, never completed. A ruleset flushed by another loader is detected by reconcile and documented as an operator hazard.
4. **Promote (4.1.6).** No signal and no promote verb; this matches the tierstore owner's text. v-next takes the cache lock by itself when v-current's lock is released; mode is a function of the lock, which also covers a reboot. The first release with the lock is installed by a plain restart.
5. **Zero failed requests (4.1.3, 4.1.6, 9).** Idle keep-alive connections get a lingering close (`shutdown(SHUT_WR)`, keep reading, close after the peer's EOF). One client policy for every document: **no retries**; zero failures must hold without them. SYN retransmits are reasoned out from conntrack: the settle must cover the full retransmit schedule or the flows stuck in SYN_SENT must be removed. The deciding tests are deterministic (in-memory pipes, handshake-driven namespace scenarios); the hammering test is a stress test.
6. **CI (9).** Hosted-runner user namespaces are not relied on. The plan is a root network namespace through `cargo xtask sudo-test`; the first PR is the probe.
7. **Coordinator (5).** The kbf-specific code the review listed is in 5.4. Shared role slots are dropped: slots are per (service, role), and the only cross-service rule is the per-host serialisation that already exists. Membership is decided by the coordinator's host-set selector, so a host that lacks a service cannot read as converged.
8. **Corrections.** The `[Install]` claim is fixed (the kbf units have one; only executor-managed services do not). Question 9 no longer claims established flows survive the first rule load; it is a test, and the first conversion is a planned restart.
9. **Alternatives considered (4.1.7).** Socket activation (parked as a draft PR) and `SO_REUSEPORT` with `tcp_migrate_req` were weighed; the forward is the decision.
10. **Open items from the js-agent owner, answered in the generic sections (3.6).** Cordon durability, the cordon route the updater needs, kbf's cluster drain against js-agent hosts, and what a drain bound that passes with leases still running does (it fails `prepare`).
11. **Also added:** the trust of the services file (2.5), the update of the updater and executor themselves (3.7), observability (3.8), sizes and order against the update path (8).

No owner hunk is rewritten by this revision. Where 3.3 puts the commit of js-agent at the stop of the agent, it matches the terms agreed with the js-agent owner (commit is the stop; a drain bound that passes with live leases fails `prepare`; 4.2 points to the cordon route of 3.6); 4.2 itself is aligned by its owner after this revision.

## 0. The proposal in one page

Today every host runs one agent per host (`kbf-updater agent`, unprivileged) beside one root helper per product (`kbf-host-install --serve`), and a coordinator that already keeps a desired version per service. The mechanism is nearly generic; the knowledge of what to do to a particular product is not: the A/B in the agent is one fixed sequence (fetch, drain the node, switch, health, confirm or revert, undrain), and the helper installs a component whose name is the literal `kbf`.

This document proposes:

1. **One updater per host**, the existing agent, generalised. Services **register** with it when they start (name, version, health endpoint, strategy name, strategy parameters). A registration claims; it grants nothing. What a service may have done to it (its units, its release channel and signer, its ports) comes from a root-owned file, as the helper's channel allow-list does today.
2. **A strategy per service**, behind one interface. The updater does what is the same for every service (fetch, verify the sha256 and the attestation, stage, persist progress, check health, report, roll back) and calls the strategy for what differs: how a new version takes over from the running one. Three strategies are named: `node-drain-restart` (today's kbf-daemon behaviour), `port-forward-handover` (tierstore) and `lease-drain-restart` (js-agent).
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
| `capabilities` | what this instance can take part in (for example `handover-v1`: it holds a cache lock and reports its mode) | a strategy `requires` them of both the running and the new version, else its `fallback` is used (3.3) |
| `params` | the strategy's parameters (for `port-forward-handover`: public port, private ports, ready path, families, settle) | a closed schema per strategy; values bounded by the services file; unknown keys refused |

### 2.2 Socket or file drop: socket, with the capabilities in a root-owned file

**Option A, a unix socket.** The updater binds `/run/kbf-updater/register.sock`. The service connects on start, sends one JSON line, and holds the connection or reconnects. Authentication is `SO_PEERCRED` on the connection (uid, gid, pid as the kernel recorded them), which the repository already uses in exactly this way for a local signing socket (`crates/kbf-signer/src/server.rs:44-53`, "without `SO_PEERCRED` no peer can be told from another: none is admitted"). The uid must be the one the root-owned services file names for that service. Advantages **[I]**: the updater learns that a service is *running* (the connection), learns of a stop (the close), and can ask the service a question over the same connection; it needs no filesystem the service can write; stale registrations vanish with the connection. Costs: the service needs a small client, and the updater must be up when the service starts (the client retries; the updater's `Restart=always` makes the gap short).

**Option B, a file drop.** The service (or its `ExecStartPre=`) writes a file into a directory the updater watches. Advantages: no runtime dependency in either direction; survives an updater restart trivially. Costs: the directory must be writable by the service accounts, so any of them can write any name unless the directory is per-service (`/run/kbf-updater/registrations/<name>/`, owned by that account); a directory watch has its own limits and failure modes (inotify watch limits, missed events on a full queue); a file does not say the service is still alive; atomic replacement and torn files are each a failure to handle.

**Pick A.** The deciding reasons: peer credentials are a proof the file path cannot give, the connection is a liveness signal, and the repository has a tested pattern for the check. The capability side is still a root-owned file (B's good idea): `/etc/kbf/updater/services.d/<name>.toml`, owned by root and not writable by the registrant, naming the uid, the release channel and signer (repository and workflow, the way the helper's channel allow-list does), the unit names or template, the port ranges, the strategy and its parameter bounds. A service cannot register a name that file does not list, cannot name a unit, and cannot widen a bound. A package or an operator installs the file; "registers when it starts" means the *running* facts (version, health address, strategy parameters inside the bounds), not the grant.

### 2.3 A service that is not running, or never registered

- **Declared, never registered.** The services file lists it, no connection has come. The updater reports it to the coordinator as `absent` (a new health value; today a heartbeat has only `healthy` and `unhealthy`, `wire.rs:29-35`). The coordinator does not count an absent service toward `min_up` and does not order it, unless the service's definition says `install_when_absent` (so a new host can be given a service that has not run yet). This replaces the static `--service` flags: a flag becomes a synthetic registration read from the services file at start, so a host that never gets a registering client still behaves as today.
- **Registered, now not running.** The connection closed. The registration stays for the stop bound in the file (`registration_ttl`, a config value); the service is reported unhealthy with `stopped`. A strategy is told the service is stopped (`ServiceState::Stopped`); `port-forward-handover` and `node-drain-restart` can both update a stopped service (install, start), `lease-drain-restart` has nothing to drain.
- **Registered twice.** A second registration of the same name from the same uid replaces the first (a restarted process). A second from another uid is refused and logged.

### 2.4 Persistence across an updater restart

The accepted registration is written to the updater's state directory (mode 0700, as today `StateDirectory=kbf-updater`) by temporary file, fsync, rename, directory fsync, the pattern of `desired.rs`. On start the updater loads it, marks every service `unconfirmed` and reports it as its last known state until the service reconnects (the clients reconnect on their own). A registration that does not reconnect within `registration_ttl` becomes `stopped`. The file is never trusted for authority: the capabilities are re-read from the services file on every start.

### 2.5 The trust of the services file and of the registration socket

The services file is where grants live, and a grant is as strong as the root executor that reads it. So:

- **Who may place a file.** Files under `/etc/kbf/updater/services.d/` are root-owned, mode 0644 or tighter, in a root-owned directory, and are placed by a package or by an operator as root; the executor refuses a file that is not. It is treated like a sudoers file: a file can name a release channel, a signer, units and ports, so whoever can write one can run a release of their choosing as that service's user. Placing one is a root act and is audited. Grants (uid, channel and signer, units, ports) and tunables (bounds, timeouts) are in **two files** per service (`<name>.grant` and `<name>.toml`) so a tunable change in review is never mistaken for a grant change.
- **Load-time checks, across files.** Two names map to one uid: refused. Two files claim the same port, unit, slot or private port range: refused. A unit name outside the grammar of 3.4: refused. A public or private port at or below 1024, or in the deny list: refused. A user name is resolved to a uid at start and re-resolved on reload; a name whose uid changed is refused (a reused uid after an account is deleted must not become a registrant).
- **One uid per registrable service.** Services that share a uid cannot be told apart by `SO_PEERCRED`; `kbf-daemon` and others run as `kbf` today (`packaging/linux/units/kbf-daemon.service:27`). The kbf strategy therefore keeps its synthetic registration from the file, and a service that registers must have a uid of its own.
- **Who can connect.** `SO_PEERCRED` is only a control if the socket is reachable by the service accounts and not by every local user. The registration socket sits in a directory owned by root with group `kbf-register`, mode 0750, and the socket is 0660 in that group; a service account joins the group in its unit (`SupplementaryGroups=`). The signer refuses a world-accessible directory for the same reason (`crates/kbf-signer/src/server.rs:59-82`, `WorldAccessible`); the updater's socket code does the same. The uid check is then a second control, not the only one.
- **What a registration can make the updater do.** The updater, unprivileged, issues a `GET` to the registered health address. The address must be loopback and its port within the range the grant allows, so the surface is a request to a loopback port in a small range, which is accepted.
- **N signers on every host.** Each service adds one repository and workflow to the list of accepted signers (`crates/kbf-installer/src/attest.rs:136-137`). A compromise of a service's release pipeline gets code onto every host that lists it, as that service's user. That is the price of the generic model and is stated; the grant file lists the signer so a reviewer sees each one added.

## 3. The generic updater

### 3.1 The pipeline

For one update order of one service, in this order, each step only if the previous one succeeded, and each step recorded in the journal (3.5) before it begins:

1. **Precondition.** The host is healthy enough to update (today `health.refusal`, `agent/update.rs`), the service is declared, no other update runs on the host (the coordinator also enforces it, `decide.rs:105`; the updater enforces it again because it is the one that would be hurt).
2. **Fetch and verify** (executor job `stage`). Download the release the version names from the channel the services file names; check the sha256 against the signed sums and the attestation (repository, signer workflow, issuer, hosted runners, commit, branch) on the host, with the pinned `gh` and trusted root; lay it out beside the running release. Nothing of the running service changes. The updater never takes a URL or a path from the coordinator's reply: the version is the only input.
3. **Plan** (strategy). The strategy reads the staged release, the registration and what it needs to read of the world, and returns an ordered list of steps, each with a reversibility and a bound, or refuses. `plan` may read (the lease server's host list, `/proc`, the executor's `query`); it changes nothing.
4. **Run the steps** in order. The updater journals "begin" and "done" for each. Before the first step whose reversibility is `Irreversible` it journals and fsyncs **"commit intent"**; that line is the commit.
5. **Health gate** (updater, generic). The strategy names the observations; the updater repeats them until they pass or the strategy's bound passes. Pass means: the registered health answers (where the service has one), the reported version equals the staged version, and the strategy's extra checks pass. A strategy that can run old and new together places the gate **before** its first irreversible step; one that cannot has the gate after it and must offer `restore_previous` (3.3).
6. **Finish and report.** Undo what the update placed (cordons, retired slots), and report in the next heartbeat (`UpdateResult`, `wire.rs:48`).

Two rules hold for every strategy:

- **No irreversible step over running work.** A drain with a bound is a `prepare` step: if the bound passes while work is still running, the step **fails**, the update reports `Failed` with the reason `drain_bound`, and the strategy lifts the cordon it placed. It never proceeds to an irreversible step (a stop, a kill) on the strength of a timeout. This is today's kbf behaviour (`docs/design/updates.md`, section 4, step 2: "it never installs over running leases"; a deadline that passes stops the update with the node left drained) and it is the rule for every service. For a service where a stop can taint a host (3.6) it is also the safe one: the updater cannot see inside the host's state directory, so it must not guess.
- **Every wait is a bound from the services file**, never a literal in a strategy.

### 3.2 What is generic and what is the strategy

Generic, owned by the updater and the executor: the order of the pipeline and the journal; fetch, sha256, attestation, staging; one update per host; the health gate loop; reporting; the dead-man deadlines (3.4); the audit log; the rule that commit is journalled before it happens.

Strategy, owned by the per-service code: the plan (steps, reversibility, bounds), running and undoing each step, the extra checks, `restore_previous`, `recover`.

### 3.3 The strategy interface

```rust
pub enum Reversibility {
    /// No client or other process can tell. Undo is discarding.
    Invisible,
    /// Clients can tell, and the step can be undone, restoring the earlier
    /// behaviour. The undo has its own bound.
    Visible,
    /// Cannot be undone by the updater. Work or state is lost, or a process is
    /// told to end. The first of these is the commit.
    Irreversible,
}

pub struct Step { id: StepId, reversibility: Reversibility, bound: Bound }

pub struct Capabilities {
    /// Can the old and the new version run together on this host?
    overlap: Overlap,            // Yes | No
    /// Capabilities the running and the new version must both advertise at
    /// registration (for example "handover-v1"), else `fallback` is used.
    requires: Vec<Capability>,
    fallback: Option<StrategyId>,
}

pub trait Strategy {
    fn capabilities(&self) -> Capabilities;
    /// May read; changes nothing. Refuses with a reason, or returns the steps.
    fn plan(&self, cx: &Cx, from: &Release, to: &Release) -> Result<Plan, Refusal>;
    fn run(&self, cx: &mut Cx, step: &Step) -> Result<(), StepError>;
    /// Only for Invisible and Visible steps. Idempotent.
    fn undo(&self, cx: &mut Cx, step: &Step) -> Result<(), StepError>;
    /// Extra observations for the health gate (the generic ones always run).
    fn checks(&self, cx: &Cx, plan: &Plan) -> Vec<Check>;
    /// After commit only. A repair: puts the previous release back to serve.
    /// It does not restore what the irreversible step lost.
    fn restore_previous(&self, cx: &mut Cx, reached: &Step) -> Result<Restored, StepError>;
    /// After a crash: the journal, and what the executor says is true.
    fn recover(&self, cx: &mut Cx, journal: &Journal, observed: &Observed) -> Resume;
}
```

**What abort means.** *Before commit*, `abort` is the undo of every `Visible` and `Invisible` step that ran, in reverse order, each idempotent; at the end the host behaves as it did before the order and the update reports `Failed`. *After commit* there is no abort: the updater never claims the previous behaviour is back. A failed health gate after commit calls `restore_previous`, which is a repair; whatever the irreversible step lost stays lost, and the update still reports `Failed`. A later fix is a new update (a rollback is an update to an older version through the same strategy).

**What the updater guarantees a strategy:** it calls `run` for a step only after the previous step's "done" is journalled; it journals and fsyncs "commit intent" before the first irreversible step runs; it calls `undo` with the journal's step, so a strategy that crashed in a step sees that step, not the one before; it never runs two strategies at once on a host; every bound the strategy reads is a services-file value; it gives a strategy no shell, no path outside the service's staged directory and no unit the services file does not list. **What a strategy guarantees the updater:** every step is idempotent; a step reports failure rather than hanging (it has a bound); a step declared `Invisible` really is, and the declaration is tested (a test drives each `Invisible` step and asserts nothing observable on the public path changed).

**The three strategies against the interface.** Sources: kbf-daemon is read from the code (`agent/update.rs`, `local/core.rs`); tierstore is this design (4.1); js-agent is its owner's text (4.2), re-derived here.

| step | kbf-daemon (code) | tierstore (design) | js-agent (4.2, re-derived) |
|---|---|---|---|
| overlap | No: one daemon per host, the units restart (`update.rs:162`) | Yes: two slots, two private ports | No: one agent per host id and scratch root (4.2) |
| stage | `install`: fetch and verify, no link moves (`update.rs:106`). Invisible | `stage`. Invisible | `stage`, then `<new> --version` as the service user. Invisible |
| prepare | drain the node: cordon, wait for no lease (`update.rs:253`). Visible; undone by `back_on_a` (`update.rs:211`) | start v-next on the idle private port, read-only; wait for its `/readyz`. Invisible: the guard drops outside callers, the rule does not point at it | cordon on the lease server, then wait for no lease. Visible; undone by uncordon. A bound that passes fails this step (3.1) |
| switch | the executor's `trial`: pending marker with deadline first, links move, units restart. Visible; undone by the helper's `revert`, which it does itself at the deadline (`core.rs:173`) | `forward-apply` as a trial (3.5, 4.1.2). Visible; undone by `forward-apply <old>` or by the executor at the forward's deadline | none before the stop |
| gate | node health through the frontend or a ready URL, then `confirm` (`update.rs:162`) | the instance is observed through the **public** port as well as its own, before anything irreversible | cannot run before the stop (no overlap) |
| **commit: the first irreversible step** | `confirm`: from here the helper no longer owns the revert; the old release stays as `previous` but a later problem is a new update | the **stop of v-current** (`unit stop` of its slot: SIGTERM, which cannot be taken back) | the **stop of the agent**: leases still running at the drain bound are stopped or killed and rerun from the lease server's budget |
| abort before commit | undo drain and trial in reverse | `forward-apply <old>`, stop v-next, uncordon n/a | uncordon |
| after commit | the dead-man revert still exists until `confirm` returns; after it only a new update | `restore_previous`: start the old release in the vacated slot and `forward-apply` it, a cold start (bounded by the service's start time); in practice the coordinator's rollback | `restore_previous`: the executor's trial revert puts the old release back; runs stopped at the commit are not brought back |
| finish | undrain (only if the updater's drain placed the cordon) | wait for v-current to exit (its drain bound); v-next promotes itself on the cache lock (4.1.6); retire the old slot | uncordon (only if this update placed it) |

Two corrections to rev 1 follow from the table. For tierstore the commit is the stop of v-current, not the end of the settle: a SIGTERM cannot be rescinded, and from that moment v-current will stop accepting and exit, so a swap back would point new connections at a dying server. For js-agent the commit is the stop of the agent, not the confirm: the lost runs are lost before any health is known. 4.2's own table puts the commit at `confirm`; its owner is asked to re-read its commit and abort rows against this table. Everything else in 4.2 stands.

### 3.4 Privileges and the executor

**Keep the split.** The updater holds the farm client certificate, talks to the network and parses a coordinator's reply; it stays `User=kbf-updater` with an empty capability set as today (`kbf-updater-agent.service:32-43`, hardening through `:52`). Making the whole updater root would put a network parser and a certificate in root. The root half is the **executor**, the helper (`kbf-host-install --serve`) made service-generic and made concurrent. The updater reaches it over a unix socket of mode 0600 owned by one account, as today (`local/serve.rs:1-30`).

**Verbs.** A closed set; none takes a command line, a path or nft text.

| verb | kind | what it does as root |
|---|---|---|
| `query` | short | what is staged, running, on trial, the forward intent, the jobs and their states |
| `slot-set <service> <slot> <release>` | short | point a slot's link at a staged release |
| `forward-show <service>` | short | the recorded forward and what the kernel has |
| `forward-apply <service> <port>` | short | the forward trial: record intent, apply, return (3.5) |
| `forward-confirm <service>` | short | make the applied forward permanent |
| `revert`, `confirm` | short | today's, with the pending marker and the dead-man deadline |
| `stage <service> <version>` | **job** | fetch, verify, lay out |
| `unit <service> start\|stop\|restart <slot>` | **job** | `systemctl` on a unit the services file lists, for a slot that exists. `stop` is the SIGTERM of tierstore's commit: no separate signal verb is needed or offered |
| `trial <service> <release>` | **job** | links move and units restart; today's `switch` |

A **short** verb is answered in the connection. A **job** verb returns `{"job": id}` at once; the updater polls `query` (or `job <id>`) until it is `done` or `failed`. Today's blocking `install` and `switch` stay for the kbf helper's current clients, so the wire is compatible.

**Concurrency.** Rev 1 inherited the helper's single connection (`serve.rs:14-20`: "one connection at a time ... no other request is answered meanwhile"; the revert deadline is checked "between connections"). That cannot carry a 960 s `systemctl stop` of tierstore (`TimeoutStopSec=960`, `packaging/tierstore/tierstore.service`) or a 3900 s stop of the js-agent (4.2). So the executor is built as:

- one thread accepts and answers short verbs and starts jobs; each connection is read on its own thread;
- **long jobs run on worker threads, at most one per service**, holding only that service's lock; `query`, `revert`, `forward-*`, and every other service's verbs never take it;
- **the tick runs on a thread of its own**: the dead-man deadlines (the trial's and the forward's) and the reconcile (3.5) are never behind a job or a connection;
- `revert` and `forward-apply <old>` for a service whose job is in flight are accepted at once and answered with a job id; they run after that service's job ends (they cannot start a unit that is still stopping), and nothing else waits for them.

The restart timeout and active wait (`crates/kbf-host-install/src/plan.rs:88`, `:94`, 120 s each) and the confirm window (600 s default, accepted from 120 to 1800, `serve.rs:58`, `:173`) become per-service values in the services file. kbf keeps today's numbers and cap byte for byte; a service file may lift the cap under a load-time invariant: window at least restart timeout plus active wait plus the health bound plus a margin. The red test for all of it: **a long `unit stop` (a fake that does not finish until released) for service A must not delay a `revert`, a `query` or a reconcile for service B, and a `revert` for A itself is accepted at once.**

**What the executor will not do.** `nft` text is generated from a fixed template and integers: the service's public port and its two private ports, each bounded at load (above 1024, disjoint from every other service's, none of them in a deny list that has 22, the updater's, the coordinator's and the registration socket's ports), and `forward-apply` takes one of the two private ports from the file, never a free integer. Units are named by a grammar fixed in code. Today the check is the prefix `kbf-` (`crates/kbf-host-install/src/request.rs:154-175`), and generalising it must not remove it: the services file may name a unit only as `<prefix>-<name>.service` or `<prefix>-<name>@<slot>.service` for the prefix the file's own name implies, and never an existing system unit). The paths the executor reads as root stay constants (`serve.rs:24-26`); only the files under `/etc/kbf/updater/services.d/` are data, and 2.5 says who may place them.

### 3.5 Persistence and recovery

Two journals, one authority, one ordering rule.

- The **executor's record** (root, `/var/lib/kbf-updater/executor/`, rename-and-fsync) is **authoritative for what is true on the host**: which release is staged and in which slot, the pending trial, the forward intent. The kernel's nftables state is *derived* from it.
- The **updater's journal** (unprivileged state directory) is authoritative only for what the updater was doing for which order: service, target, step begun, "commit intent".
- **Ordering for every host-changing verb that has a dead-man: record the intent, then apply, then confirm.** The record holds the target, the previous state and a deadline. The executor reverts to the previous state at the deadline unless the confirm arrived.

**`forward-apply` as a trial.** (1) Write and fsync `{service, target_port, previous_port, deadline, state: pending}`. (2) `nft -f` the file for the target port. (3) Return. After the gate passes, the updater sends `forward-confirm`, which rewrites the record `{active: target_port, state: confirmed}`. Crash and reconcile cases:

| where it stopped | record | kernel | what the executor does (at start, at each tick, at the deadline) |
|---|---|---|---|
| after (1), before (2) | pending | previous | marks the record `reverted`; nothing to apply. Never completes a forward nobody confirmed |
| after (2), before confirm | pending | target | when the deadline passes, checks that the previous port still accepts a connection and applies the previous port's file, marks `reverted`. If the previous port is dead too, leaves the kernel as it is and raises the alert `forward_unrecoverable` |
| after confirm | confirmed | target | nothing; the record is the truth |
| kernel differs from a confirmed record (flushed or edited) | confirmed | other | re-applies the file for the confirmed port and the guard table, counts it in `forward_repaired_total`, logs it |

The updater, on seeing `reverted`, either starts the forward again from the beginning or aborts; on `confirmed` it goes on. The record never moves toward the *new* state without a confirm. The confirm is sent **before** the commit (the stop of v-current), because after commit the previous slot is gone and there is nothing to revert to.

**Another ruleset load.** `nftables.service` with `flush ruleset` in `/etc/nftables.conf` (a common default), or a firewall front end that reloads, removes both tables. Then nothing redirects 9300 (an outage for that service on the host) and the guard is gone (the private ports are open on every interface) until the reconcile corrects it. This is an **operator hazard and is documented as one**: the packaging notes say not to use `flush ruleset`, to flush only the operator's own tables, and to order any loader before the executor (`After=nftables.service` on the executor unit). The executor detects the change at its tick (the interval is `reconcile_secs` in the services file, the stated exposure bound) and repairs it; the repair count is an alert. The nft `flags owner,persist` option would let the kernel keep our tables when the loader exits; it is kernel-dependent and **not relied on [V]**.

**Crash cases by process.**

- *Updater dies:* on start it reads its journal and calls `query`. If a step began and did not end, `recover` gets both and decides to go forward or abort, never both; the executor's record wins any disagreement. If it died before "commit intent", abort is available. If it died after, the strategy resumes forward.
- *Executor dies, or the host reboots:* the executor starts before the services it manages (`Before=` on its unit), loads its record, **reconciles the kernel to it** (a pending forward is reverted as in the table; a confirmed one is re-applied; the guard is re-installed), starts the confirmed active slot of the services whose strategy owns their start (the port-forward ones, which have no `[Install]`; the kbf units keep their `[Install]` and their ordering unchanged, `packaging/linux/units/kbf-daemon.service:72-73`), then ticks. A trial whose deadline passed while the host was down is reverted at once (`core.rs:173`).
- *A service crashes:* systemd restarts it per its unit; the updater reports it unhealthy until it registers again. A crash of v-next before commit is `abort`; after commit it is the coordinator's rollback.

### 3.6 Cordons, drains and what they promise (answers to the js-agent owner)

A **cordon** keeps new work off a host so a drain can finish. The rules, for every strategy that cordons:

- **(b) Durability.** kbf's cordon is the frontend's, durable on the frontend side, and the agent additionally keeps a mark of the cordons it placed on disk (`--cordon-dir`, `agent/cordon.rs`; `agent/update.rs:12-13`). The lease server's cordon rows persist in its store (`js-store/src/cordon.rs`, per 4.2; **[V]** in code). So a cordon survives an agent restart and a host restart. It also **has no expiry**. So the updater journals every cordon it places with the order, the owner (this update) and an updater-side deadline (`cordon_max_secs`, a services-file value); `recover` lifts a cordon the journal says is ours; a cordon with no live journal entry and no operator is the service owner's to expire (a request to the lease server's owners, Question 14).
- **(c) The route the updater needs.** An unprivileged updater must not hold an operator token (`docs/design/updates.md`, section 5, "Own-node calls"). kbf's answer is the one to copy: the frontend's worker listener admits, for a certificate with the role `updater`, `POST /v1/nodes/{id}:drain`, `:uncordon` and `GET /v1/nodes/{id}` for the certificate's own node id and nothing else. For a js-agent host the lease server needs the same three for the host id in the certificate: `POST /v1/hosts/{id}:cordon`, `POST /v1/hosts/{id}:uncordon` and `GET /v1/hosts/{id}` (the host's leases and its phase). Without that route js-agent has no cordon and a busy host never reaches zero leases; it is a **prerequisite of the js-agent strategy**, not optional, and it is listed in the migration (8). Until it exists the strategy cannot be run.
- **(d) The cluster drain.** `kbf cluster drain <node>` is the kbf frontend's node drain (`kbf-cli/src/cluster.rs`) and reaches kbf nodes only; whether a js-agent host is a kbf node depends on whether it also runs the kbf daemon. A cordon is therefore **per service, through that service's own adapter** (the kbf adapter calls the frontend, the js-agent adapter calls the lease server). A host that runs both has two independent cordons; the per-host serialisation (`decide.rs:105`) keeps their updates from overlapping; neither update depends on the other's cordon.
- **(e) The drain bound.** A bound that passes with work still running **fails `prepare`** (3.1). Reasons, for js-agent: a stop inside a job's owed-checks window taints the next start, and nothing clears a taint (4.2); the updater cannot read the host's state directory, so it cannot know the window is closed. Failing is a refused update, not a killed run. With the cordon in place no new leases arrive, so the wait terminates in the normal case and the order is retried by the coordinator's retry rule (`--retry-secs`). An update that spends rerun budget is a strategy's declared cost (`consumes_rerun_budget`), and the plan refuses a host whose running jobs have none left.

### 3.7 Updating the updater and the executor

The updater and the executor are part of the `kbf` release and are updated by the **kbf-daemon strategy exactly as today** (their units are `kbf-updater-agent.service`, with an `[Install]`, and `kbf-host-local.service`). They are never updated through the strategy of another service, and a strategy is never needed to repair a strategy: a broken strategy ships as a new updater, and the way back is the kbf path, which stays permanently simple and is the fallback for every host (today's package install, `docs/deploy/kbf-updater.md`). How the helper restarts itself while serving the request that restarts it is **not read here [V]**; PR 3 (8) must state it before the executor becomes concurrent.

### 3.8 What the updater reports

The heartbeat today carries four facts per service (`wire.rs:57-100`). The generic updater adds, per service: the strategy step it is in and the journal phase, `commit` reached or not, the forward record state and the count of repairs, the number of reconcile corrections, taint or blocked reasons, and the duration of each step. They are shown in `/v1/status` of the coordinator beside the desired state. Alerts named by this design: `no_instance_up` (a service with a port-forward strategy and no ready instance), `forward_unrecoverable`, `forward_repaired_total` increasing, `drain_bound` failures, a cordon older than `cordon_max_secs`, an instance read-only for longer than the lock-poll bound with no lock holder alive.

## 4. Strategies

### 4.1 tierstore: port-forward hand-over

Written by the tierstore owner for the service side; the mechanics of the forward are here because they are updater-side.

**Service side.** The full contract, with code references, failure cases and tests, is in
[tierstore-handover.md](tierstore-handover.md).

Registers: name `tierstore`, strategy id `port-forward-handover`, health `GET /readyz` on the instance's
private port (exists), version from a new `tierstore_build_info` gauge (today only the start line and
`--version` carry it, and that is the crate version, not a commit).

What v-next offers, and what is new in tierstore:
- **Listeners** (new). The redirect rewrites a remote packet to the incoming interface's address, so the private
  ports listen on the wildcard of both families, not on loopback; the guard keeps direct connections out. Today
  `--listen` is one `SocketAddr` (`server/cli.rs`), so a repeatable `--listen` with an explicit `IPV6_V6ONLY` is new.
- **The cache lock decides the mode** (new): `<cache>/.lock`, flock, exclusive, non-blocking. An instance that finds
  it held serves read-only and retries on an interval, without blocking a thread. It must not clear `.stage/`, delete
  sidecars, chmod, drop entries on a read error, or stamp file times. The kernel frees the lock when the holder dies,
  so a hung v-current killed at the unit's stop timeout, a crashed one, and a reboot all resolve; a lone instance is
  read-write at once. On the lock it cleans `.stage/`, rescans, enables fills and eviction (a storage node: the
  reconciler, under a `--lock-file`).
- **No promote verb.** v-next takes over by itself, so the updater observes it: `/readyz` still answers 200 and its
  body and a gauge say `read-only` or `read-write`. A lock never protects against a v-current that predates it, so the
  first release with the lock goes in by a plain restart (plan checks v-current reports a mode).
- **The drain** (changed): `Connection: close` on every answer from drain start. v-current keeps accepting through
  the settle wait (`--drain-lb-wait`, never 0 here). At its end an idle connection gets a lingering close
  (`shutdown(SHUT_WR)`, read to end of stream, then close), and a request on the wire is served, not answered 503.
  The race that remains is a request within one round trip of the FIN, counted in a metric.

**Commit point and client policy.** The commit is the SIGTERM to v-current: it cannot be rescinded. Before it, abort
is swap back, then stop v-next. After it there is no swap back; the coordinator's rollback is a new hand-over. Clients
do not retry: a retried create-only put is ambiguous (412), so the hand-over test allows no retry.

Storage node: each slot has its own proxy id (`tierstore-a`, `tierstore-b`), so two processes never write one
journal; a `dir:` hot tier cannot have two lease holders and keeps the plain restart; a restarted holder can reproduce
an older lease body and tag (kbf#2108), so a per-process nonce is needed.
Metrics: the directory gauges (`cache_bytes`, `cache_objects`, `cache_fs_free_bytes`) double-count while both run, and
the fill and evict families stay 0 on v-next; the build and mode gauges are new. Needed from the mechanics below:
established flows stay on v-current.

**Shape [D].** Clients always call the public port, here 9300. No process listens on it. Each tierstore instance listens on its own private port, 9301 or 9302, alternating. One nftables redirect per family sends public-port connections to the current instance. An update starts v-next on the idle private port in read-only mode, waits for its `/readyz`, swaps the rule as a trial, observes v-next through the public port, **confirms** the forward, and only then stops v-current (the commit). v-current drains on the flows conntrack still maps to it; v-next takes the cache by itself when v-current's lock is released (4.1.6).

Mapped onto the pipeline of 3.1 and the table of 3.3:

| step | reversibility | port-forward |
|---|---|---|
| plan | | refuse if the guard table is missing or stale; refuse if either instance lacks the capability `handover-v1` (else use the fallback `restart`: a plain restart with a gap, which is how the first release with the lock is installed); refuse if the conntrack established timeout is not longer than the service's idle bound (4.1.3) or the conntrack table is above its threshold (4.1.3); refuse if an interface that can receive the public port has no address of a family in `families` (4.1.1); choose the idle slot (the port not in the confirmed forward) |
| start v-next | Invisible | `slot-set` the idle slot to the staged release, `unit start`; wait for the instance's `/readyz` 200 on its private port (its start-up self-test passes before it binds) |
| forward | Visible | `forward-apply <idle port>`, a trial with a deadline (3.5) |
| gate | | probe through the **public** port from the node (answered by v-next) and the private one; the version reported equals the staged version; v-next's mode is read-only and it is ready |
| `forward-confirm` | Visible | the forward becomes permanent; sent before the commit |
| **commit** | **Irreversible** | `unit stop` of v-current's slot: SIGTERM. Journalled first |
| settle | | v-current keeps accepting for `settle` (4.1.3), then closes idle connections with a lingering close and finishes what is in flight (its drain bound) |
| finish | | wait for v-current's exit; v-next takes the lock and becomes read-write on its own; the updater waits for `cache_mode=read_write` on v-next's metrics (4.1.6) and records the new active slot; the old slot is retired |

Between the forward and the commit the updater observes v-next through the public path for the strategy's health bound, **before** it stops v-current. That keeps `abort` alive while the evidence about v-next is gathered. The cost is a longer window in which both instances run; the cache is not yet v-next's during it (read-only), so the window has no extra write hazard.

#### 4.1.1 The ruleset and the listeners

One `inet` table per service for the redirect and one for the guard. The `inet` family handles IPv4 and IPv6 in one rule; NAT in an `inet` chain needs Linux 5.2 or later (nftables wiki, "Performing Network Address Translation (NAT)": stateful NAT in the inet family "since Linux kernel 5.2") **[V on the oldest supported kernel]**. For an older kernel the same rules are generated twice, once for `ip` and once for `ip6`. The families generated are those in the registration's `families` (a service that is IPv4 only gets no IPv6 rule).

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

The nft syntax above is the intended text; the first rule-generation test loads it with `nft -c -f` in a root network namespace to pin it **[V]**. Points about it:

- **`fib daddr type local` on both rules.** Without it, `tcp dport 9300` in the output chain also redirects the host's own outgoing connections to port 9300 of *other* hosts, and in prerouting it would redirect forwarded traffic if the host routes. The match restricts the rule to addresses that are the host's own. It is valid in both hooks without an interface qualifier (kernel `nft_fib` validation; reviewed **[V]**).
- **Redirect, not DNAT; and what the listener must therefore be.** `redirect` rewrites the destination to the local machine. For packets arriving from outside the destination becomes the **first address of the incoming interface** (IPv4: the first address of the interface; IPv6: the first usable address of matching scope); for locally generated packets it becomes 127.0.0.1 or ::1 (iptables-extensions, REDIRECT; the kernel's `nf_nat_redirect` follows it, reviewed **[V]**). If the incoming interface has no usable address the packet is dropped. So the private listener must accept connections on **both** the node's addresses and loopback: it must listen on the **wildcard** of each family. A loopback-only listener behind this rule refuses every remote client; this document and the tierstore deep dive must not say otherwise.
  What tierstore's listener must be: **two sockets per private port, `0.0.0.0:P` and `[::]:P` with `IPV6_V6ONLY` set explicitly** (so the outcome does not depend on `net.ipv6.bindv6only`), or, for an IPv4-only service, the first alone. `--listen` is one `SocketAddr` today (`crates/tierstore/src/server/cli.rs`, the `listen` field), so this is new tierstore work: a repeatable `--listen`, one value per bind, with an explicit `IPV6_V6ONLY`, which is what the tierstore owner's section already specifies.
  The alternative, `dnat to 127.0.0.1:P` with a loopback-only listener, does not work for packets from outside unless `net.ipv4.conf.<interface>.route_localnet=1` is set (kernel documentation: "Do not consider loopback addresses as martian source or destination while routing. This enables the use of 127/8 for local routing purposes", default false). That setting makes the host accept packets to 127/8 from the wire for every service on the interface, a known attack surface. **Chosen: redirect plus wildcard listeners plus the guard (Question 6).** The price is that the private port is open on every interface unless the guard is loaded; the guard is therefore part of `plan` and of the reconcile.
- **A trap in the guard.** After the redirect the packet reaches the *input* hook with destination port 9301, not 9300. A host firewall that allows 9300 from some networks stops matching: remote callers are dropped by the host's own policy even though the redirect worked. The guard's second rule accepts the redirected flows in the guard table, but an accept ends only its own base chain; another table's drop or default-drop policy is **not** overridden. So the operator's firewall must match `ct original proto-dst 9300` instead of `dport 9300`. This is stated in the service's packaging notes and checked by a test that connects from a veth peer with a restrictive second table loaded (9).
- **The third rule drops** anything else to the private ports, so a remote caller that addresses 9301 directly reaches nothing. Loopback sources are accepted (the first rule), so the updater can probe v-next directly. Any local process can reach the private port directly too; that gives it nothing it lacks on the public port, because the service authenticates every request itself. The guard does not distinguish interfaces beyond `lo`; there is no UDP service on these ports.

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

The same file installs the rules the first time (`add table` is a no-op when it exists), applies a swap and applies a rollback; only the port differs. The wiki ("Atomic rule replacement") says `nft -f` reads the whole file, builds the new objects beside the existing configuration and swaps "in one single transaction", so "there is no moment when the firewall is partially configured"; it documents `flush table` at the top of a file for exactly this use. A packet is evaluated against the old ruleset or the new, never a mix and never an empty chain. The guard table is separate and is not in the file, so a swap never flushes it. `flush table` does not clear sets; the redirect table has none (the guard's port set is inline).

What clients see during a swap: nothing. The redirect runs only for the first packet of a connection (wiki: "No rule lookup happens for follow up packets in the flow"); a SYN just before the swap is mapped to v-current, one just after to v-next, and neither is dropped or refused by the swap. The swap is a **trial** with a deadline and a confirm (3.5): the executor records the intent before it applies, reverts an unconfirmed forward itself, and never completes one nobody confirmed. Not claimed: that the swap is free of every race; 4.1.3 names the ones that matter.

#### 4.1.3 Conntrack

A flow's NAT mapping is stored in its conntrack entry when the first packet creates it, and every later packet of the flow uses that entry, whatever the rules now say. So connections established before the swap keep going to v-current, which is what the hand-over needs; connections made after go to v-next.

- **The handshake race and the settle.** A connection whose SYN was mapped to v-current just before the swap may still be completing its handshake, or sit in v-current's accept queue, when the swap happens. v-current must **keep accepting for a settle period after the swap** and only then stop; closing the listener resets what is queued. The existing `--drain-lb-wait` does this (`server/mod.rs:596-615`); its meaning changes from "time for a load balancer to notice" to "time for connections already mapped to me to arrive". It is a config value and never zero here.
- **SYN retransmits beyond the settle, reasoned from conntrack.** A client whose SYN is unanswered retransmits at about 1, 3, 7, 15, 31 and 63 s and gives up at about 127 s (Linux default `tcp_syn_retries=6`). What the swap does to a retransmit depends on whether the first SYN created a conntrack entry. (1) If the first SYN reached the host before the swap, the entry exists with a NAT binding to v-current's port; its retransmits keep that mapping (the rule is not consulted again), whatever the swap did. Such a flow exists only if v-current did not answer (the SYN queue or backlog was full, or the SYN-ACK was lost on the way back). The entry lives in `SYN_SENT` for `nf_conntrack_tcp_timeout_syn_sent` (120 s by default, **[V]**). If v-current has by then closed its listener, the retransmit is refused. (2) If the first SYN never reached the host, no entry exists; the retransmit is a new flow and the swap sends it to v-next: harmless. So a SYN aimed at v-current after the swap is harmless only in case (2). The 10 s default of rev 1 does **not** cover case (1) past 7 s. The two ways to cover it: a settle at least as long as the client's whole schedule (**130 s**, bounded by config, costing only update time because v-current serves everything throughout), or the executor removing the `SYN_SENT`/`SYN_RECV` entries mapped to the old port at the end of the settle so retransmits become new flows (a conntrack netlink operation in a root executor, more machinery). **Recommended: the 130 s settle** (Question 13). A client with a tuned `tcp_syn_retries` outside the default is outside the guarantee, and the documentation says so.
- **Idle flows and conntrack timeouts.** The established-TCP timeout is `nf_conntrack_tcp_timeout_established`, default 432000 s (5 days). A keep-alive connection idle longer than its entry's timeout loses its entry. When the client then sends on it, the packet is not a SYN, but with `nf_conntrack_tcp_loose` enabled (the default) conntrack picks it up as a new entry and the NAT hook runs for it: it is mapped by the **current** rule, which after a swap is v-next, and v-next answers with a reset. Before the swap the same pickup would have been mapped to the same port as the socket and worked, so the hand-over adds one failure mode, for one kind of flow. Two bounds close it: the service closes idle keep-alive connections itself after its idle bound (`header_timeout`, 60 s default, `server/mod.rs:346`, `:409`), far below five days; and the plan refuses the update if `nf_conntrack_tcp_timeout_established` read from `/proc` is not longer than that bound plus a margin. Both are configuration checks, not timing.
- **Conntrack pressure.** An entry exists per flow, with its NAT binding. A full table (`nf_conntrack_max`) drops new flows, and early drop evicts unreplied, non-assured entries first: a handshake in `SYN_RECV` can lose its entry, so its final ACK is picked up loosely and mapped by the current rule, to v-next, which has no such connection, while v-current holds a half-open socket. Established, assured flows are evicted last, so draining flows are mostly safe. The plan reads `nf_conntrack_count` against `nf_conntrack_max` and refuses above a threshold from the services file. This design adds the failure mode for handshakes in flight at swap time, so "with or without this design" was wrong in rev 1.
- **Other failures to name.** `conntrack -F` drops all mappings: later packets on established flows are picked up as new entries and mapped by the current rule, so a flush during the drain resets the flows still on v-current. The executor does not run `conntrack`.

#### 4.1.4 On-node callers

A process on the host connecting to 9300 goes through the **output** chain (its destination is local, so `fib daddr type local` matches), mapped to 127.0.0.1:P or ::1; a container on a bridge arrives at **prerouting** like a remote caller. A process that connects to the node's own LAN address uses the output chain as well. Loopback must be up in the namespace the rules live in (a test namespace forgets).

#### 4.1.5 Readiness and metrics on private ports

The probes (`/healthz`, `/readyz`) and `/metricz` are served by the same listener as the data path, so they exist on each private port. The updater probes v-next directly on `127.0.0.1:<idle port>` (accepted by the guard's first rule) and probes the public port from the host to see who answers it. A remote scraper or a load balancer that addresses 9300 sees whichever instance is current and never the other. A scraper that wants both instances scrapes the node's two private ports locally; the instance label is the service's proxy id, which differs between the instances (the unit comments already require "one of its own" per process over the same tiers, `packaging/tierstore/tierstore.service`). Counters reset per process, as they do at any restart.

#### 4.1.6 What the strategy needs from the service

The service-side contract is in the deep dive; this is what the updater side relies on, with the points that rev 2 changed.

- **Promotion is self-promotion on the cache lock.** There is no promote signal and no promote verb. Every instance tries the exclusive, non-blocking lock on the cache directory at start and on a polled interval; the one that holds it is read-write, the others are read-only and promote themselves when the lock frees, which is when v-current exits. Reasons: the unprivileged updater cannot signal a process of another account, the executor's verb list would need a signal verb, and a release without a handler for the signal would be killed by it (tierstore handles only SIGTERM and SIGINT, `server/cli.rs:924-926`). It also covers a **reboot**: the one instance that starts takes the lock and is read-write, so the mode is a function of the lock and not of an in-flight update. Requirements on the lock (the owner's section carries the detail): `O_CLOEXEC` and no inherited descriptors (the signer documents the trap, `crates/kbf-signer/src/server.rs:118-134`), `O_NOFOLLOW`, a local filesystem checked at open. `/readyz` still means "serves", and the mode shows in the body and in a `tierstore_cache_mode` gauge; the updater's `finish` waits for `read_write` before it reports success.
- **The first release with the lock** cannot be handed over to or from a release that has no lock (an older v-current runs without it and would race v-next's `.stage/` cleanup). The `requires = ["handover-v1"]` capability (3.3) makes the plan fall back to a plain restart in that case.
- **The drain.** `Connection: close` on every answer from the drain's start. At the end of the settle the server closes idle keep-alive connections with a **lingering close**: `shutdown(SHUT_WR)`, keep reading, serve any request that was already on the wire (with `Connection: close`), and close after the peer's EOF or a linger bound. A plain close of an idle connection races a request the client has already sent and the client sees a reset with no response byte; the half-close removes that race for a conforming HTTP/1.1 client. Requests on already-open connections are served until the connection closes, bounded by the drain deadline for a client that never closes. Today's drain answers 503 on connections it still holds from the moment it stops accepting until in-flight work is done (`server/mod.rs:614`, `:748-749`); that would turn every keep-alive client of v-current into a failure after the swap, and the drain is the main service change for this strategy.
- **One client policy.** Clients do **not retry**. The zero-failure claim holds without retries, in this document, in the deep dive and in every test. A retried create-only PUT is ambiguous (a 412 after a retry cannot tell the retry's own earlier success from a competing writer), so the hammering clients never retry and any failure fails the test.

#### 4.1.7 Alternatives considered

Two ways to hand over a listening socket without a second port, conntrack or a root `nft` were weighed.

- **Socket activation by the service manager.** The socket is held by the manager and survives the process, so a restart refuses nothing. It was built and parked as a draft pull request for reference (`--listen-from-systemd`, kbf#2091). It covers a restart, not an overlap: the new and the old process cannot both be set up as separate instances over one socket unit, and the drain changes it needs are the same ones this design needs. It also puts the socket in the manager's unit system, one more place to keep in step with the flags.
- **`SO_REUSEPORT` with `tcp_migrate_req`** (kernel 5.14 or later). Two processes bind the same port; the kernel can migrate connections queued on a closing listener to another listener in the group. It has no rule to swap and no conntrack dependence. It is not chosen because the kernel decides how connections are spread between the two listeners while both run, so there is no moment at which "all new connections go to v-next" can be asserted or reverted, there is no equivalent of the confirm before the old process is stopped, and it needs a kernel feature every supported host would have to be checked for. The frontend studied file-descriptor passing against `SO_REUSEPORT` (`docs/design/`, frontend handover) with its own constraints.

The forward was chosen because the swap is one atomic, reversible, observable step with a confirm, at the price of a netfilter dependency (conntrack sizing, firewall coexistence, the `flush ruleset` hazard of 3.5). That is the decision; it is recorded here so it is not reopened without a new fact.

### 4.2 js-agent: lease-drain-restart

Written by the js-agent owner; aligned with rev 2 of the generic sections (3.1, 3.3, 3.4, 3.6). Facts are from the jobscheduler repository: **[F]** is merged code on its main (a path or PR is given), **[F-open]** is an open pull request (jobscheduler#509 the farm unit, #493 the image pull and the store lock, #516 the `crun` runtime wiring), **[new]** is work this design adds, **[D]** a design choice, **[V]** to verify before building on it.

**The shape [D].** The strategy cordons the host, waits for it to hold no lease, and only then stops the agent, swaps the release, starts it, and waits to see it join the lease server again. **The commit is the stop of the agent** (3.3): from the SIGTERM on, the previous behaviour cannot be given back. Everything before it is `prepare` and can be aborted; a drain bound that passes with leases still running **fails `prepare`** and the agent is never signalled (below).

The agent is never killed first. Until the keeper exists, nothing inside the agent ties `crun run` to the agent's life: only a graceful exit kills it, and there is no parent-death signal, so after a crash (SIGKILL, abort) `crun run` and its container keep running. What ends them is the unit: lease cgroups sit under the unit's delegated cgroup (`<unit>/actions/<lease>`), and when the main process dies systemd stops the unit and `KillMode=mixed` kills everything left in it before any restart **[F]** (the unit and its test in jobscheduler#509, which asserts it; the operator note in jobscheduler#516). This holds only for an agent run under that unit. The start sweep then clears the stale lease state **[F]** (jobscheduler#508). No lease is handed over. A job finishes before the stop, or the update fails in `prepare`; the lease server reruns a job from its rerun budget only when the agent or the host dies on its own.

**What exists.**
- *The agent's own drain [F]* (`crates/js-agent/src/lib.rs`, `cli.rs`): SIGTERM or SIGINT sends a heartbeat with the phase `draining` at once, takes no new job, lets running jobs finish for `--drain-wait` (default the stop grace, at most 3600 s), stops the rest with `--stop-grace` (default 10 s, at most 3600 s), and keeps heartbeating until the server has acknowledged every result, then exits 0. A second signal ends it at once. After the updater's own drain it finds no lease and returns quickly; it stays the worst-case bound of the stop.
- *The unit [F-open #509]* (`packaging/js-agent/js-agent-crun.service`): `Delegate=yes`, `DelegateSubgroup=supervisor` (agent in `<unit>/supervisor`, lease cgroups in `<unit>/actions`), `KillMode=mixed`, `--drain-wait 3600 --stop-grace 120`, `TimeoutStopSec=3900`, `Restart=always`, `RestartSec=30`. A host that lacks what `crun` needs makes the agent exit 2 with every missing piece named (`crun_host.rs`).
- *The sweep [F]* (jobscheduler#508): `CrunRuntime::open` removes every lease whose sidecar (`<scratch>/oci-labels/*.json`) names this agent id: kills what runs in the lease cgroup, deletes the container, the cgroup, the directories, then the sidecar. It leaves another agent's and malformed sidecars alone and reports them; a lease it cannot remove keeps its sidecar for the next start. The swept runs are lost, not adopted.
- *Taint and owed checks [F]* (`lifecycle/mod.rs`): a marker under `--state-root` taints the host; an `owed-*` record means a job's checks did not finish, and a restart or SIGTERM inside that window taints the next start. A tainted agent refuses every start (`host-tainted`) and nothing clears it: "the host is reimaged".
- *Locks [F-open #493, #516]*: agents take the image store's lock shared; the start-time stage sweep runs only if `LOCK_EX|LOCK_NB` succeeds; #516 adds a per-scratch-root claim, so a second `crun` agent on the same roots is refused.
- *The server's view [F]*: the hello carries `agent_version` (`js-agent-wire/src/hello.rs`); `GET /v1/hosts` lists hosts with it (`js-lease-server/src/hosts.rs`), and kbf already has a typed client for it (`kbf-lease-client`). A second hello for a live host id is a 409 `host-live`, retried with backoff.
- *Cordon [F]*: `js-sched` has `Cordon`, `Drain(deadline)` and `Uncordon` as operator actions on a worker; a drain never kills a lease and pauses at its deadline (`js-sched/src/cordon.rs`, `js-server-core/src/core.rs`); the rows persist in the store (`js-store/src/cordon.rs`). There is no HTTP route for them in the lease server or `js-cli` **[V]**; the route the updater needs is the own-host route of 3.6(c).
- *`js-agent --version` [F]*: clap prints the package version. The line `runtimes: docker crun` lands with jobscheduler#516 **[F, open]**, and the runtime in the hello is a pending addition to the hello (`contract.rs`) **[new]**.

**Registration.** The agent has no loopback health listener and no registration client **[F]** (no listen or health flag in `cli.rs`). The services file gives the host a synthetic registration (2.3): name, unit, uid, strategy `lease-drain-restart`, and no `health` address. Health is learned from the lease server instead (checks, below) **[new]**.

**Cordon: a prerequisite, not a choice.** The strategy cordons and uncordons through the own-host route on the lease server (`POST /v1/hosts/{id}:cordon`, `:uncordon`, `GET /v1/hosts/{id}` for the host id in the updater's certificate), as 3.6(c) specifies. Without it a busy host never reaches zero leases, so the strategy cannot run until the route exists; this is migration step 9's `needs` (8). Every cordon the updater places is journaled with the order, as owner, and a deadline (`cordon_max_secs`), and `recover` lifts one the journal says is ours (3.6(b)). The agent's SIGTERM `draining` heartbeat is not a substitute: it is sent only at the stop, which is the commit.

**Mapped onto 3.1 and 3.3.**

| step | reversibility | lease-drain-restart |
|---|---|---|
| plan | | May read (`GET /v1/hosts/{id}`, the staged unit, the executor's `query`); changes nothing. Refuses if the staged release is not an agent of this service, the services file's bounds are inconsistent (below), the staged unit's flags or cgroup layout differ from the file's or the running one's, or the host is tainted. Returns the steps with their bounds. |
| stage | Invisible | Fetched and verified (3.1 step 2); then the executor runs `<new> --version` as the service's user; the line must name the runtimes the services file requires (`crun`) **[new]**. Nothing runs beside the old agent (one agent per host id and scratch root): "checked" means staged, not started. |
| prepare | Visible | Cordon (journaled), then wait up to `drain_wait_secs` for `GET /v1/hosts/{id}` to show no lease. Undo: uncordon. **If the bound passes with a lease still running, `prepare` fails** (`drain_bound`): uncordon, report `Failed`, and no SIGTERM is sent. |
| **commit** | Irreversible | "Commit intent" is journaled, then the executor's `trial` (3.4) runs: `unit stop` (SIGTERM; the agent's own drain, at most `drain_wait + stop_grace`, inside `TimeoutStopSec`; only after that does `KillMode=mixed` kill what is left), `slot-set` to the new release, `unit start`. The three are idempotent given the executor's record. The stop is the commit; the trial record and its dead-man deadline only guard the swap and the start. |
| checks | | All of: a hello after the start time carrying the staged `agent_version` and `runtime=crun`; the sweep ran clean (no foreign or malformed sidecar, no lease kept: nothing reports the `SweepReport` outside the log today, **[new]**); no taint and no `owed-*` record; the host `ready` in `GET /v1/hosts/{id}` for `bake_polls` consecutive polls. Bounded by `health_bound_secs`. Then the executor's `confirm` ends the dead-man. |
| abort | | Only before commit: uncordon. After commit there is none. |
| restore_previous | | After commit, when the checks fail or the deadline passes: the executor's trial revert puts the old release back and starts it. A repair: runs stopped at the commit are not brought back. The update reports `Failed`. |
| finish | | Uncordon, only if this update placed the cordon. |
| recover | | Below. |

**Why a passed drain bound fails `prepare` and is not followed by the stop.** (1) The agent has its own `--drain-wait`; stopping it after the updater's wait would start a second wait of up to `drain_wait` inside the agent, so the host would be draining for twice the bound the services file states, with the unit's `TimeoutStopSec` as the only real limit. (2) Whatever the agent does not finish is stopped and rerun from the lease server's rerun budget; an update should not spend a user's budget, and a host whose jobs have none left cannot be updated this way at all. (3) A SIGTERM inside a job's owed-checks window taints the next start, and nothing clears a taint; the updater cannot read the host's state directory, so it cannot know the window is closed and must not guess (3.1, 3.6(e)). A failed `prepare` is a refused update, not a killed run: the cordon keeps new leases away, the wait ends in the normal case, and the coordinator retries the order (`--retry-secs`).

**Recovery after a crash [D].** The executor's record wins (3.5). From the journal phase and `query`:
- *Updater died during the wait in `prepare`:* nothing but the cordon was changed. If the cordon is ours and the drain bound has passed, abort (uncordon); else resume the wait.
- *Updater died after "commit intent":* if the executor shows no trial, nothing ran: report `Failed` and uncordon. If a trial is recorded, go forward from the checks; the unit's state and the slot say which of stop, slot-set and start were done, and each is re-run only if not.
- *Agent crashed mid-drain, or the host rebooted:* systemd restarts it (`Restart=always`). The new process sweeps the previous run's leases (the sidecar ledger), which are lost and rerun, and starts clean. A crash inside the cleanup window leaves an `owed-*` record, so it starts tainted: **the updater stops, reports `Failed` with the taint, and pages; it never clears a taint or an owed record, and never retries** (no clear flow exists **[F]**).
- *Trial deadline passes while stopping:* the window is sized below so that a healthy stop is not reverted.

**Bounds: per-service values of the services file (3.4) [D].** None is a literal in the strategy. Defaults match the unit.

| name | default | meaning |
|---|---|---|
| `drain_wait_secs` | 3600 | the updater's most time to wait in `prepare` for no lease; failing it fails `prepare` |
| `stop_grace_secs` | 120 | the unit's `--stop-grace` |
| `stop_timeout_secs` | 3900 | the unit's `TimeoutStopSec` (3600 + 120 + a 180 s margin); this is the executor's restart timeout for js-agent |
| `active_wait_secs` | to set | most time from `unit start` to a running unit (the unit's `RestartSec` is 30 s) |
| `health_bound_secs` | to set | most time for the checks after start (new work: no default exists) |
| `bake_polls` | to set | consecutive ready observations before `confirm` |
| `cordon_max_secs` | to set | the journaled deadline of a cordon the updater placed (3.6(b)) |
| `confirm_window_secs` | `stop_timeout + active_wait + health_bound` plus a margin | above the 1800 s cap of 3.4, lifted for this service under its load-time invariant |

**The window.** The trial's dead-man window must exceed the longest honest run of the trial: a stop that lasts the full `TimeoutStopSec`, the start, and the checks. Otherwise the executor would revert a healthy drain. After a successful `prepare` the stop is normally seconds; the window is sized for the worst case, a lease that slipped in before the cordon took effect and is finished by the agent's own drain. The plan refuses a services file whose window is below the 3.4 invariant, whose `drain_wait + stop_grace` exceeds `stop_timeout`, or whose values differ from the flags of the staged unit; otherwise systemd would kill a drain the updater is still waiting on.

**New work [new].** What rev 2 covers generically (the strategy interface, the executor's jobs, per-service timeouts, the own-host cordon route, journaled cordons) is not repeated. Left: (1) the runtime in the hello (`contract.rs`), after jobscheduler#516; (2) reporting the sweep result outside the log; (3) the synthetic registration without a health address; (4) the js-agent values above in the services file; (5) the plan's comparison of the staged unit's flags and cgroup layout.

**Open points.** (a) *The keeper:* until it lands, a stop that reaches `TimeoutStopSec` or a cgroup kill is the only way a container leaves; the strategy never kills the agent first, and a kill is the unit's. (b) *Lease hand-off:* none, by design; a job that cannot rerun must not be on a host that is updated, since an agent that dies on its own still reruns it. (c) *Cgroup layout across versions:* `supervisor` and `actions` are names two versions must agree on; a release that changes them cannot be adopted by the sweep of the old one, so plan compares the staged unit's layout with the running one and refuses a change **[new]**.

### 4.3 kbf-daemon: node-drain-restart (today's behaviour)

This strategy is `agent/update.rs:89-250` moved behind the interface of 3.3, run by the executor's verbs. Its behaviour for kbf does not change; two things the executor does around it do, and rev 1 was wrong to call the whole thing byte for byte: the executor serves connections concurrently (3.4), and the restart timeout, active wait and confirm window are read from the services file (with kbf's defaults and cap unchanged). The kbf units keep their `[Install]` and ordering (`packaging/linux/units/kbf-daemon.service:72-73`); a port-forward service is the only kind the executor starts at boot.

- `plan`: the host health precondition (`health.refusal`); the node's cordon state before the update (a node already out of placement is never undrained). Reads only.
- Steps, with the reversibility of 3.3: stage (Invisible); drain the node (Visible; cordon, wait until it holds no lease, however long that takes unless `--drain-deadline-secs` is set, in which case a deadline that passes **fails `prepare`** with the node left drained and nothing installed over running leases, the rule of 3.1; the cordon mark is kept on disk, `--cordon-dir`); `trial` (Visible: pending marker with deadline first, links move, units restart); health gate (the node's health through the frontend, or a service's ready URL, bounded by `--health-timeout-secs`); `confirm` (**the first irreversible step: the commit**).
- `finish`: undrain, only if healthy and only if the updater's own drain placed the cordon.
- `abort` before commit: `revert`; the executor does it by itself at the deadline whatever the updater is doing. After commit there is no abort.
- `recover`: the helper's pending marker means a trial is on; finish it from the health step; do not fetch, drain or switch a second time (`agent/update.rs:135`).

A frontend that runs as a container and a host on the `kbf` package (`deb`, `rpm` layouts) are variants of the same strategy with another backend under the executor, as they are today (`local/mod.rs`: `frontend`, `package`, `linux`, `mac_backend`).

## 5. The generic coordinator

### 5.1 Desired version per service; host sets; who should run a service

The coordinator already stores one desired state per service (`desired.rs:239`) and defines services as data (`services.rs`). Today a host belongs to a service only if its own heartbeat lists it and the service's definition has the host's role (`service.rs:200-207`). That is **self-announced membership**: a host that lost its registration disappears from the service's denominator, and convergence can read `converged` while a host has no copy of the service at all (`converge.rs` counts `at_desired` over hosts that announced). Proposal: the service definition holds a **host-set selector** (certificate roles and labels) and the coordinator decides from it who *should* run the service. A selected host that has not reported the service is `missing`, and `converged` fails while any selected, live host is `missing` or `absent` (2.3). A service that no selected host announces is `in_progress`, not converged.

### 5.2 One host at a time

Per host, already true: `decide.rs:105` orders nothing to a host that has any service updating or an order outstanding (`fleet.rs:179-189`). This is the only serialisation across services this design keeps (Question 3).

**Slots stay per (service, role).** Rev 1 proposed slots shared across services so at most one host of a role is ever down for any service. That couples unrelated services: a tierstore update on one storage host would block a kbf update on another storage host, and `min_up` means different things for different services (kbf: blobs stay readable; tierstore: cache nodes). Slots remain per service (`decide.rs:100-102`), with an optional **named pool** in a definition for two services that really do share a capacity. Two further rules, because a per-host queue has a head-of-line problem (a kbf node drain waits for leases however long it takes, and a js-agent drain up to an hour, so a security fix for another service on that host waits behind them): each definition has a `priority`, which is the order of the reply loop (today it is kbf first and then the agent's announcement order, `service.rs:219`, an accident), and a desired state may carry `urgent`, which makes a host that is not yet updating take that service's order first; it never interrupts an update in progress.

### 5.3 Canary, health gate, rollback; how health is learned

- **Canary.** None in the coordinator today. A one-host canary can be modelled as a separate service with `min_up = 0`, a workaround (without it a lone host is never ordered, `decide.rs:116-120`). Proposal: a first-class `canary` in the service definition: a count or a host selector, ordered first; the rest wait for it.
- **Health gate.** The coordinator never probes: the agent reports `Health` and `UpdateResult` (`wire.rs:29-52`), and a failed update shows as `last_update.outcome == Failed` for the desired version, which keeps the host's slot and makes the service `blocked` (`converge.rs:1-14`). Proposal: keep that, and add the gate as a rule on the *reports*: after a host (or the canary) updates, it must report `healthy` and the desired version for `bake` consecutive heartbeats (a config count) before the next host or the rest of the set is ordered. The coordinator has no network path to a host's loopback and should not get one; the updater's strategy runs the real checks and reports the result.
- **Rollback.** Today a human or CI sets an older version. Proposal: the coordinator remembers `last_good` per service (the last version for which the set converged and passed the gate) and, when a service is `blocked` or a gate fails and its definition says `rollback = "automatic"`, sets desired to `last_good` and marks the failed version `bad` (not ordered again until another version is set). CI can still set a version by hand, which clears the mark. The hosts roll back through the same strategy as any update; a host past a strategy's commit point is rolled back by that strategy's own update path (3.3), not by `abort`.
- **Compatibility and order of rollout.** A new agent to an old coordinator degrades quietly: a `services` list the coordinator cannot read is dropped and the host is treated as kbf-only (`wire.rs:175-196`), so a new service would look unmanaged. The coordinator is rolled first, and the agent refuses to run non-kbf strategies against a coordinator that does not advertise the new heartbeat.

### 5.4 Where today's coordinator is kbf-specific

`decide.rs` is generic: it branches on no service name (`decide.rs:79-121`; `decide()` at `:49-54` is a kbf wrapper). The rest is not.

| where | what | change |
|---|---|---|
| `services.rs:90` | `kbf` is built in and cannot be redefined (`services.rs:206`, `:214` reserve the name) | a definition in the same file format, shipped as a default |
| `desired.rs:36-49`, `:85-116`, `:165`, `:319` | `Desired` requires `max_updating` with four named role fields and `min_up` with `storage` and `frontend` (no serde default); the store special-cases `KBF` | the desired state carries a version and optional overrides keyed by role name; `RoleDef` already has the optional fields (`services.rs:35-46`), so the work is making `Desired` stop requiring them |
| `converge.rs:128`, `:138`, `:194` | `converge` for kbf reads `d.min_up.of(role)` and iterates `HostRole::ALL` | `min_up` per role from the definition; iterate the definition's roles; `missing` hosts (5.1) |
| `service.rs:219`, `:225` | the reply loop is `once(KBF).chain(announced)`; the first `Update` wins | explicit `priority` (5.2) |
| `service.rs:294-311`, `:340`; `http.rs:150` | `status()` lists only `kbf` hosts and computes `up` and `holds_slot` with `KBF`; the HTTP default service is `KBF` | status per service, including update progress (3.8) |
| `service.rs:200-207` | membership is self-announced | selector-decided membership (5.1) |
| `wire.rs:57-100`, `:138`, `:154`, `:235`, `:254` | `kbf` is the top-level heartbeat, others sit in `services`; two reply shapes | a heartbeat version that lists every service including `kbf`, one reply shape with a service field; the old shapes read for as long as old agents exist |
| `wire.rs:29-35` | `Health` is `healthy` or `unhealthy` | `absent` and `stopped` (2.3); a wire change, covered by the compatibility rule above |
| `kbf-types/src/host_role.rs:14`, `:35` | roles are an enum of four | roles as strings. This touches the **certificate**: the role is a URI in the host's certificate (`urn:kbf:host-role:<name>`) and is what the coordinator believes about the host. `git grep` finds `HostRole` in 38 files with 252 references, among them the CA, the enrollment registry and the server's enroll route, the signer's validation and the updater's identity. It is a trust change and its own PR (8) with its own red tests (a certificate with an unknown role string is refused; a role that no definition names is ordered nothing) |

## 6. Where the generic code lives

**Option one: kbf's existing crates, generalised in place.** The crates exist (`kbf-updater`, `kbf-coordinator`, `kbf-host-install`, `kbf-installer`), their tests pin today's behaviour, and the release pipeline already signs and attests them: hosts verify kbf's artifacts workflow as the signer (`crates/kbf-installer/src/attest.rs:1-17`), with a list of accepted signers (`attest.rs:136-137`) that is the existing mechanism for a second signer. Cost: the generic parts carry the name and the build times of a repository that is mostly something else.

**Option two: new crates, `host-updater` and `fleet-coordinator`, in kbf or in a repository of their own.** In kbf this is renaming plus a boundary lint; in its own repository it needs its own release pipeline, its own signer identity on every host (a second trust anchor, a decision of its own), and its own cross-repository testing against the services.

**The dependency question.** tierstore is moving to a repository of its own, and its strategy must not make the updater depend on tierstore or tierstore on the updater. The `port-forward-handover` strategy needs neither: it is **parameterised, generic code in the updater** (public port, private ports, ready path, families, settle) and works for any TCP service that offers the same things. What is tierstore-specific is a **contract** the service implements: private-port listening on the wildcard of each family, a read-only mode decided by a cache lock, a drain with a lingering close, and the `handover-v1` capability. The contract is a document and a golden test in the updater's repository; the service implements it with no code from the updater. The registration protocol is one JSON line over a unix socket, small enough that a service reimplements the client. So: **the updater depends on no service; services depend on no updater crate; the contract is the interface** (Question 5).

**Recommendation: option one now, with the boundary drawn as option two would.** Generalise in kbf; keep the generic code in crates whose names and public items mention no product; add a lint that the generic crates name no service; move to their own repository only when a second owner needs to change them.

## 7. The interim per-service deploy workflow

Until the pieces above exist, a service that is not `kbf` is rolled by an interim per-service deploy workflow (kept in kbf, off unless a repository variable is set, in its own concurrency group). What it does now: after a release is published, it sets that release as the desired version of the service with the CI token (an allow-list entry that names only that service, `docs/design/updates.md`, section 5.1), polls the convergence of that service (`GET /v1/convergence?service=S`, `docs/design/updates.md`, section 3), runs a check against the hosts, and on a failed check sets the desired version back to the version recorded before the roll. A stall or a cancelled job is "incomplete", not a failure of the release, because the coordinator goes on rolling by itself once the version is set.

How it maps onto the generic pieces later:

| the workflow does | later done by |
|---|---|
| sets desired, polls convergence | unchanged; the workflow becomes "set desired and read the result" |
| runs a check on the hosts | the strategy's `checks` and the coordinator's health gate (5.3) |
| rolls back by setting the recorded version | the coordinator's `last_good` and automatic rollback (5.3) |
| a one-host canary service with `min_up = 0` | the first-class canary (5.3) |
| restart timeout, active wait and confirm window as per-service config of the helper | the same values, in the services file (3.4) |
| records the attempt | the coordinator's `bad` mark |

Nothing in the workflow is thrown away without its replacement being in place first; it is deleted by the last PR of section 8.

## 8. Migration order

Each step is one PR, independently shippable, tests first with a red proof. Sizes are estimates [I]: S under a week of one engineer, M one to two weeks, L more. The three strategies are checked against the trait **in the same PR that freezes it** (step 3), so the trait is not frozen by kbf alone. Steps 2, 3, 5b and 6 touch the update path of every host; none lands during a roll in flight, and the coordinator (5a, 5b) is rolled before the agents (5.3).

| # | step | size | needs | red proof first |
|---|---|---|---|---|
| 1 | **Characterisation.** A kbf-only golden for `decide`; goldens for the helper's timeouts and window; the agent's `run_inner` call order against the fake ports | S | | each golden fails against an altered constant |
| 2 | **Executor concurrency and per-service config.** Connection threads, jobs for `stage`/`unit`/`trial`, the tick on its own thread, restart timeout, active wait and window per service with the load-time invariant; kbf's defaults and cap unchanged. States how the helper restarts itself (3.7) | M | 1 | a long `unit stop` does not delay a `revert`, `query` or reconcile; a configuration violating the invariant is refused; a slow healthy start under a lifted cap is not reverted (logical time) |
| 3 | **Strategy trait, with kbf extracted and tierstore and js-agent prototyped against a fake executor.** Reversibility, `overlap`, commit journalled first, `abort` and `restore_previous` | M | 1 | a fake strategy failing at each step; `abort` called with the journal's step; nothing after commit is aborted; an `Invisible` step that changes the public path fails its test |
| 4 | **Registry and the services files** (two files per service, 2.5), `absent`, persistence, the packaging and migration for hosts that have `--service` flags today (a host with the flags keeps working with no file) | M | 3 | wrong uid, unlisted name, out-of-bound parameter, a name mapped to a shared uid, a reused uid: each refused; a registration survives an updater restart |
| 5a | **Coordinator generalisation.** `Desired` without kbf's fields, `converge` and `status` per service, `priority`, `urgent`, selector-decided membership, `missing`, canary, `bake`, `last_good` and automatic rollback, the heartbeat that lists every service | L | 1 | with only `kbf` defined, decisions unchanged; a selected host without the service is not converged; a rolled-back version is not ordered again |
| 5b | **Roles as strings** through CA, enrollment, signer and identity | L | 5a | a certificate with an unknown role string is refused |
| 6 | **Port-forward mechanism.** Pure rule generation, `forward-apply`/`confirm`/`show` as a trial, reconcile, the probe (9) | M | 2 | see 9 |
| 7a | **tierstore: the cache lock, read-only mode, self-promotion, the build and mode gauges**, released but not yet used by a hand-over (the first upgrade is a plain restart) | M | | the lock is free after the holder is killed with a child alive (`O_CLOEXEC`); the read-only start touches nothing on disk |
| 7b | **tierstore: wildcard listeners, the drain (`Connection: close`, lingering close, no 503), the settle** | M | 7a | a request injected after the server's FIN is answered; an idle connection is closed with a half-close |
| 8 | **`port-forward-handover` strategy and the hand-over tests** | M | 3, 6, 7a, 7b | see 9 |
| 9 | **js-agent strategy** | M | 2, 3, the lease server's own-host cordon route (3.6) | from its owner |
| 10 | **Retire the interim workflow** and the static `--service` flags | S | 8, 9 | |

The critical path is 1, 2, 3, 6, 8 for tierstore; 5a and 5b are independent of it and are the largest part.

## 9. Tests

The rules for all of them: **no test asserts on or depends on elapsed time**; sequences are driven by handshakes (a count of completed requests, an exit status, a start line, an answer, a file that appears), and hang guards live in `.config/nextest.toml`. **Red proofs come first**: each test is written, seen to fail against the wrong behaviour, and only then is the behaviour written.

**Which tests decide.** The claim "zero failed requests" is decided by deterministic tests, not by a stress run:

1. **Pipe tests** (the `drain_tests.rs` pattern, in-memory pipes and a paused clock): the server's lingering close; a request injected after the server's FIN is answered with `Connection: close` and then the connection closes; an idle connection with no request gets a clean end of stream and no reset; requests on open connections are served after the server stopped accepting; the happy path ends with `Drained.abandoned == 0`.
2. **Namespace scenario tests**, each a fixed sequence over real sockets and the real rules, sequenced by handshakes: (a) open K idle keep-alive connections to v-current; swap; a new connection is answered by v-next (a header with the proxy id) and each of the K still by v-current; drain; each of the K ends in a response followed by end of stream, never a reset; (b) the previous port is swapped back before the commit and a new connection reaches v-current; (c) kill the updater after `forward-apply` and let the executor's deadline pass (a manual tick in the test): the forward is back on the previous port; (d) crash between the record and the apply, and between the apply and the confirm: the reconcile table of 3.5; (e) `flush ruleset` in the namespace: the next reconcile restores both tables; (f) a connection established **before** the first load of the rules is driven after it (with the conntrack module state both loaded and unloaded); this decides Question 9; (g) a connection from a veth peer (a remote caller) reaches v-next through the prerouting rule with a restrictive second table loaded that matches `ct original proto-dst 9300`; (h) a remote connection to the private port directly is dropped by the guard.
3. **The hammering test.** N clients loop signed create-only puts of unique keys, gets and ranged gets, over keep-alive and fresh connections, **with no retry**; the harness swaps and drains in the order of 4.1 with each step released by a count (every client has completed a request answered by v-current; then by v-next; ...). The assertion is zero failed requests. It is a **stress test**: the interleaving at the swap is arbitrary, a pass does not prove no failure can occur, and the deterministic tests above carry the proof. Its invariant (zero errors under any legal interleaving) means any failure is a real bug, and the request trace is written on failure so it can be replayed. Red proofs: the current 503-on-reuse drain fails it; a plain close of idle connections instead of the lingering close fails the pipe test; a settle of zero fails scenario (a) variants; a swap to a port nothing listens on fails with refusals.

**Rule generation.** Unit tests, no namespace: a golden file per port pair; ports outside the bounds, equal ports, reserved ports and a `forward-apply` to a port that is not one of the two are refused; the swap file differs between the two ports only in the two port numbers; a mutation of the family or hook fails the golden; the guard accepts only `lo` sources and redirected flows. `nft -c -f` of the golden in a root namespace pins the syntax.

**Registry.** Wrong uid refused; unlisted name; out-of-bound parameter; shared uid; a second registration from the same uid replaces, from another is refused; persistence across a restart; `absent` when never registered; `stopped` after the close (logical time). Each with its red proof.

**Strategy fakes.** The `Strategy` fake fails at each step; the tests assert that `undo` is called with the journal's step, that "commit intent" is on disk before the first irreversible step runs, that a crash between the journal line and the step is resumed by `recover` exactly once, and that nothing after commit is aborted.

**Coordinator over fake hosts.** The shape of `decide_tests.rs` with several services: per-service slots, per-host serialisation, `priority`, `urgent`, canary, `bake`, `last_good` rollback, the `bad` mark, a selected host missing the service, a coordinator restart mid-roll. The kbf-only golden is unchanged.

**CI: what can run nftables.** The plan is a **root network namespace through `cargo xtask sudo-test`**, which the repository already uses on hosted Linux runners (`.github/workflows/host-install-linux.yml:47-51`). It does not rely on unprivileged user namespaces: the repository's own sandbox documents that on Ubuntu with `apparmor_restrict_unprivileged_userns` a new user namespace has no capabilities and raising `lo` fails (`crates/kbf-task/src/network.rs:36-56`, `docs/platform-properties.md:464-465`), and no workflow uses `unshare`, `nft` or `ip netns` today (checked with `git grep`). The tests create their namespaces as root (`unshare --net`, or `ip netns add` with a veth pair for the prerouting path) and raise `lo`. The first PR of the series (step 6) is the **probe, written as a test that can fail**: it asserts the kernel features the design uses (NAT in an `inet` chain, `fib daddr type local`, `ct original proto-dst`, `ct status dnat`) by loading them with `nft -c`, asserts that the conntrack sysctls named in 4.1.3 are readable and writable in the namespace, and installs `nftables` if the image lacks it. If it fails on the hosted image, the design does not proceed on the assumption that it works.

## 10. Questions

Each with a recommendation. A decision goes to the owner of this design.

- **Question 1: registry transport.** A unix socket with `SO_PEERCRED`, or a file drop. *Recommend the socket, in a root-owned group directory, with grants in root-owned files (2.2, 2.5).*
- **Question 2: privilege.** One root updater, or the unprivileged updater plus a root executor. *Recommend the split of 3.4, with a concurrent executor and jobs.*
- **Question 3: serialisation.** Per host only, per host plus slots shared across services, or global. *Recommend per host (`decide.rs:105`, already true) and slots per (service, role), with an opt-in named pool: shared slots couple unrelated services, and a global lock makes a fleet's update time the sum of all services.*
- **Question 4: who owns automatic rollback.** The coordinator (`last_good`, `bad` mark) or each deploy workflow. *Recommend the coordinator.*
- **Question 5: where the code lives and who depends on whom.** *Recommend option one, in kbf, product-free crates, a name lint, and the contract as the only interface to services (6).*
- **Question 6: private listeners.** Wildcard listeners plus a guard with redirect, or loopback listeners plus DNAT and `route_localnet`. *Recommend wildcard plus guard: `route_localnet` widens the host's acceptance of loopback addresses for every service. The cost is that a missing guard exposes the private port, so the guard is a plan precondition and part of the reconcile.*
- **Question 7: one drain or two.** The drain changes replace the current drain, or sit behind a flag. *Recommend replacing it unless a deployment relies on 503-on-reuse; a second mode will rot. The tierstore owner decides.*
- **Question 8: the keep-alive close race.** *Resolved by the lingering close and one no-retry client policy (4.1.6); the residual is a request within one round trip of the FIN and is counted in a metric.*
- **Question 9: first conversion of a host that listens on 9300 today.** *Recommend a planned restart, not a hand-over.* Loading the first NAT chain makes conntrack track flows that were not tracked; a mid-stream packet of an old flow is picked up as new and the rule remaps it to the private port, which resets it. Scenario (f) of 9 decides this; until it exists, the first conversion is a restart with a gap.
- **Question 10: the service-side registration client.** *Recommend a documented one-line protocol and an optional small crate later; tierstore's own repository takes no kbf dependency.*
- **Question 11: canary and bake as first-class definition fields.** *Recommend yes (5.3).*
- **Question 12: install a declared service that never registered.** *Recommend off by default (`install_when_absent = false`), on per service while provisioning.*
- **Question 13: the settle.** 130 s, covering the whole SYN schedule of a default client, or removing the `SYN_SENT` entries at the end of a short settle. *Recommend 130 s: it costs update time only and needs no conntrack operation in the executor.*
- **Question 14: cordon expiry on the lease server.** The updater journals and lifts its own cordons, but a cordon with no live owner never expires on the server. *Recommend the lease server gives a cordon an optional expiry set by the requester, and the updater always sets one.*
- **Question 15: the js-agent cordon route.** *Recommend own-host `cordon`, `uncordon` and `GET` on the lease server's worker listener for the host in the certificate (3.6); the js-agent strategy waits for it.*
- **Question 16: the drain-bound rule.** A bound that passes with work running fails `prepare` for every service (3.1). *Recommend yes; it is today's kbf behaviour and the only safe one where a stop can taint a host.*
