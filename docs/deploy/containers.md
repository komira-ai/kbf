# Running the frontend and the coordinator as containers

The frontend (`kbf --mode=frontend`) and the update coordinator (`kbf-coordinator`) come as
container images, for a farm that runs them under an orchestrator (the text below uses k3s) or, for
the coordinator, on one host under Docker ([Run the coordinator on one host](#run-the-coordinator-on-one-host)). There is no registry. Each
image is a release asset, an OCI image layout in one tar:

| image | asset | runs |
|---|---|---|
| frontend | `kbf-frontend-image-<commit>-<platform>.oci.tar` | `kbf --mode=frontend --config=/etc/kbf/kbf.toml` |
| coordinator | `kbf-coordinator-image-<commit>-<platform>.oci.tar` | `kbf-coordinator`, its files at `/etc/kbf-coordinator` |

`<platform>` is `linux-x86_64` or `linux-arm64`. What the files are, how they are built and
checked, and how they are attested is [artifacts.md](../artifacts.md#the-frontend-and-coordinator-images-oci). The
Dockerfiles are in [`packaging/container`](../../packaging/container).

## Verify and import (k3s)

Verify the archive before it goes anywhere, offline, with its bundle
([artifacts.md](../artifacts.md#provenance-bundles-verifying-a-tarball-offline)), exactly as a
tarball is verified:

```text
gh attestation verify kbf-frontend-image-<commit>-linux-x86_64.oci.tar \
  --bundle kbf-frontend-image-<commit>-linux-x86_64.oci.tar.sigstore.json \
  --custom-trusted-root trusted_root.jsonl --repo komira-ai/kbf \
  --signer-workflow komira-ai/kbf/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main --source-digest <commit> --deny-self-hosted-runners
```

Then, on each host that runs the image, as root (this changes the host; it is the operator's
step):

```text
sudo k3s ctr -n k8s.io images import kbf-frontend-image-<commit>-linux-x86_64.oci.tar
sudo k3s ctr -n k8s.io images ls | grep kbf-frontend
```

The image is known by the tag it was built under, `kbf-frontend:<commit>` (the runtime
normalises it; the listing shows the name to give the pod). With no registry there is nothing to
pull from: set the pod's `imagePullPolicy` to `Never` (or `IfNotPresent`), so a host that lacks
the image fails visibly instead of waiting. An update is a new tag: verify and import the next
archive on every host, then change the tag the pod names. The image does not update itself.

## The frontend

The image holds `/usr/bin/kbf` on a distroless base with the system CA roots, and runs as the user id 65532 with
`kbf --mode=frontend --config=/etc/kbf/kbf.toml`. It has no installer, no proxy and no Envoy: put
it behind the orchestrator's own proxy or load balancer. Nothing is baked in. The orchestrator
mounts:

| path in the container | what |
|---|---|
| `/etc/kbf/kbf.toml` | the settings ([kbf-modes.md](kbf-modes.md)); the process exits 2 when the file is missing, and it must not be writable by group or others |
| the files `kbf.toml` names | certificates, keys, the operator token (`api-token-file`); owned by uid 65532, a key or token with no group or other permission bit |
| `/var/lib/kbf-server` | a volume the frontend may write (uid 65532, mode 0750) |

The listeners of `kbf` are on loopback by default, which no other container reaches, so
`kbf.toml` sets them to the addresses the orchestrator publishes. The image exposes the three
ports of the mode: 8980 (REAPI), 8981 (the worker listener) and 8982 (the operator API). An
example `kbf.toml` (names are documentation names; use the farm's own):

```toml
[listener]
listen = "0.0.0.0:8980"
worker-listen = "0.0.0.0:8981"
api-listen = "0.0.0.0:8982"

[tls]
reapi-tls-cert = "/etc/kbf/tls/reapi.pem"
reapi-tls-key = "/etc/kbf/tls/reapi.key"
worker-tls-cert = "/etc/kbf/tls/worker.pem"
worker-tls-key = "/etc/kbf/tls/worker.key"
worker-client-ca = "/etc/kbf/tls/ca.pem"
api-token-file = "/etc/kbf/operator.token"

[storage-client]
storage = "https://storage.example.invalid:8983"

[scheduler]
state-store = "postgres://kbf@db.example.invalid/kbf"
```

`state-store` and `storage` are required: a frontend given none exits 2 naming the flag. The
state database is brought to the schema this `kbf` needs by `kbf boot-check --mode=frontend`
before the start (the units run it; [kbf-units.md](kbf-units.md)). In a container, run it once per
image tag as a job with the entrypoint replaced (`/usr/bin/kbf boot-check --mode=frontend
--installer-config <file>`); it reads the `database` key of that `installer.toml`, and refuses a
URL that carries a password, so the database it migrates is one a password is not needed for.

## The coordinator

The image holds `/usr/bin/kbf-coordinator` on a distroless base with the system CA roots (CI's OIDC
keys are fetched over https), and runs as the user id 65532. Its default command names these files and ports; every
flag may be replaced:

| path in the container | what |
|---|---|
| `/etc/kbf-coordinator/tls.pem`, `tls.key` | the coordinator's certificate and key (role `coordinator`) |
| `/etc/kbf-coordinator/ca.pem` | the farm CA certificate the updaters' certificates must chain to |
| `/etc/kbf-coordinator/oidc.toml` | the allow-list of CI's OIDC token; give it an audience no other verifier accepts |
| `/etc/kbf-coordinator/operator.token` | the operator token, owned by uid 65532, mode 0600 or 0400 (any other mode is refused) |
| `/var/lib/kbf-coordinator` | a volume the coordinator may write, for the desired state (uid 65532, mode 0750) |

Port 8443 is the heartbeat listener (mutual TLS: updaters dial it), 8444 the control listener (CI
sets the desired state; an operator reads the status).

**The coordinator's DNS name is a setting you give, and there is no default for it.** The image
has no name in it, and the coordinator takes none: the name is the one in the certificate the farm
CA issued for the role `coordinator`, and each host's agent is told it, with the port, on its own
command line (`kbf-updater agent --coordinator coordinator.example.invalid:8443`, required; see
[kbf-updater.md](kbf-updater.md#the-agent-mode)). An agent refuses a coordinator whose
certificate is not for that name.

## Run the coordinator on one host

The coordinator is one process with one small state file, so it may run as a single container
under Docker on one machine instead of under k3s. kbf contains nothing about the network it is
reached over: the address the container is published on, and the DNS name in its certificate,
are configuration (the unit below, the certificate, and each agent's `--coordinator`).

1. **Verify and load.** Download `kbf-coordinator-image-<commit>-<platform>.oci.tar`, its
   `.sigstore.json` bundle and `SHA256SUMS`. Check the archive's sha256 against the line of
   `SHA256SUMS`, then verify the attestation offline as in [Verify and import](#verify-and-import-k3s)
   (the same `gh attestation verify` command, with the coordinator's file name). Then, as root:

   ```text
   sha256sum --check --ignore-missing SHA256SUMS
   docker image load -i kbf-coordinator-image-<commit>-linux-x86_64.oci.tar
   docker image ls kbf-coordinator
   ```

   The image is `kbf-coordinator:<commit>`, where `<commit>` is the full 40-character commit
   (`--version` prints only its first 12 digits).
2. **Make the files**, on the host, under the paths the image's default command names
   (`packaging/container/kbf-coordinator/Dockerfile`, `CMD`):

   | host path (mounted at the same path) | what | owner, mode |
   |---|---|---|
   | `/etc/kbf-coordinator/tls.pem`, `tls.key` | the certificate and key (role `coordinator`) | key: uid 65532, 0600 |
   | `/etc/kbf-coordinator/ca.pem` | the farm CA certificate | readable by 65532 |
   | `/etc/kbf-coordinator/oidc.toml` | the allow-list of CI's OIDC token ([kbf-updater.md](kbf-updater.md#turn-on-the-gamma-stage)) | readable by 65532 |
   | `/etc/kbf-coordinator/operator.token` | the operator token, 32 to 4096 visible ASCII characters | uid 65532, 0600 or 0400; any other owner or mode is refused at start |
   | `/var/lib/kbf-coordinator` | the state directory | uid 65532, 0700 |

   `/etc/kbf-coordinator` itself must be searchable by uid 65532 (for example `root:root`, 0755);
   otherwise every file in the bind mount fails to open, whatever its own mode. The state
   directory is mounted read-write and the rest read-only (`:ro`). The coordinator checks no
   mode on the directory: it must let uid 65532 create and rename files in it (the state file is
   written as `desired.json.new`, mode 0600, and renamed over `desired.json`), and 0700 is the
   least that does. The certificate and key are looked at again every 60 s
   (`--tls-reload-secs`), so a renewed pair is picked up without a restart.
3. **Install the unit** [`kbf-coordinator-container.service`](../../packaging/container/kbf-coordinator/kbf-coordinator-container.service)
   as `/etc/systemd/system/kbf-coordinator.service`, replace `<commit>` and the placeholder
   addresses after `--publish`, and `systemctl enable --now kbf-coordinator`. It runs
   `docker run --rm --name kbf-coordinator --user 65532:65532 ... kbf-coordinator:<commit>` with
   no flags after the image, so the image's default command applies. **Both listeners are
   published on addresses you choose**: 8443 is the heartbeat listener (the hosts' updaters
   dial it) and 8444 the control listener (CI and operators). Publish each on the address its
   callers reach, and nothing wider. The unit's `Restart=always` plays the part of Docker's
   `--restart unless-stopped` (the unit uses `--rm`, so systemd alone starts the container); run
   by hand without a unit, use `--restart unless-stopped` and no `--rm`.
4. **Check it:** `docker run --rm kbf-coordinator:<commit> --version` prints the version and the
   first 12 digits of the commit; the status is
   `curl --cacert ca.pem -H "Authorization: Bearer $(cat operator.token)" https://<name>:8444/v1/status`.

**An update** is a new image, a new tag: verify and load the next archive as in step 1, change
`<commit>` in the unit, `systemctl daemon-reload`, `systemctl restart kbf-coordinator`. The
desired state is in the state directory and survives; a restarted coordinator waits 30 s before
it says a farm has converged ([kbf-updater.md](kbf-updater.md#turn-on-the-gamma-stage)). To go
back, name the old tag again.

**`kbf-host-install --frontend-container` cannot drive this.** That backend is the frontend's:
it loads `kbf-frontend` images, drains the container with `/usr/bin/kbf cluster drain`, and waits
for the frontend's `/readyz` and `/healthz` ([host-install.md](host-install.md#a-frontend-container));
the coordinator has none of them. The coordinator is also never ordered by itself (it orders the
updaters). So its update is by hand, as above. This document does not extend the backend.

**Backup and restore.** The whole state is `/var/lib/kbf-coordinator/desired.json`; the
heartbeats are not kept and come back within one heartbeat of each host. The file is replaced
whole by a rename, so a copy taken while the container runs is a complete old or new state:
`install -m 0600 /var/lib/kbf-coordinator/desired.json <backup>`. Keep the certificate, key,
CA, `oidc.toml` and token as you keep any secrets; they are not in the state directory. To
restore, stop the unit, put the file back owned by uid 65532 with mode 0600 in a 0700
directory of 65532's, and start it. A state file that is present and unreadable or invalid makes
the coordinator refuse to start; a missing one is an empty state, which CI's next deploy sets.

## Checking what runs

Run an image with the argument `--version` (the pod's `args`, or the runtime's command): it prints
`kbf <version>+<first 12 digits of the commit>`, and the coordinator's prints the same with its
own name. The release workflow has built each image from the binary it had already checked for the
commit and refused it unless that binary is the one inside, and `beta.yml` loads each push's
x86_64 images, runs them with `--version`, and checks the binary taken out of the image.
