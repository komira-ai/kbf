# Build artifacts: what CI builds, and how a node verifies it

kbf's binaries are built by CI, never on a developer's machine, and a node is to run
only bytes CI built and attested. The workflow is
[`.github/workflows/artifacts.yml`](../.github/workflows/artifacts.yml).

What exists today is the build, the signing, the sums, on `main` the attestations,
and releases of those files ([`.github/workflows/release.yml`](../.github/workflows/release.yml)).
The deployment job that verifies an asset before it reaches a node and the node's own
SHA-256 check are **planned** and not built yet
([mac-node-provisioning.md](design/mac-node-provisioning.md) sections 4.1 and 4.2).
Sections below that describe them say so.

## What is built

| Asset | Runner | Target |
|---|---|---|
| `kbf-daemon-<commit>-darwin-arm64.tar.gz` | `macos-15` (Apple silicon) | `aarch64-apple-darwin`, `MACOSX_DEPLOYMENT_TARGET=14.0` |
| `kbf-daemon-<commit>-linux-x86_64.tar.gz`, `kbf-server-<commit>-linux-x86_64.tar.gz`, `kbf-deploy-<commit>-linux-x86_64.tar.gz` | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` |
| `kbf-daemon-<commit>-linux-arm64.tar.gz`, `kbf-server-<commit>-linux-arm64.tar.gz`, `kbf-deploy-<commit>-linux-arm64.tar.gz` | `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` |

`kbf-deploy` is the deploy tool (its kits are listed in
[ARCHITECTURE.md](../ARCHITECTURE.md)), shipped so an operator installs the tool the
same way as the binaries it installs: by digest, from these attested files, not built
on the operator's machine. It is built for Linux only. Every kit that changes a host
changes a Linux one (systemd units, the MDM services, a systemd user service, or a
cluster through `kubectl`), and no CI job builds or tests `kbf-deploy` on macOS, so a
darwin asset would ship bytes no test ran. The one verb meant for an operator's own
machine, `mdm erase-request`, runs on a Linux one as well. A darwin asset needs a macOS
job that builds and tests the crate first.

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

`kbf-server` embeds the first 12 characters of the commit it was built from, and
`--version` prints `kbf-server <package version>+<commit>` (so do its start line and
the `server` field of `GET /v1/nodes`, [api.md](api.md#get-v1nodes)). Before
packaging, each Linux job checks that the binary's `--version` names `GITHUB_SHA`
(`cargo xtask build-commit check`). `KBF_BUILD_COMMIT_OVERRIDE` replaces the
commit in a build, for CI tests that need two stamps from one checkout; a build that
set it here would fail that check.

The Linux binaries link glibc dynamically and need the glibc of Ubuntu 24.04 or
newer.

## When

| Event | What happens |
|---|---|
| Pull request, merge queue | A build check: every asset is built, signed, packaged and summed, and `SHA256SUMS` is checked. Nothing is attested, so nothing is published. |
| Push to `main` | The same, then the `attest` job signs build provenance for every file in `SHA256SUMS` (`actions/attest-build-provenance`) and an SBOM attestation for every tarball (`actions/attest-sbom`), and nothing else. The `verify` job then assembles `SHA256SUMS` again from the files, refuses it unless it is byte for byte the published one, checks the files against it, and verifies every attestation with the command below. |

Only the `attest` job holds `id-token: write` and `attestations: write`, and only
under the exact `if:` that limits it to a push to `main`. The workflow lint
(`crates/kbf-it/tests/workflows`) refuses those scopes in any other job, so no pull
request run holds a token that can sign an attestation. No repository code runs in
`attest` and nothing is built there: it holds those scopes for the pinned signing
actions alone. Every action is pinned by commit SHA, and that lint refuses one that
is not.

The attest job names each tarball and its SBOM, one `actions/attest-sbom` step per
tarball, and runs only on `main`. So that a binary added to a build job cannot reach
`main` without its step, the repository lint (`crates/kbf-it/tests/attested_assets`)
reads the workflow's `cargo xtask package` lines and its attest steps on every pull
request, and refuses a packaged tarball with no step pairing it with its own SBOM, or a
step for a tarball no job packages.

## Where the files are, and for how long

The assets are workflow artifacts of the run (`assets-darwin-arm64`,
`assets-linux-x86_64`, `assets-linux-arm64`, `sha256sums`). GitHub keeps workflow
artifacts for at most 90 days, so nothing may depend on them for longer: a script that
must still fetch an asset later pins a release asset (below). The attestations
themselves do not expire with the artifacts.

## Releases

A release is the files a push run of `artifacts.yml` on `main` left for one commit,
attached unchanged to a GitHub release
([`.github/workflows/release.yml`](../.github/workflows/release.yml)). Nothing is
rebuilt. Its assets are every tarball and SBOM in the table above, for that commit, and
their `SHA256SUMS`.

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
  or that run no longer holds all four artifacts (they expire after 90 days, so a
  release is cut within 90 days of the commit);
- a file does not match `SHA256SUMS` (`sha256sum --check --strict`), a file is not
  listed in it, or a listed file is not `<binary>-<commit>-<platform>`;
- any file's attestations do not verify with the command below (`cargo xtask
  verify-assets attestations`, with its two controls that must fail).

`publish` then creates the release with those files. It is the only job with a token
that can write (`contents: write`), and the workflow lint refuses that scope in a job
whose `if:` is not exactly the tag-push condition it runs under. Running the workflow
again for the same tag creates no second release and replaces no asset: each asset the
release already holds must be one of the verified files, byte for byte, or the run
fails and changes nothing; the files it lacks are uploaded (`cargo xtask release
publish`).

The workflow can also be run by hand (`workflow_dispatch`) with a commit SHA. Only
`verify` runs then: every check above, and nothing is published. On every pull request
into `main`, CI asks GitHub the same on-`main` question for the pull request's base
commit (which must pass) and its merge commit (which must be refused), so a check that
accepts every commit, or none, fails there (`cargo xtask release main-control`).

Pushing a tag runs the workflow file as that tagged commit holds it. The checks above
hold for a tag on a commit of `main`; who may push a tag at all is a repository
setting, not something this workflow controls.

To use an asset, download it from the release and verify it before anything runs it,
with the command below (the release's notes give it too). The release's `SHA256SUMS`
lists each asset's SHA-256, but is not itself attested: the attestation is the check.

## How a node's deployment verifies an asset (planned)

No deployment job exists yet. When it does, it is to verify each asset before it
touches a node, in CI (a node has no `gh`), with every flag below
([mac-node-provisioning.md](design/mac-node-provisioning.md) section 4.2). The `verify`
job already runs these exact checks on what `attest` signed (below). A release asset
verifies with the same command, as its bytes are the attested ones.

```text
gh attestation verify kbf-daemon-<commit>-darwin-arm64.tar.gz \
  --repo komira-ai/komira-build-farm \
  --signer-workflow komira-ai/komira-build-farm/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main \
  --deny-self-hosted-runners
