# Build artifacts: what CI builds, and how a node verifies it

kbf's binaries are built by CI, never on a developer's machine, and a node is to run
only bytes CI built and attested. The workflow is
[`.github/workflows/artifacts.yml`](../.github/workflows/artifacts.yml).

What exists today is the build, the signing, the sums, the attestations (and, for each
tarball, a provenance bundle a host verifies it with offline) on a push to `main` or to
`feature/arch-v2`, and releases of those files ([`.github/workflows/release.yml`](../.github/workflows/release.yml)).
The deployment job that verifies an asset before it reaches a node and the node's own
SHA-256 check are **planned** and not built yet
([mac-node-provisioning.md](design/mac-node-provisioning.md) sections 4.1 and 4.2).
Sections below that describe them say so.

A pull request builds none of this unless it carries the label `needs-artifacts`; adding
the label to an open pull request starts the build. Pushes and the merge queue always
build.

## Pull request labels

Two labels opt a pull request in to the work that is otherwise skipped, to spare the
small macOS runner pool. Adding either to an open pull request starts a run.

| Label | Effect |
|---|---|
| `needs-artifacts` | `artifacts.yml` builds, packages and sums the files for the pull request (never for the darwin-arm64 job of a pull request into `feature/arch-v2`, which no label enables: nothing is attested from a pull request, and the push to `feature/arch-v2` builds it) |
| `macos` | `ci.yml`'s `native-macos` job runs on a pull request into `feature/arch-v2`, where it is skipped without the label. Pushes, pull requests into `main` and the merge queue run it as before. The job builds and tests on its own runner; it does not use the darwin-arm64 artifacts build |

`ci.yml` runs on the `labeled` event so that the label starts something. Any label added
to a pull request therefore starts a whole `ci` run, which replaces the pull request's
earlier run: add the labels together, before the run you want to keep. The condition of
`native-macos` is pinned by `crates/kbf-it/tests/release_artifacts`.

## What is built

