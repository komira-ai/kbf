# kbf frontend on Kubernetes

Manifests for the kbf frontend and its Envoy on a single-node, on-prem cluster, and the
image the frontend runs from. Nothing here is applied by CI. A cluster write is an
operator's step: render with `kubectl kustomize deploy/k8s`, review, then apply.

## What is here

| file | what it is |
|---|---|
| `kustomization.yaml` | the file list and the two images, each by digest |
| `namespace.yaml` | namespace `kbf`, privileged pod security (host network, host paths) |
| `frontend-deployment.yaml` | the frontend: one replica, host network, host paths, no liveness probe, a 30-day grace period |
| `envoy-deployment.yaml` | Envoy on the host network, reading the `envoy.json` that `kbf-deploy envoy render --topology box` wrote |
| `serviceaccount.yaml`, `role.yaml`, `rolebinding.yaml` | the frontend may `get` and `patch` its own Deployment, `kbf-frontend`, in `kbf`, and nothing else |
| `image/` | the frontend image: `Dockerfile` (distroless, non-root, the `kbf` binary only) |

There is no Service, Ingress, NodePort, PersistentVolumeClaim or Secret. Pods share the
host's network, so clients, nodes and certificates see the host's own name and ports. The
state directory, the handover socket directory and `/etc/kbf` (configuration,
certificates, keys and the API token) are host paths: both frontend pods must see the same
files, a renewal rewrites the certificates in place, and SQLite must be on a local file
system.

## What the host has to provide

The manifests assume (the install script is a separate, held step):

- a node labelled `kbf/frontend-host=true`;
- `/var/lib/kbf-server` (state, on a local file system), `/run/kbf-frontend` (mode 0700)
  and `/etc/kbf` (with `kbf.toml`, which must give the frontend's `storage` and `state-store`: the Deployment's args carry only the mode, `--config`, `--instance` and `--handover-socket`), `/etc/kbf/envoy/envoy.json` and the certificates and keys it names (under `/etc/kbf`; the Envoy pod mounts `/etc/kbf` read-only), all existing and owned by
  uid and gid 65532, the user both pods run as (the handover socket accepts a peer of the
  same uid only). `type: Directory` makes a missing path fail the pod instead of kubelet
  creating it with the wrong owner;
- the image digests: `kustomization.yaml` carries a placeholder registry and an all-zero
  digest for `kbf-frontend`; set both with `kustomize edit set image`.

## The image

`artifacts.yml` job `frontend-image` builds it from the linux-x86_64 `kbf` tarball (checked
against `SHA256SUMS`, the bytes `attest-linux` signs on a push), tests it with
`cargo xtask image-check` (runs as 65532:65532; `kbf --version` runs and names a commit;
`kbf frontend --config /etc/kbf/kbf.toml --check` with no `kbf.toml` exits 2 and says it cannot read `/etc/kbf/kbf.toml`; an unknown-flag usage error also exits 2 and fails the check; `--config` is given because the default path is read only when it exists), and uploads it as the artifact
`frontend-image` (a `docker save` archive). It runs for a pull request with the label
`needs-artifacts`, and on pushes.

Not done, and why:

- **Nothing is pushed to a registry, and the image is not signed.** The workflow lint
  allows `write` only for the signing scopes, so a push needs a decision on where the image
  lives and a lint change. Until then an operator imports the archive into the node's
  container runtime, which names the image by tag, not by digest: the install script then
  sets the tag in a patch, outside this directory. The manifests stay digest-only.
- **No Envoy in the frontend image.** Envoy is its own Deployment, from the upstream
  distroless image at its pinned digest. Check that digest against `KBF_ENVOY_IMAGE` on the
  host before relying on it.

## The rolling shape (not enabled yet)

The frontend update the design wants is a rolling one: a second pod starts, takes the
state and listeners over by the handover (`docs/design/frontend-handover.md`), becomes
ready, and the old pod drains its open builds and exits. `strategy: Recreate` and
`--instance=a` here are the interim shape, because kbf cannot yet do the following:

1. `--instance=auto`: both pods run one command line, so the instance letter must be
   chosen at start. With `--instance=a` in both, the second pod cannot bind.
2. `--after-handover=hold`: today the old process exits 0 after handing over, and a
   Deployment restarts it.
3. SIGTERM waits for Envoy to see the new pod healthy before the old one refuses calls.

When they land, change `strategy` to `RollingUpdate` with `maxSurge: 1`,
`maxUnavailable: 0`, add `minReadySeconds: 10`, and the flags. `k8s_manifests.rs` already
holds that shape to those values. **One more change is needed that the design does not
list:** the readiness and startup probes name `127.0.0.1:8982`, instance a's operator API.
Instance b listens ten ports higher, and both pods share the host's loopback, so while the
new pod runs as b a probe on 8982 would reach the old pod and report the new one ready at
once. The probe must follow the pod's own instance (an `exec` probe, or an
instance-aware flag of kbf); fix it together with `--instance=auto`.

With `Recreate`, an update cuts builds opened between the old pod's SIGTERM (it stops
accepting calls and relays its open ones to their end) and the new pod becoming ready, and
the new pod starts a new epoch, so daemons run their leases again.

## Checks

`crates/kbf-it/tests/k8s_manifests.rs` parses these files (no `kubectl` is needed) and
refuses: a second replica, a grace period under a week, a liveness probe or a `preStop`
hook, a missing readiness or startup probe, a pod network, `runAsUser` that is root or not
`fsGroup`, requests that differ from limits, a state volume that is a claim or a Secret or
not a `Directory` host path, a Role with a wildcard or any verb besides `get` and `patch`
or more than the one Deployment, a cluster-wide role, a service account token in the Envoy
pod, an image by tag or a short digest, a manifest `kustomization.yaml` does not list, a
Secret, and an IP address or tailnet name. Each rule has a test that plants the defect in
the real file and requires the check to name it.
