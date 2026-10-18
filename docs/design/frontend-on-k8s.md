# Frontend on Kubernetes

Status: design to build now. Scope: the kbf frontend only, on-prem, one node. Nodes keep
updating through the frontend's orders to each node's kbf-installer. Nothing here changes
that.

Facts below come from the code at `origin/feature/arch-v2` (25279c9d) and from research.
Pull-updates (#1039) landed after they were collected, so line references may have moved.

What is built: section 3's remote listener and section 4's token checks (PR unit a,
[`kbf_server::oidc`] and [`kbf_server::remote`], `--remote-operator-listen`,
`--remote-operator-config`). Sections 1, 2 and the other PR units are not built yet.
"What unit a does and does not do" at the end says exactly where the built part stops.

## 1. Topology on a single-node host

### Cluster
- The host is in no cluster. Any existing cluster is separate and is not used. The host
  gets a new single-node k3s, installed by a held operator script (PR unit f).
- k3s flags:
  - `--disable traefik` and `--disable servicelb`.
  - Swap must be off, or kubelet `failSwapOn=false`. The held script decides and prints which.
- Budget about 12 GiB RAM for k3s and the kubelet. The host also runs other daemons.
- Namespace `kbf`. The cluster runs only the frontend and the Envoy pod. The k3s default
  `local-path` storage class is available.

### Frontend workload
- One `Deployment`, pinned to the node, with `replicas: 1`. Not a StatefulSet and not
  `Recreate`.
- The rollout is the existing instance a/b handover (`frontend-handover.md`). Kubernetes is
  the supervisor. It does not replace the handover.
- Shape:
  - `hostNetwork: true` and `dnsPolicy: ClusterFirstWithHostNet`.
    - Both pods share the node's loopback. The handover refuses a non-loopback successor
      REAPI address (`outgoing.rs:264`), and the operator API write gate requires a
      loopback peer.
    - Clients, nodes and the certificate name see the same host, same stable ports and
      same name as today.
  - `strategy: RollingUpdate`, `maxSurge: 1`, `maxUnavailable: 0`, `minReadySeconds: 10`
    (at least the Envoy settle, 4 s by default).
  - Every pod runs with the same `runAsUser` and `fsGroup`. The handover socket accepts
    only a peer with the same uid (`outgoing.rs:192-197`).
- Volumes, all `hostPath`, because both pods must see the same directories:
  - State directory holding the SQLite file and its `.lock`. It must be a local
    filesystem, not NFS (`guard.rs:100-115`), and the file must never be copied or
    hard-linked (#835).
  - `/run/kbf-frontend`, mode 0700, holding the handover control socket.
  - `/etc/kbf`, read-only, holding the TLS certificate and key, `ca.pem` and the API
    token. Nothing is created as a Secret.
- Why hostPath for the certificate: the renewal timer rewrites the files in place, so the
  pod sees renewals with no Secret copy to go stale. The frontend still has to reload
  them. That is an existing item, not a new one.
- Instance letter and ports:
  - Instance `a` uses base ports (REAPI 8980, worker 8981, operator API 8982, to be
    confirmed against `docs/api.md`). Instance `b` uses base+10 (`frontend.rs:394-423`).
  - Both pods run the same command line, so the letter must be chosen at start (PR unit d).

### Envoy
- Envoy is kept. A plain Service is L4, so Buck2's long-lived HTTP/2 connection would stay
  pinned to the old pod.
- Envoy runs as a second Deployment, also on hostNetwork. It is configured by
  `kbf-deploy envoy render --topology box`, which routes to the a and b ports, and it has
  no stream idle timeout. This is the same single-box shape as today with the supervisor
  changed.
- No Traefik, no klipper, no Ingress, no NodePort.

### Resources and probes
- Requests equal limits, giving Guaranteed QoS. Size from the current unit's observed use.
  Initial guess is 8 GiB of memory, to be confirmed.
- `readinessProbe`: HTTP `/readyz` on the operator API, `host: 127.0.0.1`.
- `startupProbe`: generous, because a handover can block up to `--handover-quiesce-secs`
  (10 s by default).
- No liveness probe. Killing a busy frontend fails builds.
- No `preStop` hook. kbf drains on SIGTERM itself (`drain_and_stop`), and a hook only
  burns grace period.
- `terminationGracePeriodSeconds` is very large (for example 30 days). Kubernetes has no
  infinity.
- `--shutdown-timeout-secs` stays 0, meaning no deadline. A node reboot still cuts streams,
  as it does under systemd.

### What this design cannot guarantee
- Streams cut by node shutdown or kubelet eviction.
- An old pod that exits 0 after handing over would be restarted by kubelet. PR unit d
  removes this (see section 2).

## 2. Update path on Kubernetes

Order of events for a new image:

1. The trigger (section 3) sets the Deployment image. The Deployment controller creates
   pod 2 with the new image.
2. Pod 2 starts with `--instance=auto`. It binds the free port block and knocks on the
   handover socket.
3. Pod 1 (the holder) releases the SQLite lock, and pod 2 opens the store at the offered
   generation.
4. Pod 2 passes its state-store and map probes and reports serving. It becomes Ready.
5. The controller sends SIGTERM to pod 1. Pod 1 stops scheduling, answers new Execute
   calls on the relay path, and relays its open Execute streams until they close, with no
   timeout. OSS Buck2 never calls WaitExecution, so this is mandatory.
6. Pod 1 stays up until its streams are done, then exits.

The Deployment shape cannot work with today's kbf code. The list below is the set of kbf
code changes needed, each a PR unit:

- **d1. `--instance=auto`.** Try the a block and fall back to b when it is busy. Both pods
  get an identical command line. The letter check in `outgoing.rs:243` stays. This is the
  one blocker.
- **d2. `--after-handover=hold`.** Today `departed` ends serving and the process exits 0
  (`frontend.rs:1076`, `1204`). A Deployment pod then restarts, and the restarted process
  could take the state back. In `hold` mode the process stays up, unready, with its
  listeners serving its open streams. It leaves on SIGTERM or when its last stream closes.
  It never exits 0 while waiting to be deleted.
- **d3. SIGTERM waits out the Envoy settle before refusing.** Today `Drain::begin` makes
  the pod unready and refuses at once (`drain.rs:164-170`). If Envoy has not yet seen the
  new pod healthy, both are unavailable. Buck2 retries UNAVAILABLE at open, so builds
  survive, but "one frontend always ready" breaks. `minReadySeconds` is the stopgap until d3.
- **d4. Docs and preflight.** Update `frontend-handover.md` (the "no GOAWAY" statement is
  false on the SIGTERM path). Add a preflight that checks the hostNetwork and hostPath
  assumptions: same uid, shared socket directory, local filesystem.

Until d1 and d2 land, the manifests (unit c) do not enable the rolling shape. Cold start
bumps the epoch and daemons re-run leases, and that is the fallback.

## 3. Trigger without ssh

### Credential and route
- GitHub Actions requests an OIDC token with `audience` set to a kbf-specific string.
- It sends the token as a bearer to the frontend:
  - `POST /v1/update/desired` sets the desired version for node updates.
  - `POST /v1/update/frontend` is the new route. It carries only a target image digest.
- Remote listener:
  - `api.rs` is deliberately loopback-only, and its `gate()` is unchanged.
  - The new routes are served on a separate, off-by-default listener
    (`--remote-operator-listen`), with its own gate list: OIDC verification, a body
    limit, JSON content type, no `Origin`.
  - That listener serves only these two routes. It never accepts an `admin` write, and a
    request with a strategy field is refused. This is the `rollout` role from
    `fleet-updates-security.md` S9.
- Allow-list, in `kbf.toml` (`[remote_operator]`, named by `--remote-operator-config`), all
  values required when the feature is on: issuer, audience, repository id, allowed `ref`
  or `environment`, and `job_workflow_ref`.

### Frontend self-update
- For its own update, the frontend patches its own Deployment image. It uses in-cluster
  credentials from a ServiceAccount.
- The Role allows `get` and `patch` on `deployments` (apiGroup `apps`) with
  `resourceNames: [kbf-frontend]`, in namespace `kbf` only. It grants no `list`, `watch`
  or `create`, and no access to other resources.
- The image is accepted only as a digest from a configured registry prefix. After the
  patch, the rollout in section 2 runs.
- The self-patch is the trigger, not the mechanism: the handover is what keeps SQLite
  single-writer.

### Reachability
- GitHub-hosted runners must reach the remote listener. This is open decision 1 below.
- A tailnet-federated runner, a narrow public HTTPS endpoint, a self-hosted runner and a
  pull model are the candidates. No hostname goes in this repository.

### Argo CD or Flux
- Not used now. They add a controller to run and update on a one-node cluster for one
  Deployment, and they do not solve the handover.
- They still need a reachable path for a webhook or must poll.
- Their strength is drift correction and auditable git history of desired state. If more
  workloads join the cluster, revisit.

### Kubernetes API-server OIDC
- Native `AuthenticationConfiguration` plus `kubectl` from CI is the alternative.
- It needs no kbf code, but it exposes the cluster API to the internet and makes the API
  server the verifier, so the kbf `rollout` role, `exp` limit and `jti` rules do not apply.
  Rejected for the first version.

## 4. Security

### Token checks
In order, using one mutually exclusive rule set (RFC 8725):

1. `alg` pinned to RS256, never taken from the header. Reject `jku` and `x5u`.
2. `iss` equals `https://token.actions.githubusercontent.com` exactly.
3. `aud` equals the configured kbf audience, accepted for nothing else.
4. `exp`, `nbf`, `iat` valid with 60 s leeway. Reject tokens whose `exp - iat` exceeds a
   configured maximum (default 10 min). Measure a real token first; third-party sources
   say about 5 min.
5. `repository_id` (and `repository`) equals the kbf repository.
6. `job_workflow_ref` equals the deploy workflow at an allowed ref. This is the workflow
   actually running, including a reusable one.
7. `ref` is the allowed branch, or the environment claim names the protected environment.
   An environment with deployment-branch rules is the stronger gate.
8. `event_name` is `push` or `workflow_dispatch`, never `pull_request`.
9. `runner_environment` is `github-hosted`, unless configured otherwise.
10. The `sub` form is not matched by string. Check which form the repository emits (the
    immutable owner-id and repo-id form may apply) and decode one real token before fixing
    the matcher.

### JWKS
- Fetched from the issuer's `jwks_uri` only (discovery document).
- Cached in memory. On an unknown `kid`, refetch once, rate-limited, then reject.
- On fetch failure with a warm cache, keep verifying with cached keys. With a cold cache,
  fail closed.
- Fetching needs HTTPS, and `kbf-server` has no TLS client today. Use `rustls` with the
  ring provider, already in the lockfile. `reqwest` has no TLS feature enabled.
- Verification uses `ring` (`RSA_PKCS1_2048_8192_SHA256`) with `n` and `e` from the JWK.
  No new crypto stack. Add no JWT crate without a licence check against `deny.toml`.

### Clock skew and replay
- Leeway 60 s on `exp`, `nbf`, `iat`. The frontend host must run time sync. A skew beyond
  leeway fails closed and is logged.
- An in-memory `jti` cache is kept until `exp`. It is lost on restart, and the short `exp`
  limit bounds that window.
- The route is idempotent: "deploy digest X" when X is already desired does nothing.
  Replay of an old token cannot downgrade, because the frontend refuses a digest older
  than the current one unless the request carries an explicit, separately authorized flag
  (not in this design).

### What a fork PR can and cannot do
- A fork PR workflow gets no `id-token: write` by default and no secrets. Even if it could
  mint a token, `event_name=pull_request`, the `repository_id` and the `ref` all fail the
  allow-list.
- Branch protection and the environment's deployment-branch rule are the actual control on
  who can run the deploy workflow. These are GitHub settings, and an owner action (held
  script or manual).
- A `pull_request_target` workflow must never get `id-token: write`. A lint in CI checks
  that the deploy workflow is the only one that requests it.

### Kubernetes scope and blast radius
- The Role is the one in section 3. A compromised frontend can change its own image and
  nothing else, but that is code execution on the control path. This is accepted for the
  first version.
- The service account token is mounted only in the frontend pod.
- The operator API write token stays on the host, mode 0600, and loopback-only.
- A mistaken or malicious digest is limited to the configured registry prefix. Image
  signature or attestation verification at the pod (kbf-updater already verifies signed
  binaries) is a follow-up, listed as open decision 5.

## 5. PR units

Each is independently testable and lands behind a default-off switch or in a new directory.

- **a. OIDC verifier and remote route** (`kbf-server`; the design named `kbf-auth` too,
  see below).
  - Verifier module, the separate remote listener, and the config allow-list. Off by default.
  - Tests: a table of claim mutations (wrong `iss`, `aud`, `repository_id`,
    `job_workflow_ref`, `ref`, `event_name`, expired, long `exp - iat`, wrong `alg`
    including `none` and HS256 with the public key, unknown `kid`, replayed `jti`). Each
    must be refused.
  - A `rollout` request with a strategy field is refused, and `admin` routes are not
    served on this listener.
  - Planted mutants: skip `aud`, skip the `jti` check, accept `alg` from the header.
- **b. Container image** (`artifacts.yml`). Built in CI from the attested kbf binary,
  minimal base, non-root. No local builds. Publishes by digest. Test: the image runs
  `kbf --version` and refuses to start without a config.
- **c. `deploy/k8s/` kustomize.** Namespace, Deployment, Envoy Deployment, ServiceAccount
  and Role (get/patch on one Deployment). hostNetwork, hostPath, probes and resources as in
  section 1. Public-safe: no hostnames or IPs, values via kustomize patches that the held
  script supplies. Tests: `kubectl kustomize` output checked in CI by a schema validator
  and a policy test for what must hold (single replica, `maxUnavailable: 0`, no liveness
  probe, Role has no wildcard verbs).
- **d. Handover and SIGTERM changes** (d1 to d4 in section 2). Each has a test that fails
  first. Example for d2: a successor handover completes, the old process stays up serving
  a held stream, and the process does not exit 0 while the stream is open.
- **e. `gamma.yml` variant.** Requests the OIDC token and calls the remote route, with no
  ssh step. Uses the protected environment. `id-token: write` only on this job. Dry-run
  mode prints the decoded claims (not the token) for the allow-list setup.
- **f. Held operator script.** Dry-run default, `--apply`, `bash -n` clean. Installs k3s
  with the flags above (and handles swap), creates the namespace and the PV directories,
  applies the manifests, optionally stops the systemd frontend unit for the first cutover,
  and prints the repository variables and environment rules to set in GitHub. It creates
  no secrets other than what the operator names. It does not change GitHub settings itself.

Order: a, b and c in parallel. d1 and d2 before the rolling shape is enabled. e after a and
b. f last. The first cutover from the systemd unit fails in-flight builds, so schedule it.

## 6. Open decisions

1. **How CI reaches the host.** Options: tailnet workload-identity federation for the
   runner, a narrow public HTTPS endpoint for the OIDC route only, a self-hosted runner, or
   pull. Recommendation: tailnet federation, because it adds no public port and no code on
   the host. It conflicts with the "no push deploy" non-goal in `kbf-cicd.md`, so that
   line needs a one-line amendment for the gamma deployment.
2. **Rolling handover or Recreate.** Recommendation: rolling with hostNetwork (this
   design). Recreate has no handover and cuts builds at the grace period.
3. **Who verifies the token.** kbf (this design) or the Kubernetes API server.
   Recommendation: kbf, since it gives the `jti`, `exp` limit and role separation and does
   not expose the cluster API. Revisit when Postgres and several frontends exist.
4. **Remote listener versus a host-side authenticator in front of the loopback API.** The
   listener widens the surface that `api.rs` keeps loopback-only. A host-side proxy would
   keep `gate()` untouched but adds a new component. Recommendation: the in-process
   listener with its own gate and only two routes, off by default.
5. **Image trust.** Digest-only from a registry prefix now. Recommendation: add attestation
   verification (as `kbf-installer.md` does for node binaries) in a follow-up, before this
   is used outside the dev host.
6. **Swap on the host.** Turn it off or set `failSwapOn=false`. Recommendation:
   `failSwapOn=false` for the first run, so nothing else on the host changes.
7. **Image delivery to the cluster.** A registry the host can pull from, or a tarball
   import. Recommendation: the registry already used by CI, pulled by digest.
8. **Liveness probe.** Recommendation: none. A tolerant one only if a hung-process incident
   shows up.

## What unit a does and does not do

Does (all in `kbf-server`; off unless both `--remote-operator-listen` and
`--remote-operator-config` are given, and only on a frontend with a state store):

- `oidc`: the allow-list (`[remote_operator]`: `issuer`, `audience`, `repository_id`,
  `job_workflow_ref`, and `refs` or `environments` are required; `repository`,
  `event_names`, `runner_environment`, `max_token_lifetime_secs` and
  `frontend_image_repository` are optional), the token checks of section 4 in the order
  there (signature before any claim is read; `jti` last, so a refused token is not spent),
  a JWKS cache (replaced on refetch, one refetch per 30 s for an unknown `kid`, held keys
  kept when a fetch fails), and an HTTPS key source (discovery document, then `jwks_uri`
  on the issuer's own origin, no redirects, 256 KiB cap, 10 s).
- `remote`: `POST /v1/update/desired` (`{"commit"}`, offered to the update rows; the
  commit already desired changes nothing) and `POST /v1/update/frontend`
  (`{"image_digest": "sha256:..."}`). Status codes: 413, 403 (`Origin`), 401 (no or refused
  token; the answer names the check, never the claim value), 503 (issuer's keys
  unavailable and none held), 415, 400 (any other body field, including a strategy), 501.
- An instance b frontend moves the remote listener ten ports above the flag, like the
  other listeners.

Does not:

- `POST /v1/update/frontend` answers 501 until the Deployment self-patch (section 3,
  "Frontend self-update") exists; the downgrade refusal of section 4 belongs to that change.
- Nothing yet runs the node update controller (`update/mod.rs`: "No call from the
  frontend"), so an offered desired release is recorded and nothing acts on it. This unit
  is the door, not the room.
- After a handover the old instance keeps its remote listener until it exits; its store
  handle no longer writes, so an offer answers 503 there.
- The verifier lives in `kbf-server`, not `kbf-auth`: `kbf-auth` is the gRPC policy crate
  and is linked by daemons, which must not gain `ring`-verified JWT parsing or an HTTP client.
- `sub` is not matched; decode one real token before relying on it.
- The `exp - iat` default (10 minutes) is from the design, not measured; run the dry-run
  of unit e on a real token and set `max_token_lifetime_secs`.
