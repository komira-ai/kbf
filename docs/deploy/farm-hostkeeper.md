# The farm's Linux nodes: deployed by hostkeeper

The farm's Linux nodes take kbf from [hostkeeper](https://github.com/komira-ai/hostkeeper), not from
`kbf-updater`. A release of this repository publishes the `kbf` Debian package to the shared apt
repository `https://packages.komira.ai/apt`, first to the suite `testing` and, once it installs on a clean
Ubuntu, to `stable`; then `hostkeeper-desired.yml` sets that version as the desired version of the
hostkeeper service `kbf-storage-and-daemon`, and each node's hostkeeper daemon installs it with apt and
hands the node over with kbf's update hook, `kbf package-hook`.

What this page does not cover:

- **The frontend.** It is deployed as a container (komira-ai/docker-swarm-updater), not by this path.
- **Macs.** No macOS package is published to a channel; a Mac keeps the path it has.
- **MinIO.** The object store is not part of the `kbf` package. It stays as its own installation put it.
- **kbf-updater.** The agent, the root helper and the coordinator stay in this repository, and `rehearse`
  and `rehearse-agent` still rehearse them, but nothing in a release run talks to a coordinator any more.

## The path of a release

1. **The package.** `artifacts.yml` builds, in each Linux job, a second Debian package from the same
   `kbf` binary: the release asset `kbf-native-<commit>-<platform>.deb`, attested by `artifacts.yml` with
   its bundle beside it, like every asset ([artifacts.md](../artifacts.md)). It is the `kbf` package
   ([packaging/linux/README.md](../../packaging/linux/README.md)) without the host agent (no
   `kbf-host-local.service`, no `kbf-updater-agent.service`, no `kbf-updater` account), and its version
   is `<major>.<minor>.<seq>` of the binary's `kbf --version --json` (`0.1.7472`: the workspace version's
   first two numbers and the number of commits of the build), the `X.Y.Z` form the shared repository
   requires, which grows with every commit of `main`. Upgrading a node from the `kbf` package of a
   release asset (`0.1.0+<commit>`) to it removes the host agent's units.
2. **testing.** After `publish`, `release.yml`'s jobs `testing-apt-read`, `testing-apt-index` and
   `testing-apt-write` put the two packages into the suite `testing`
   (`pool/kbf/kbf_<version>_<arch>.deb`), keeping every other product's entries. The signing key and
   the cloud role are never in one job, and every job that takes the files checks them against
   `SHA256SUMS` and each package against its attestation first. A write that finds the index changed
   by another product's release ends with `conflict=true`, and the attempts `-2` and `-3` read, merge,
   sign and write again; the third fails instead.
3. **The install check.** `check-apt-amd64` and `check-apt-arm64` install `kbf=<version>` from
   `testing` in a clean Ubuntu container, pinned by digest, and check that its `kbf` names the
   release's commit and version, that the packaged files are there, that no host-agent unit is, and
   that nothing is enabled or running.
4. **stable.** Only then `stable-apt-read`, `stable-apt-index` and `stable-apt-write` list the same
   pool files in the suite `stable` (nothing is uploaded again), held to the digests of the release's
   `SHA256SUMS`.
5. **The desired version.** `hostkeeper-desired.yml` runs when a release run ends. When the run
   published a release, `cargo xtask release desired` reads that release's `SHA256SUMS` and the
   `stable` index, as a node reads it, and refuses unless the index lists both packages at those
   digests; then hostkeeper's reusable workflow `set-desired.yml` (pinned by commit) sets the version
   of the service `kbf-storage-and-daemon` on the hostkeeper coordinator, with one `apt` entry per
   platform carrying the hook's manifest. It does not wait for the farm: the coordinator's update plan
   rolls the version out host by host. A run by hand (`workflow_dispatch`, from `main`) sets an older
   release the same way, which is how a version is rolled back.

`gamma`, the last job of `release.yml`, no longer sets kbf's update coordinator. It prints a notice
saying so, and, only when `KBF_SETS_DESIRED` is `true`, runs the remote-only Buck2 build of
[gamma-cicd.md](../design/gamma-cicd.md) against the farm.

