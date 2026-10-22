# Spike: `kbf-vmm probe` on a Mac with virtualization

The gates in [macos-vm-guests.md](../design/macos-vm-guests.md) section 11 (G1, G2, G3,
G5/P5, P0, P2, P6) and the timings in [macos-vms.md](../design/macos-vms.md) need a host
that can run a macOS guest. A GitHub-hosted macOS runner cannot (it is a virtual machine
without nested virtualization), so CI builds, signs and tests what needs no hypervisor and
a person runs the rest on a Mac. Every command prints one JSON object on stdout. Status
`ok` means the probe ran and `details` holds the answer, whatever it is; `needs-manual`
means something the binary cannot create is missing and `reason` says what; `error` means
the probe broke.

## Getting the binary

The `vmm-probe` workflow builds `kbf-vmm` and `kbf-guest` for darwin-arm64 from a commit,
signs both with `cargo xtask sign-darwin` (ad hoc, hardened runtime; `kbf-vmm` alone gets
`com.apple.security.virtualization`, from `tools/ci/darwin-entitlements.txt`), and keeps
them as the artifact `vmm-probe-darwin-arm64` for 30 days. It is not attested; check the
SHA-256 in the job summary.

```sh
gh run list -R komira-ai/kbf --workflow vmm-probe.yml --branch <branch> --limit 1
gh run download <run-id> -R komira-ai/kbf -n vmm-probe-darwin-arm64 -D vmm
shasum -a 256 vmm/*.tar.gz            # compare with the job summary
tar -xzf vmm/kbf-vmm-*.tar.gz -C vmm && tar -xzf vmm/kbf-guest-*.tar.gz -C vmm
xattr -d com.apple.quarantine vmm/kbf-vmm vmm/kbf-guest 2>/dev/null
codesign -dv --entitlements - vmm/kbf-vmm   # adhoc, runtime, the virtualization key
```

## Running

Use a drained Mac, a work directory on one APFS volume with room for the base image and a
few clones (`clonefile` does not cross volumes), and nobody logged in for G5.

```sh
W=/Volumes/vm/work
vmm/kbf-vmm image base --ipsw <url-or-path> --sha256 <hex> --out $W/base --offline-setup   # as root for --offline-setup
for g in g1 g2 g3 g5 p0 p2 p6; do sudo vmm/kbf-vmm probe $g --work-dir $W --guest-binary vmm/kbf-guest; done
sudo vmm/kbf-vmm probe-bench --work-dir $W --guest-binary vmm/kbf-guest --runs 20
```

`--offline-setup` is probe P0's question applied to the new image: as root it attaches the
disk image, writes the Setup Assistant marker and four launch daemons (a probe as root, a
probe as `nobody`, a marker when the guest's launchd runs, a marker when the login window
exists) into the guest's Data volume, and detaches. Without it, or if it fails, the
probes that need code running in the guest report `needs-manual`.

G5 only answers fully when `kbf-vmm probe g5` is itself a child of launchd (a launch
daemon that runs it) as root; otherwise it runs the arms it can and names what is missing.
`probe-bench` empties the page cache for its cold runs with `purge` (root); without root
it reports only warm runs.

## What is not known until a Mac runs it

Nothing here has run against a hypervisor. The macOS code type-checks against
`aarch64-apple-darwin`; the parts that call Virtualization.framework (`src/vz/`), the
`hdiutil` steps (`src/offline.rs`) and the guest-side probes (`kbf-guest probe-guest`) are
exercised only by a run on a Mac.
