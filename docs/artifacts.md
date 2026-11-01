# Build artifacts: what CI builds, and how a node verifies it

kbf's binaries are built by CI, never on a developer's machine, and a node is to run
only bytes CI built and attested. The workflow is
[`.github/workflows/artifacts.yml`](../.github/workflows/artifacts.yml).

What exists today is the build, the signing, the sums, the attestations (and, for each
tarball, a provenance bundle a host verifies it with offline) on a push to `main`,
and releases of those files ([`.github/workflows/release.yml`](../.github/workflows/release.yml)).
The deployment job that verifies an asset before it reaches a node and the node's own
SHA-256 check are **planned** and not built yet
([mac-node-provisioning.md](design/mac-node-provisioning.md) sections 4.1 and 4.2).
Sections below that describe them say so.

A pull request builds none of this unless it carries the label `needs-artifacts`; adding
the label to an open pull request starts the build. Pushes and the merge queue always
build.

## The seed path: plain cargo on GitHub-hosted runners

The build jobs of `artifacts.yml` (`darwin-arm64`, `linux-x86_64` and `linux-arm64`) build every
release binary with `cargo build --locked --release` on GitHub-hosted runners, and what they build is
what is signed, attested and released. This seed path needs no kbf, no farm and no Buck2, and it must
stay. A build of kbf by kbf may run beside it, never instead of it, because the seed path is how a
broken kbf is rebuilt without kbf. A repository lint
(`crates/kbf-integration-test/tests/seed_build`) holds the jobs to plain cargo on hosted runners, and
holds signing, the sums, the pipeline and the release to the files they build.

## Pull request labels

A pull request runs `ci.yml` (the checks `lint` and `test`, which gates the coverage of the fast set
run in the `test-shard` matrix beside it, and `task-macos`, the task runner's tests on a hosted Mac) and nothing else; the full suite, the macOS jobs and the packages run on a push to `main`
(`beta.yml`). One label opts a pull request in to more, to spare the small
macOS runner pool. Adding it to an open pull request starts a run.

| Label | Effect |
|---|---|
| `needs-artifacts` | `artifacts.yml` builds, packages and sums the files for the pull request (without the label a pull request builds nothing; nothing is attested from a pull request, and the push to `main` builds and attests every file) |

`artifacts.yml` runs on the `labeled` event so that the label starts something. Any label
added to a pull request therefore starts a whole `artifacts` run, which replaces the pull
request's earlier run: add labels together, before the run you want to keep. The `native-macos` job (which also runs
the package tests) runs in `beta.yml` on every push and never on a pull request; that is pinned by `crates/kbf-integration-test/tests/release_artifacts`.

## What is built