Thirteen tarballs, each with its SBOM and (once the push is attested) its provenance bundle
([below](#provenance-bundles-verifying-a-tarball-offline)), built by one job per platform:

| Asset | Runner | Target |
|---|---|---|
| `kbf-<commit>-darwin-arm64.tar.gz`, `kbf-deploy-<commit>-darwin-arm64.tar.gz`, `kbf-installer-<commit>-darwin-arm64.tar.gz` | `macos-15` (Apple silicon) | `aarch64-apple-darwin`, `MACOSX_DEPLOYMENT_TARGET=14.0` |
| `kbf-<commit>-linux-x86_64.tar.gz`, `kbf-daemon-<commit>-linux-x86_64.tar.gz`, `kbf-server-<commit>-linux-x86_64.tar.gz`, `kbf-deploy-<commit>-linux-x86_64.tar.gz`, `kbf-installer-<commit>-linux-x86_64.tar.gz` | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` |
| `kbf-<commit>-linux-arm64.tar.gz`, `kbf-daemon-<commit>-linux-arm64.tar.gz`, `kbf-server-<commit>-linux-arm64.tar.gz`, `kbf-deploy-<commit>-linux-arm64.tar.gz`, `kbf-installer-<commit>-linux-arm64.tar.gz` | `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` |

`kbf` is the one binary with all five modes (`kbf --mode=daemon`, `kbf --mode=all-in-one`,
`kbf --mode=storage`, and the two modes not built yet; [cluster.md](design/cluster.md) sections 1
and 5.1), the one a host runs. `kbf-daemon` and `kbf-server` are the separate daemon
and server binaries (Linux only: a Mac runs `kbf --mode=daemon`, and darwin has no
`kbf-daemon` tarball; the ones of earlier commits stay where they were published).
`kbf-deploy` is the deploy tool (its kits are listed in
[ARCHITECTURE.md](../ARCHITECTURE.md)), shipped so an operator installs the tool the
same way as the binaries it installs: by digest, from these attested files, not built
on the operator's machine. `kbf-installer` is the one root tool that sets a host up and
updates it ([kbf-installer.md](design/kbf-installer.md)); it is built, signed, attested
and released like the others, so a package or an operator takes it by digest from these
files. It is not a component of the release it installs on a host, and it does not
update itself (the design's section 1).

The set above is pinned: `crates/kbf-it/tests/release_artifacts` fails a pull request
that drops one of these files from the workflow, packages one that is not listed, or
leaves this table describing another set.

Every kit that changes a host changes a Linux one (systemd units, the MDM services, a
systemd user service, or a cluster through `kubectl`), and `kbf-deploy` for macOS is
there for the verbs a Mac runs on itself. The macOS job builds it, signs it and runs
it as it does every macOS binary (below), but no CI job runs `kbf-deploy`'s tests on
macOS: the bytes are known to start, and nothing more is claimed for them until a job
tests the crate there.

A pull request into `feature/arch-v2` builds only the Linux files: the macOS job and the
sums are skipped there to spare the macOS runners, and the push to `feature/arch-v2`
builds and attests all of them for its head. A newer push to `feature/arch-v2` cancels
the older push's run; every commit on `main` keeps a run of its own.

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

`kbf-server`, `kbf` and `kbf-installer` embed the first 12 characters of the commit they
were built from (`kbf` through the server library it links), and `--version` prints
`<binary> <package version>+<commit>` (so do the server's start line and the `server`
field of `GET /v1/nodes`, [api.md](api.md#get-v1nodes)). Before packaging, each Linux job
checks that `kbf-server`'s `--version` names `GITHUB_SHA` (`cargo xtask build-commit
check`), and that `kbf`'s and `kbf-installer`'s do too and that `kbf --version --json`
and `kbf-installer --version --json` (below) name the same commit and a release build
(`build-commit check --json`); the macOS job makes the second check on the signed `kbf`
and `kbf-installer`, the bytes it packages. Those checks run on the binary before it is
packaged. After packaging, every job opens the `kbf` tarball and the `kbf-installer`
tarball it wrote and makes the second check again on the binary inside each, with `cargo
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
{"admin_protocol":1,"commit":"0123456789ab","meta_log":4,"min_protocol":1,"profile":"release","protocol":1,"state_schema":1,"version":"0.1.0"}
```

- `admin_protocol`: the version of the admin calls of `kbf --mode frontend` (`GET role`,
  `POST quiesce`, `resume`, `takeover`, `stand-down`: `docs/api.md`). It is the contract
  between a proxy and the frontend processes it starts; a call or an answer field that
  changes meaning raises it.
- `commit`: the first 12 hex digits of the commit built (`unknown` for a build with no
  git checkout); compare it with the first 12 of the commit in the asset's name.
- `meta_log`: the highest metadata-log machine version the binary runs
  (`kbf_server::raft_log::RUNS`). Going from release N to N-1 is reversible only if
  `meta_log(N) <= meta_log(N-1)`; a build that prints no `meta_log` is unknown, and
  unknown is not reversible.
- `profile`: `release` for every asset here (the workflow refuses a `debug` one).
- `protocol` and `min_protocol`: the newest and the oldest `kbf.worker.v1` version the
  binary speaks. The daemon half speaks exactly `protocol`; the server half accepts
  `min_protocol` through `protocol`.
- `state_schema`: the schema version of the SQLite state file the binary creates and
  reads.
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
| Push to `main` or to `feature/arch-v2` | The same, then, per platform group (Linux: `sums-linux`, `attest-linux`, `verify-linux`; macOS: `sums-darwin-arm64`, `attest-darwin-arm64`, `verify-darwin-arm64`, so the Linux files are attested as soon as the Linux builds finish, however long the macOS runners queue), the `attest-*` job signs build provenance for every file in `SHA256SUMS` (`actions/attest-build-provenance`), an SBOM attestation for every tarball (`actions/attest-sbom`) and a build provenance attestation of each tarball alone, whose bundle it saves (artifact `bundles-linux` or `bundles-darwin-arm64`), and nothing else. The `verify-*` job of the group then assembles the group's `SHA256SUMS` (artifact `sha256sums-linux` or `sha256sums-darwin-arm64`, listing only that group's files, each with the same subject as in the commit's one `SHA256SUMS`) again from the files, refuses it unless it is byte for byte the published one, checks the files against it, and verifies every attestation with the command below, for the branch that was pushed (`--source-ref "$GITHUB_REF"`), and every tarball against its bundle, offline, for the commit (`--commit "$GITHUB_SHA"`). |

An attestation names the ref it was signed on: `refs/heads/main` or
`refs/heads/feature/arch-v2`. The second is there so that a rehearsal of a deployment
runs attested builds before they reach `main`; whoever verifies one asks for the branch
they mean, and an asset attested on `feature/arch-v2` does not verify as `main`'s. A
release (below) verifies as `main` and only takes the files of a push run on `main`, so
a file built from the development branch is never released.

Only the `attest-*` jobs (`attest-linux`, `attest-darwin-arm64`) hold `id-token: write` and `attestations: write`, and only
under the exact `if:` that limits it to a push to `main` or to `feature/arch-v2` (the
`verify-*` jobs run under the same one). The workflow lint
(`crates/kbf-it/tests/workflows`) refuses those scopes in any other job, and in one
whose `if:` is any other spelling, so no pull request or merge-queue run holds a token
that can sign an attestation. No repository code runs in
an `attest-*` job and nothing is built there: it holds those scopes for the pinned signing
actions and for a `cp` that names each bundle (below). Every action is pinned by commit
SHA, and that lint refuses one that is not.

The attest job of a platform group names each tarball of the group and its SBOM, one `actions/attest-sbom` step per
tarball, and runs only on a push to one of those two branches. So that a binary added to a build job cannot reach
`main` without its step, the repository lint (`crates/kbf-it/tests/attested_assets`)
reads the workflow's `cargo xtask package` lines and its attest steps on every pull
request, and refuses a packaged tarball with no step pairing it with its own SBOM, or a
step for a tarball no job packages. The lint `crates/kbf-it/tests/attested_bundles` does
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
  one bundle, and nothing else has one.
- **Not in `SHA256SUMS`.** The bundle is made after the sums, from the files they list, so
  they cannot list it. `cargo xtask verify-assets sums` leaves out names that end
  `.sigstore.json`, and `cargo xtask release check` holds the directory to one
  `<tarball>.sigstore.json` per tarball the sums list and none for anything else.
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

## Where the files are, and for how long

The assets are workflow artifacts of the run (`assets-darwin-arm64`,
`assets-linux-x86_64`, `assets-linux-arm64`, `sha256sums` for the whole commit, `sha256sums-linux` and `sha256sums-darwin-arm64` for the platform groups, and the provenance
bundles of a push's tarballs, `bundles-linux` and `bundles-darwin-arm64`). GitHub keeps workflow
artifacts for at most 90 days, so nothing may depend on them for longer: a script that
must still fetch an asset later pins a release asset (below). The attestations
themselves do not expire with the artifacts.

## Releases

A release is the files a push run of `artifacts.yml` on `main` left for one commit,
attached unchanged to a GitHub release
([`.github/workflows/release.yml`](../.github/workflows/release.yml)). Nothing is
rebuilt. Its assets are every tarball and SBOM in the table above, for that commit, their
`SHA256SUMS`, and the provenance bundle of each tarball, `<tarball>.sigstore.json`, beside
it.

To cut one, tag a commit on `main` and push the tag:

```text
git tag v0.1.0 <commit on main>
git push origin v0.1.0
```

The tag push runs two jobs. `verify`, with read-only scopes, fails the run (and
nothing is published) when:

- the commit is not on `main` (GitHub's compare of `main...<commit>` is neither
  `identical` nor `behind`);
- there is no successful `push` run of `artifacts.yml` on `main` for that exact commit,
  or that run no longer holds every artifact a release needs (the assets, the sums and
  the bundles; they expire after 90 days, so a release is cut within 90 days of the
  commit);
- a file does not match `SHA256SUMS` (`sha256sum --check --strict`), a file is not
  listed in it, or a listed file is not `<binary>-<commit>-<platform>`;
- a tarball has no bundle, or a bundle file belongs to no listed tarball, is not exactly
  one bundle of at most 256 KiB, or does not have its tarball's digest as its one subject
  (`cargo xtask release check`);
- any file's attestations do not verify with the command below, or a tarball does not
  verify against its bundle offline for the tag's commit (`cargo xtask verify-assets
  attestations`, with its controls that must fail).

`publish` then creates the release with those files. It is the only job with a token
that can write (`contents: write`), and the workflow lint refuses that scope in a job
whose `if:` is not exactly the tag-push condition it runs under. Running the workflow
again for the same tag creates no second release and replaces no asset: each asset the
release already holds must be one of the verified files, byte for byte, or the run
fails and changes nothing; the files it lacks are uploaded (`cargo xtask release
publish`).

Two further verbs read the annotated `v*` tag, whose message is the gamma verdict
(`docs/design/kbf-cicd.md`, section 8.1; `release.yml` does not run them yet).
`cargo xtask release verdict` refuses a lightweight tag, a commit not on main, a message
that is not a green gamma verdict, a verdict that names another commit, tag or set (the
set hash is the SHA-256 of `SHA256SUMS`, `kbf_gate::SetHash::of_sums`), a bake under 24
hours, and a thin bake without a recorded `min-load` waiver; it prints `set-hash=<hex>`.
`cargo xtask release notes` runs the same checks and writes the notes. `release publish`
takes that hash as `--set-hash` and publishes nothing when the files are another set.

The workflow can also be run by hand (`workflow_dispatch`) with a commit SHA. Only
`verify` runs then: every check above, and nothing is published. On every pull request
into `main`, CI asks GitHub the same on-`main` question for the pull request's base
commit (which must pass) and its merge commit (which must be refused), so a check that
accepts every commit, or none, fails there (`cargo xtask release main-control`).

Pushing a tag runs the workflow file as that tagged commit holds it. The checks above
hold for a tag on a commit of `main`; who may push a tag at all is a repository
setting, not something this workflow controls.

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
  was built on: `refs/heads/main` above, `refs/heads/feature/arch-v2` for a build of
  the development branch (`cargo xtask verify-assets attestations --source-ref`, which
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
builds (`kbf`, `kbf-deploy` and `kbf-installer`, one step each) with `codesign --force
--sign - --options runtime`: an ad-hoc signature (no certificate) with the hardened
runtime. That works without an Apple Developer account, and the job checks it every
run: `codesign --verify --strict` passes, the signed binary runs (`<binary> --version`
exits 0 with no `DYLD_*` variable set, on every runner), the code directory's flags are
exactly `adhoc,runtime`, the signature carries exactly the entitlements the allow
list `tools/ci/darwin-entitlements.txt` gives the binary (none for `kbf`,
`kbf-deploy` or `kbf-installer`; only the `kbf-vmm` stub gets one, `com.apple.security.virtualization`;
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
  `kbf` does; `kbf-deploy` and `kbf-installer` are signed the same way,
  and CI does not list the libraries they load. On a Mac with System Integrity Protection on, a library it linked at start-up that
  Apple did not sign would stop it from starting. CI does not show this: the hosted
  runners have SIP off and library validation off (`vm.cs_library_validation` 0), so
  the check above that runs the signed binary would pass with such a library.

Developer ID signing and notarization need an Apple Developer Program membership; when
the project has one, it is one more step in the darwin job.