## The update hook: `kbf package-hook`

hostkeeper runs `/usr/bin/kbf package-hook <step> [<staged release>] --params <file>` as root, from the
copy of the program in the package it staged, with the steps of hostkeeper's hook contract (its
`crates/hostkeeper-hook`; kbf's copy of the contract is [crates/update-hook](../../crates/update-hook)).
The hook's params are `unit` (`kbf-storage-and-daemon.service`), `mode` (`storage-and-daemon`) and
`state_dir` (`/var/lib/kbf-package-hook`, where it keeps its record and its lock).

| step | what it does |
|---|---|
| `prepare` | runs the staged `kbf --mode=<mode> --check` against the node's `/etc/kbf/kbf.toml`: a release that refuses the node's configuration (exit 2) fails here, with nothing changed; then stops the unit. kbf drains on SIGTERM and the unit's `TimeoutStopSec=infinity` lets every lease and write in flight finish, however long that takes: `prepare` is the one step hostkeeper lets run without a bound, and the manifest gives it none |
| (hostkeeper) | `apt-get install kbf=<version>` while the unit is stopped |
| `cutover` | starts the unit |
| `verify` | passes when the unit is active and the installed `kbf` is the version set; while the unit is still starting it defers |
| `rollback` | after hostkeeper has put the previous package back: restarts the unit (a first install: stops it) |
| `cleanup` | removes the record, only after `verify` passed |

Every step can run twice, and works from its record, not from the order it was called in.

## What the owner sets

The jobs do nothing until these exist; a job without its settings ends green with a notice.

| Where | Name | Holds |
|---|---|---|
| environment `apt-publish` (exists) | variables `APT_PUBLISH_ROLE_ARN`, `APT_BUCKET`, `APT_DISTRIBUTION_ID`, `APT_HOSTNAME` | the role the apt read and write jobs assume through OIDC, the bucket, the CDN distribution and the public host. The role must trust `release.yml` of this repository on `main` in this environment and may write `apt/pool/kbf/*` (only when absent) and the shared `apt/dists/*` (with `If-Match`) |
| environment `apt-signing` (to create, `main` only) | variable `APT_GPG_FINGERPRINT`, secrets `APT_GPG_PRIVATE_KEY`, `APT_GPG_PASSPHRASE` | the organisation's archive signing subkey, the one the other products sign the shared index with |
| repository variable | `KBF_SETS_DESIRED` | `true`, exactly, turns on `hostkeeper-desired.yml` and the proof in `gamma`; anything else leaves both off |
| repository variables | `HOSTKEEPER_COORDINATOR_URL`, `HOSTKEEPER_COORDINATOR_CA` | the hostkeeper coordinator's private listener and the CA its certificate chains to |
| repository variables | `HOSTKEEPER_TAILNET_CLIENT_ID`, `HOSTKEEPER_TAILNET_AUDIENCE`, `HOSTKEEPER_TAILNET_COORDINATOR`, `HOSTKEEPER_TAILNET_TAG` | the tailnet's federated identity for this repository, when the coordinator is reachable on the tailnet only (an empty client id: the runner does not join) |

On the hostkeeper coordinator: the service `kbf-storage-and-daemon` placed on the farm's storage nodes,
the registry entry `komira` (suites `testing` and `stable`), and an allow-list entry for this
repository (by id), `refs/heads/main`, the events `workflow_run` and `workflow_dispatch`, the audience
`kbf-hostkeeper-desired`, the service `kbf-storage-and-daemon` and the `job_workflow_ref` of the pinned
`set-desired.yml`. A node that has the `kbf` package from a release asset holds it as a package
installed by hand, which hostkeeper does not replace until it adopts it.

The variables `KBF_COORDINATOR_URL`, `KBF_COORDINATOR_CA`, `KBF_MIN_UP_STORAGE`, `KBF_MIN_UP_FRONTEND` and
`KBF_REQUIRE_COORDINATOR_CURRENT` are no longer read by any workflow.
