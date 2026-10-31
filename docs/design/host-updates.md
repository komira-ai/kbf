# One host updater with update hooks, and a generic coordinator

Status: DRAFT. **The shape in this document is decided** (rev 4): a generic updater on every host that knows no service, and an update hook that each service ships as a subcommand of its own binary. What remains open are the Questions in section 10. No code. Facts were read at `origin/main` 80436afe7 unless a section says otherwise; a `path:line` is a line of that file at that commit. Where a statement is an inference or a claim about software we do not own, it says so.

Labels: **[F]** read in the code at the commit above, **[I]** inference, **[D]** a design choice made here, **[V]** to verify before building on it.

Sections 4.1 (tierstore) and 4.2 (js-agent) are written by their owners. In rev 4 each is to be restated as the service's `update-hook` steps; a note at the top of each says so. The nftables mechanics of 4.1 moved under the tierstore hook and are the tierstore owner's from now on.

## Changes in rev 4

The shape changed; rev 3 is superseded. What was decided:

1. **The updater is generic and knows no service.** The per-service strategy library and the closed set of root verbs are gone.
2. **Each service ships its hook as a subcommand of its own binary**, for example `tierstore update-hook <step>`: Rust, tested with the service, no shell. The cutover logic for a version ships with that version.
3. **A fixed step contract:** `prepare <artifact>`, `cutover`, `verify`, `rollback`, `cleanup`. Each step is safe to run twice; the updater persists the step it reached and resumes after a crash (3.2, 3.5).
4. **Hooks are declared in a root-owned file the package installs** (`/etc/updater.d/<service>.toml`): the hook binary, the rights it needs, bounds per step. They are never registered at runtime. At runtime a service registers only liveness and version (2). A hook runs with exactly its declared rights (3.4).
5. **Trust (3.3):** the hook binary is the artifact the updater verified (sha256 and attestation) before anything from it runs; a declaration that is not root-owned, or is group- or world-writable, is refused.
6. **The tierstore nftables swap and the js-agent cordon and drain become those services' hooks** (4.1, 4.2). The kbf daemon's current behaviour becomes a hook too (4.3). The coordinator stays generic (5).
7. **Superseded and removed:** the strategy interface and its capability flags, the closed root verb set, the executor's per-verb concurrency table (replaced by the step runner and its persisted step record), the registry transport, privilege and strategy-location questions.
8. **Kept from rev 3:** `Deferred` and `halt` outcomes and their advertisement (`deferred-v1`, `halt-v1`), `promotion_pending` as a heartbeat state, release/environment gate classification, per-(service, role) slots with a named pool, canary and bake, `priority` and `urgent`, selector membership with `excluded` and `unmanaged`, the services-file load rules (as the declaration's), observability.

Earlier revisions (2, 3, 3.1, 3.2) answered an independent review of the strategy-library design; their changelogs are dropped with that design.

## Reviewer notes

From the independent review of rev 3. Marked where they still apply.

1. *Superseded by rev 4:* what was decided (the strategy library and the closed verb set). The shape is now decided by the ruling above; port forwarding is a given for tierstore.
2. *Superseded by rev 4:* size and critical path; see the migration list in 8.
3. *Still applies:* nothing was built or run. The two facts that gate the nftables tests are the hosted runner's `nft` and a root network namespace; the probe test decides.
4. *Still applies, now tierstore's:* a client with a tuned `tcp_syn_retries` voids the zero-failure claim; the first release with the cache lock, and every release before it, is a plain restart with a gap. A js-agent update costs up to about two hours of host downtime and is `Deferred` indefinitely on a host with a job that cannot be stopped.
5. *Still applies:* prerequisites outside kbf: the own-host cordon route and cordon expiry on the lease server, a runtime in the js-agent hello, and the keeper.
6. *Still applies:* needs an owner's answer rather than an edit: the `rollback_after_failed` default; whether `promotion_pending` is a failure on a storage node; the supported kernel floor for NAT in an `inet` chain.
7. *Superseded by rev 4:* the executor's `schema` versioning and `forward-set`; the step record of 3.5 carries a schema number, and `forward-set` is the tierstore hook's business.

## 0. The proposal in one page

Today every host runs an unprivileged agent (`kbf-updater agent`) beside a root helper (`kbf-host-install --serve`) per product, and a coordinator that keeps a desired version per service. The agent runs one fixed sequence (fetch, drain the node, switch, health, confirm or revert, undrain); the helper installs a component whose name is the literal `kbf`. What it takes to move a particular product from one version to the next is written into the updater.

The decided shape moves that knowledge out of the updater and into the service:

1. **The updater is a generic process.** On every host it fetches the release, verifies it (sha256 and attestation), stages it, runs the service's hook through a fixed five-step contract, persists which step it reached, reports to the coordinator, and rolls back. It knows no service.
2. **Each service ships its update hook as a subcommand of its own binary.** The cutover logic for a version ships with that version, is written in Rust, and is tested in the service's own CI. The updater never contains a service's cutover logic.
3. **The declaration of a hook is a root-owned file the package installs.** It names the hook binary and the rights it needs. A service cannot register code at runtime; at runtime it registers only that it is alive and which version it runs.
4. **The coordinator is generic over services**: it holds the desired version per service, the hosts, slots, canary and bake (5).

**What this replaces.** The earlier direction was that no update logic lives in a service and that the updater stays a simple program that fetches, verifies, replaces binaries and restarts units. This design keeps the generic half (the updater is one process that is the same for every service and decides nothing about any of them) and replaces the other: the cutover of a service is the service's own code, shipped with it as a hook that runs only when an update runs, never in the serving process.

## 1. Today

**Coordinator** (`crates/kbf-coordinator`). It keeps one desired state per service name (`desired.rs:239`, `DesiredStore`; the file is replaced whole by temporary file, fsync, rename and directory fsync, `desired.rs:1-14`). A service is data: a name, the roles it runs on with `max_updating`, `min_up`, `never_last` and per-role `after`, and `after` between services (`services.rs:1-20`, `:90` for the built-in `kbf`). `decide` is a pure function of services, desired states, the fleet built from heartbeats, the host and the time (`decide.rs:1-34`); it orders a host to update only if seven rules hold: the coordinator has been up for `QUIET_START` (30 s, `fleet.rs:30`); the host is not updating and no other service updates on it or has an order outstanding for it (**one update per host at a time**, `decide.rs:105`, `is_busy_besides`); the host is behind; it did not fail this version; the role has room (`max_updating` slots, counted per service); the role and service `after` conditions hold; enough other hosts of the role are up (`min_up`). A host silent for `DOWN_AFTER` (45 s, `fleet.rs:27`) is down. An order the host has not reported holds its slot for `ORDER_AGE` (120 s, `fleet.rs:36`). The state is rebuilt from heartbeats after a restart; outstanding orders are forgotten (`docs/design/updates.md`, section 6). Convergence is `converged`, `blocked` or `in_progress`, computed from heartbeats alone (`converge.rs:1-30`); `blocked` means a live host failed an update to the desired version and is behind it. A rollback is an operator (or CI) setting an older desired version (`docs/design/updates.md`, section 3, "Rollback: set X older").

**Wire.** A heartbeat carries platform, version, health, last update result and `updating`, all for `kbf`, plus `services: Option<Vec<ServiceBeat>>` with the same four facts per other service (`wire.rs:57-100`); `kbf` is never listed in `services` (`wire.rs:138`, `:154`). A reply is `nothing` or `update to X`; for another service it names the service, in a shape only an updater that sent `services` is sent (`wire.rs:235`, `:254`). Node id and role come from the host's certificate, not the heartbeat (`docs/design/updates.md`, section 2).

**Agent** (`crates/kbf-updater/src/agent`). Unprivileged: `User=kbf-updater`, empty `CapabilityBoundingSet=`, `NoNewPrivileges=yes`, address families unix, inet and inet6 (`packaging/host-agent/linux/kbf-updater-agent.service:32-43`). Two threads: a heartbeat thread, and an update thread that runs one update at a time (`agent/run.rs:1-20`). Extra services are configured **statically** by flag: `--service NAME,helper-socket=PATH[,channel=C][,drain=node|none][,ready-url=URL]` (`agent/flags.rs:52-57`, `ServiceConfig` at `:186`), each with its own helper socket; the agent refuses a service whose helper socket is its own (`flags.rs:324`). The update is one sequence (`agent/update.rs:1-34`, `run_inner` at `:89`): health precondition, `query` the helper, resume a trial the helper has pending (`:135`), install (fetch and verify, nothing of the node changes), drain the node at the frontend (cordon, wait for leases; `:253`), switch on trial, health (`:162`), confirm or revert, undrain only if healthy; a node that was out of placement before is never undrained. Failure to switch puts the node back (`back_on_a`, `:211`). The health clock starts after the switch returns; `--health-timeout-secs` defaults to 300 and is agent-wide (`flags.rs`, doc at `:60`). A service with `ready-url` is healthy when `GET path` on a loopback address answers 2xx; that "says nothing of which version answered" (`agent/service_health.rs:1-6`).

**Root helper** (`crates/kbf-host-install`, `--serve`). Root on purpose (`packaging/host-agent/linux/kbf-host-local.service:21-26`), with a unix socket of mode 0600 owned by the one account `kbf-updater` as the whole access control (`local/serve.rs:1-30`; the unit passes `--socket-user kbf-updater`, `kbf-host-local.service:26`). Five requests: `install`, `switch`, `confirm`, `revert`, `query` (`local/mod.rs:1-30`). It owns the revert: a release on trial gets a pending marker with a deadline on disk, and `Ab::tick` puts the old release back when the deadline passes unconfirmed, including the first tick after a host restart (`local/core.rs:173`; `local/mod.rs`, "This program owns the revert"). One connection at a time. Every request but `query` and `revert` is audited. It verifies the release itself: the pinned `gh attestation verify`, repository, signer workflow, hosted runners, full commit and allowed branch, from root-owned files (`crates/kbf-installer/src/attest.rs:1-17`), with a list of accepted signers (`attest.rs:136-137`, loop at `:251`). **What is kbf-specific in it:** the component name `"kbf"` and the staged file `kbf.tar.gz` (`local/linux.rs:369`, `:379`, `:387`); the restart timeout and the active wait, 120 s each, constants (`crates/kbf-host-install/src/plan.rs:88`, `:94`); the confirm window, 600 s by default and accepted only from 120 to 1800 (`local/serve.rs:58`, `:175`); the restart units named by `--restart-unit` on its command line (`kbf-host-local.service:26`). A second product therefore needs a second helper process with its own socket today (`docs/deploy/kbf-updater.md:347`).

**tierstore.** `crates/tierstore/src/server/handover.rs` is **not** a process hand-over: it is the hand-over of a put's body from the request to the backend call over a bounded queue (`handover.rs:1-14`). Nothing in the server passes a listening socket. The drain (`server/mod.rs:164-172`, `serve_from` at `:596`) keeps accepting through `--drain-lb-wait` (10 s default, `:410`), then sets `REFUSING` (`:614`), answers every new request 503 on connections it still holds (`:748-749`), waits for requests in flight, then closes connections (`finish`, `:644-647`). The local cache removes `.stage/` at start (`server/local_cache/mod.rs:289`, `layout::clear_stage`) and has no lock today (`local_cache/mod.rs:1-8` says it has "no journal, no lease and no reconciler"), which is why two processes cannot share a cache directory today. Nothing updates tierstore: it is not a managed service of the host updater.

## 2. The registry and the declaration

There are two different things, and keeping them apart is the security property.

**The declaration** is a file, `/etc/updater.d/<service>.toml`, root-owned, installed with the package (3.3). It says what the updater may run for this service and with what rights. It is read by the updater; it is never written by a service at runtime.

**The runtime registration** is a message from the running service to the updater's socket. It carries only **liveness and version**: the service name, the version text it runs, and a loopback health address and path. It carries no command, no path to run, no rights and no parameter of any hook. A process that reaches the socket can therefore claim to be alive; it cannot make the updater run anything. Running code as root is only ever the result of a verified artifact and a root-owned declaration.

### 2.1 The registration socket

A unix socket in a directory owned by root with group `updater-register`, mode 0750, the socket 0660 in that group; a service account joins the group in its unit (`SupplementaryGroups=`). Authentication is `SO_PEERCRED`, the pattern the repository already uses for its signing socket (`crates/kbf-signer/src/server.rs:44-53`, which refuses a world-accessible directory, `:59-82`). The uid must be the one the declaration names for that service; one uid per service (a name mapped to a shared uid is refused when the declarations load). The uid name in a declaration is resolved at start and again on reload; a uid that changed is refused. The health address must be loopback and its port within the range the declaration grants; the updater, unprivileged, only issues a `GET` to it.

| field | meaning | checked against |
|---|---|---|
| `name` | service name (`wire::is_service_name`, at most 64 bytes) | a declaration exists |
| `version` | the version text the service runs | the version the updater recorded when it staged the release; a mismatch is reported unhealthy, never believed |
| `health` | a loopback address and path answering 2xx when ready | loopback; port in the declared range |

### 2.2 A service that is not running, or never registered

- **Declared, never registered:** reported to the coordinator as `absent` (a new health value; today there are `healthy` and `unhealthy`, `wire.rs:29-35`). The coordinator does not count an absent service toward `min_up` and does not order it unless the service definition says `install_when_absent`.
- **Registered, now not running:** the connection closed. The registration stays for `registration_ttl` (a declaration value); the service is reported unhealthy with `stopped`. A hook is told the service is stopped (an input, not a different step set).
- **Registered twice:** a second registration of the same name from the same uid replaces the first; from another uid it is refused and logged.
- **kbf itself** has a declaration like any service; hosts configured today with `--service` flags get a synthetic declaration read from the same file format until the flags are retired (8).

### 2.3 Persistence across an updater restart

The accepted registration is written to the updater's state directory (mode 0700) by temporary file, fsync, rename and directory fsync. On start the updater loads it, marks every service `unconfirmed`, and reports it as last known until the service reconnects; one that does not within `registration_ttl` becomes `stopped`. Capabilities are never read from this file: the declarations are re-read on every start.

### 2.4 The declaration

```toml
# /etc/updater.d/tierstore.toml   root:root, mode 0644, installed with the package
name   = "tierstore"
binary = "tierstore"                    # file name inside the artifact; the hook is `<binary> update-hook <step>`
signer = { repository = "...", workflow = "artifacts.yml" }   # whose attestation the artifact must carry
uid    = "tierstore"                    # the account the service runs as and registers as
registration_ttl_secs = 300
health_ports = [9301, 9302]             # loopback ports a registration may name

[hook]
user  = "tierstore"                     # the account the hook runs as (a name; resolved at load)
group = "tierstore"
capabilities = ["CAP_NET_ADMIN"]        # ambient and bounding; nothing else
read_paths   = ["/usr/libexec/kbf", "/etc/tierstore"]
write_paths  = ["/var/lib/tierstore", "/run/tierstore"]
units        = ["tierstore@9301.service", "tierstore@9302.service"]   # the only units the hook may start, stop, mask
address_families = ["AF_UNIX", "AF_INET", "AF_INET6", "AF_NETLINK"]
environment  = { }                      # only these variables; nothing is inherited

[hook.timeout_secs]                     # per step; the updater kills a step that passes its bound
prepare = 600
cutover = 300
verify  = 600
rollback = 600
cleanup = 4000

verify_deadline_secs = 1200             # after cutover succeeds: verify must succeed by then, or the updater runs rollback
```

Load rules, applied to every declaration and across all of them: the file is root-owned and not group- or world-writable (else refused); a name mapped to a uid that another declaration uses is refused; two declarations that claim the same unit or port range are refused; a unit name must match the grammar for that service (never an existing system unit); `verify_deadline_secs` must be at least the `verify` bound plus the `rollback` bound (else the updater would roll back a verify that is still allowed to run). **A declaration that fails a rule refuses that one service** (it becomes `unmanaged` and raises an alert); the updater goes on with every other service. Grants (user, capabilities, paths, units, signer) and tunables (timeouts, deadlines) may be kept in two files per service so a change of tunables in review is not mistaken for a change of grant.

## 3. The generic updater

### 3.1 The pipeline

For one update order of one service, in this order, each step only if the one before it succeeded. Before each step the updater writes the step record (3.5).

1. **Precondition.** The host is healthy enough to update; the service is declared; no other update runs on the host (the coordinator enforces it, `decide.rs:105`; the updater enforces it again because it is the one that would be hurt).
2. **Fetch and verify.** Download the release the version names from the channel the declaration's signer names; check the sha256 against the signed sums and the attestation (repository, signer workflow, issuer, hosted runners, commit, branch) on the host, with the pinned `gh` and trusted root (`crates/kbf-installer/src/attest.rs:1-17`); lay it out beside the running release. The updater never takes a URL or a path from the coordinator's reply: the version is the only input. **Nothing from the artifact runs before this step has succeeded** (3.3).
3. **Declaration.** The new release carries its declaration; 3.3 says when and how it is installed.
4. **`prepare <artifact>`**, then **`cutover`**, then **`verify`**, then **`cleanup`**, each a run of the hook (3.2), each with its bound. **`rollback`** runs when a step after the commit point fails (3.2).
5. **Report** in the next heartbeat, including `Deferred`, `halt` and `promotion_pending` (3.8).

The updater decides nothing about any service in this sequence: it does not drain, restart, swap or cordon. Everything that touches the service happens inside a hook.

### 3.2 The step contract

A hook is `<binary> update-hook <step> [args]`, run by the updater with the rights of the declaration (3.4). Rust, no shell. The five steps:

| step | arguments | what it must do | reversibility is the hook's |
|---|---|---|---|
| `prepare` | `<artifact>`: the staged release directory | check the host can take the update, make the new version ready beside the old, and change nothing a client can see; place anything it must undo later (a cordon, a started instance) | |
| `cutover` | | the visible change: make the new version take over | |
| `verify` | | observe that the new version serves and is the one running; may be run again and again | |
| `rollback` | | undo what `prepare` and `cutover` did, as far as they can be undone | |
| `cleanup` | | finish: remove what only the update needed (the old instance, a cordon, staged files) | |

**Every step is safe to run twice.** The updater does not know what a step did; it knows only that a step it started and did not see finish may be started again, and that doing so leaves the same state. The conformance suite (9) enforces this on every hook.

**Exit codes.** `0` success. `1` failure. `75` deferred: the host is busy or not ready, nothing irreversible happened, try later (rev 3's `Deferred`). `76` halt: the failure is one no version can repair and acting further would make it worse (rev 3's `halt`, 3.8). A step killed by its timeout or a signal is a failure. Any other code is a failure.

**Structured output.** The last line the hook writes to stdout is one JSON object, which the updater reads; a missing or malformed line is read as `{}`:

```json
{"phase":"after_commit","class":"release","reason":"...","retry_after_secs":300,
 "reversible":true,"promotion_pending":false,"detail":"..."}
```

`class` is `release` or `environment` (3.8). `retry_after_secs` is used with `75`. `reversible` says whether the hook can still undo what it has done (the updater reports it; it does not decide with it). `promotion_pending` is set by `verify` when the service is serving but not yet fully its own (3.8). `detail` is a short text for the log and the alert. The hook's stderr goes to the log.

**The commit point.** The updater's commit point is the success of **`cutover`**. Before it, a failure in `prepare` or `cutover` runs `cleanup` and reports `Failed` with phase `before_commit` (or `Deferred` for 75, or `halt` for 76): nothing visible is left. After it, only `rollback` remains. **`verify` failing after `cutover` succeeded runs `rollback`**; `verify` is also bounded by `verify_deadline_secs` of the declaration, and if the deadline passes with `verify` not yet successful the updater runs `rollback` by itself. A hook that has an irreversible step of its own should put it in `cleanup`, after `verify`, so that `rollback` stays available through verification; a hook for which the visible change cannot be undone (the js-agent's stop of the agent) puts it in `cutover` and its `rollback` is then a repair that does not give back what was lost, which it says with `"reversible": false`. If `rollback` itself fails, the host is in a state nothing automatic can repair: it is a `halt`.