```

- `--signer-workflow` and `--source-ref`: without them, an attestation signed by any
  workflow of the repository, on any branch, would verify.
- `--deny-self-hosted-runners`: the signing job ran on a GitHub-hosted runner.
- For the SBOM attestation, add `--predicate-type https://cyclonedx.org/bom`.

The node itself is then to check the asset's SHA-256 against the value the job hands
it, before it installs anything (planned, with the node's `apply`; design section 4.2).

The `verify` job runs exactly these checks on what `attest` signed (`cargo xtask
verify-assets attestations`, with read-only scopes), and two that must fail: the first
tarball verified as signed by `ci.yml`, and a copy with one flipped byte. If either
verifies, the job fails.

## macOS signing: what ad hoc with the hardened runtime gives

`cargo xtask sign-darwin` (crates/xtask/src/darwin.rs) signs `kbf-daemon` with
`codesign --force --sign - --options runtime`: an ad-hoc signature (no certificate)
with the hardened runtime. That works without an Apple Developer account, and the job
checks it every run: `codesign --verify --strict` passes, the signed binary runs (`kbf-daemon --version`
exits 0 with no `DYLD_*` variable set, on every runner), the code directory's flags are
exactly `adhoc,runtime`, the signature carries exactly the entitlements the allow
list `tools/ci/darwin-entitlements.txt` gives the binary (none for `kbf-daemon`; only
the `kbf-vmm` stub gets one, `com.apple.security.virtualization`;
[macos-vm-guests.md](design/macos-vm-guests.md) section 7), and the binary's minimum
macOS is 14.0. After packaging, `cargo xtask darwin-asset` takes the binary back out
of its tarball, the bytes that ship (refusing a tarball that is not exactly the one
entry `package` writes), and checks it again: `codesign --verify --strict`, the flags,
a run of `--version`, and the entitlements against the list.

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
  signed. That is fine for `kbf-daemon`, which links and loads only system libraries.
  On a Mac with System Integrity Protection on, a library it linked at start-up that
  Apple did not sign would stop it from starting. CI does not show this: the hosted
  runners have SIP off and library validation off (`vm.cs_library_validation` 0), so
  the check above that runs the signed binary would pass with such a library.

Developer ID signing and notarization need an Apple Developer Program membership; when
the project has one, it is one more step in the darwin job.
