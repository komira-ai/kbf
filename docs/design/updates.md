# kbf updates: a coordinator and an updater on every host

Status: approved design. The coordinator (section 8, PR 1) is built as the `kbf-coordinator` crate, with the wire types in `kbf-updater` (`wire`). The host agent (PR 2) is built as `kbf-updater agent` and `kbf-host-install --serve` ([../deploy/kbf-updater.md](../deploy/kbf-updater.md#the-agent-mode), [../deploy/host-install.md](../deploy/host-install.md#the-local-socket)), enrollment (PR 3) is built as `kbf-updater enroll` and the agent's renewal ([../deploy/kbf-updater.md](../deploy/kbf-updater.md#enrolling-a-host)), with the proxy's scoped self-drain; the pipeline deploy step (PR 4) is built ([../deploy/kbf-updater.md](../deploy/kbf-updater.md#turn-on-the-deploy-step)); the deletion of the ssh path (PR 5) is **planned**.

Related: [fleet-updates.md](fleet-updates.md) and [fleet-updates-security.md](fleet-updates-security.md) (the earlier push-shaped design), [pull-updates.md](pull-updates.md) and [split-mode-updates.md](split-mode-updates.md) (the pull-shaped design and the order of a rollout), [install-and-enrollment.md](install-and-enrollment.md) (enrollment and the farm CA), [../deploy/kbf-updater.md](../deploy/kbf-updater.md) (the updater as it is today).

## 1. Pieces

- **Coordinator**: its own deployable, not part of the frontends. New crate `kbf-coordinator`; wire types live in `kbf-updater`.
- **kbf-updater**: today's crate, reshaped into an unprivileged per-host agent, configured with the coordinator's DNS name. It owns that host's update.
- **kbf-host-install**: the root helper, reached on a local 0600 unix socket. Release checks reuse `kbf-installer` (`attest`).

## 2. Heartbeat and reply

Updaters dial out; no host listens.

| | content |
|---|---|
| Heartbeat (at start, then every ~15 s) | platform, running version, health, last update result, and `updating` (with target version) while an A/B runs |
| Reply | `nothing`, or `update to X now`, where X is only a version string |

Node id and role come from the certificate (section 5), not the heartbeat. The updater heartbeats through the whole A/B.

## 3. Desired state and how the coordinator decides

CI sets one desired state with one OIDC-authenticated call (`kbf-oidc`): version X and per-role limits, including the minimum number of storage hosts and of frontends that must be up. The coordinator stores only that, in a small file.

On each heartbeat it answers `update to X` only if all hold:

- the host is behind X;
- no update is running on it;
- its role has room, counting `updating` hosts.

Limits and order:

- **Per role**: at most N hosts updating at once. Default 1 for frontends and storage, more for daemons and Macs.
- **Never the last frontend** that is up.
- **Storage**: never so many down that a blob stops being readable.
- **Minimums**: live heartbeats are checked against the stored minimums, never against the hosts currently known, so a silent host after a restart still counts as missing.
- **Order**: storage and daemons, then frontends.
- **Rollback**: set X older.

Host state is rebuilt from heartbeats. No heartbeat for 45 s means down. After a restart the coordinator waits 30 s before ordering any update.

## 4. One host's A/B

1. Fetch B and verify it (section 5).
2. Drain the node (`NodeDrain`, durable cordon).
3. Install B beside A, then switch (a Mac uses the kit layout and a detached launchd apply).
4. Health check (`node_health`).
5. `kbf-host-install` (on a Mac, the detached apply) owns the revert: if B has not confirmed healthy within a timeout, it switches back to A. Rollback does not need B to run. Stay drained if A is not healthy.
6. Undrain. Report the result in the next heartbeat.

## 5. Trust

- **Enrollment**: a host presents a one-time token to the farm CA (`kbf-enroll`, `kbf-ca`) and gets a certificate. The token fixes node id and role, bound to the certificate. The coordinator holds no CA key. The token is made by the operator with the role `updater`, the host's `node_id` and its `host_role`, and has one use; the key is made on the host and never leaves it; the CA signs the `updater` certificate (the host role as a second URI, the node id as the one DNS name, client use only). The updater has its own row in the registry, apart from the node of the same id, so the two enrollments neither supersede nor deny each other, and its renewal is the node's (`Renew`, signed by the key the registry holds) run by the agent at half the certificate's life. A host re-enrolled with a `replace` token denies the old key's serial and key hash and no node id.
- **Self-drain**: a host's updater drains, uncordons and reads the status of **its own node, and nothing else**, with its certificate and no token. It dials the frontend's public proxy (the operator API is on loopback), whose mutual TLS already verifies a client certificate against the farm CA. For a certificate with the role `updater` the proxy admits exactly `POST /v1/nodes/{id}:drain`, `POST /v1/nodes/{id}:uncordon` and `GET /v1/nodes/{id}`, when `{id}` is the certificate's node id (compared as written: no decoding, no query string, no other spelling), puts the operator token in place of whatever the host sent, and refuses every other call with `403`: another node, the cordon, a listing, the update API, the worker and build-cache paths whatever `buck2_auth` is. The host never holds the token. A certificate of any other role gets none of this. A storage host's health is read from routes a certificate cannot reach, so a storage host's agent needs the opt-in token (`--operator-api`, `--token-file`) or is updated by the roll; the token flags stay as that fallback for every role.
- **mTLS**: every heartbeat. The coordinator accepts only enrolled `updater` certificates. The updater accepts only the `coordinator` certificate.
- **OIDC**: only CI's token may set the desired state or read whether the farm has converged (`GET /v1/convergence`); the operator token reads the status and nothing else.
- **Attestation**: each updater checks the GitHub attestation itself: signer `komira-ai/kbf` `artifacts.yml`, hosted runners, full commit, channel ref. The updater resolves X itself from the channel ref of the attested release, validates the version format, and never takes a URL or path from the reply. A bad coordinator can only choose among attested builds.

## 6. Failures

- **Coordinator down**: nothing new starts. Hosts mid-update finish or revert alone.
- **Host silent**: counts as down. When it returns it converges.
- **Update fails**: the host is back on A and reports it. Its slot stays used until it heartbeats healthy, so a bad version stops at the role limit.
- **Coordinator restarts mid-roll**: it waits, rebuilds from heartbeats (`updating` hosts included), then continues.

## 7. Reused and deleted

- **Reused**: `NodeDrain`, `node_health`, durable cordons, `kbf-host-install`, `kbf-installer` attest, `kbf-enroll`, `kbf-oidc`.
- **Deleted in the last PR**: `ssh_installer`, the ssh forced command and its key, `inventory` (the host list is now the heartbeats), `roll.json`/`record`, and the roll API (`server`, `http`, `listener`) with its socket.

## 8. PR split

1. **Coordinator**: `kbf-coordinator`, wire types, decision logic and limits, desired-state call, mTLS, OIDC, image. Fakes. ssh untouched.
2. **Host agent**: heartbeat loop in `kbf-updater`, local A/B with a local `kbf-host-install` socket beside the ssh forced command, systemd and launchd packaging. ssh still works. The certificate comes from files an operator places.
3. **Enrollment**: the one-time token, the farm CA, the certificate for the agent, and the scoped self-drain through the proxy (built).
4. **Pipeline deploy step** (done): after `release.yml` publishes a release, its `deploy` job sets the release as the desired state with CI's OIDC token and waits until the farm converges or a deadline passes (`cargo xtask deploy-farm`). The coordinator gains `GET /v1/convergence`, an OIDC-authenticated read of the per-role counts at the desired version, updating, failed and down, and whether the farm is `converged`, `blocked` or `in_progress`, computed from the heartbeats alone. No change to how hosts update. The step is off until an operator sets the repository variable and deploys the coordinator ([../deploy/kbf-updater.md](../deploy/kbf-updater.md#turn-on-the-deploy-step)). A farm with no progress for a while is the client's to judge (`--stall-min`): the coordinator keeps no time of the last change.
5. **Delete the ssh path**: the items in section 7. Merge only when every host on the maintainers' own host list shows in the coordinator's heartbeats, since `inventory` is gone.