### 3.3 Which binary runs, from where, and what is trusted

**Trust.**
- The declaration is **root-owned and installed with the package**. A declaration that is not owned by root, or is group- or world-writable, or lives in a directory that is, is refused.
- The hook binary is **inside the artifact the updater verified** (sha256 and attestation). The updater runs a hook only from a staged release it verified; a hook from an unverified artifact never runs, as root or as anything. The check is made again when a step is resumed after a crash (the staged directory's digest is in the step record).
- Running the hook as root is never an effect of a runtime message: the registration socket carries liveness and version only (2).

**Which binary runs which step.**

| step | binary | from |
|---|---|---|
| `prepare <artifact>` | the **new** version's hook | the staged release, `<artifact>/<binary>` |
| `cutover`, `verify`, `cleanup` | the **new** version's hook | the staged release (or, once the updater has moved the link, the current path, which is the same file) |
| `rollback` | the hook of the version that did the cutover, i.e. the **new** one, because it knows what it changed | the staged release |
| `rollback`, if the new hook cannot run at all (the binary is missing or killed by a signal on start) | the **previous** version's hook | the previous release, which the updater keeps staged until `cleanup` has succeeded |
| any step of an update **to an older version** (a rollback of the fleet) | that older version's hook | its staged release; it is an ordinary update whose "new" version is the older one |

**The declaration and the package.** The declaration comes **with the release**: each artifact carries `updater.d/<service>.toml`, and the updater's own trusted install step, not the hook and not a runtime message, installs it into `/etc/updater.d/` after the artifact is verified and **before `prepare`**, because `prepare` runs the new hook and the new hook is declared by the new release. A package manager that installs the same file on a host that has one is equivalent and wins; the updater installs the file only when no package manager owns it. Two copies are kept, the active declaration and the previous one, because `rollback` of the previous version and the fall-back above need the old rights. **A declaration may not widen rights by itself.** If the new declaration asks for more than the installed one (a capability, a path, a unit, a larger bound) the update is `Deferred` with the reason `rights_change` until an operator has approved that exact declaration (`updater approve <service> <sha256>`, a root act, audited); equal or narrower is installed automatically. Reason: a compromise of a service's release pipeline can already run a hook of its choosing with the rights already granted; it must not also be able to grant itself more.

### 3.4 Rights: how a hook runs

The updater runs each step as a **transient systemd unit** with the properties its declaration fixes and nothing else (`systemd-run --wait --collect --pipe` with properties). From the declaration:

| declaration | systemd property |
|---|---|
| `hook.user`, `hook.group` | `User=`, `Group=` |
| `hook.capabilities` | `AmbientCapabilities=` and `CapabilityBoundingSet=` set to exactly the list; `NoNewPrivileges=yes` |
| `hook.read_paths`, `hook.write_paths` | `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `ReadOnlyPaths=`, `ReadWritePaths=` |
| `hook.address_families` | `RestrictAddressFamilies=` |
| `hook.environment` | `Environment=` of exactly those, and no inherited environment |
| `hook.timeout_secs.<step>` | `RuntimeMaxSec=` |
| (fixed) | `ProtectKernelTunables=yes`, `ProtectKernelModules=yes`, `ProtectKernelLogs=yes`, `ProtectControlGroups=yes`, `RestrictNamespaces=yes`, `RestrictRealtime=yes`, `LockPersonality=yes`, `MemoryDenyWriteExecute=yes`, `SystemCallFilter=@system-service`, `SystemCallArchitectures=native` |

**Controlling units.** A hook that must start, stop or mask a unit (`hook.units`) needs authority over those units and no others. The first way: the updater installs a polkit rule generated from the declaration that lets the hook's account manage exactly the listed units **[V]**. If that proves unworkable on a supported host, the second way is a declaration with `user = "root"`, which is the widest right there is: it is accepted only with the approval of 3.3, and it keeps every other property above (a read-only filesystem except the declared paths, only the declared capabilities, a bounded environment). The kbf daemon's hook (4.3) is of the second kind today.

**Least privilege of the updater.** Two parts of one program, as the agent and the helper are today. The part that talks to the coordinator over mutual TLS, parses its reply and reads the registration socket runs unprivileged (`kbf-updater-agent.service:32-43`, empty capability set). The part that needs root runs only to **launch hooks with the declared rights, to install a verified release and its declaration, and to replace binaries**; it takes no free-form command, path or nft text from the other part, only "run step S of service N for version V", and everything it runs comes from a verified release and a root-owned declaration. It writes the step record (3.5). It is reached over a unix socket of mode 0600 owned by the one account (`local/serve.rs:1-30` is the pattern), and is concurrent: it serves queries and the step record on threads of their own, so a step that runs for an hour never delays a read of the record or the enforcement of a deadline.

### 3.5 The step record, resume and the dead-man

Before each step the root part writes and fsyncs a **step record**: `{schema, service, target version, artifact digest, step, attempt, state: running, started (boot id and a monotonic base), deadline}`; when the step ends it records `done` with the exit code and the hook's last line. The record is the updater's memory; it holds no secrets and is versioned with a `schema` number (a record newer than the binary is read-only).

- **After a crash** of the updater, the root part or the host, the updater reads the record. A step that is `running` is **run again**: it is idempotent by contract, so that is safe. A step that is `done` is not run again. The next step is the one after the last `done`. A step record under a different boot id is treated as running.
- **The dead-man.** The root part enforces the deadlines itself, on a thread of its own, whatever the unprivileged part is doing: a step that passes its bound is killed and recorded as failed; after `cutover` succeeds, if `verify` has not succeeded by `verify_deadline_secs` it runs `rollback`. So a host whose unprivileged updater is gone is still brought back, which is the property today's helper has (`local/core.rs:173`).
- **Resume and the first step.** If the artifact digest in the record no longer matches the staged directory, the update stops as a `halt` and pages; nothing from the directory runs.
- **Updating the updater itself.** The updater and its root part are part of the kbf release and are updated by the kbf daemon's hook as today, but never inside the trial they guard: after `verify` of the kbf update has succeeded, a follow-on step stages the new binary beside the old, finishes the current step record, replies and exits cleanly, and systemd starts the new binary; an `OnFailure=` fallback unit flips the binary link back to the previous one after `StartLimitBurst` failed starts, and the test plants a binary that exits at start. A hook that needs a newer updater than the one running declares it (`min_updater_protocol`); the update is `Deferred` with the reason `updater_old` until the follow-on step has run.

### 3.6 Cordons and drains

A drain is the hook's. The generic rules, to which every hook that cordons or drains is held:

- **A drain bound that passes with work still running is `75` (deferred), never a stop.** The hook lifts what it placed (a cordon), exits `75` with `retry_after_secs`, and nothing is stopped. This is today's behaviour for kbf (`docs/design/updates.md`, section 4, step 2: it never installs over running leases) and the safe one wherever a stop can taint a host.
- **A cordon is a renewable lease**, not a fixed expiry: the hook sets an expiry of a few minutes and renews it at every step it takes while it waits, up to a cap from the declaration; a dead updater then costs minutes, a live one keeps the cordon through a long update. The hook records the cordon where `prepare`, `rollback` and `cleanup` can find it, so a cordon is lifted by whichever step runs last.
- **A cordon is per service, through that service's own route**: the kbf hook calls the frontend's own-node calls (`docs/design/updates.md`, section 5); the js-agent hook calls the lease server's own-host route (4.2). `kbf cluster drain` reaches kbf nodes only.

### 3.7 What the updater reports

The heartbeat carries four facts per service today (`wire.rs:57-100`). The updater adds, per service: the step it is in and the attempt, the commit state, the outcome class, `promotion_pending`, and the duration of each step. They are shown in the coordinator's `/v1/status`. Alerts named by this design: `no_instance_up`, `halt`, `drain_bound` deferrals older than a service's cap, a cordon older than its cap, a step record that is `running` after its deadline, an artifact digest mismatch on resume, a declaration refused at load, an instance read-only for longer than its service says it should be.

### 3.8 Outcomes: Succeeded, Failed, Deferred, halt

The wire has two outcomes (`wire.rs:39-44`). A `Failed` for the desired version stops the coordinator from ordering that host again for it, keeps its slot, and makes the service `blocked` (`decide.rs:95-99`, `converge.rs:1-14`). A busy host that cannot drain within its bound is not a bad release, and must have none of those effects.

**`Deferred`** (a hook exits `75`). The update did not start or was stopped before anything irreversible, for a reason about the host's moment and not the release (`drain_bound`, `rights_change`, `updater_old`, a plan precondition). The hook has undone what it placed. It holds **no slot** and does not make the service `blocked`; the coordinator does not re-order the host until `retry_after` (`defer_backoff = { initial, factor, max }` in the service definition); convergence shows `deferred` hosts as a count.

**Failure classes.** A failed `verify` after `cutover` is classed by the hook: `release` (the new version is wrong) or `environment` (a check that depends on something remote or shared failed, such as the lease server being unreachable). Only `release` failures count toward automatic rollback of the fleet (5.3); an environment outage cannot roll a healthy release back. The shipped default of `rollback_after_failed` is a fraction of the set (and at least two hosts), never one.

**`halt`** (a hook exits `76`, or `rollback` fails): **a check whose failure no version can repair.** The failure means the host, not the release, is in a state that neither the new version, the old one, nor a retry can repair, so acting further can only make it worse. The first instance is a js-agent taint or an `owed-*` record: a tainted agent refuses every start, the old release included, and nothing clears a taint. Other services may declare their own. A `halt` is reported as `Failed` with phase `halt`; it **pages**; it **halts the rollout of that service fleet-wide** (the coordinator orders no further host, the service's convergence state is `halted`, shown in the convergence JSON with the host and the check named); it is **not retried** and **never triggers automatic rollback**; and it is held until an operator clears it. **Only the operator token may clear it** (the same restriction as `urgent`, 5.2), every clear is audited, and setting another desired version does not clear it.

**`promotion_pending`** (set by `verify`): the service is serving but not yet fully its own (for a storage node, the cache promotion of the tierstore hook). The update is `Succeeded`, but the service reports `promotion_pending` in the heartbeat; the coordinator **holds the host's slot** until it clears; the updater keeps running `verify` (idempotent, so a watcher) with its own bound; a ceiling declared by the service turns it into `Failed` with phase `after_commit` where the service says it must, or into an escalated alert.

**Wire and compatibility.** `UpdateResult` keeps its two outcomes. The new information is optional heartbeat fields that old coordinators ignore (`docs/design/updates.md`, section 3): `deferred: { target, reason, retry_after_secs }`, and, on `last_update`, `phase: before_commit | gate_release | gate_environment | after_commit | halt`. **The agent uses them only after the coordinator advertises support.** The reply has a closed shape (`wire.rs:298`, `:324`), so the advertisement follows the existing precedent that a reply for another service is sent only to an agent whose last heartbeat carried `services`: a new agent adds `agent_features: ["deferred-v1", "halt-v1"]` to its heartbeat; a new coordinator answers a heartbeat that carries it with a reply that has `features` naming the ones it supports; an old agent never sends the field and never receives one; an old coordinator ignores it and never echoes it.

| agent | coordinator | what happens |
|---|---|---|
| new | new | the agent reports `deferred` and `halt`, declines the order for its persisted `retry_after`; the coordinator holds no slot and backs off |
| new | old | no `features` comes back: a deferred step is reported as `Failed` as today and the agent never declines an order it could accept; a halt is a plain `Failed` and the agent still does not retry or roll back itself (an old coordinator sees `blocked` for that version, but neither halts the fleet nor pages) |
| old | new | the old agent never sends the field; its failures are `Failed` as today |

The reason for the gate: under an old coordinator a host that declined orders would not cost nothing. An order the host does not act on ends only by ageing at `ORDER_AGE` (120 s, `fleet.rs:132-139`), and the next heartbeat orders it again (`decide.rs:100-110`), so the host would hold its role's slot about 120 s of every 135 s for the whole backoff. The coordinator is rolled first (5.3) and the agent half ships after it (8).

## 4. Hooks

Each service ships its hook with its own binary; the updater contains none of them (3). This section describes the hooks that exist or are planned. **4.1 and 4.2 are written by the service owners and are to be restated as `update-hook` steps**: what each of the five steps does, its exit codes and its declaration, with the tests that run in that service's own CI against the conformance suite (9). A note at the top of each says so.

### 4.1 tierstore: the port-forward hand-over (the tierstore hook)

> Restated as the tierstore hook (rev 4): `tierstore update-hook <step>`. The mechanics in 4.1.1 to 4.1.5 are what the hook's `cutover` and `cleanup` do and are the tierstore owner's; `forward-apply`, `forward-confirm`, `retire-slot`, "the executor" and "a job" mean steps of the hook, which keeps its own state file. The updater's part is the generic step record and its deadline (3.5).

**Hook side.** The contract, code references, failure cases and tests are in [tierstore-handover.md](tierstore-handover.md).
The service registers liveness only: name `tierstore`, its version, health `GET /readyz` on its private port. Exit codes and
the last-line JSON are those of 3.2; every step reads the hook's state file (`/var/lib/tierstore/update/state.json`) and
sets state, so a re-run after a crash converges.

| step | what it does | why a re-run converges |
|---|---|---|
| `prepare <artifact>` | `tierstore --capabilities` of both releases must list `handover-v1` (else `mode=restart`); `75` if the guard, conntrack timeout or table fill fails; start v-next on the idle port, read-only; wait for `/readyz` | idle port derived from the state; link, `systemctl start` and guard table are set-to-state |
| `cutover` | the 4.1.2 swap in one `nft -f`; probe through 9300 until v-next answers; on a failed probe swap back, exit 1 | the file replaces the table and names the port |
| `verify` | the gate: counted probes, the staged version; exit 0 with `promotion_pending` while v-current holds the lock (always, before `cleanup`); after `cleanup`, wait for `read-write` up to `promotion_bound`, `1` at `promotion_ceiling` on a storage node | observation only |
| `rollback` | swap back, stop v-next; valid only before `cleanup` writes `stopping`, after that `1` with `reversible:false` | stop of a stopped unit and a repeated swap are no-ops |
| `cleanup` | **the commit point**: without `verified` it acts as `rollback`; with it, write `stopping`, stop v-current (70 s settle, drain, exit), retire the slot (link to none, runtime mask) | each part is set-to-state |

Declaration: `CAP_NET_ADMIN` for `prepare`, `cutover`, `rollback` only (needs per-step capabilities, else all five hold
it); `AF_NETLINK`; units `tierstore@9301`, `tierstore@9302` from one template (`%i` is the port, `--drain-lb-wait 70s`,
`--proxy-id tierstore-%i`); state directory `/var/lib/tierstore/update/`; lock `<cache>/.lock` or
`/var/lib/tierstore/instance.lock`; tables `kbf_forward_tierstore`, `kbf_guard_tierstore`; timeouts 600, 300, 600, 600, 1300 s.
A boot unit runs `tierstore update-hook restore` to re-apply the rule for the active port.

New in tierstore: wildcard listeners (a repeatable `--listen`, `IPV6_V6ONLY` explicit; `--listen` is one `SocketAddr`
today, `server/cli.rs`); the cache lock, which decides the mode (an instance that finds it held is read-only and takes
it by itself when it frees, so there is no promote verb and a reboot, crash or hung v-current all resolve); the drain
(`Connection: close` from drain start; a lingering close of idle connections at the end of the settle; the race that
remains is counted); clients never retry (a retried create-only put is ambiguous). Storage node: a proxy id per slot, the
reconciler gated on `--lock-file`, a `dir:` hot tier keeps the plain restart. Metrics: the directory gauges double-count
while both run; the build and mode gauges are new.

**Shape [D].** Clients always call the public port, here 9300. No process listens on it. Each tierstore instance listens on its own private port, 9301 or 9302, alternating. One nftables redirect per family sends public-port connections to the current instance. In hook terms (the tierstore owner restates this): `prepare` starts v-next on the idle private port in read-only mode and waits for its `/readyz`; `cutover` swaps the rule (4.1.2) and observes v-next through the public port; `verify` is the gate and reports `promotion_pending` while v-next has not taken the cache lock; `rollback` swaps back and stops v-next; `cleanup` stops v-current (the irreversible step, after `verify`), waits out the settle and retires the old slot. v-next takes the cache by itself when v-current's lock is released (4.1.6).

#### 4.1.1 The ruleset and the listeners (what the cutover step does; the tierstore owner's from now on)

One `inet` table per service for the redirect and one for the guard. The `inet` family handles IPv4 and IPv6 in one rule; NAT in an `inet` chain needs Linux 5.2 or later (nftables wiki, "Performing Network Address Translation (NAT)": stateful NAT in the inet family "since Linux kernel 5.2") **[V on the oldest supported kernel]**. For an older kernel the same rules are generated twice, once for `ip` and once for `ip6`. The families generated are those the hook's own configuration lists (a service that is IPv4 only gets no IPv6 rule).

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
  The alternative, `dnat to 127.0.0.1:P` with a loopback-only listener, does not work for packets from outside unless `net.ipv4.conf.<interface>.route_localnet=1` is set (kernel documentation: "Do not consider loopback addresses as martian source or destination while routing. This enables the use of 127/8 for local routing purposes", default false). That setting makes the host accept packets to 127/8 from the wire for every service on the interface, a known attack surface. **Chosen: redirect plus wildcard listeners plus the guard (Question 6).** The price is that the private port is open on every interface unless the guard is loaded; the guard is therefore loaded by `prepare` and re-applied by the hook's boot-time `restore`.
- **A trap in the guard.** After the redirect the packet reaches the *input* hook with destination port 9301, not 9300. A host firewall that allows 9300 from some networks stops matching: remote callers are dropped by the host's own policy even though the redirect worked. The guard's second rule accepts the redirected flows in the guard table, but an accept ends only its own base chain; another table's drop or default-drop policy is **not** overridden. So the operator's firewall must match `ct original proto-dst 9300` instead of `dport 9300`. This is stated in the service's packaging notes and checked by a test that connects from a veth peer with a restrictive second table loaded (9).
- **The third rule drops** anything else to the private ports, so a remote caller that addresses 9301 directly reaches nothing. Loopback sources are accepted (the first rule), so the hook can probe v-next directly. Any local process can reach the private port directly too; that gives it nothing it lacks on the public port, because the service authenticates every request itself. The guard does not distinguish interfaces beyond `lo`; there is no UDP service on these ports.

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

What clients see during a swap: nothing. The redirect runs only for the first packet of a connection (wiki: "No rule lookup happens for follow up packets in the flow"); a SYN just before the swap is mapped to v-current, one just after to v-next, and neither is dropped or refused by the swap. The swap is the hook's `cutover`: it records its intent in its state file before it applies, probes through the public port to confirm, and swaps back itself if the confirm fails. A crash between the apply and the record is resolved by the updater re-running `cutover`, which is idempotent, or by the updater's dead-man running `rollback` when `verify` has not succeeded by its deadline (3.5). Not claimed: that the swap is free of every race; 4.1.3 names the ones that matter.

#### 4.1.3 Conntrack

A flow's NAT mapping is stored in its conntrack entry when the first packet creates it, and every later packet of the flow uses that entry, whatever the rules now say. So connections established before the swap keep going to v-current, which is what the hand-over needs; connections made after go to v-next.

- **The handshake race and the settle.** A connection whose SYN was mapped to v-current just before the swap may still be completing its handshake, or sit in v-current's accept queue, when the swap happens. v-current must **keep accepting for a settle period after the swap** and only then stop; closing the listener resets what is queued. The existing `--drain-lb-wait` does this (`server/mod.rs:596-615`); its meaning changes from "time for a load balancer to notice" to "time for connections already mapped to me to arrive". It is a config value and never zero here.
- **SYN retransmits beyond the settle, reasoned from conntrack.** A client whose SYN is unanswered retransmits at about 1, 3, 7, 15, 31 and 63 s after its first SYN (Linux default `tcp_syn_retries=6`; the kernel documentation gives 131 s as the total before it gives up, which includes the last wait after the final retransmit, so the last SYN that can arrive is the one at about 63 s). What the swap does to a retransmit depends on whether the first SYN created a conntrack entry. (1) If the first SYN reached the host before the swap, the entry exists with a NAT binding to v-current's port; its retransmits keep that mapping (the rule is not consulted again), whatever the swap did. Such a flow exists only if v-current did not answer (the SYN queue or backlog was full, or the SYN-ACK was lost on the way back). The entry lives in `SYN_SENT` for `nf_conntrack_tcp_timeout_syn_sent` (120 s by default, verified against the kernel documentation). If v-current has by then closed its listener, the retransmit is refused. (2) If the first SYN never reached the host, no entry exists; the retransmit is a new flow and the swap sends it to v-next: harmless. So a SYN aimed at v-current after the swap is harmless only in case (2). The 10 s default of rev 1 does **not** cover case (1) past 7 s. The quantity that matters is the **last retransmit still mapped to v-current**, measured from the swap: a flow whose first SYN arrived just before the swap sends its last mappable SYN about 63 s after the swap. The settle runs from the stop of v-current, which is after the swap and after the gate, so **a settle of about 70 s from the stop covers a default client** with a margin, and costs only update time because v-current serves everything throughout (rev 2 said 130 s, derived from the give-up time; that was the wrong event). The other way to cover it is the executor removing the `SYN_SENT`/`SYN_RECV` entries mapped to the old port at the end of a shorter settle, so retransmits become new flows (a conntrack netlink operation in the hook, more machinery). **Recommended: the 70 s settle** (Question 13). The guarantee assumes the client uses the operating system's default `tcp_syn_retries`; a tuned value voids it, a newer kernel with `tcp_syn_linear_timeouts` shortens the schedule (about 67 s total), and the documentation says so.
- **Idle flows and conntrack timeouts.** The established-TCP timeout is `nf_conntrack_tcp_timeout_established`, default 432000 s (5 days). A keep-alive connection idle longer than its entry's timeout loses its entry. When the client then sends on it, the packet is not a SYN, but with `nf_conntrack_tcp_loose` enabled (the default) conntrack picks it up as a new entry and the NAT hook runs for it: it is mapped by the **current** rule, which after a swap is v-next, and v-next answers with a reset. Before the swap the same pickup would have been mapped to the same port as the socket and worked, so the hand-over adds one failure mode, for one kind of flow. Two bounds close it: the service closes idle keep-alive connections itself after its idle bound (`header_timeout`, 60 s default, `server/mod.rs:346`, `:409`), far below five days; and `prepare` exits `75` if `nf_conntrack_tcp_timeout_established` read from `/proc` is not longer than that bound plus a margin. Both are configuration checks, not timing.
- **Conntrack pressure.** An entry exists per flow, with its NAT binding. A full table (`nf_conntrack_max`) drops new flows, and, **as I understand the kernel (unverified [V])**, early drop evicts unreplied, non-assured entries first: a handshake in `SYN_RECV` can lose its entry, so its final ACK is picked up loosely and mapped by the current rule, to v-next, which has no such connection, while v-current holds a half-open socket. Established, assured flows are evicted last, so draining flows are mostly safe. `prepare` reads `nf_conntrack_count` against `nf_conntrack_max` and exits `75` above a threshold from the hook's configuration. This design adds the failure mode for handshakes in flight at swap time, so "with or without this design" was wrong in rev 1.
- **Other failures to name.** `conntrack -F` drops all mappings: later packets on established flows are picked up as new entries and mapped by the current rule, so a flush during the drain resets the flows still on v-current. The hook does not run `conntrack`.

#### 4.1.4 On-node callers

A process on the host connecting to 9300 goes through the **output** chain (its destination is local, so `fib daddr type local` matches), mapped to 127.0.0.1:P or ::1; a container on a bridge arrives at **prerouting** like a remote caller. A process that connects to the node's own LAN address uses the output chain as well. Loopback must be up in the namespace the rules live in (a test namespace forgets).

#### 4.1.5 Readiness and metrics on private ports

The probes (`/healthz`, `/readyz`) and `/metricz` are served by the same listener as the data path, so they exist on each private port. The hook probes v-next directly on `127.0.0.1:<idle port>` (accepted by the guard's first rule) and probes the public port from the host to see who answers it (the build gauge names the slot). A remote scraper or a load balancer that addresses 9300 sees whichever instance is current and never the other. A scraper that wants both instances scrapes the node's two private ports locally; the instance label is the service's proxy id, which differs between the instances (the unit comments already require "one of its own" per process over the same tiers, `packaging/tierstore/tierstore.service`). Counters reset per process, as they do at any restart.

#### 4.1.6 What the hook needs from the service

The service-side contract is in the deep dive; this is what the hook relies on, with the points that rev 2 changed.

- **Promotion is self-promotion on the cache lock.** There is no promote signal and no promote verb. Every instance tries the exclusive, non-blocking lock on the cache directory at start and on a polled interval; the one that holds it is read-write, the others are read-only and promote themselves when the lock frees, which is when v-current exits. Reasons: the unprivileged updater cannot signal a process of another account, the hook runs with no right over the service's process beyond its units, and a release without a handler for the signal would be killed by it (tierstore handles only SIGTERM and SIGINT, `server/cli.rs:924-926`). It also covers a **reboot**: the one instance that starts takes the lock and is read-write, so the mode is a function of the lock and not of an in-flight update. Requirements on the lock (the owner's section carries the detail): `O_CLOEXEC` and no inherited descriptors (the signer documents the trap, `crates/kbf-signer/src/server.rs:118-134`), `O_NOFOLLOW`, a local filesystem checked at open. `/readyz` still means "serves", and the mode shows in the body and in a `tierstore_cache_mode` gauge; the hook's `verify`, once `cleanup` has stopped v-current, waits for `read-write` before it reports no pending promotion.
- **Where the lock lives.** For a storage node the lock file must be in a directory nothing removes: a `StateDirectory=` (or `/var/lib/...`) shared by both slots, **not** a `RuntimeDirectory=`, which systemd removes when the unit that owns it stops and would unlink the file under v-next, after which a third instance would create a new one and two processes would each hold "the" lock. The holder also re-checks, periodically while holding, that the file it locked is still the file at the path (`fstat` against `stat`), not only once after taking it. A build node's lock in the cache directory is fine. (For the tierstore owner's section.)
- **The wait for read-write after commit is bounded, is a heartbeat state, and is resolved.** Taking the lock runs `clear_stage`, a scan and an index replacement, which can take minutes on a large cache. The hook's `verify` watches `cache_mode` for `promotion_bound` (a declared parameter sized from the cache; a proposed field of the declaration); before `cleanup` it always exits `0` with `promotion_pending`, because v-current still holds the lock. If the bound passes afterwards, the update is `Succeeded` and the service reports the state `promotion_pending` in the heartbeat (3.8). The coordinator **holds the host's slot** while the state lasts, so the next host is not ordered into a fleet that is read-only, and the updater keeps running `verify` after it has reported (a post-success watcher with its own bound), so a late promotion clears the state. A ceiling (`promotion_ceiling`) turns it into `Failed` with phase `after_commit` on a **storage node**, where a v-next that never promotes copies nothing from the hot tier to the cold one and the hot tier fills, and into an escalated alert on a build node. On timeout `verify` names the holder of the lock in `detail`, read from `/proc/locks` (world-readable **[V]**), so the hook needs no root.
- **The first release with the lock** cannot be handed over to or from a release that has no lock (an older v-current runs without it and would race v-next's `.stage/` cleanup). The `requires = ["handover-v1"]` capability (3.3) makes `prepare` record `mode=restart` in that case (a plain restart with a gap).
- **The drain.** `Connection: close` on every answer from the drain's start. At the end of the settle the server closes idle keep-alive connections with a **lingering close**: `shutdown(SHUT_WR)`, keep reading, serve any request that was already on the wire (with `Connection: close`), and close after the peer's EOF or a linger bound. A plain close of an idle connection races a request the client has already sent and the client sees a reset with no response byte; the half-close removes that race for a conforming HTTP/1.1 client. Requests on already-open connections are served until the connection closes, bounded by the drain deadline for a client that never closes. Today's drain answers 503 on connections it still holds from the moment it stops accepting until in-flight work is done (`server/mod.rs:614`, `:748-749`); that would turn every keep-alive client of v-current into a failure after the swap, and the drain is the main service change for this strategy.
- **One client policy.** Clients do **not retry**. The zero-failure claim holds without retries, in this document, in the deep dive and in every test. A retried create-only PUT is ambiguous (a 412 after a retry cannot tell the retry's own earlier success from a competing writer), so the hammering clients never retry and any failure fails the test.

#### 4.1.7 Alternatives considered for the hand-over itself

Two ways to hand over a listening socket without a second port, conntrack or a root `nft` were weighed.

- **Socket activation by the service manager.** The socket is held by the manager and survives the process, so a restart refuses nothing. It was built and parked as a draft pull request for reference (`--listen-from-systemd`, kbf#2091). It covers a restart, not an overlap: the new and the old process cannot both be set up as separate instances over one socket unit, and the drain changes it needs are the same ones this design needs. It also puts the socket in the manager's unit system, one more place to keep in step with the flags.
- **`SO_REUSEPORT` with `tcp_migrate_req`** (kernel 5.14 or later). Two processes bind the same port; the kernel can migrate connections queued on a closing listener to another listener in the group. It has no rule to swap and no conntrack dependence. It is not chosen because the kernel decides how connections are spread between the two listeners while both run, so there is no moment at which "all new connections go to v-next" can be asserted or reverted, there is no equivalent of the confirm before the old process is stopped, and it needs a kernel feature every supported host would have to be checked for. The frontend studied file-descriptor passing against `SO_REUSEPORT` (`docs/design/`, frontend handover) with its own constraints.

The forward was chosen because the swap is one atomic, reversible, observable step with a confirm, at the price of a netfilter dependency (conntrack sizing, firewall coexistence, the `flush ruleset` hazard of 3.5). That is the decision; it is recorded here so it is not reopened without a new fact.

### 4.2 js-agent: lease-drain-restart (the js-agent hook)

> **Restate as the js-agent hook (rev 4).** This section was written for the previous shape. Under the decided shape the js-agent owner restates it as `js-agent update-hook <step>`: `prepare` (the version check of the staged agent, the cordon through the lease server's own-host route, the wait for no lease; a bound that passes exits `75` and lifts the cordon), `cutover` (stop the agent, link, start: the irreversible stop is here, and `rollback` then says `"reversible": false`), `verify` (the checks, classed `release` or `environment`; a taint or `owed-*` record exits `76`, a `halt`), `rollback` (the old release back, a repair), `cleanup` (uncordon). The generic rules it is held to are 3.2 (the contract, the commit point), 3.6 (cordons and drains) and 3.8 (outcomes).


Written by the js-agent owner; aligned with rev 2 of the generic sections (3.1, 3.3, 3.4, 3.6). Facts are from the jobscheduler repository: **[F]** is merged code on its main (a path or PR is given), **[F-open]** is an open pull request (jobscheduler#509 the farm unit, #493 the image pull and the store lock, #516 the `crun` runtime wiring), **[new]** is work this design adds, **[D]** a design choice, **[V]** to verify before building on it.

**The shape [D].** The strategy cordons the host, waits for it to hold no lease, and only then stops the agent, swaps the release, starts it, and waits to see it join the lease server again. **The commit is the stop of the agent** (3.3): from the SIGTERM on, the previous behaviour cannot be given back. Everything before it is `prepare` and can be aborted; a drain bound that passes with leases still running ends the update as `Deferred` (3.9) and the agent is never signalled (below).

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

**Cordon: a prerequisite, not a choice.** The strategy cordons and uncordons through the own-host route on the lease server (`POST /v1/hosts/{id}:cordon`, `:uncordon`, `GET /v1/hosts/{id}`, with the host id the grant file maps from the node id in the updater's certificate, 2.5), as 3.6(c) specifies. Without it a busy host never reaches zero leases, so the strategy cannot run until the route exists; this is migration step 9's `needs` (8). Every cordon the updater places is journaled with the order as owner and is a renewable lease (3.6(b)): `cordon_lease_secs` (minutes), renewed at each step and poll, capped in total by `cordon_max_secs`; `recover` lifts one the journal says is ours. For js-agent the cap must cover the whole update, `drain_wait_secs` (3600) plus `stop_timeout_secs` (3900) plus `active_wait`, `health_bound` and a margin, so it is set above two hours; the lease keeps a dead updater to minutes, the cap only bounds a live one that stalls. The agent's SIGTERM `draining` heartbeat is not a substitute: it is sent only at the stop, which is the commit.

**Mapped onto 3.1 and 3.3.**

| step | reversibility | lease-drain-restart |
|---|---|---|
| plan | | May read (`GET /v1/hosts/{id}`, the staged unit, the executor's `query`); changes nothing. Refuses if the staged release is not an agent of this service, the services file's bounds are inconsistent (below), the staged unit's flags or cgroup layout differ from the file's or the running one's, or the host is tainted. Returns the steps with their bounds. |
| stage | Invisible | Fetched and verified (3.1 step 2); then the executor runs `<new> --version` as the service's user; the line must name the runtimes the services file requires (`crun`) **[new]**. Nothing runs beside the old agent (one agent per host id and scratch root): "checked" means staged, not started. |
| prepare | Visible | Cordon (journaled), then wait up to `drain_wait_secs` for `GET /v1/hosts/{id}` to show no lease. Undo: uncordon. **If the bound passes with a lease still running, `prepare` ends the update as `Deferred`** (`drain_bound`, 3.9): uncordon, report, and no SIGTERM is sent. It holds no slot, does not block convergence, never triggers rollback, and is re-ordered after the backoff in the service definition. `Deferred` is reported only to a coordinator that advertises `deferred-v1` (`agent_features`/`features`, 3.9, agent half in migration step 5c); behind an older coordinator the same drain deadline reports `Failed`. What js-agent does is the same either way: uncordon, no SIGTERM. `retry_after` is persisted by the agent, so a restart keeps the backoff, and an `urgent` order is not re-offered during it. |
| **commit** | Irreversible | "Commit intent" is journaled, then the executor's `trial` (3.4, as a job on the agent's own unit; the self-restart rule of 3.4.1 applies only to the executor's unit and not here) runs: `unit stop` (SIGTERM; the agent's own drain, at most `drain_wait + stop_grace`, inside `TimeoutStopSec`; only after that does `KillMode=mixed` kill what is left), `slot-set` to the new release, `unit start`. The three are idempotent given the executor's record. The stop is the commit; the trial record and its dead-man deadline only guard the swap and the start. |
| checks | | All of: a hello after the start time carrying the staged `agent_version` and `runtime=crun`; the sweep ran clean (no foreign or malformed sidecar, no lease kept: nothing reports the `SweepReport` outside the log today, **[new]**); no taint and no `owed-*` record; the host `ready` in `GET /v1/hosts/{id}` for `bake_polls` consecutive polls. Bounded by `health_bound_secs`. Then the executor's `confirm` ends the dead-man. |
| abort | | Only before commit: uncordon. After commit there is none. |
| restore_previous | | After commit, when the checks fail or the deadline passes: the executor's trial revert puts the old release back and starts it. A repair: runs stopped at the commit are not brought back. The update reports `Failed`. |
| finish | | Uncordon, only if this update placed the cordon. |
| recover | | Below. |

**Classifying the checks (3.9).** Only `gate_release` counts toward rollback.
- `gate_environment`: the lease server is unreachable (no hello or host listing can be read). Reported and alerted; it never rolls a healthy release back.
- `gate_release`: a hello with the wrong `agent_version` or runtime; an unclean sweep.
- *Halt, not a gate class:* a tainted host or an `owed-*` record. The update reports `Failed`, pages a human and halts the rollout; it is neither retried nor rolled back automatically, because nothing clears a taint and a tainted agent refuses every start, the old release included. A human decides the repair and the rollback. **[new]** in 3.9, which today knows only the two classes.

**Why a passed drain bound defers the update and is not followed by the stop.** (1) The agent has its own `--drain-wait`; stopping it after the updater's wait would start a second wait of up to `drain_wait` inside the agent, so the host would be draining for twice the bound the services file states, with the unit's `TimeoutStopSec` as the only real limit. (2) Whatever the agent does not finish is stopped and rerun from the lease server's rerun budget; an update should not spend a user's budget, and a host whose jobs have none left cannot be updated this way at all. (3) A SIGTERM inside a job's owed-checks window taints the next start, and nothing clears a taint; the updater cannot read the host's state directory, so it cannot know the window is closed and must not guess (3.1, 3.6(e)). A deferred update is a refused update, not a killed run and not a bad release: the cordon keeps new leases away, the wait ends in the normal case, and the coordinator re-orders it after `retry_after` (3.9; the backoff is Question 17).

**Recovery after a crash [D].** The executor's record wins (3.5). From the journal phase and `query`:
- *Updater died during the wait in `prepare`:* nothing but the cordon was changed. If the cordon is ours and the drain bound has passed, abort (uncordon, outcome `Deferred`); else resume the wait.
- *Updater died after "commit intent":* if the executor shows no trial, nothing ran: report `Failed` (phase before the commit took effect) and uncordon. If a trial is recorded, go forward from the checks; the unit's state and the slot say which of stop, slot-set and start were done, and each is re-run only if not.
- *Agent crashed mid-drain, or the host rebooted:* systemd restarts it (`Restart=always`). The new process sweeps the previous run's leases (the sidecar ledger), which are lost and rerun, and starts clean. A crash inside the cleanup window leaves an `owed-*` record, so it starts tainted: **the updater stops, reports `Failed` with the taint, and pages; it never clears a taint or an owed record, and never retries** (no clear flow exists **[F]**).
- *Trial deadline passes while stopping:* the window is sized below so that a healthy stop is not reverted.

**Bounds: per-service values of the services file (3.4) [D].** None is a literal in the strategy. Defaults match the unit.

| name | default | meaning |
|---|---|---|
| `drain_wait_secs` | 3600 | the updater's most time to wait in `prepare` for no lease; passing it defers the update |
| `stop_grace_secs` | 120 | the unit's `--stop-grace` |
| `stop_timeout_secs` | 3900 | the unit's `TimeoutStopSec` (3600 + 120 + a 180 s margin); this is the executor's restart timeout for js-agent |
| `active_wait_secs` | to set | most time from `unit start` to a running unit (the unit's `RestartSec` is 30 s) |
| `health_bound_secs` | to set | most time for the checks after start (new work: no default exists) |
| `bake_polls` | to set | consecutive ready observations before `confirm` |
| `cordon_lease_secs` | to set (minutes) | the expiry a cordon is given and renewed to (3.6(b)) |
| `cordon_max_secs` | above `drain_wait + stop_timeout + active_wait + health_bound` plus a margin | total cap on renewals of one update's cordon |
| `confirm_window_secs` | `stop_timeout + active_wait + health_bound` plus a margin | above the 1800 s cap of 3.4, lifted for this service under its load-time invariant |

**The window.** The trial's dead-man window must exceed the longest honest run of the trial: a stop that lasts the full `TimeoutStopSec`, the start, and the checks. Otherwise the executor would revert a healthy drain. After a successful `prepare` the stop is normally seconds; the window is sized for the worst case, a lease that slipped in before the cordon took effect and is finished by the agent's own drain. The plan refuses a services file whose window is below the 3.4 invariant, whose `drain_wait + stop_grace` exceeds `stop_timeout`, or whose values differ from the flags of the staged unit; otherwise systemd would kill a drain the updater is still waiting on.

**New work [new].** What rev 2 covers generically (the strategy interface, the executor's jobs, per-service timeouts, the own-host cordon route, journaled cordons) is not repeated. Left: (1) the runtime in the hello (`contract.rs`), after jobscheduler#516; (2) reporting the sweep result outside the log; (3) the synthetic registration without a health address; (4) the js-agent values above in the services file; (5) the plan's comparison of the staged unit's flags and cgroup layout.

**Open points.** (a) *The keeper:* until it lands, a stop that reaches `TimeoutStopSec` or a cgroup kill is the only way a container leaves; the strategy never kills the agent first, and a kill is the unit's. (b) *Lease hand-off:* none, by design; a job that cannot rerun must not be on a host that is updated, since an agent that dies on its own still reruns it. (c) *Cgroup layout across versions:* `supervisor` and `actions` are names two versions must agree on; a release that changes them cannot be adopted by the sweep of the old one, so plan compares the staged unit's layout with the running one and refuses a change **[new]**.

### 4.3 kbf-daemon: the kbf hook (today's behaviour)

The kbf daemon becomes a hook like the others: `kbf update-hook <step>` does what `agent/update.rs:89-250` does today, split at the five steps, with no change in behaviour. The declaration for kbf has `hook.user = "root"` today (the helper is root, `kbf-host-local.service`), with the rights of 3.4 narrowed as far as the units it restarts allow.

| step | what it does (today's code) |
|---|---|
| `prepare <artifact>` | the host health precondition (`health.refusal`); the node's cordon state before the update (a node already out of placement is never undrained); drain the node at the frontend (cordon, wait until it holds no lease, however long that takes unless `--drain-deadline-secs` is set, in which case a deadline that passes exits `75` with the node left drained and nothing installed over running leases, `agent/update.rs:253`); the cordon mark is kept on disk (`--cordon-dir`) |
| `cutover` | the trial: pending marker with the deadline first, links move, units restart (`local/core.rs`; today's `switch`) |
| `verify` | the node's health through the frontend, or a service's ready URL, bounded by `--health-timeout-secs` (`agent/update.rs:162`) |
| `rollback` | `revert` (the old release back) and the uncordon this update placed (`agent/update.rs:211`, `back_on_a`) |
| `cleanup` | `confirm`; undrain only if healthy and only if this update's drain placed the cordon |

Today the helper reverts by itself at the trial's deadline; in the hook shape that guarantee is the step record's `verify_deadline_secs`, enforced by the root part of the updater (3.5). A frontend that runs as a container and a host on the `kbf` package (`deb`, `rpm`) are variants of the same hook with another backend, as they are today (`local/mod.rs`: `frontend`, `package`, `linux`, `mac_backend`). The updater and its root part are updated as 3.5 says, never inside this hook.

## 5. The generic coordinator

### 5.1 Desired version per service; host sets; who should run a service

The coordinator already stores one desired state per service (`desired.rs:239`) and defines services as data (`services.rs`). Today a host belongs to a service only if its own heartbeat lists it and the service's definition has the host's role (`service.rs:200-207`). That is **self-announced membership**: a host that lost its registration disappears from the service's denominator, and convergence can read `converged` while a host has no copy of the service at all (`converge.rs` counts `at_desired` over hosts that announced). Proposal: the service definition holds a **host-set selector** (certificate roles and labels) and the coordinator decides from it who *should* run the service. A selected host that has not reported the service is `missing`, and `converged` fails while any selected, live host is `missing` or `absent` (2.2). A service that no selected host announces is `in_progress`, not converged.

Four rules make this safe to roll out. **An exit:** an operator can mark a host `excluded` for a service, with a reason, audited; an excluded host does not count, and the convergence JSON shows the counts of `missing`, `excluded`, `unmanaged` and `deferred` hosts, so CI sees why a service is not converged. **No deadlock with old agents:** a host whose agent has not advertised the new heartbeat is `unmanaged` for services that have a selector, not `missing`; it does not count until it is upgraded, and the rollout order is coordinator, then agents, then definitions with selectors (a definition whose selector matches such a host is accepted and logged). **Where labels come from:** labels are kept coordinator-side in a host table keyed by node id, set by the operator or carried by the enrollment record; they are never taken from a heartbeat, so a host cannot select itself into or out of a service. **Node ids across systems:** see 2.4.

### 5.2 One host at a time

Per host, already true: `decide.rs:105` orders nothing to a host that has any service updating or an order outstanding (`fleet.rs:179-189`). This is the only serialisation across services this design keeps (Question 3).

**Slots stay per (service, role).** Rev 1 proposed slots shared across services so at most one host of a role is ever down for any service. That couples unrelated services: a tierstore update on one storage host would block a kbf update on another storage host, and `min_up` means different things for different services (kbf: blobs stay readable; tierstore: cache nodes). Slots remain per service (`decide.rs:100-102`), with an optional **named pool** in a definition for two services that really do share a capacity. Two further rules, because a per-host queue has a head-of-line problem (a kbf node drain waits for leases however long it takes, and a js-agent drain up to an hour, so a security fix for another service on that host waits behind them): each definition has a `priority`, which is the order of the reply loop (today it is kbf first and then the agent's announcement order, `service.rs:219`, an accident), and a desired state may carry `urgent`, which makes a host that is not yet updating take that service's order first; it never interrupts an update in progress and never overrides `after`; a service in its `Deferred` backoff on a host is not re-offered first by `urgent`. Three more rules close the holes this opens. **A named pool by default where capacity is shared:** the shipped definitions of tierstore and kbf on `storage` join one named pool, because tierstore's storage node is the blob data path and a failed tierstore update on one host takes capacity that kbf's own `min_up` does not see; the coordinator tests carry that cross-service case. **`priority` ages:** after a service has been ordered N times on a host without progress, the reply loop skips it for the next lower service for `skip_secs` (an in-memory counter beside the orders), so a flapping high-priority service cannot starve the rest. **`urgent` is restricted:** it can be set only by the operator token or by an allow-list entry with `may_urgent = true`, and every use is audited; a service's own CI token cannot set it.

### 5.3 Canary, health gate, rollback; how health is learned

- **Canary.** None in the coordinator today. A one-host canary can be modelled as a separate service with `min_up = 0`, a workaround (without it a lone host is never ordered, `decide.rs:116-120`). Proposal: a first-class `canary` in the service definition: a count or a host selector, ordered first; the rest wait for it.
- **Health gate.** The coordinator never probes: the agent reports `Health` and `UpdateResult` (`wire.rs:29-52`), and a failed update shows as `last_update.outcome == Failed` for the desired version, which keeps the host's slot and makes the service `blocked` (`converge.rs:1-14`). Proposal: keep that, and add the gate as a rule on the *reports*: after a host (or the canary) updates, it must report `healthy` and the desired version for `bake` consecutive heartbeats (a config count) before the next host or the rest of the set is ordered. The coordinator has no network path to a host's loopback and should not get one; the hook's `verify` runs the real checks and the updater reports the result.
- **Rollback.** Today a human or CI sets an older version. Proposal: the coordinator remembers `last_good` per service (the last version for which the set converged and passed the gate) and, when `Failed` outcomes with a phase of `after_commit` or `gate_release` reach the definition's `rollback_after_failed` (3.8) and its definition says `rollback = "automatic"`, sets desired to `last_good` and marks the failed version `bad` (not ordered again until another version is set). CI can still set a version by hand, which clears the mark. The hosts roll back through the same hooks as any update; a host past its hook's commit point (the success of `cutover`, 3.2) is rolled back by the hook's own `rollback`, or, for the fleet, by an update to the older version through the same hooks. A `Deferred` host, a failure before the commit and a post-commit wait that only says the service is slow (4.1.6) never trigger it.
- **Compatibility and order of rollout.** A new agent to an old coordinator degrades quietly: a `services` list the coordinator cannot read is dropped and the host is treated as kbf-only (`wire.rs:175-196`), so a new service would look unmanaged. The coordinator is rolled first. The agent learns what the coordinator supports from `features` in the reply, sent only to an agent that announced `agent_features` (3.8); it refuses to run non-kbf hooks, and does not use `Deferred`, against a coordinator that has not advertised them.

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
| `wire.rs:29-35` | `Health` is `healthy` or `unhealthy` | `absent` and `stopped` (2.2); a wire change, covered by the compatibility rule above |
| `kbf-types/src/host_role.rs:14`, `:35` | roles are an enum of four | roles as strings. This touches the **certificate**: the role is a URI in the host's certificate (`urn:kbf:host-role:<name>`) and is what the coordinator believes about the host. `git grep` finds `HostRole` in 38 files with 252 references, among them the CA, the enrollment registry and the server's enroll route, the signer's validation and the updater's identity. It is a trust change and its own PR (8) with its own red tests (a certificate with an unknown role string is refused; a role that no definition names is ordered nothing) |

## 6. Where the code lives, and what was considered

**The updater** is generic code in kbf: the crates exist (`kbf-updater`, `kbf-host-install`, `kbf-installer`), their tests pin today's behaviour, and the release pipeline already signs and attests them (`crates/kbf-installer/src/attest.rs:1-17`, a list of accepted signers at `:136-137`). It names no product; a lint checks that its public items and its tests name no service.

**Hooks** live with their services, in their repositories, as subcommands of the service's binary. The updater depends on no service; a service depends on no updater crate except one **dev-dependency**, the conformance suite (9): a crate with no kbf product dependency, which a service uses in its own CI to prove that its hook satisfies the contract. The contract is a document and a golden test in the updater's repository.

**The coordinator** is generic code in kbf (5), with a lint of the same kind.

**Alternatives considered.** The previous shape (rev 3) was a library of per-service strategies compiled into the updater, driving a closed set of root verbs: each verb was generic, each strategy was a Rust implementation of a trait, and a service's cutover logic therefore lived in the updater and shipped with the updater, not with the version it cut over to. Two things were wrong with that: the updater had to know every service and to be released whenever a service's cutover changed; and a service's cutover logic could not be tested with the service. **Hooks were chosen** because the cutover logic then ships with the version that needs it, is tested in that service's CI, and the updater stays generic. The cost is that a hook is code that runs as root with declared rights, which is why the declaration, the verified artifact and the rights of 3.3 and 3.4 are the security core of the design. For the tierstore hand-over itself, socket activation and `SO_REUSEPORT` with `tcp_migrate_req` were weighed and port forwarding is a given (4.1.7).

## 7. The interim per-service deploy workflow

Until the pieces above exist, a service that is not `kbf` is rolled by an interim per-service deploy workflow (kept in kbf, off unless a repository variable is set, in its own concurrency group). What it does now: after a release is published, it sets that release as the desired version of the service with the CI token (an allow-list entry that names only that service, `docs/design/updates.md`, section 5.1), polls the convergence of that service (`GET /v1/convergence?service=S`, `docs/design/updates.md`, section 3), runs a check against the hosts, and on a failed check sets the desired version back to the version recorded before the roll. A stall or a cancelled job is "incomplete", not a failure of the release, because the coordinator goes on rolling by itself once the version is set.

How it maps onto the generic pieces later:

| the workflow does | later done by |
|---|---|
| sets desired, polls convergence | unchanged; the workflow becomes "set desired and read the result" |
| runs a check on the hosts | the hook's `verify` and the coordinator's health gate (5.3) |
| rolls back by setting the recorded version | the coordinator's `last_good` and automatic rollback (5.3) |
| a one-host canary service with `min_up = 0` | the first-class canary (5.3) |
| restart timeout, active wait and confirm window as per-service config of the helper | the same values, in the declaration (2.4) |
| records the attempt | the coordinator's `bad` mark |

Nothing in the workflow is thrown away without its replacement being in place first; it is deleted by the last PR of section 8.

## 8. Migration order

Each step is one PR, independently shippable, tests first with a red proof. Sizes are estimates [I]: S under a week of one engineer, M one to two weeks, L more. None lands during a roll in flight; the coordinator (6a, 6b) is rolled before the agents.

| # | step | size | needs | red proof first |
|---|---|---|---|---|
| 1 | **Characterisation.** A kbf-only golden for `decide`; goldens for the helper's timeouts and window; the agent's `run_inner` call order against the fake ports | S | | each golden fails against an altered constant |
| 2 | **The hook contract and the conformance crate.** The contract document, the exit codes and the JSON line, a crate with a fake service and a harness: run each step twice, crash-resume at every step boundary. No updater yet | M | | a hook whose step is not idempotent fails the suite; a hook that ignores a crash between steps fails it |
| 3 | **Declarations and rights.** The declaration loader and its load rules (root-owned, not group- or world-writable, across-file conflicts, per-service refusal), the transient-unit launcher with the rights of 3.4, the approval of a widened declaration. A root job (`cargo xtask sudo-test`) | M | 2 | a hook that tries to exceed its rights fails (9); a non-root-owned or writable declaration is refused; a widened declaration without approval is `Deferred` |
| 4 | **The step runner.** The persisted step record and its schema, resume, the dead-man on its own thread, the exit codes to outcomes, the verified-artifact check before any run, the registration socket (liveness and version), `absent`/`stopped`, the self-update follow-on with its systemd fallback | M | 2, 3 | kill the updater at every boundary and resume; an artifact altered after verification never runs; a planted updater binary that exits at start is replaced by the previous |
| 5 | **The kbf hook.** Port today's sequence to `kbf update-hook` (4.3), behind the step runner; behaviour tests against the existing fake ports | M | 1, 2, 3, 4 | the order of calls equals today's; a drain deadline exits `75` with the node left drained |
| 6a | **Coordinator generalisation.** `Desired` without kbf's fields, `converge` and `status` per service, `priority`, `urgent`, selector-decided membership with `excluded`, `unmanaged` and `missing`, canary, `bake`, `last_good` and automatic rollback limited to post-commit and release-gate failures, `Deferred` and `halt` with their backoff and clearing, the heartbeat that lists every service | L | 1 | with only `kbf` defined, decisions unchanged; a selected host without the service is not converged; a rolled-back version is not ordered again; a `halt` stops the service and survives a new desired version |
| 6b | **Roles as strings** through CA, enrollment, signer and identity | L | 6a | a certificate with an unknown role string is refused |
| 6c | **Agent half of `Deferred` and `halt`.** `agent_features`, `deferred` and `phase` fields, the persisted `retry_after`, declining orders only on the advertised features. Ships after 6a | S | 4, 6a | an agent that sees no `features` never defers or declines; with it, a busy host holds no slot; a restart keeps the `retry_after` |
| 7 | **tierstore service changes** (the tierstore owner): the cache lock, read-only mode, self-promotion, wildcard listeners, the drain with a lingering close, the settle, the build and mode gauges | M | | the owner's section |
| 8 | **The tierstore hook and the hand-over tests** (the tierstore owner): the nftables mechanics of 4.1.1 to 4.1.5, its declaration, the root-namespace CI probe and tests | M | 3, 4, 7 | the owner's section; the probe of 9 first |
| 9 | **The js-agent hook** (the js-agent owner) | M | 3, 4, the lease server's own-host cordon route | the owner's section |
| 10 | **Retire the interim workflow**, the static `--service` flags and the helper's old verbs | S | 5, 8, 9 | |

The critical path for tierstore is 2, 3, 4, 7, 8. 6a and 6b are independent of it and are the largest part.

## 9. Tests

The rules for all of them: **no test asserts on or depends on elapsed time**; sequences are driven by handshakes (a count of completed requests, an exit status, a start line, an answer, a file that appears), and hang guards live in `.config/nextest.toml`. **Red proofs come first**: each test is written, seen to fail against the wrong behaviour, and only then is the behaviour written.

**The conformance suite.** A crate that a service uses as a **dev-dependency** and that has no kbf product dependency. It runs the service's real hook binary against a fake service and checks the contract: (1) each of the five steps run **twice in a row** leaves the same observable state as once; (2) **crash-resume at every step boundary**: the harness kills the hook (and, in the updater's own tests, the updater) at the start, in the middle and at the end of each step, runs the step again, and checks the final state is the one an uninterrupted run reaches; (3) the exit codes and the last-line JSON are well formed for success, failure, `75` and `76`; (4) `rollback` after `cutover` returns the fake service to its earlier state, or says `"reversible": false`; (5) `cleanup` after `rollback` and after success leaves nothing behind. A hook must pass it in its service's CI; the updater's own repository runs it against the kbf hook.

**Rights.** In a root job, a test hook declared with a narrow set of rights **tries to exceed it, and must fail**: open a file outside its paths, write where the filesystem is read-only, use `CAP_NET_ADMIN` without it (an `nft` call fails), start or stop a unit not in `units`, read an environment variable not listed, open an address family not listed, run past its timeout (it is killed), gain privilege through `setuid` or `NoNewPrivileges` bypass. Each is a separate test with a red proof (the same hook with the rights widened succeeds).

**Trust.** A declaration not owned by root, or group- or world-writable, or in a writable directory, is refused; a hook in an artifact whose digest changed after verification never runs, even when a step is resumed; a declaration that widens rights is `Deferred` until approved; a name mapped to a shared uid, two declarations claiming one unit, a bad file beside a good one (only the bad service is refused).

**The step record.** Kill the updater at every boundary of a full update (before the record, after the record and before the step, mid-step, after the step and before `done`), and check that resume reaches the same end state; a step record from another boot id is treated as running; a record with a newer `schema` is read-only; a `verify` that does not succeed by its deadline runs `rollback` even when the unprivileged part is dead.

**Outcomes.** Exit `75` reports `Deferred`, holds no slot, leaves the service not `blocked`, is not re-ordered before `retry_after`, and never triggers automatic rollback; exit `76` and a failed `rollback` halt the service fleet-wide until cleared with the operator token and survive a new desired version; a `Failed` after commit of class `release` triggers rollback once `rollback_after_failed` is reached, one of class `environment` does not; an old coordinator given the new fields reads them; the pairings of 3.8.

**Coordinator over fake hosts.** The shape of `decide_tests.rs` with several services: per-service slots, per-host serialisation, a named pool, `priority` with ageing, `urgent` restricted to the operator token and not overriding `after`, canary, `bake`, `last_good` rollback, the `bad` mark, a selected host missing the service, `excluded`, `unmanaged`, a coordinator restart mid-roll. The kbf-only golden is unchanged.

**The tierstore hand-over test** and its nftables CI plan belong to the tierstore hook and are the owner's (8): a root network namespace through `cargo xtask sudo-test` (the repository already uses it on hosted Linux runners, `.github/workflows/host-install-linux.yml:47-51`; unprivileged user namespaces are not relied on, `crates/kbf-task/src/network.rs:36-56`, `docs/platform-properties.md:464-465`), a probe that asserts the nft features and the conntrack sysctls and can fail, deterministic scenario tests sequenced by handshakes, a hammering stress test with no retry. The generic updater's contribution to it is only the step record and the rights.

## 10. Questions

Each with a recommendation. The shape (generic updater, hooks in the service's binary, declarations in root-owned files) is decided and is not a question.

For the design:

1. **Serialisation.** Per host only, per host plus slots shared across services, or global. *Recommend per host (`decide.rs:105`, already true) and slots per (service, role), with an opt-in named pool, tierstore and kbf sharing one on `storage`.*
2. **Who owns automatic rollback.** *Recommend the coordinator (`last_good`, a `bad` mark), limited to post-commit and release-gate failures, never `Deferred` or `halt`.*
3. **A declaration that widens rights.** *Recommend it is `Deferred` until an operator approves that exact declaration (`updater approve`, audited); equal or narrower is installed automatically (3.3).*
4. **How a hook controls units.** A polkit rule generated from the declaration, or `user = "root"` with every other restriction. *Recommend polkit first [V], with root-and-restrictions as the approved fallback, which the kbf hook uses today.*
5. **Exit codes and the JSON line.** `0` ok, `1` failure, `75` deferred, `76` halt, and a last-line JSON. *Recommend as written; the alternative of reporting everything in the JSON line makes a crashed hook ambiguous.*
6. **The declaration comes with the release** and is installed by the updater's trusted step before `prepare`. *Recommend yes, with the package manager winning where it owns the file.*
7. **Canary and bake as first-class definition fields.** *Recommend yes.*
8. **Install a declared service that never registered.** *Recommend off by default (`install_when_absent = false`).*
9. **The defer backoff.** *Recommend an initial wait of minutes, a factor of 2 and a cap of an hour for js-agent.*
10. **An operator-set `excluded` state for an ineligible host.** *Recommend yes.*
11. **`halt` and who clears it.** *Recommend yes, cleared only with the operator token, audited.*

For the service owners (stated by them in their hook sections):

12. **One drain or two** (tierstore). *Recommend replacing the 503 drain unless a deployment relies on it.*
13. **Private listeners** (tierstore). *Recommend wildcard plus guard with redirect, not loopback plus `route_localnet`.*
14. **First conversion of a host that listens on 9300** (tierstore). *Recommend a planned restart; a flow not tracked before is reset, one already tracked is not.*
15. **The settle** (tierstore). *Recommend about 70 s from the stop of v-current, from the last SYN retransmit of a default client.*
16. **js-agent bounds with no default.** *Recommend active wait 120 s, health bound 300 s, bake 3 polls, cordon lease 300 s renewed at every step, margin 300 s; the confirm window and the cordon cap follow by rule.*
17. **The js-agent cordon route and a renewable cordon lease on the lease server.** *Recommend own-host cordon, uncordon and GET for the host in the certificate, with an expiry set by the requester and renewed.*

## 11. What is unverified

Everything marked [V] or "unverified" in this document, in one place: that a polkit rule generated from a declaration can limit a hook's account to the listed units (3.4); the oldest supported kernel for NAT in an `inet` chain (a decision, not a fact); that the nft syntax of 4.1.1 loads, that the conntrack sysctls are writable in a root namespace of a hosted runner, and that `nft` exists on the image (the probe decides); that early drop evicts unreplied, non-assured conntrack entries first; that `systemctl mask --runtime` then `stop` on an active templated instance behaves as the tierstore hook assumes; the lease server's cordon rows persisting and not expiring when an agent stops (another repository). Nothing in this document was built or run.