23 tarballs, each with its SBOM and (once the push is attested) its provenance bundle
([below](#provenance-bundles-verifying-a-tarball-offline)), two Debian packages of `kbf`
([below](#the-kbf-package-deb)), one frontend, one update coordinator and one task runner container image per
Linux architecture ([below](#the-frontend-and-coordinator-images-oci)) and one macOS package of `kbf-installer`
([below](#the-kbf-installer-package-pkg-macos)), built by one job per platform:

| Asset | Runner | Target |
|---|---|---|
| `kbf-<commit>-darwin-arm64.tar.gz`, `kbf-deploy-<commit>-darwin-arm64.tar.gz`, `kbf-installer-<commit>-darwin-arm64.tar.gz`, `kbf-host-install-<commit>-darwin-arm64.tar.gz`, `kbf-updater-<commit>-darwin-arm64.tar.gz`, `kbf-mac-provision-<commit>-darwin-arm64.tar.gz`, `kbf-task-bundle-<commit>-darwin-arm64.tar.gz` | `macos-15` (Apple silicon) | `aarch64-apple-darwin`, `MACOSX_DEPLOYMENT_TARGET=14.0` |
| `kbf-<commit>-linux-x86_64.tar.gz`, `kbf-daemon-<commit>-linux-x86_64.tar.gz`, `kbf-server-<commit>-linux-x86_64.tar.gz`, `kbf-deploy-<commit>-linux-x86_64.tar.gz`, `kbf-host-install-<commit>-linux-x86_64.tar.gz`, `kbf-updater-<commit>-linux-x86_64.tar.gz`, `kbf-signer-<commit>-linux-x86_64.tar.gz`, `kbf-task-<commit>-linux-x86_64.tar.gz` | `ubuntu-24.04` | `x86_64-unknown-linux-gnu`; `kbf-task` is built for `x86_64-unknown-linux-musl` |
| `kbf-<commit>-linux-arm64.tar.gz`, `kbf-daemon-<commit>-linux-arm64.tar.gz`, `kbf-server-<commit>-linux-arm64.tar.gz`, `kbf-deploy-<commit>-linux-arm64.tar.gz`, `kbf-host-install-<commit>-linux-arm64.tar.gz`, `kbf-updater-<commit>-linux-arm64.tar.gz`, `kbf-signer-<commit>-linux-arm64.tar.gz`, `kbf-task-<commit>-linux-arm64.tar.gz` | `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu`; `kbf-task` is built for `aarch64-unknown-linux-musl` |

`kbf` is the one binary with all five modes (`kbf --mode=daemon`, `kbf --mode=all-in-one`,
`kbf --mode=storage`, and the two modes not built yet; [cluster.md](design/cluster.md) sections 1
and 5.1), the one a host runs. `kbf-daemon` and `kbf-server` are the separate daemon
and server binaries (Linux only: a Mac runs `kbf --mode=daemon`, and darwin has no
`kbf-daemon` tarball; the ones of earlier commits stay where they were published).
`kbf-deploy` is the deploy tool (its kits are listed in
[ARCHITECTURE.md](../ARCHITECTURE.md)), shipped so an operator installs the tool the
same way as the binaries it installs: by digest, from these attested files, not built
on the operator's machine. `kbf-installer` is the root tool that sets a Mac up and
updates it ([kbf-installer.md](design/kbf-installer.md)); it is built, signed, attested
and released like the others, so a package or an operator takes it by digest from these
files, for macOS only. It is not a component of the release it installs on a host, and it does
not update itself (the design's section 1). On Linux a release has no `kbf-installer` tarball
and no `kbf-installer` package: what a Linux host needs of it that is still used (the users,
the directories, the slice, and the pinned `gh` and trust list that verify attestations) is in
[the `kbf` package](#the-kbf-package-deb), and the hosts are updated by `kbf-host-install`.

`kbf-host-install` is the root side of the host-driven update: the forced command of the
key `kbf-updater` holds, which installs a release on the host it runs on
([deploy/host-install.md](deploy/host-install.md)). `kbf-updater` is the daemon that
drives it over ssh from the operator's Linux box, and, as `kbf-updater agent`, the host agent a
Mac runs, so it has Linux and macOS tarballs. Neither
is a package, and neither is a component of a release: a host or an operator takes the
tarball by digest, verifies it as every tarball is (below), and places the binary where
the guide says. Each tarball holds one member, the binary under its own name (the macOS `kbf-task` bundle's holds a layout, [below](#the-macos-kbf-task-bundle)). The macOS
`kbf-host-install`, `kbf-updater` and `kbf-mac-provision` are signed and checked like the other macOS binaries. `kbf-host-install
--version` prints its name and package version, which is all the release asks of it.
`kbf-mac-provision` is the tool that provisions a Mac node (its host settings, worker profile and the `_kbf-updater` account); it has a darwin-arm64 tarball only.

`kbf-signer` is the process that holds the CA key and serves a Unix socket to the frontend
(`kbf --mode=frontend --ca-signer`; [the design](design/install-and-enrollment.md), section 6.1). It has Linux
tarballs only, no package and no container image; it is taken by digest, verified, and placed like the
two tools above. Its tarball holds one member, the binary under its own name.

`kbf-task` is the runner of one build action inside a job (`crates/kbf-task`). Its Linux tarballs hold
one member, the binary under its own name, built for the musl target of the architecture, so it is
statically linked: it needs no libc of the image it runs in. A host does not mount it into any image: the
kbf image carries it as its `ENTRYPOINT`, and the frontend's js-agent executor runs the kbf image (by digest,
built from the served base) and gives the tool its flags only (`kbf-task` takes every path as a flag). It is built and
tested on a native runner of each architecture, which installs the target with `rustup target add` and
`musl-tools` (ring compiles C for the target with `musl-gcc`); the job runs `build-commit check` on the
binary, which runs it, and `cargo xtask static-elf`, which refuses it unless it has no program
interpreter and no needed library. The task image
carries the same file. It writes its pack with `kbf-task-pack` (`crates/kbf-task-pack`), the task pack types
the frontend ingests with, a crate of pure Rust that does not link the pack index or the object store client.

A Mac gets `kbf-task` only as [the bundle below](#the-macos-kbf-task-bundle); there is no bare `kbf-task` tarball, binary asset or image for a Mac. `kbf-task` has a macOS confine: it runs the Command alone under `/usr/bin/sandbox-exec`, with a profile (`crates/kbf-seatbelt`, also used by the Mac daemon's driver) that denies every network operation but to loopback and Unix sockets for a Command denied the network, and denies reading, writing, renaming and linking each file holding a credential or presigned URL it was given (the grant token file, the pack URL file, the PUT URLs file), and renaming the directories above it, for any Command. It does not limit other writes, nor `mach-lookup` of helper daemons, does not stop DNS (any Unix socket is allowed, and the system resolver is reached through one), and a connect to the node's own non-loopback address was allowed under the denial on the macOS runner. A system that is neither Linux nor macOS has no confine, and `kbf-task` refuses a Command denied the network there (exit 15). The `task-macos` job of `ci.yml` runs `cargo test -p kbf-task` on a hosted Mac, without the test-only feature. A repository lint (`crates/kbf-integration-test/tests/no_darwin_task.rs`) allows exactly the bundle and refuses: a bare `kbf-task` asset, package line or image for a Mac (any name but `kbf-task-bundle-<commit>-darwin-arm64`), a `cargo build`, `b` or `install` for a Mac target that would produce `kbf-task`, a Mac runner job that names it other than in a `cargo test`, `check` or `clippy` line, or builds the whole workspace without `--exclude kbf-task`, in any job but the bundle job (`darwin-arm64` of `artifacts.yml`), the bundle made (`cargo xtask package-task-bundle`) by any other job, and any workflow that names the test-only feature `unconfined-for-tests`; a `cargo check`, `clippy` or `test` of it for a Mac target, which ships nothing, is allowed. The backstop is the pin of the release set (the asset table here and the workflow must agree: `crates/kbf-integration-test/tests/release_artifacts`); the lint's text match of workflow jobs is a tripwire.

`tierstore` (the hot and cold S3 object store proxy) is no longer built or released by this repository: its source, its
Linux tarballs and its packaging (the example unit and the flags to fill in) are in
[komira-ai/tierstore](https://github.com/komira-ai/tierstore). The host updater does not update it: the farm places it,
taking the tarball from that repository's release and checking it against that repository's release signer.

The packages are the two Linux jobs' debs (one each) and container images (three each, and the facts and the SBOM of the task image), and the macOS job's `.pkg`:

| Asset | Built by | Holds |
|---|---|---|
| `kbf-<commit>-linux-x86_64.deb` | the `linux-x86_64` job (`ubuntu-24.04`, nfpm for amd64) | the `kbf` of the tarball of the same name at `/usr/bin/kbf`, the unit of every mode, `kbf.slice`, the users and directories they run with, and the trust directory (the pinned `gh` for amd64, the Sigstore trusted root, `installer.conf`); it replaces the old `kbf-installer` |
| `kbf-<commit>-linux-arm64.deb` | the `linux-arm64` job (`ubuntu-24.04-arm`, nfpm for arm64) | the same for arm64 |
| `kbf-frontend-image-<commit>-linux-x86_64.oci.tar` | the `linux-x86_64` job (`ubuntu-24.04`, BuildKit) | an OCI image of the `kbf` of the tarball of the same name, which runs `kbf --mode=frontend` as a user that is not root |
| `kbf-frontend-image-<commit>-linux-arm64.oci.tar` | the `linux-arm64` job | the same for arm64 |
| `kbf-coordinator-image-<commit>-linux-x86_64.oci.tar` | the `linux-x86_64` job (`ubuntu-24.04`, BuildKit) | an OCI image of the `kbf-coordinator` the job built, which runs it as a user that is not root |
| `kbf-coordinator-image-<commit>-linux-arm64.oci.tar` | the `linux-arm64` job | the same for arm64 |
| `kbf-task-image-<commit>-linux-x86_64.oci.tar` | the `linux-x86_64` job (`ubuntu-24.04`, BuildKit) | an OCI image of the `kbf-task` the job built as one layer on the pinned Ubuntu 24.04 base of the farm's Linux actions, which runs it as a user that is not root |
| `kbf-task-image-<commit>-linux-arm64.oci.tar` | the `linux-arm64` job | the same for arm64, on the arm64 manifest of the same base |
| `kbf-task-image-<commit>-linux-x86_64.facts.json` | the `linux-x86_64` job (`pkg-image-asset`) | the digest of the task image's manifest, of its config and of its layers, and the digest of the base manifest it is built on |
| `kbf-task-image-<commit>-linux-arm64.facts.json` | the `linux-arm64` job | the same for arm64 |
| `kbf-task-image-<commit>-linux-x86_64.cdx.json` | the `linux-x86_64` job (`pkg-image-sbom`, syft) | the CycloneDX SBOM of the task image, read from its layers: the packages of the Ubuntu base and the `kbf-task`; the SBOM of the `kbf-task` tarball describes the binary alone |
| `kbf-task-image-<commit>-linux-arm64.cdx.json` | the `linux-arm64` job | the same for arm64 |
| `kbf-installer-<commit>-darwin-arm64.pkg` | the `darwin-arm64` job (`macos-15`, `cargo xtask pkg build`) | the `kbf-installer` of the tarball of the same name, its LaunchDaemon property list, the pinned darwin `gh` and a trust directory |

The set above is pinned: `crates/kbf-integration-test/tests/release_artifacts` fails a pull request
that drops one of these files from the workflow, packages one that is not listed, or
leaves this table describing another set.

There is no Linux `kbf-installer` deb, rpm or tarball in a release: [the `kbf` package](#the-kbf-package-deb)
carries what a Linux host still used of them, and
[upgrading a host that has `kbf-installer`](#upgrading-a-host-that-has-kbf-installer) says what
happens to the files of the old package.

Every kit that changes a host changes a Linux one (systemd units, the MDM services, a
systemd user service, or a cluster through `kubectl`), and `kbf-deploy` for macOS is
there for the verbs a Mac runs on itself. The macOS job builds it, signs it and runs
it as it does every macOS binary (below), but no CI job runs `kbf-deploy`'s tests on
macOS: the bytes are known to start, and nothing more is claimed for them until a job
tests the crate there.

A running build is never cancelled by a newer push, so a recent attested release always
finishes even under steady merges. Every commit on `main` keeps a run of its own.

`<commit>` is the full SHA of the commit built (`GITHUB_SHA`: on `main`, the pushed
commit; on a pull request, the test merge commit). Each tarball holds one release binary (`cargo build
--locked --release`), mode 0755, owned by 0:0, and nothing else. `cargo xtask package`
writes it the same way on every runner: one ustar entry with no owner names, time 0
(the epoch) and no extended attributes, in a gzip stream with no file name or time,
so one binary always packages to the same bytes. It then reads the tarball back and
refuses it unless it holds exactly that entry. Next to each tarball
is its CycloneDX SBOM, `<same stem>.cdx.json`, made from `Cargo.lock` for that binary
and target by `cargo-cyclonedx` (pinned by version and SHA-256 in the workflow, and
installed by `cargo xtask fetch`). One `SHA256SUMS` covers every tarball and SBOM of the commit: each job writes
`SHA256SUMS.<platform>`, and the `sums` job merges them, sorted by name (`cargo xtask
sums assemble`), and checks every file against it (`cargo xtask verify-assets sums`:
every file listed once, each matching, and a copy of the first tarball with one byte
flipped not matching).

The `kbf` tarball of each platform has a third file beside it, `kbf-<commit>-<platform>.version.json`:
the exact bytes `kbf --version --json` printed when the job that built the `kbf` ran it
(below). A node's offer and the age check of a release need what the `kbf` of each platform
says of itself (`meta_log`, the protocols, `seq`), nothing on the frontend records it, and
only a runner of the platform can run its binary: a Linux runner cannot run the macOS
`kbf`. Each build job therefore runs `cargo xtask release version-facts --binary
target/<target>/release/kbf --commit "$GITHUB_SHA" --platform <platform> --out dist` after
the `kbf` is built (on macOS, signed) and its identity is checked, and before it is
packaged. The verb runs `kbf --version --json` as the installer runs a staged binary: by
absolute path, with an empty environment (the runner's tokens are not passed to it), no
standard input and a limit of 10 seconds, and reads what it printed with the installer's
own strict reader (`kbf_installer::binary::Version`: at most 4096 bytes and exactly the
fields below). It refuses, and writes nothing, unless the binary exited 0 and says it is
the `release` build of `$GITHUB_SHA`; the file is those bytes unchanged, never a document
read and written again. The file is listed in the job's `SHA256SUMS.<platform>` like a
tarball, so the one `SHA256SUMS`, the build provenance, the verification and the release
carry it. It has no SBOM and no bundle of its own. The set is closed over it:
`verify-assets sums` (so `release check`, `release publish` and `release offer`) refuses a
listed `kbf` tarball whose `kbf-<commit>-<platform>.version.json` is not listed, and a
listed facts file whose `kbf` tarball is not; `release check` takes that name and no other
`.version.json`, and `release offer` calls a platform incomplete without its facts, whatever
other tarballs it lists. No flag waives the rule: `release check` holds the previous release to it as it
holds the release being made, and the same for the task image, which a release lists with its facts and
its SBOM (the rules of [the images](#the-frontend-and-coordinator-images-oci) below). The previous release
is v0.0.41 or later, which lists them; `release.yml` passes no waiver (the lint holds that). The workflow lint
(`crates/kbf-integration-test/tests/release_artifacts`) names a build job that does not run
the verb once, for its own platform, between the identity check of `kbf` and its package,
for the same `--binary` path the job packages.

The build jobs check out the full history (`fetch-depth: 0`, which the workflow lint holds
for every job that records the facts), so the facts carry the real `seq` (see `seq` below);
a shallow local build still says `seq: 0`. The bundle `kbf-installer deploy --bundle` reads
holds `kbf` and `kbf-daemon` tarballs and their attestations and refuses any other entry: the
facts file is not packed into a bundle, and a packer that adds it needs a reader that accepts
it installed first (the deploy order of `seq`, below).

`kbf-server`, `kbf` and (on macOS) `kbf-installer` embed the first 12 characters of the commit they
were built from (`kbf` through the server library it links), and `--version` prints
`<binary> <package version>+<commit>` (so do the server's start line and the `server`
field of `GET /v1/nodes`, [api.md](api.md#get-v1nodes)). Before packaging, each Linux job
checks that `kbf-server`'s `--version` names `GITHUB_SHA` (`cargo xtask build-commit
check`), and that `kbf`'s does too and that `kbf --version --json` (below) names the same
commit and a release build (`build-commit check --json`); the macOS job makes the second check
on the signed `kbf` and `kbf-installer`, the bytes it packages. Those checks run on the binary
before it is packaged. After packaging, every job opens the `kbf` tarball (and the macOS job
the `kbf-installer` tarball) it wrote and makes the second check again on the binary inside each, with `cargo
xtask build-commit check-asset <tarball> "$GITHUB_SHA"`: the binary's name and the
commit are read from the tarball's file name (`<name>-<commit>-<platform>.tar.gz`, and
the commit must be `GITHUB_SHA`), the tarball must be the one entry `package` writes, and
the binary extracted from it (to a directory under `target/` that the check removes) must
pass `check --json`. So the bytes that ship, not only the bytes that were built, name
the commit. `KBF_BUILD_COMMIT_OVERRIDE` replaces the commit in a `kbf-server` build (and
so in `kbf`), for CI tests that need two stamps from one checkout; a build that set it
here would fail that check. `kbf-installer` takes its commit from the checkout only.

The Linux binaries link glibc dynamically and need the glibc of Ubuntu 24.04 or
newer.

## What `kbf --version --json` says

A host checks what it runs with one command, `kbf --version --json` (exactly those two
words, in either order): one JSON object on one line of stdout, exit 0, the fields
sorted by name.

```text
{"admin_protocol":1,"commit":"0123456789ab","commit_full":"0123456789abcdef0123456789abcdef01234567","meta_log":4,"min_protocol":1,"profile":"release","protocol":1,"relay":1,"seq":812,"state_schema":1,"version":"0.1.0"}
```

- `admin_protocol`: the version of the admin calls that `kbf --mode frontend` served
  (`GET role`, `POST quiesce`, `resume`, `takeover`, `stand-down`: `docs/api.md`, "The admin
  calls (removed)"). A frontend no longer serves them; the field stays 1 for the readers of
  the version line.
- `commit`: the first 12 hex digits of the commit built (`unknown` for a build with no
  git checkout); compare it with the first 12 of the commit in the asset's name.
- `commit_full`: the 40 hex digits of the commit built (`kbf_server::BUILD_COMMIT_FULL`);
  `commit` is its first 12. It is absent, not empty, from a build that did not know them (no
  git checkout, or a `KBF_BUILD_COMMIT_OVERRIDE` stamp), and a build before this field prints
  none. `kbf_types::Version` reads it as optional.
- `meta_log`: the highest metadata-log machine version the binary runs
  (`kbf_server::raft_log::RUNS`). It is informational and frozen at 5: this build raises
  no log, so it gates no feature. Going from release N to N-1 is reversible only if
  `meta_log(N) <= meta_log(N-1)`; a build that prints no `meta_log` is unknown, and
  unknown is not reversible.
- `profile`: `release` for every asset here (the workflow refuses a `debug` one).
- `protocol` and `min_protocol`: the newest and the oldest `kbf.worker.v1` version the
  binary speaks. The daemon half speaks exactly `protocol`; the server half accepts
  `min_protocol` through `protocol`.
- `relay`: how far the build carries what its host's installer reports to the frontend. At
  `1` it reads `available` and `release_source` from the installer's `status.json` and sends
  them in its `UpdateStatus`. `kbf-installer rollback` on a host whose
  `release_source` is `bundled` refuses a release whose `kbf` declares less than the level the
  installer needs (`kbf_installer::binary::RELAY_NEEDED`); a build before this field prints none. An
  installer whose reader refuses keys it does not know refuses to stage a `kbf` that prints
  it, so the installers ship first.
- `seq`: the number of commits behind the commit built (`git rev-list --count HEAD`), so
  two builds of one branch are put in order by it: a later commit has the larger number.
  It is `0` when the build could not count them (a shallow checkout, which holds only part
  of the history, or a source archive with no git), and nothing is newer than `0`: a build
  that cannot count is never ranked above another. A build before this field prints none.
  The build jobs of `artifacts.yml` check out with `fetch-depth: 0`, so the assets they
  build print the real count; a shallow local build still prints `seq: 0`. The strict
  reader of this line in `kbf-installer` (`deny_unknown_fields`) accepts `seq` from this
  change on: an installer older than that refuses a kbf that prints it
  (`version-unreadable`), so the installer package that reads `seq` ships before the kbf
  that prints it.
- `state_schema`: the newest Postgres state schema version the binary embeds a
  migration for.
- `version`: the package version.

`--json` without `--version` is a usage error. The fields are a contract: the test
`version_json_reports_the_build_identity` (`crates/kbf-cli/tests/binary.rs`) fails when
one is added, dropped or renamed.

`kbf-installer --version --json` asks the installer the same question (exactly those
two words, in either order) and answers with three of those fields, which mean what they
mean above:

```text
{"commit":"0123456789ab","profile":"release","version":"0.1.0"}
```

It has no `protocol`, `min_protocol` or `state_schema`: those say what a kbf release
speaks, which the installer checks of a release before it installs it, not of itself.
`kbf-installer --version` prints `kbf-installer <package version>+<commit>`, the line
`build-commit check` reads. The test `version_json_reports_the_build_identity`
(`crates/kbf-installer/tests/version.rs`) fails when a field is added, dropped or
renamed.

## When

| Event | What happens |
|---|---|
| Pull request, merge queue | A build check: every asset is built, signed, packaged and summed, and `SHA256SUMS` is checked. Nothing is attested, so nothing is published. |
| Push to `main` | The same, as the second stage of the one pipeline (`pipeline.yml`: `ci` and `beta` (the source tests) together, `artifacts` (the builds and sums, `stage: build`) beside them, `artifacts-sign` (the attest and verify jobs, `stage: sign`) after `ci` and `artifacts`, `smoke` on these files after `artifacts-sign`, then the release; `artifacts.yml` is called twice by it, and reads the push as its own event; the attest jobs are in that file because it is the signer identity the installers verify), then, per platform group (Linux: `sums-linux`, `attest-linux`, `verify-linux`; macOS: `sums-darwin-arm64`, `attest-darwin-arm64`, `verify-darwin-arm64`, so the Linux files are attested as soon as the Linux builds finish, however long the macOS runners queue), the `attest-*` job signs build provenance for every file in `SHA256SUMS` (`actions/attest-build-provenance`), an SBOM attestation for every tarball and for the task image (`actions/attest-sbom`) and a build provenance attestation of each tarball alone, whose bundle it saves (artifact `bundles-linux` or `bundles-darwin-arm64`), and nothing else. The `verify-*` job of the group then assembles the group's `SHA256SUMS` (artifact `sha256sums-linux` or `sha256sums-darwin-arm64`, listing only that group's files, each with the same subject as in the commit's one `SHA256SUMS`) again from the files, refuses it unless it is byte for byte the published one, checks the files against it, and verifies every attestation with the command below, for the branch that was pushed (`--source-ref "$GITHUB_REF"`), and every tarball against its bundle, offline, for the commit (`--commit "$GITHUB_SHA"`). |

An attestation names the ref it was signed on, and only `refs/heads/main` signs now.
Until main became the only integration branch, pushes to a development branch,
`feature/arch-v2`, were attested too (historic: those assets and packages still exist,
and a host that trusts one asks for that branch with `--source-ref`; an asset attested on
`feature/arch-v2` never verifies as `main`'s). A release (below) verifies as `main` and
only takes the files of the pipeline run of a push to `main`, so a file built from any other branch is
never released.

Only the `attest-*` jobs (`attest-linux`, `attest-darwin-arm64`) hold `id-token: write` and `attestations: write` (the release workflow's `gamma` job holds `id-token: write` alone, to present it to the update coordinator, and signs nothing; see [Releases](#releases)), and only
under the exact `if:` that limits it to a push to `main` (the
`verify-*` jobs run under the same one). The workflow lint
(`crates/kbf-integration-test/tests/workflows`) refuses those scopes in any other job, and in one
whose `if:` is any other spelling, so no pull request or merge-queue run holds a token
that can sign an attestation. No repository code runs in
an `attest-*` job and nothing is built there: it holds those scopes for the pinned signing
actions and for a `cp` that names each bundle (below). Every action is pinned by commit
SHA, and that lint refuses one that is not.

The attest job of a platform group names each tarball of the group and its SBOM, one `actions/attest-sbom` step per
tarball (and, in the Linux group, one for each task image, whose subject is the image tarball and whose SBOM is the image's own,
`kbf-task-image-<commit>-<platform>.cdx.json`), and runs only on a push to one of those two branches. So that a binary added to a build job cannot reach
`main` without its step, the repository lint (`crates/kbf-integration-test/tests/attested_assets`)
reads the workflow's `cargo xtask package` lines and its attest steps on every pull
request, and refuses a packaged tarball with no step pairing it with its own SBOM, or a
step for a tarball no job packages. The lint `crates/kbf-integration-test/tests/attested_bundles` does
the same for the bundles: a packaged tarball with no provenance step of its own, no step
that names its bundle, or a bundle that is not uploaded, downloaded by the verify job and
by the release workflow, and required by `release locate`, is named in the pull request. It
also names a `run:` step in an `attest-*` job that is not the `cp` right after a
provenance step, so the claim above (no repository code runs while the job holds the
signing scopes) is checked, not only written.

## Provenance bundles: verifying a tarball offline

A host that runs a tarball verifies it without asking GitHub anything: it is given the
tarball and `<tarball>.sigstore.json` (for example
`kbf-<commit>-linux-x86_64.tar.gz.sigstore.json`), the Sigstore bundle of a SLSA build
provenance attestation whose one subject is that tarball, signed by `artifacts.yml` on
the push. The bundle holds the signature, the signing certificate and the transparency
log entry, so `gh attestation verify` needs only a trusted root, a file the host keeps.

```text
gh attestation trusted-root > trusted_root.jsonl      # on a machine with network, to keep
gh attestation verify <tarball> \
  --bundle <tarball>.sigstore.json \
  --custom-trusted-root trusted_root.jsonl \
  --repo komira-ai/kbf \
  --signer-workflow komira-ai/kbf/.github/workflows/artifacts.yml \
  --cert-oidc-issuer https://token.actions.githubusercontent.com \
  --source-ref refs/heads/main \
  --source-digest <commit> \
  --predicate-type https://slsa.dev/provenance/v1 \
  --deny-self-hosted-runners
```

- Renamed repository: the repository was `komira-ai/komira-build-farm` before it became
  `komira-ai/kbf`, and a certificate carries the name at signing time (GitHub redirects
  the old name for git and the API, not inside a signature). A release attested before the
  rename verifies only with `--repo komira-ai/komira-build-farm` and
  `--signer-workflow komira-ai/komira-build-farm/.github/workflows/artifacts.yml`.
  `gh` offers no repository-id flag, so the name is what is pinned. The installer
  ([`kbf-installer`](../crates/kbf-installer/src/attest.rs)) tries the new name, then
  the old, for one release cycle; the old pair is then deleted (`FORMER_REPO`).
- `--source-digest <commit>`: the commit the host is installing (the one in the
  tarball's name). Without it an attestation made for any commit of the branch that
  built the same bytes would verify; with it the signed commit must be the one asked for.
- `--source-ref`, `--signer-workflow`, `--deny-self-hosted-runners`: as for the online
  command below. `--cert-oidc-issuer` and `--predicate-type` are `gh`'s defaults; they are
  passed so the command does not change with the `gh` it runs on.
- `--bundle` and `--custom-trusted-root` make `gh` read the attestation from the file and
  trust the root given, and fetch nothing. A trusted root is a snapshot of Sigstore's
  keys: a host that pins one refreshes it with its own updates, and a bundle signed with
  keys the pinned root does not know does not verify until it is.

How a bundle is made, and what is checked of it:

- **Its own subject.** The `attest-*` job runs `actions/attest-build-provenance` once per
  tarball with `subject-path` naming that one tarball, so the bundle has that tarball as
  its only subject and stays the same size however many files a release holds. (The step
  over `SHA256SUMS` still attests every listed file as one of many subjects; it is what
  the online check of the SBOM files finds. A tarball is therefore attested twice, and a
  host is given only its own.)
- **Its name.** The action leaves the bundle in a temporary directory, as
  `attestation.json` for every attestation. The step after each provenance step copies it
  to `dist/<tarball>.sigstore.json`, and the job uploads `dist/*.sigstore.json` as the
  artifact `bundles-linux` or `bundles-darwin-arm64`.
- **Not an SBOM's.** The SBOM attestation of a tarball carries the whole SBOM and can be
  larger than a bundle may be (below); it is not given to a host. A tarball has exactly
  one bundle, a package (`kbf`, or `kbf-installer` on a Mac) has exactly one (`<package>.sigstore.json`,
  the same kind of attestation over the one `.deb`), and nothing else has one.
- **Not in `SHA256SUMS`.** The bundle is made after the sums, from the files they list, so
  they cannot list it. `cargo xtask verify-assets sums` leaves out names that end
  `.sigstore.json`, and `cargo xtask release check` holds the directory to one
  `<tarball>.sigstore.json` per tarball (and per `.deb` and `.pkg`) the sums list and none for anything else.
- **Its size and shape.** A bundle is one line of JSON (`mediaType` of a Sigstore bundle,
  `dsseEnvelope`, `verificationMaterial`) of at most 256 KiB, the room a host's order
  record has for one. `release check` refuses an empty file, two bundles in one file and
  a bigger one.
- **Its subject.** The statement in the bundle's `dsseEnvelope.payload` (base64 of an
  in-toto statement) is build provenance (`https://slsa.dev/provenance/v1`) with exactly
  one subject, whose `sha256` digest is the one `SHA256SUMS` lists for the tarball. A host
  makes the same comparison with the digest of the bytes it read. `release check` and
  `verify-assets attestations` refuse a bundle copied under another tarball's name, one
  that names two subjects or none, and an SBOM attestation, before `gh` is run. (They do
  not judge the signature: that is `gh`'s.)
- **Verified as a host does.** `cargo xtask verify-assets attestations dir --commit
  <commit> --trusted-root trusted_root.jsonl ...` runs the command above on every
  tarball and its bundle, with the trusted root fetched by `gh attestation trusted-root`,
  the proxy variables (both spellings) pointing at nothing and `NO_PROXY` removed, so a
  check that passes did not use the network. Its controls, which must fail: another
  tarball's bundle for the first tarball (a bundle is one tarball's own), the first
  tarball's bundle for another commit, and a copy of the first tarball with a byte
  flipped. The bundles in `dir` are first held to one per listed tarball and none for
  anything else, so a stray bundle fails the `verify-*` job rather than the release.

## The kbf package (deb)

`kbf` is the package of a Linux host. Installing a newer one is the update of a host
([deploy/host-install.md](deploy/host-install.md) is the guide for the update itself). It holds:

- the runtime `/usr/bin/kbf` and the systemd unit of every mode (`kbf-all-in-one.service`,
  `kbf-daemon.service`, `kbf-frontend.service`, `kbf-storage-and-daemon.service`,
  `kbf-storage.service`) and `kbf.slice`, which they run in;
- `/usr/lib/sysusers.d/kbf.conf` (the users `kbf`, `kbf-server`, `kbf-fetch` and `kbf-signer`
  and the group `kbf-signer-clients`) and `/usr/lib/tmpfiles.d/kbf.conf` (`/run/kbf-installer`,
  where the lock is, and `/var/cache/kbf-fetch`): `systemd-sysusers` and `systemd-tmpfiles`
  make them at boot, and no maintainer script does;
- `/etc/needrestart/conf.d/kbf.conf`, a configuration file a host may edit, which keeps
  needrestart from restarting a kbf unit in the middle of a build;
- the trust directory `/usr/share/kbf/trust/`, which `kbf-host-install` and the installer read to
  verify an attestation without a network: `gh` is the pinned `gh` the job downloaded (checked by
  SHA-256 before use) and executable, `trusted_root.jsonl` is the Sigstore trusted root that `gh`
  fetched, and `installer.conf` is exactly `SOURCE_REFS=refs/heads/<branch>`,
  **the branch the package is attested on**: a package built on `main` trusts `main` releases; a
  package built on a branch other than `main` (historically `feature/arch-v2`) trusts that
  branch's, never the other. (A host that has not taken this package yet still has these files in
  `/usr/share/kbf-installer/trust/`, from the old package, and is read there until it does; that
  fallback is removed in v0.0.9.)

The package has no maintainer script, so the package manager replaces the file and restarts
nothing: the operator restarts the mode unit that runs it (a stop drains).
Each mode unit runs `kbf boot-check` before it starts `kbf`: the check is the `kbf` file itself
(it brings the state database to the schema that `kbf` needs, for the modes that keep state in
it). The package depends on no package: a host updates it with a plain `dpkg --install`
(`kbf-host-install` does, and so does `--migrate-to-package`), which resolves nothing, and a
dependency the host lacks would leave `kbf` unpacked, unconfigured and then undone. It recommends
`crun`, `uidmap`, `systemd` and `ca-certificates` (the dependencies the `kbf-installer` package
had; `crun` and `uidmap` are the container driver's, whose preflight names `crun` when it is
missing, and `systemd` and `ca-certificates` are on any host that runs these units, but a
recommendation cannot fail an install where a dependency can) and suggests `postgresql`, for a
frontend or all-in-one host that keeps its state on the host. It is built, like every other, from
the files the job has just built and checked: `cargo xtask pkg-trust`, `pkg-config` and
`pkg-build` (nfpm, pinned by SHA-256, builds the deb and the rpm twice and the bytes must match),
and a fresh Ubuntu 24.04 container installs, runs, removes and purges the deb
(`cargo xtask pkg-install-test`). Then `cargo xtask pkg-upgrade-test` takes it over a host that
has the `kbf-installer` and `kbf` debs of the release before, installed by apt and dpkg
(see below). Then `cargo xtask pkg-kbf-asset` makes the deb the asset
`kbf-<commit>-<platform>.deb` and lists it in `SHA256SUMS`, refusing it unless:

- `usr/bin/kbf` is byte for byte the `kbf` in the tarball `kbf-<commit>-<platform>.tar.gz`
  that `build-commit check-asset` checked for the commit, mode 0755;
- it holds the files above, with their modes (0644 but the executable `kbf` and `gh`, 0755), and
  no other file;
- its control file says the package `kbf`, the architecture of the platform, the recommendations
  and the suggestion above, no `Depends`, `Pre-Depends`, `Breaks` or `Conflicts` at all, and
  `Replaces` of `kbf-installer (<< 0.1.1)`: every `kbf-installer` deb that was published is
  `0.1.0+<commit>`;
- `usr/share/kbf/trust/gh` is the pinned `gh` the job downloaded, executable, `trusted_root.jsonl`
  is JSON lines, and `installer.conf` is exactly `SOURCE_REFS=` and the branch the package is
  attested on.

It is attested like a tarball: build provenance (as one subject of the `SHA256SUMS`
attestation, and alone, with its bundle `kbf-<commit>-<platform>.deb.sigstore.json` in
`bundles-linux`) and **no SBOM attestation**: provenance and a bundle only; the `kbf` it carries
has the SBOM of its tarball, and the `gh` it carries has none here. The verify job checks both
offline with the installer's flags, and runs `cargo xtask pkg-kbf-asset --verify-only` on the
signed file with `--expect-ref "$GITHUB_REF"`, so the package that was attested is the one whose
contents were checked.

To install one, verify it first, then `dpkg -i` (every flag is the installer's: `GhCheck::args`
in `crates/kbf-installer/src/attest.rs`; a host that has no pinned `gh` yet uses the operator's own):

```text
gh run download <run-id> -R komira-ai/kbf -n assets-linux-x86_64 -n bundles-linux -n sha256sums-linux
sha256sum --check --strict --ignore-missing SHA256SUMS
gh attestation trusted-root > trusted_root.jsonl
gh attestation verify kbf-<commit>-linux-x86_64.deb \
  --bundle kbf-<commit>-linux-x86_64.deb.sigstore.json \
  --custom-trusted-root trusted_root.jsonl \
  --repo komira-ai/kbf \
  --signer-workflow komira-ai/kbf/.github/workflows/artifacts.yml \
  --cert-oidc-issuer https://token.actions.githubusercontent.com \
  --source-ref refs/heads/main \
  --source-digest <commit> \
  --predicate-type https://slsa.dev/provenance/v1 \
  --deny-self-hosted-runners
sudo dpkg -i kbf-<commit>-linux-x86_64.deb
```

(`--source-ref` is the branch the package was built on: `refs/heads/main` for a release's
package.) Known limits: a package manager checks no Sigstore attestation, only the package
file you give it, so the verification above is the operator's step; the package is not
signed with a key and is not published through an apt repository; and the trusted root
inside it is a snapshot, which a later Sigstore key rotation outdates (no code reads its
`valid_until` yet).

### Upgrading a host that has kbf-installer

The files of the retired `kbf-installer` package that are still used are the `kbf` package's now:
`kbf.slice`, the needrestart drop-in, and the sysusers, tmpfiles and trust files (the last three at
new names, `kbf.conf` and `/usr/share/kbf/trust/`). The deb of `kbf` says `Replaces: kbf-installer
(<< 0.1.1)` so that dpkg unpacks it over the files the old package owns. It does not say `Breaks`
or `Depends`: a native-package host is updated with a plain `dpkg --install` (no apt, no force
flag), and `Breaks` of an installed `kbf-installer` makes that refuse, which is exactly the host
that moves from the hand-placed layout with `--migrate-to-package`. So installing `kbf` leaves
`kbf-installer` installed, with the rest of its files, until an operator purges it:

```text
sudo dpkg --install kbf-<commit>-linux-x86_64.deb      # or: sudo apt install ./kbf-<commit>-linux-x86_64.deb
sudo dpkg --purge kbf-installer                        # when ready
```

(With apt, `--allow-downgrades` too: a deb's version is `0.1.0+<commit>` and apt compares the
commit as text, which says nothing about which build is newer.) What happens to the host:

- the users keep their ids, and the group `kbf-signer-clients` keeps its members: no package
  deletes an account, and `/var/lib/kbf-installer` (the hand-placed layout of the releases, which
  `kbf-host-install` and `kbf-deploy host` use) is not in any package;
- the files above are `kbf`'s: dpkg says so for each (`dpkg -S`), and purging `kbf-installer`
  afterwards deletes none of them;
- what only the old package carried goes when it is purged: `/usr/bin/kbf-installer`,
  `kbf-installer.service`, `kbf-proxy.service` and `kbf-proxy.socket`, and its sysusers and
  tmpfiles files under the old name (the new files make the same users and directories). A
  host that still runs the proxy or the installer's service stops them first
  (`systemctl disable --now kbf-proxy.socket kbf-proxy.service kbf-installer.service`): the
  package has no maintainer script that does. (No farm host does: they run the mode's unit only);
- `kbf setup` enables the unit of the mode and no other: it no longer enables
  `kbf-installer.service`, which no package carries, nor writes the `kbf-proxy.socket` drop-in;
- a host that has not taken the new package yet is read at `/usr/share/kbf-installer/trust/`,
  until v0.0.9.

`cargo xtask pkg-upgrade-test` runs this in fresh containers with systemd as PID 1 and the real
apt and dpkg, on the debs of the release before (`v0.0.7`, fetched and checked against their
SHA-256), for both architectures, three ways: `apt-get install` over a host with the old
`kbf-installer` and `kbf`; a plain `dpkg --install` over the same host; and a plain `dpkg
--install` over a host that has only the old `kbf` deb, with neither `crun` nor `uidmap` (that deb
had no dependencies). In each it requires that `kbf` is `install ok installed` at the new
version, that `dpkg --audit` is clean, that every file of the package is at its path, `root:root`
with its mode and bytes, and owned by `kbf` and not by `kbf-installer`, that `kbf-installer` is
still installed, that `dpkg --purge kbf-installer` then deletes none of those files, and that the
users have their ids.

The rpm of `kbf` names no replacement (an rpm `Obsoletes` would remove the old package, and no rpm
is published): it is built and installed in a container by CI, nothing more.

## The frontend and coordinator images (OCI)

The frontend, the update coordinator and the task runner ship as container images for each Linux architecture,
release assets like the packages: `kbf-frontend-image-<commit>-linux-x86_64.oci.tar` and
`kbf-frontend-image-<commit>-linux-arm64.oci.tar`, and the same two for `kbf-coordinator-image` and for `kbf-task-image`.
Each is an OCI image layout in one tar. Nothing is pushed to a registry: load the file where the
image is to run.

```text
docker image load --input kbf-frontend-image-<commit>-linux-x86_64.oci.tar
# or, on a containerd host
ctr images import kbf-frontend-image-<commit>-linux-x86_64.oci.tar
```

The loaded images are named `kbf-frontend:<commit>`, `kbf-coordinator:<commit>` and `kbf-task:<commit>`. Each holds one
binary the job built and checked to name the commit, never a rebuild, on a base pinned by digest
(the frontend and the coordinator on a distroless `cc` base, `packaging/container/frontend/Dockerfile`
says why that base; the task image on Ubuntu 24.04, below). Each runs as uid
65532 and holds no secret and no address.

- **Frontend.** The `kbf` unpacked from `kbf-<commit>-<platform>.tar.gz` into the build context.
  Its entrypoint is `kbf --mode=frontend` and its configuration is a `/etc/kbf/kbf.toml` you
  mount. It declares the ports 8980, 8981 and 8982 (the defaults of `docs/deploy/kbf-modes.md`).
- **Coordinator.** The `kbf-coordinator` the job built (it has no tarball of its own), copied
  into the build context after `build-commit check` showed it names the commit.
  Its entrypoint is `kbf-coordinator`, with the flags of its mounted files as the default command
  (`packaging/container/kbf-coordinator/Dockerfile`), and it declares the ports 8443 and 8444.
- **Task.** The `kbf-task` the job built (the same file as its tarball's), copied into the build
  context after `build-commit check` showed it names the commit. Its entrypoint is `kbf-task`
  with no default command and no declared port: what a task is given is the arguments of the
  entrypoint (`packaging/container/kbf-task/Dockerfile`). The binary is the static musl build
  (the tarball's). The base is the Ubuntu 24.04 image the farm's Linux actions run in
  (`docker.io/library/ubuntu`, tagged `noble-20260917`: an image index, which the release does not
  pin), so the Commands the task runs find the glibc, CA roots and tzdata they find in an action's image.
  Each architecture is pinned by the digest of its own manifest in that index
  (linux/amd64 `sha256:f610ab94...`, linux/arm64 `sha256:08571ca1...`; the full digests are in the
  Dockerfile). The two manifests are committed beside the Dockerfile
  (`packaging/container/kbf-task/base-<platform>.manifest.json`, the bytes the registry serves), so
  the layer digests the check below compares are a file in the repository whose sha256 is the
  digest the Dockerfile pins, not a value fetched at build time. The image adds nothing to the base
  but the one layer that holds `/usr/bin/kbf-task` (the `COPY` is last, so a `kbf-task` change leaves the
  base layers' digests as they are): no shell tool, no package, no user. It runs as the numeric
  `65532:65532` like the other two images; a numeric user needs no `/etc/passwd` entry (an entry
  would be a second changed layer), and `kbf-task` reads no user database.

Both are run as `docs/deploy/containers.md` says. The archives are small, a binary on a distroless
base: about 34 MB for a frontend image and 13 MB for a coordinator image,
and they add that much to every run's artifacts and to every release.

The build is reproducible: the job builds each image twice from the commit's time
(`SOURCE_DATE_EPOCH`, with BuildKit's own provenance and SBOM attachments off) and the two files
must be the same bytes. `cargo xtask pkg-image-asset` lists the image in `SHA256SUMS`, refusing it
unless:

- `usr/bin/kbf` (frontend) is, in the layers as they stack, byte for byte the `kbf` in the tarball
  `kbf-<commit>-<platform>.tar.gz` that `build-commit check-asset` checked for the commit, mode
  0755; `usr/bin/kbf-coordinator` and `usr/bin/kbf-task` are byte for byte the `kbf-coordinator` and
  the `kbf-task` the job built (`--binary`),
  mode 0755;
- its config says Linux and the architecture of the platform, the entrypoint
  `/usr/bin/kbf --mode=frontend` (frontend), `/usr/bin/kbf-coordinator` or `/usr/bin/kbf-task`, and a user that is not
  root;
- every blob it names is present and hashes to its digest;
- (task image) its layers are the layers of the pinned base manifest of its platform, in order, and
  exactly one more layer above them, which holds `usr/bin/kbf-task` and no other file. The layers of the
  base are compared by digest and not opened.

For the task image `pkg-image-asset` also writes `kbf-task-image-<commit>-<platform>.facts.json` beside
the image and lists it in `SHA256SUMS`: `manifest` (the digest `index.json` names for the image's
manifest, the digest a registry pin or a pull by digest uses; the manifest blob is hashed to it), `config`,
every layer digest bottom first, `base` (the digest of the pinned base manifest), the commit and the
platform. The file is a function of the image: `--verify-only` computes it again from the signed image
and refuses unless the file beside it is those bytes. `verify-assets sums` refuses a listed task image
without its facts and facts without their image, and `verify-assets attestations` checks the facts for build provenance, as it does an
SBOM. They carry no bundle and no SBOM attestation of their own; the attestation of `SHA256SUMS`
that covers every other file covers them.

Also for the task image, `cargo xtask pkg-image-sbom` (after the image is an asset) writes
`kbf-task-image-<commit>-<platform>.cdx.json` beside it and lists it in `SHA256SUMS`: the CycloneDX SBOM syft
reads from the image's OCI layout, so it lists the deb packages of the Ubuntu base as well as the `kbf-task`.
syft is fetched by version and by the SHA-256 of its release archive (`SYFT_VERSION`, `SYFT_SHA256_X86_64_LINUX`
and `SYFT_SHA256_AARCH64_LINUX` in the workflow), as nfpm and `gh` are. The file is reproducible: syft stamps a
random `serialNumber` and the time it ran, so `pkg-image-sbom` replaces them with a UUID made of the digest of the
image's manifest and with the commit's time (`SOURCE_DATE_EPOCH`, the one the image's own build uses); the rest of
what syft prints is a function of the image and of syft's version. (The SBOM is attested, and is not one of the two
files the job compares: the two image builds are.) `pkg-image-sbom` refuses a document that is not CycloneDX, that
is not of a container whose version is the image's manifest digest, that lists no deb package, or that is over
8 MiB. `pkg-image-sbom --verify-only` makes the same checks on the SBOM beside the signed image, in the verify job.
`verify-assets sums` refuses a listed task image without its SBOM and an SBOM without its image.

It is attested like a deb: build provenance (one subject of the `SHA256SUMS` attestation, and alone,
with its bundle `kbf-<service>-image-<commit>-<platform>.oci.tar.sigstore.json` in `bundles-linux`)
and **no SBOM attestation**, except the task image's: it also has an SBOM attestation (`actions/attest-sbom`, predicate
type `https://cyclonedx.org/bom`) whose subject is the image tarball and whose SBOM is its own
`kbf-task-image-<commit>-<platform>.cdx.json`, which `verify-assets attestations` requires exactly when the sums list that
SBOM. The `kbf` of the frontend image has the SBOM of its tarball. The
coordinator has no tarball and so no SBOM asset yet; the task runner's SBOM that is the tarball's describes the binary and
not the Ubuntu base the task image carries, so it is not attested as the image's: the image has its own. The verify job checks both
offline and runs `cargo xtask pkg-image-asset --verify-only` on each signed file (for the
coordinator and the task runner that makes the checks of the image alone: the verify job has not the binary the build
compared it with). Verify one before loading it, as for the `kbf` deb above:

```text
gh attestation trusted-root > trusted_root.jsonl
gh attestation verify kbf-frontend-image-<commit>-linux-x86_64.oci.tar \
  --bundle kbf-frontend-image-<commit>-linux-x86_64.oci.tar.sigstore.json \
  --custom-trusted-root trusted_root.jsonl \
  --repo komira-ai/kbf \
  --signer-workflow komira-ai/kbf/.github/workflows/artifacts.yml \
  --cert-oidc-issuer https://token.actions.githubusercontent.com \
  --source-ref refs/heads/main \
  --source-digest <commit> \
  --predicate-type https://slsa.dev/provenance/v1 \
  --deny-self-hosted-runners
```

`beta.yml` loads the x86_64 image of each push into Docker, runs its `kbf --version` (and the
coordinator's `kbf-coordinator --version`) and checks that the binary in it names the commit (and,
for `kbf`, a release build).

## The kbf-installer package (pkg, macOS)

A Mac gets `kbf-installer` the same way, as one file that holds everything it needs to verify a
release offline ([packaging/macos/README.md](../packaging/macos/README.md)). The `darwin-arm64`
job builds it from the signed binary it has just built and checked: the pinned darwin `gh`
(the 2.102.0 release's `macOS_arm64` zip, checked by SHA-256 before use), the Sigstore trusted
root that `gh` fetched, and an `installer.conf` from `cargo xtask pkg-trust`, then `cargo xtask
pkg build` writes the package (the same bytes for the same inputs; `pkgbuild` is not
deterministic). It installs these entries, all root:wheel, at the paths of the installer's macOS
layout (`crates/kbf-setup/src/layout.rs`; a test compares the package's paths with it):

| Path | Mode | Layout |
|---|---|---|
| `/usr/local/kbf-installer/bin/kbf-installer` | 0755 | the install root, `Layout::install_root` |
| `/usr/local/kbf-installer/trust/gh` | 0755 | `Layout::gh`: the pinned `gh` that verifies attestations offline |
| `/usr/local/kbf-installer/trust/trusted_root.jsonl` | 0644 | `Layout::trusted_root` |
| `/usr/local/kbf-installer/trust/installer.conf` | 0644 | `Layout::settings`, below |
| `/Library/LaunchDaemons/com.komira.kbf-installer.plist` | 0644 | not loaded, and not running until `/usr/local/etc/kbf/installer.toml` (`Layout::installer_toml`) exists |
| `/var/db/kbf-installer`, `/Library/Logs/kbf-installer` | 0700, 0755 | the state directory (`Layout::state_dir`) and the job's log directory, empty |

`installer.conf` is exactly `SOURCE_REFS=refs/heads/<branch>`, **the branch the package is
attested on**, as in the deb: a package built on `main` trusts `main` releases, one built on
a branch other than `main` (historically `feature/arch-v2`) trusts that branch's. It sets nothing else, so `RUN_AS_UID` and `RUN_AS_GID` are
the installer's defaults (`nobody`). What is per host is not shipped: `kbf-installer setup`
writes `installer.toml` (and `kbf.toml`, the tokens, the users) at adopt time.

`cargo xtask pkg-macos-install-test` installs the package as root on the runner
(`sudo -n installer -pkg ... -target /`) and checks that Apple's `pkgutil` lists the paths this
repository's reader finds, every file and directory it added is root:wheel with its mode and bytes,
the job is not loaded and nothing is running, `installer.toml` does not exist, the installed `gh`
runs and the installed `kbf-installer --version --json` names the commit. Then `cargo xtask
pkg-macos-asset` makes it the asset `kbf-installer-<commit>-darwin-arm64.pkg` and lists it in
`SHA256SUMS.darwin-arm64`, refusing it unless it holds exactly the files above (no script, nothing
else), the binary is byte for byte the one in the tarball `build-commit check-asset` and
`darwin-asset` checked, `gh` is the pinned one, the trusted root is JSON lines, the property list is
gated, `installer.conf` trusts the branch it is attested for, and its `PackageInfo` version ends
`+<the first 12 digits of the commit>`.

It is attested like the deb: build provenance (one subject of the `SHA256SUMS` attestation, and
alone, with its bundle `kbf-installer-<commit>-darwin-arm64.pkg.sigstore.json` in
`bundles-darwin-arm64`) and **no SBOM attestation**. The verify job checks both offline and runs
`cargo xtask pkg-macos-asset --verify-only` on the signed file. The package itself is not signed
(no Developer ID): Gatekeeper does not open it by double click, and the binary inside is signed ad
hoc, as the tarballs' are; the verification below is the operator's step.

```text
gh run download <run-id> -R komira-ai/kbf -n assets-darwin-arm64 -n bundles-darwin-arm64 -n sha256sums-darwin-arm64
shasum -a 256 --check --strict --ignore-missing SHA256SUMS
gh attestation verify kbf-installer-<commit>-darwin-arm64.pkg \
  --bundle kbf-installer-<commit>-darwin-arm64.pkg.sigstore.json \
  --custom-trusted-root trusted_root.jsonl \
  --repo komira-ai/kbf \
  --signer-workflow komira-ai/kbf/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main \
  --cert-oidc-issuer https://token.actions.githubusercontent.com \
  --source-digest <commit> \
  --predicate-type https://slsa.dev/provenance/v1 \
  --deny-self-hosted-runners
sudo installer -pkg kbf-installer-<commit>-darwin-arm64.pkg -target /
```

(The flags are the installer's, as for the deb.) Installing places files and starts nothing; `sudo
/usr/local/kbf-installer/bin/kbf-installer setup` configures the host.

## The macOS kbf-task bundle

The Mac executor runs `kbf-task` inside the sandbox as a bundle placed by digest, not as a layer of the
toolchain bundle. The `darwin-arm64` job makes that bundle, and no other job does:

| Asset | What it is |
|---|---|
| `kbf-task-bundle-<commit>-darwin-arm64.tar.gz` | the files of an OCI image layout directory, at the paths they have in it: `oci-layout`, `index.json` and `blobs/sha256/<hex>` |
| `kbf-task-bundle-<commit>-darwin-arm64.cdx.json` | the CycloneDX SBOM of the `kbf-task` binary, which is all the one layer holds |

Both are in the macOS `SHA256SUMS`, the tarball is attested like every tarball (build provenance, an SBOM
attestation and its own `.sigstore.json` bundle; [above](#provenance-bundles-verifying-a-tarball-offline)),
and the tarball is one of the 23. It is not a binary tarball: it holds no member named `kbf-task`, the layout
holds it in a layer.

The steps, in order, each unconditional (`crates/kbf-integration-test/tests/release_artifacts` pins them):
`kbf-task` is built for the job's Mac target with `cargo build --locked --release -p kbf-task` and its default
features, so it has the macOS confine (the test-only feature `unconfined-for-tests` is never named); it is signed
ad hoc with the hardened runtime and checked by `cargo xtask sign-darwin`, exactly as the other Mac binaries
([below](#macos-signing-what-ad-hoc-with-the-hardened-runtime-gives)), and has no entitlements; the pinned
`js-bundle` is fetched by one command, `cargo xtask fetch-attested-tool`, which refuses the download unless its SHA-256
is the pinned one, writes it, runs `gh attestation verify` on that file, and only then takes the program out of that
same file; `cargo xtask stage-task-tree` copies the signed binary alone to a directory of its own, which `js-bundle pack` packs as the one tree `task` of a bundle named
`kbf-task` (minimum macOS 14.0, the entrypoint `kbf-task`, the network level `on` and the reads `system-certs` and
`resolver-config`, which `kbf-task` needs to reach the store and the frontend); `js-bundle verify` extracts every
layer under the format's rules; and `cargo xtask package-task-bundle` refuses a layout that is not exactly
one `kbf-task` bundle for a Mac (the files of a layout and no others, one manifest, every blob under the SHA-256 of
its bytes, the config's `os`, `architecture`, `name` and entrypoint) and writes the tarball: entries sorted by
path, mode 0644 (0755 for a directory), owner and group 0, time 0, in a gzip stream with no name and time 0, so
the same layout is the same bytes on every runner.

**What the network level means.** `--network on` sets the bundle's `seatbelt.network_max`, a ceiling the agent
clamps a job's request to; a job's own level (`NativeSpec.network`) defaults to off. It does not bound what
`kbf-task` itself can reach: that bound is the pf anchor for the pool uid range, not Seatbelt. Not yet verified,
to be proven on a farm Mac in the M7/M8 slice: whether the agent runs a bundle program under its own Seatbelt,
which would refuse the confine's nested `sandbox-exec` (`sandbox_apply: Operation not permitted`).

**The digest rule.** A file is taken by the SHA-256 `SHA256SUMS` lists for its tarball. What a host places in
its store is named by the bundle's manifest digest (`sha256:<hex>`, the one entry of `index.json`, which
`js-bundle pack` prints and `js-bundle inspect` shows); the manifest names the config and the layer by digest,
so the tarball's attested digest fixes the manifest digest, and a manifest digest that does not come from an
attested tarball is not this release's `kbf-task`.

**The packer pin.** `js-bundle` is a release of the jobscheduler repository, pinned in the workflow's `env`:
`JS_BUNDLE_VERSION` is `0.1.79` (the release `v0.1.79`), `JS_BUNDLE_SHA256_AARCH64_DARWIN` is
`dd21b0beee4d3f2d11a5f0e4f31ec2128a6fd85891f60405a283e1bf640ee50b` (the SHA-256 of
`js-bundle-0.1.79-aarch64-apple-darwin.tar.gz` in that release's `SHA256SUMS`, checked by `cargo xtask fetch`
before anything is unpacked), and `JS_BUNDLE_SOURCE_DIGEST` is
`1f304ae75dbef20c9b01231b666b1e91e1f21743`, the commit that release was built from. The command runs the pinned `gh attestation verify` on the archive file (before anything is taken out of it) for the repository `komira-ai/jobscheduler`, the signer workflow
`komira-ai/jobscheduler/.github/workflows/release.yml`, `--source-ref refs/heads/main` and
`--source-digest` that commit, on a hosted runner. Moving the pin means
changing the three values together with this paragraph (a test holds this file to the workflow's values).

**Not notarized.** As for the other Mac binaries: signed ad hoc with no Developer ID, so no identity and no
notarization; authenticity comes from the attestation of the tarball.

## Where the files are, and for how long

The assets are workflow artifacts of the run (`assets-darwin-arm64`,
`assets-linux-x86_64`, `assets-linux-arm64`, `sha256sums` for the whole commit, `sha256sums-linux` and `sha256sums-darwin-arm64` for the platform groups, and the provenance
bundles of a push's tarballs, `bundles-linux` and `bundles-darwin-arm64`). GitHub keeps workflow
artifacts for at most 90 days, so nothing may depend on them for longer: a script that
must still fetch an asset later pins a release asset (below). The attestations
themselves do not expire with the artifacts.

## Releases

A release is the files the `artifacts` stage of the pipeline run of a push to `main` left for one commit,
attached unchanged to a GitHub release
([`.github/workflows/release.yml`](../.github/workflows/release.yml)). Nothing is
rebuilt. Its assets are every tarball and SBOM in the table above, for that commit, the
`kbf-<commit>-<platform>.version.json` of each platform, their
`SHA256SUMS`, and the provenance bundle of each tarball, `<tarball>.sigstore.json`, beside
it.

One run of `release.yml` cuts a release; nobody tags. A push to `main` runs one sequential pipeline
(`pipeline.yml`, [design](design/kbf-cicd.md), section 8.0): `ci` and `beta` (the source tests) together, `artifacts` (the builds) beside them, `artifacts-sign` (the attestations) after `ci` and `artifacts`, and `smoke`, which installs and
tests the `kbf` of the files `artifacts` built and attested in that same run, after `artifacts-sign`; the run fails fast (its `watch` job cancels it when a job has failed). `release.yml`
is started by the completion of a pipeline run (`workflow_run`, `branches: [main]`) and acts only when that run
succeeded for a push to `main`; it reads the files from that run's artifacts by the run's id and publishes those same
files. It is a workflow of its own so that nothing a push does can cancel a
release that has started: its group never cancels, and a pipeline run that is red starts it only to skip
every job. There is no hand-started run: a failed job is re-run from the run's page (failed jobs only). A tag pushed by hand
starts nothing, and only the release bot (a GitHub App, the one bypass of the `v*` tag ruleset)
can create one. The jobs, each with the least token it needs:

| job | does | scopes |
|---|---|---|
| `plan` | the next version from `VERSION` and the tags, and whether the push is code, the site or the pipeline itself (`release plan`, measured from the highest release tag that is an ancestor of the commit); the stop switch `RELEASE_FREEZE` is read here | `contents`, `actions`: read |
| `verify` | refuses a commit not on `main` and a run that is not the successful push run for it (`release locate`), checks the files of that run and every attestation (below), before anything is tagged | `contents`, `actions`, `attestations`: read |
| `tag` | writes the Beta record and creates the annotated tag at the commit, create-only, with the release bot's token (`release verdict-write`, `release tag`) | `contents: read`; environment `release` |
| `notes` | checks the tag (`release verify`), the identity of this release and of the one before it, and writes the notes | read |
| `publish` | creates the release with those files and notes (`release publish`) | `contents: write`; environment `release` |
| `bump` | opens the pull request in the internal repository that moves its submodule to the release (`release bump-internal`) | `contents: read`; environment `release` |
| `gamma` | after `publish`, derives the version its hosts report (`<version>+g<commit>`, from the release's version facts: `release farm-version`), sets it as the desired version on the farm's update coordinator and waits for the farm to converge (`deploy-farm`); passes with a notice when the repository variable `KBF_COORDINATOR_URL` is unset. Then builds a small Buck2 project against the farm, remote only, and fails unless some action ran remotely and none locally (`prove-farm`); passes with a notice when `FARM_ADDRESS` is unset ([deploy/kbf-updater.md](deploy/kbf-updater.md#turn-on-the-gamma-stage)) | `contents: read`, `id-token: write`; environment `release` |

Every job after `plan` runs only when `plan` says the push ships code and does not change the
pipeline (`.github/`, `infra/`, `crates/xtask/` and the like): a push of docs only releases
nothing and the run is green, and a push that changes the pipeline is released by the next one
that does not. `tag`, `notes`, `publish`, `bump` and `gamma` also repeat the condition on the run that started the
workflow (conclusion `success`, event `push`, branch `main`, this repository). The pipeline run is never cancelled
and the newest waiting push queues behind it; `plan` measures from the highest release tag that is an ancestor of the
commit, so the code of a push that was replaced while waiting is released with the next one. No job runs after a red one (no `always()`, no
`continue-on-error`). `release tag` and `release publish` both refuse a commit that is behind the
highest release tag, so a re-run of an old run releases nothing; re-run the failed jobs only.
`verify`, with read-only scopes, fails the run (and nothing is tagged) when:

- the commit is not on `main` (GitHub's compare of `main...<commit>` is neither
  `identical` nor `behind`);
- the run that started the workflow is not the successful `push` run on `main` for that exact
  commit, or it no longer holds every artifact a release needs (the assets, the sums and
  the bundles; they expire after 90 days, so a release is cut within 90 days of the
  commit);
- a file does not match `SHA256SUMS` (`sha256sum --check --strict`), a file is not
  listed in it, a `kbf` tarball's `kbf-<commit>-<platform>.version.json` is not listed,
  or a listed file is not `<binary>-<commit>-<platform>` (or that version file);
- a tarball has no bundle, or a bundle file belongs to no listed tarball, is not exactly
  one bundle of at most 256 KiB, or does not have its tarball's digest as its one subject
  (`cargo xtask release check`);
- any file's attestations do not verify with the command below, or a tarball does not
  verify against its bundle offline for the commit (`cargo xtask verify-assets
  attestations`, with its controls that must fail).

`publish` then creates the release with those files. It is the only job with a token
that can write (`contents: write`), and the workflow lint refuses that scope in a job
whose `if:` is not exactly the release condition it runs under. Running the workflow
again for the same tag creates no second release and replaces no asset: each asset the
release already holds must be one of the verified files, byte for byte, or the run
fails and changes nothing; the files it lacks are uploaded (`cargo xtask release
publish`).

Two further verbs read the annotated `v*` tag. Its message is the Beta record (`docs/design/kbf-cicd.md`,
section 8.1), which `release verdict-write` wrote for the pipeline run (`ci`, `artifacts` and `beta` all succeeded) that
started the release, and names that run. The record is an audit record: **nothing gates on it**, and
no step of the release workflow judges a verdict. `notes` runs both verbs, on a checkout of the whole
history (the tag object and `origin/main` must be there): `cargo xtask release verify` refuses a
tag that names another commit than the release, a tag object named other than its ref, and a
commit not on main; a tag with no record, or a record that says it failed or names something else,
verifies the same. It prints `set-hash=<hex>`, the SHA-256 of `SHA256SUMS`
(`kbf_gate::SetHash::of_sums`). `cargo xtask release notes` runs the same checks and writes the
notes, which show the record as information and say when the tag carries none; `notes` uploads them
as the artifact `release-notes`. `notes` passes the hash to `publish` as a
job output, and `release publish` takes it as `--set-hash` and publishes nothing when the files are
another set. What gates a release is the successful pipeline run that started it, then the
checks of the files themselves (`release check`, the attestations, the set hash) and the tag's
commit. (The notes are written after the tag, because they read it.)

`crates/kbf-integration-test/tests/release_workflow` holds the file to all of this (and `pipeline_workflow` holds the pipeline it follows to its order, its cancel and its triggers): the events
(the completion of a `pipeline` run for `main` only), the job graph and the scopes, `if:` and environment of each job, the halt on
red, a checkout of the commit the pipeline built alone, the release bot's key read in `tag` and `bump` only, `id-token: write` in `gamma` only (the coordinator's address and the farm's `FARM_*` settings are read from repository variables there and nowhere else),
the data that flows from job to job, pinned actions and an empty top-level `permissions`. The
job list is pinned, `publish` among them (`gamma.yml`, which followed the workflow, is deleted). On every
pull request into `main`, CI asks GitHub the same on-`main` question for the pull request's base
commit (which must pass) and its merge commit (which must be refused), so a check that
accepts every commit, or none, fails there (`cargo xtask release main-control`).

To use an asset, download it from the release and verify it before anything runs it,
with the command below (the release's notes give it too). A tarball also verifies
offline against the bundle released beside it
([above](#provenance-bundles-verifying-a-tarball-offline); the notes give that command
too). The release's `SHA256SUMS` lists each asset's SHA-256, but is not itself attested:
the attestation is the check.

## How a node's deployment verifies an asset (planned)

No deployment job exists yet. When it does, it is to verify each asset before it
touches a node, in CI (a node has no `gh`), with every flag below
([mac-node-provisioning.md](design/mac-node-provisioning.md) section 4.2). The `verify`
job already runs these exact checks on what `attest` signed (below). A release asset
verifies with the same command, as its bytes are the attested ones.

```text
gh attestation verify kbf-<commit>-darwin-arm64.tar.gz \
  --repo komira-ai/kbf \
  --signer-workflow komira-ai/kbf/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main \
  --deny-self-hosted-runners
```

- `--signer-workflow` and `--source-ref`: without them, an attestation signed by any
  workflow of the repository, on any branch, would verify. Name the branch the asset
  was built on: `refs/heads/main` above, the branch's own ref for a build of another branch
  (historically `refs/heads/feature/arch-v2`, the retired development branch) (`cargo xtask verify-assets attestations --source-ref`, which
  defaults to `main` and refuses a value that is not `refs/heads/<branch>`).
- `--deny-self-hosted-runners`: the signing job ran on a GitHub-hosted runner.
- For the SBOM attestation, add `--predicate-type https://cyclonedx.org/bom`.

The node itself is then to check the asset's SHA-256 against the value the job hands
it, before it installs anything (planned, with the node's `apply`; design section 4.2).

The `verify` job runs exactly these checks on what `attest` signed (`cargo xtask
verify-assets attestations`, with read-only scopes, for the branch pushed), the offline
check of each tarball against its bundle ([above](#provenance-bundles-verifying-a-tarball-offline)),
and checks that must fail: the first tarball verified as signed by `ci.yml`, a copy with
one flipped byte, the first tarball against the second's bundle, against its own for
another commit, and the flipped copy against its own. If any of them verifies, the job
fails.

## macOS signing: what ad hoc with the hardened runtime gives

`cargo xtask sign-darwin` (crates/xtask/src/darwin.rs) signs each macOS binary the job
builds (`kbf`, `kbf-deploy`, `kbf-installer`, `kbf-host-install`, `kbf-updater` and `kbf-mac-provision`, one step each) with `codesign --force
--sign - --options runtime`: an ad-hoc signature (no certificate) with the hardened
runtime. That works without an Apple Developer account, and the job checks it every
run: `codesign --verify --strict` passes, the signed binary runs (`<binary> --version`
exits 0 with no `DYLD_*` variable set, on every runner), the code directory's flags are
exactly `adhoc,runtime`, the signature carries exactly the entitlements the allow
list `tools/ci/darwin-entitlements.txt` gives the binary (none for `kbf`,
`kbf-deploy`, `kbf-installer`, `kbf-host-install`, `kbf-updater` or `kbf-mac-provision`; only the `kbf-vmm` stub gets one, `com.apple.security.virtualization`;
[macos-vm-guests.md](design/macos-vm-guests.md) section 7), and the binary's minimum
macOS is 14.0. After packaging, `cargo xtask darwin-asset` takes the binary back out
of its tarball, the bytes that ship (refusing a tarball that is not exactly the one
entry `package` writes), and checks it again: `codesign --verify --strict`, the flags,
a run of `--version`, and the entitlements against the list. A step for each of the
four tarballs does this, and the repository lint (`release_artifacts`) refuses a macOS
binary that is packaged without its own `sign-darwin` before and `darwin-asset` after.

What it gives:

- **A hash of every page of code, in the signature.** `codesign --verify` checks
  them, so a binary modified after signing fails the job (but one modified and signed
  again ad hoc passes; see below). Whether the kernel refuses to run such a binary
  depends on the host's code-signing enforcement: on GitHub's hosted macOS runners it
  is off (`vm.cs_process_enforcement: 0`), and a binary with a byte flipped in its
  executed code after signing still ran there. That is why the job verifies the
  signature of the shipped bytes and does not rely on running them. What a node does
  with such a binary is to be shown on a node.
- **The hardened runtime flag.** On a Mac with System Integrity Protection enabled
  (a node), dyld ignores `DYLD_*` variables for a hardened process, so
  `DYLD_INSERT_LIBRARIES` cannot inject code, and without the `get-task-allow`
  entitlement a debugger cannot attach. CI checks the flag in the signature, not the
  behaviour: GitHub's hosted macOS runners have SIP disabled (the job prints `csrutil
  status`), and there dyld loaded `DYLD_INSERT_LIBRARIES` into the hardened binary as
  well, so the behavioural check only warns on them. It fails the job on a runner with
  SIP enabled. Showing the behaviour on a real node is left to the node's
  provisioning checks.
- **A stable code identity (the cdhash)** that a requirement can pin, as the
  fleet-updates design's helpers do. The job prints it.

What it does not give:

- **No identity.** Anyone can sign anything ad hoc; the signature says nothing about
  who built the binary. Authenticity comes from the attestation, checked above.
- **No notarization and no Gatekeeper approval.** A quarantined copy (one a browser
  downloaded) is refused. The planned deployment fetches assets with command-line
  tools, which do not quarantine them (an assumption design section 4.2 marks for
  checking on the pinned macOS).

What it constrains:

- **Only Apple-signed libraries load.** The hardened runtime turns on library
  validation, and with no Team ID of its own the binary may load only libraries Apple
  signed. That is fine for a binary that links and loads only system libraries, as
  `kbf` does; `kbf-deploy`, `kbf-installer`, `kbf-host-install`, `kbf-updater` and `kbf-mac-provision` are signed the same way,
  and CI does not list the libraries they load. On a Mac with System Integrity Protection on, a library it linked at start-up that
  Apple did not sign would stop it from starting. CI does not show this: the hosted
  runners have SIP off and library validation off (`vm.cs_library_validation` 0), so
  the check above that runs the signed binary would pass with such a library.

Developer ID signing and notarization need an Apple Developer Program membership; when
the project has one, it is one more step in the darwin job.
