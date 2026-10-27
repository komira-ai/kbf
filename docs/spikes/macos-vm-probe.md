# Spike: `kbf-vmm probe` on a Mac with virtualization

The gates in [macos-vm-guests.md](../design/macos-vm-guests.md) section 11 (G1, G2, G3,
G5/P5, P0, P2, P6) and the timings in [macos-vms.md](../design/macos-vms.md) need a host
that can run a macOS guest. A GitHub-hosted macOS runner cannot (it is a virtual machine
without nested virtualization), so CI builds, signs and tests what needs no hypervisor and
a person runs the rest on a Mac. Every command prints one JSON object on stdout. Status
`ok` means the probe ran and `details` holds the answer, whatever it is; `needs-manual`
means something the binary cannot create is missing and `reason` says what; `error` means
the probe broke. A `needs-manual` result exits 0, so check the `status` field, not the
exit code.

## Getting the binary

The `vmm-probe` workflow builds `kbf-vmm` and `kbf-guest` for darwin-arm64 from a commit,
signs both with `cargo xtask sign-darwin` (ad hoc, hardened runtime; `kbf-vmm` alone gets
`com.apple.security.virtualization`, from `tools/ci/darwin-entitlements.txt`), and keeps
them as the artifact `vmm-probe-darwin-arm64` for 30 days. It is not attested; check the
SHA-256 in the job summary.

```sh
RUN=$(gh run list -R komira-ai/kbf --workflow vmm-probe.yml --branch <branch> --limit 1 --json databaseId -q '.[0].databaseId')
gh run download "$RUN" -R komira-ai/kbf -n vmm-probe-darwin-arm64 -D vmm
shasum -a 256 vmm/*.tar.gz            # compare with the job summary
tar -xzf vmm/kbf-vmm-*.tar.gz -C vmm && tar -xzf vmm/kbf-guest-*.tar.gz -C vmm
xattr -d com.apple.quarantine vmm/kbf-vmm vmm/kbf-guest 2>/dev/null
codesign -dv --entitlements - vmm/kbf-vmm   # adhoc, runtime, the virtualization key
```

## Running

Use a drained Mac: no other virtual machine running (`pgrep -fl
com.apple.Virtualization.VirtualMachine` prints nothing; quit any other hypervisor
application), nobody logged in at the console for G5, and one APFS volume for the
binaries and the work directory (`clonefile` does not cross volumes) with room for the
base image and a few clones. G5 starts a helper as a throwaway user, so that user must be
able to reach the binary and the work directory: keep both under a world-searchable path
such as `/usr/local`, not under a home directory.

The base image is an `.ipsw` for a macOS release this Mac can run as a guest (Apple's
restore image for the current release, or a file from an image catalog). `--sha256` is
the digest that file must have; take it from the publisher, not from the copy you
downloaded. `--ipsw` is a path or an `https://` URL (plain `http://` is refused). The
file is copied into the new bundle directory while it is hashed, and the installer only
sees that copy.

```sh
D=/usr/local/kbf-vmm; sudo mkdir -p $D && sudo cp vmm/kbf-vmm vmm/kbf-guest $D/
W=/usr/local/kbf-vmm-work; sudo mkdir -p $W
sudo $D/kbf-vmm image base --ipsw <https-url-or-path> --sha256 <hex> --out $W/base --offline-setup --guest-binary $D/kbf-guest | tee $W/image-base.json
for g in g1 g2 g3 p0 p2 p6; do sudo $D/kbf-vmm probe $g --work-dir $W --guest-binary $D/kbf-guest | tee $W/$g.json; done
sudo $D/kbf-vmm probe-bench --work-dir $W --guest-binary $D/kbf-guest --runs 20 | tee $W/bench.json
```

The commands run as root because the offline write attaches a disk image, `probe-bench`
empties the page cache for its cold runs with `purge`, and P0 and G5 start helpers as
another user. A failed `image base` removes the directory it made, so it can be run
again.

`--offline-setup` is probe P0's question applied to the new image: as root it attaches the
disk image, writes the Setup Assistant marker, a mount point, a copy of `kbf-guest`
(`/usr/local/libexec/kbf-guest` on the guest's own disk, which is why `--guest-binary` is
required here) and six launch daemons into the guest's Data volume, and detaches. The
daemons are a root daemon that mounts the share at `/private/var/kbf-mnt` (so the guest
side needs no logged-in user) by running `kbf-guest mount-share` from that copy, a probe as root, a probe as `nobody`, a marker when the guest's launchd runs, a
marker when the login window exists, and a local marker (`com.kbf.probe.local`): `sysctl`
run by launchd with its output going to `/private/var/log/kbf-probe-local` on the guest's
own disk (the boot time and OS version), depending on nothing else, not even a shell. The
Data volume is chosen by what each mounted volume holds (a volume with `private/var/db`
that is not the sealed System volume, which has an empty one too), never by mount order;
if no volume or several qualify the step fails and lists what each volume holds. Every marker JSON records how long the guest waited
for the share (`share_wait_ms`); "boot to system ready" includes that wait. If the image
was built without the step, or the step failed, apply it to the existing base:

```sh
sudo $D/kbf-vmm image setup --base $W/base --guest-binary $D/kbf-guest
```

The mount daemon is not restarted by launchd. `kbf-guest mount-share` tries the mount 40
times, 3 seconds apart, writes every failed attempt to `/var/log/com.kbf.probe.mount.log`
in the guest and, when none worked, a last line naming the tag, the mount point, the count
and the last error, then exits non-zero once. A log with more than 40 attempts, or a
`mount_virtiofs` error repeated forever, is therefore not what this build writes.

Each probe VM has a graphics device unless `--headless` is given (a headless guest
crash-loops its window server, see E1). `probe e1` keeps its first arm headless either
way, as the comparison.

Without the daemons the probes that need code running in the guest report `needs-manual`
and list the possible causes. The guest's answers and markers do not need the share: they
travel over virtio-socket into a directory only the host can write (see "How results reach
the host").

G1 judges a clone booted by the framework's running state held for 20 seconds. With
`--guest-binary` and the daemons in the base it also waits for each clone to write the
system marker and reports how many reached guest userspace; without them the summary says
the evidence is weak.

G5 (a VM start from a launch daemon, nobody logged in, as a throwaway uid, with and
without graphics) answers fully only when `kbf-vmm probe g5` runs as root in launchd's
`System` domain with the console unowned. A shell, `sudo -b` or `nohup` does not qualify
(the report records `launchctl managername` and the parent pid), so it is not in the loop
above. Run it as a one-shot launch daemon:

```sh
cat > $W/g5.plist <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.kbf.probe.g5</string>
  <key>ProgramArguments</key><array>
    <string>$D/kbf-vmm</string><string>probe</string><string>g5</string>
    <string>--work-dir</string><string>$W</string>
    <string>--guest-binary</string><string>$D/kbf-guest</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>$W/g5.json</string>
  <key>StandardErrorPath</key><string>$W/g5.err</string>
</dict></plist>
EOF
sudo cp $W/g5.plist /Library/LaunchDaemons/com.kbf.probe.g5.plist
sudo chown root:wheel /Library/LaunchDaemons/com.kbf.probe.g5.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.kbf.probe.g5.plist
# wait until the job has finished and $W/g5.json holds one line
sudo launchctl bootout system/com.kbf.probe.g5
sudo rm /Library/LaunchDaemons/com.kbf.probe.g5.plist
```

## E1: why a booted guest never reaches its userspace

G1 and the other guest gates can say that a clone booted (the framework reported
"running") and that no marker came; they cannot say why. `probe e1` is one run that tells
the causes apart. It starts two clones of the base at once, `e1-x` headless and `e1-y`
with a graphics device (the default; `--headless` removes it from the second arm too), and for the whole guest wait (320 seconds) records
every helper event with its arrival time, samples every 10 seconds the helper's CPU
time, the CPU time of every `com.apple.Virtualization.VirtualMachine` service process
(the one whose CPU rises belongs to the guest that executes) and the clone's `disk.img`
allocation, and waits for the frames the guest sends over virtio-socket (see "How results
reach the host", below). A stop or error event ends that arm's wait at once and is
reported. `--keep` leaves both clones and their shares in the work directory.
Then `image harvest` reads the guest's own logs off each kept disk.

Run the probe as the console user (the one logged in at the Mac's own screen; a root run
or an ssh run has no session and fails to start a guest), and the setup and the harvest as root. Each
harvest writes a new directory and refuses one that exists.

```sh
# 1. as root, once: put the current daemons (including the local marker) in the base
sudo $D/kbf-vmm image setup --base $W/base --guest-binary $D/kbf-guest | tee $W/e1-setup.json
# 2. as the console user, from Terminal on the Mac (not sudo, not ssh): about 6 minutes
$D/kbf-vmm probe e1 --work-dir $W --guest-binary $D/kbf-guest --keep | tee $W/e1.json
# 3. as root: copy the allowlisted files off each kept clone's disk
sudo $D/kbf-vmm image harvest --bundle $W/e1-x --out $W/e1-x-harvest | tee $W/e1-harvest-x.json
sudo $D/kbf-vmm image harvest --bundle $W/e1-y --out $W/e1-y-harvest | tee $W/e1-harvest-y.json
sudo chown -R "$(id -un)" $W/e1-x-harvest $W/e1-y-harvest
```

`image harvest` attaches the disk read-only, picks the Data volume as `image setup` does,
and copies only: `private/var/log/kbf-probe-local`, `private/var/log/com.kbf.probe.*.log`,
and everything under `private/var/db/diagnostics`, `private/var/db/uuidtext`,
`private/var/db/timesync` and `Library/Logs/DiagnosticReports`. A symbolic link, anywhere
on the way or inside a tree, is listed and not followed; a pipe, device or directory where
a file belongs is listed; a file over 256 MiB, a total over 2 GiB, more than 50000 entries
or a tree deeper than 8 levels stops the copy of the rest of that part and is listed. The
JSON lists what was copied (`copied`), what was refused and why (`skipped`), which
allowlisted paths the guest never made (`absent`), and the times of the Setup Assistant
marker and the log directory (`stats`).

To read the guest's log, make an archive from the copied files and ask `log` about the
boot (this assembly has not been tried on a real copy; if `log` refuses the directory,
`log show --file <file>.tracev3 --info --debug` reads the files one at a time):

```sh
A=$W/e1-x.logarchive; mkdir -p $A
cp -R $W/e1-x-harvest/private/var/db/diagnostics/. $A/
cp -R $W/e1-x-harvest/private/var/db/uuidtext/. $A/
cp -R $W/e1-x-harvest/private/var/db/timesync $A/ 2>/dev/null
log show --archive $A --info --debug --predicate 'process == "launchd" OR process == "mount_virtiofs" OR process CONTAINS "kbf" OR eventMessage CONTAINS "AMFI"' | tail -200
```

Read the report (`e1.json`, per arm under `details.arms`) and the harvest together:

| observation | cause |
|---|---|
| In `activity`, `disk_blocks_512_grown` is 0 or stops growing within seconds (`disk_last_growth_ms`), `helper_cpu_ms_gained` and `vm_service_max_cpu_ms_gained_between_samples` are small; or `trace.ended` names a stop or error; and the harvest has no `kbf-probe-local` and no diagnostics newer than the setup | The guest never runs launchd to a steady state: it stalls or powers off early. If only the headless arm is like this and the graphics arm differs, the missing display is the cause |
| CPU and disk writes keep going, `kbf-probe-local` is absent, and the diagnostics show launchd running | launchd did not load our plists: the plists are ignored, or the setup wrote to a volume the guest does not boot from. The log lines name the reason; `stats` shows the marker time |
| `kbf-probe-local` is present, `com.kbf.probe.mount.log` ends with `did not mount ... after 40 attempts` (or `com.kbf.probe.system.log` says the binary under `/private/var/kbf-mnt` does not exist) | The share never mounted: the last error in the log is the reason. The first E1 run's `Resource busy` was the automount tag mounted by hand; the daemon now mounts its own tag, so a `Resource busy` here means something else holds the mount point |
| `kbf-probe-local` is present, `com.kbf.probe.mount.log` is empty, and `com.kbf.probe.system.log` shows a permission or `Killed: 9` error (or the unified log shows AMFI killing the process) | The guest binary cannot run from the share: `nobody` cannot traverse it, the mount is `noexec`, or code signing kills the binary |
| Everything is present and `guest_marker_ms` is set, or the marker appeared only in the graphics arm | The guest is only slow (beyond 320 seconds), or it needs the graphics device |

`kbf-probe-local` in the manifest's `copied` list has the guest's own modification time
(`mtime_unix`): that is when launchd ran the daemon, to compare with the boot time the file
holds and with the setup marker's time in `stats`. Compare the two arms: a file in one and
not the other points at the graphics device.

### How results reach the host (current)

This is the design the code has now; the run sections after it are history, and each says
where it is superseded.

- The guest sends every result and marker to the host on virtio-socket port 5000 as a frame
  (`kbf_guest::frame`): `ready` (a marker), `result` (a probe's answer, the diagnosis) and
  `log`. The host answers each frame with `ack` or `refused`.
- The host writes the frames into `<work dir>/<vm>-results`, a directory made `0700` before
  the VM starts and outside every directory the guest can write to (the share is
  `<vm>-share`). The helper opens that directory once, without following a link, and makes
  every file through that descriptor (`openat`, `renameat`), so nothing the guest does to
  the share can redirect a write. A VM may create at most 64 files and 64 MiB in it, a log
  is capped at 4 MiB, a connection carries at most 256 frames, and each frame must arrive
  within 30 seconds (a connection whose socket timeouts cannot be set is refused).
- The probes read only that directory (E1's `frames`, G2's answer). The share is not used
  for results. A guest that cannot reach vsock falls back to writing the file into the
  share's `results/` directory, and E1 reports those files as `share_files`; they are not
  counted as frames, because a guest can write anything there.
- The guest runs `kbf-guest` from its own disk (`/usr/local/libexec/kbf-guest`), not from
  the share, which is not mounted when launchd loads the jobs.

### E1 result, first run, and the fix

> Superseded: results and markers no longer go through the share (see "How results reach
> the host"); the diagnosis below is still the record of what the first run showed.

The first run (a Mac Studio, macOS 27.0.1 guest and host) showed that both guests boot
fully and launchd runs the offline-written plists: `com.kbf.probe.local` wrote
`/private/var/log/kbf-probe-local` with the boot time and `27.0.1` in both clones, so the
offline write reached the volume the guest boots from. The share never mounted: in both
clones `com.kbf.probe.mount.log` held 101 copies of `mount_virtiofs: failed to mount
/private/var/kbf-mnt: Resource busy`, and `com.kbf.probe.root.log` and
`com.kbf.probe.local.log` were empty, so `kbf-guest` never ran from the share and the
system marker never came. In the headless clone WindowServer aborted (SIGABRT) about every
30 seconds (ten crash reports in `Library/Logs/DiagnosticReports`); the graphics clone had
none.

The cause, from the code: the mount daemon mounted the tag
`com.apple.virtio-fs.automount`, the tag `kbf-vmm` attaches the automount share under, and
the guest already mounts that device itself at `/Volumes/My Shared Files`; a second mount
of the same device is `EBUSY`. The daemon was also restarted by launchd every 3 seconds
forever, which made the 101 lines. (That the automount mount existed was not read from the
guest in that run; it is the explanation that fits `Resource busy` and the code. The next
E1 run shows whether the mount now works.)

What changed:

| share | host directory | tag | where the guest sees it |
|---|---|---|---|
| automount | the share directory (`kbf-guest`, `results`) as the folder `kbf` | `com.apple.virtio-fs.automount`, mounted by the system | `/Volumes/My Shared Files/kbf` once a session exists; only the probes' `automount` question reads it |
| own | the same share directory | `kbfshare`, mounted by `kbf-guest mount-share` | `/private/var/kbf-mnt`; the daemons run `kbf-guest` from it and write `results` into it |
| probe | an empty world-writable directory | `kbfprobe` | mounted and unmounted by `probe-guest` at `/private/tmp/kbfmnt-*` to answer G3 |

Why a tag of our own rather than waiting for the automount path: the automount is a
system service whose timing and session dependence were not measured, and a daemon that
waits for it has nothing to say if it never appears. A tag we mount ourselves depends on
nothing but the device and fails with the tool's own error. The automount share stays so
that the probes still report whether it is usable. The mount is run from a copy of
`kbf-guest` on the guest's disk because the copy on the share cannot be reached before the
share is mounted; and `kbf-guest mount-share` refuses the automount tag.

### E1 result, second run: the share mounts, every write into it is refused

> History: the share is no longer a results channel.

With the own-tag mount, `com.kbf.probe.mount.log` is empty in both clones and `kbf-guest`
runs from the share (`com.kbf.probe.root.log` is written), so the guest reaches userspace
and runs our code; both arms behave the same, so the graphics device does not matter for
this. But `com.kbf.probe.root.log` holds 99 copies of `kbf-guest: write
/private/var/kbf-mnt/results/guest-probe-root.json: Operation not permitted (os error 1)`
and once `umount: unmount(/private/tmp/kbfmnt-root): Operation not permitted`, so no
result and no `ready-system` marker ever reaches the host.

What the code and the logs settle, and what they do not:

- The host side is not read-only (`vz/config.rs` passes `readOnly` false for both kinds of
  share), `results/` is made `0777` by the host, and the share lives in the console user's
  home. Apple documents that the framework reads and writes host files as the effective
  user of the VM process, ignoring a guest's `chown`, so a mode or an owner on the host
  does not explain a guest root being refused.
- The refusal is the same for a root file write and for a root `unmount`, which points at
  a policy in the guest that is not a file mode: a MAC-framework check (sandbox, TCC
  network-volume access, which applies to processes started by launchd that are not apps)
  or a mount flag. That is a hypothesis; nothing here proves it.
- The old code wrote through a temporary file and a rename and reported only "write": it
  could not say whether the create, the write or the rename was refused. It now tries
  the temporary file route first and then writes the result in place, and its error text
  names each step that failed. If only the rename was refused, the result files now land.
- The logs of the daemons that run as `nobody` were absent in both clones. `nobody` cannot
  create a file in `/var/log`, which is where launchd was told to put them; these daemons
  now log in `/private/var/tmp`, and the harvest copies `com.kbf.probe.*.log` from there.
  (Whether they ran at all is not known from run two.)

New diagnostics for the next run. `kbf-guest probe-diag` runs as a root daemon
(`com.kbf.probe.diag`) from the copy of `kbf-guest` on the guest's disk, so it reports even
when the mount fails. It waits up to 5 minutes for `/private/var/kbf-mnt/results`, then
prints one JSON object to `/private/var/log/kbf-probe-diag` (the harvest copies it):
`id`, `mount` (the mount table entry and its flags), `sw_vers`, `csrutil status`, `ls
-ldeO@` and `stat` (owner, group, mode, flags, extended attributes, ACL) of the mount
point, of `results/` and of a control directory on the guest's disk, and, for the mount
point, `results/` and the control directory, each step of a write (create, write, rename,
mkdir, remove) with its errno and the first step that failed. It also mounts the
`kbfprobe` tag at `/private/tmp/kbfmnt-diag`, runs the same write steps inside it, and
unmounts it with the system call, reporting the errno of each. Read it like this:

| `writes` | meaning |
|---|---|
| control all ok, share root and `results/` fail at `create` | the guest refuses to create files on virtiofs for root; compare `ls` flags and the unified log around the time (`log show --archive ... --predicate 'process == "tccd" OR process == "sandboxd" OR eventMessage CONTAINS "deny"'`) |
| `create` and `write` ok, `rename` fails | virtiofs refuses a rename; the in-place fallback now covers it |
| `results/` fails and the mount root works (or the reverse) | a per-directory difference: owner or mode as the guest maps it |
| everything ok in the diagnosis but `com.kbf.probe.root.log` still shows the refusal | the refusal is specific to the binary running from the share (the process is located on the volume it writes to) |

### E1 result, third run: nothing can be created in any share

> History: the share is no longer a results channel.

`probe-diag` settled the guest side. The first step of every write fails: `create`, with
`EPERM`, for root (uid 0, all groups), in the share root `/private/var/kbf-mnt`, in
`results/` (shown `drwxrwxrwx`, owner `_unknown`) and in a second mount of the `kbfprobe`
tag; `mkdir` fails the same way. The control directory on the guest's disk passes every
step. The daemons that run as `nobody` fail the same way (about 95 `create ...
Operation not permitted` lines each), so no guest identity can create anything in a
virtiofs share. The mount shows `(AppleVirtIOFS, nodev, nosuid)`, SIP is on, and
unmounting works now. The host's unified log shows no TCC or sandbox denial in the window.
So the refusal is on the host side of the share, or in how virtiofs maps ownership for a
macOS guest (every file shows owner uid 99).

Apple's documentation for `VZSharedDirectory` and `VZVirtioFileSystemDeviceConfiguration`
says nothing about the host directory's owner, mode, location or path form (no
requirement of a real path, no statement for `/Users` against `/private`), says the mount
commands are meant to be run by a user logged in to the guest, and gives automount
examples that share a directory in the user's home. It names no cause, so there is no
direct fix; the next run is a matrix.

### The share matrix

`probe e1` now gives each VM, besides the three shares above, one extra virtiofs device
per row of this table (`--no-matrix` leaves them out). Each has its own host directory
`kbf-<vm>-mx-<name>` and tag `kbfmx<i>`; rows differ from `base` in one factor.

| name | what differs from `base` (work directory, mode 0777, owned by the user running `kbf-vmm`, single-directory share, writable) |
|---|---|
| `base` | nothing |
| `mode755` | mode 0755 |
| `tmp` | location `/private/tmp` |
| `shared` | location the shared-users directory under the Users folder (`Shared`) |
| `multiple` | a multiple-directory share (the directory is listed as `m`) |
| `other-owner` | owned by another user; made only by a root run, otherwise listed as skipped |
| `control-mode555` | negative control: mode 0555, nobody can write on the host |
| `control-readonly` | negative control: the share is marked read-only |

The host writes the list into `matrix.json` in the main share. The guest (`com.kbf.probe.diag`)
mounts each tag at `/private/tmp/kbfmx-<name>`, runs the write steps in it (in `m/` for the
multiple-directory share), and unmounts it. The automount share
(`/Volumes/My Shared Files/kbf`) is written to as well. After the run the host lists every
matrix directory in `details.arms[].matrix_host`: its real path, owner and mode, and every
file the guest left in it (anything listed landed on the host).

Read `matrix` in `<harvest>/private/var/log/kbf-probe-diag` (one row per share, with
`cycle.inside.steps` and `first_failed_step`) together with `matrix_host` in `e1.json`:

| observation | meaning |
|---|---|
| every row fails at `create`, including `tmp` and `shared`, the host listings are empty | the location, the mode and the single/multiple type do not matter; the refusal is in the guest's virtiofs for any host directory (or in the Virtualization service), not in what we pass |
| `base` fails but `tmp` or `shared` works (or the reverse) | the location matters; compare `real_path` (a path through a symbolic link or a protected folder) and the host's TCC rules for the home directory |
| `base` fails but `mode755` works | the mode matters (it should not: the guest cannot see the host mode) |
| `multiple` works and the single-directory rows fail | single-directory shares are read-only for guests in practice; share a multiple-directory share instead |
| `control-mode555` and `base` fail alike, with the same errno | the host's file permissions are the cause; check who owns `base` and the VM helper's user in `matrix_host` |
| `control-readonly` fails with a different errno than `base` (`EROFS` against `EPERM`) | `base` is not being treated as read-only; the refusal is something else |
| a row fails in the guest but its file is listed in `matrix_host` | the write reached the host and the guest was told it failed |
| `other-owner` works and `base` fails | the directory must not be owned by the VM's user (unlikely; re-run as root to get this row) |
| the automount write works and the matrix rows fail | use the automount path for results (it needs a user session or the system's own mount; see `automount` in the guest's probe result) |

### E1 result, fourth run: no file can be read or written on a share, so results go over vsock

Four runs on a Mac Studio (host and guest on macOS 27.0.1) gave one answer: the guest boots,
launchd runs the daemons written offline, and the share mounts, but inside the guest every
file read or write on a virtiofs share fails with `EPERM`, for root and for `nobody`
(in the fourth run the guest could not even read `matrix.json`). Listing a directory works
and files show as owned by `_unknown`. The guest's own disk is fine and the host logs no
sandbox or TCC denial. Apple documents the share for a user logged in to the guest; the
probe guests rest at the login window. So results and markers no longer depend on the share:

- the host attaches a virtio socket device and listens on port 5000 (`VmSpec.vsock_ports`,
  `VmSpec.results_dir`); `src/sink.rs` writes each frame it receives into the VM's
  host-only results directory (`<vm>-results`; the first version of this change wrote into
  the share's `results/`, which a guest could redirect with a link, and that is superseded,
  see "How results reach the host") under the name the probes wait for;
- `kbf-guest probe-guest` and `probe-ready` send their answer first as a frame
  (`kbf_guest::frame`: a four-byte big-endian length, then JSON `{"kind", "name", "body"}`
  with `kind` one of `ready`, `result`, `log`; the host answers `ack` or `refused`) and
  write the file into the share only when vsock cannot be used. Each attempt, up to five two
  seconds apart, and each failure is logged to the daemon's log;
- `probe-diag` reports `vsock`: whether the guest connected and whether a `log` frame named
  `vsock-test` came back acknowledged.

What to read in the next E1 run:

| where | line | meaning |
|---|---|---|
| `e1.json`, each arm | `guest_marker_ms` is a number and `results_files` lists `ready-system`, `ready-login`, `guest-probe-root.json`, `guest-probe-user.json`, `vsock-test.log` | the vsock route works end to end |
| `e1.json`, each arm | `helper_log` has `kbf-vmm: vsock Ready ready-system -> .../results/ready-system` | the host received and wrote the frame |
| `<harvest>/private/var/log/kbf-probe-diag` | `"vsock": {"connected": true, "round_trip": true}` | the guest reached the host and got an acknowledgement |
| same | `"connected": false` with `"stage": "socket"` and an `errno` | the guest kernel has no vsock socket family for processes (`EAFNOSUPPORT`, 47): the premise of this change is wrong and a different channel is needed |
| same | `"stage": "connect"` | the family exists but nothing accepted: look at the listener (`helper_log` has no `vsock port` line) and at whether the VM has a socket device |
| same | `"connected": true`, `"round_trip": false` | the host received the connection and the frames were not answered; the `error` says which side |
| `com.kbf.probe.system.log` in the harvest (`/private/var/tmp`, the log directory of the daemons that run as `nobody`) | `vsock port 5000: ready-system delivered on attempt N` | the guest used vsock; `vsock not usable ... writing ... instead` means it fell back to the share |

The macOS socket family (`AF_VSOCK`, 40) and the address layout (`struct sockaddr_vm` with a
length byte, as in the macOS SDK) are taken from the SDK headers and have not run in a macOS
guest yet; the host's use of the connection's descriptor as an ordinary stream socket is
Apple-undocumented. Both are what this run answers.

### E1 result, fifth run: vsock works on a Studio; the headless guest's system job never started

The fifth run (results under `<work dir>/e1-run-b5/results/`) proved the vsock route on a
Mac Studio, host and guest on macOS 27.0.1:

- both arms delivered `ready-login`, `guest-probe-root.json`, `guest-probe-user.json` and
  `vsock-test` over vsock, each "on attempt 1" in the daemons' logs; the helper logs show
  every frame stored (`kbf-vmm: vsock Ready ...`, `Result ...`, `Log vsock-test ...`);
- `probe-diag` in both guests reports `"vsock": {"connected": true, "round_trip": true}`, so
  the guest kernel has the socket family, the address layout is right and the host's use of
  the connection's descriptor as a stream socket works;
- file operations on the share still fail with `EPERM` in the guest (`matrix.json` and the
  second mount's create), which is why the results no longer use it;
- the graphics arm also delivered `ready-system`, with its guest marker at 15821 ms.

The headless arm never sent `ready-system`: `e1.json` has `guest_marker_ms` null for it
although its other four jobs ran. Its `com.kbf.probe.system.log` exists and is empty.

Cause. The five probe jobs ran `kbf-guest` from the share (`/private/var/kbf-mnt/kbf-guest`),
which exists only after the mount daemon has mounted the share; the plists have
`RunAtLoad` and nothing that waits for the mount, and the daemons' `--wait-path` only
starts after the program has been executed. launchd checks the program path when it loads a
job. The headless guest's unified log (`private/var/db/diagnostics`, read with `strings`)
holds, next to the `com.kbf.probe.system` and `com.kbf.probe.login` jobs, `Service could not
initialize: access(/private/var/kbf-mnt/kbf-guest, X_OK) failed with errno 2 - No such file
or directory` and `Could not find and/or execute program specified by service: 2: No such
file or directory: /private/var/kbf-mnt/kbf-guest`. The graphics arm's log has the same
`Could not find and/or execute` line for the login job, and that job delivered later, so
launchd sometimes starts such a job again and sometimes does not; the system job on the
headless arm was the one that did not. An empty log file is what a job that launchd
could not execute leaves: launchd creates the file, the program never runs. The text of the
launchd message in the headless log is not tied to a label there (the strings are
fragments), so the match to the system job rests on the empty log, the other four jobs
having run, and the identical path; no line shows launchd giving up on that job.

Fix (this change). The probe jobs run the copy of `kbf-guest` on the guest's own disk
(`/usr/local/libexec/kbf-guest`, the one the mount daemon and `probe-diag` already use), so
the program always exists at load; the jobs still wait for the share's `results` directory
before they probe. Each probe also writes `kbf-guest: started, will deliver <file> (uid,
euid, session)` to its log before it waits, so an empty log now means the job did not run.
E1's guest marker no longer rests on the system job: the first of `ready-system`,
`ready-login`, `guest-probe-root.json` and `guest-probe-user.json` to arrive counts, and
`e1.json` reports which one in `guest_marker` next to `guest_marker_ms` (the summary prints
`guest marker <name> at <ms>`). Both were chosen: the path fix removes the cause, and the
wider marker keeps a future single failing job from reading as a silent guest.

What to read in the next E1 run:

| where | line | meaning |
|---|---|---|
| `e1.json`, each arm | `guest_marker` and a number in `guest_marker_ms` | which frame arrived first, and when; `ready-system` on both arms means the path fix worked |
| `<harvest>/private/var/tmp/com.kbf.probe.system.log` | `kbf-guest: started, will deliver ...` then `delivered on attempt 1` | the job ran and delivered |
| same | empty | the job still did not start: read the guest's unified log for the job's label |
| same | `started` and nothing after | the job started and is waiting: the share's `results` directory never appeared |

### E1 result, sixth run: the path fix works; E1 stopped the VMs too soon

The sixth run (results under `<work dir>/e1-run-b6/results/`, the head that ran the probe
jobs from the guest's own disk) confirmed the fix: both arms delivered `ready-system`
(13049 ms headless, 15352 ms with graphics), and the headless arm's four job logs each show
`started` and then `delivered on attempt 1`.

The new gap was the host's. E1 ended an arm at its first frame, took three samples and
force-stopped both VMs about 15 seconds after the first growth of the disk. The graphics
arm had delivered only `ready-system` and `guest-probe-user.json`; its root probe, login
marker and diagnosis had not run yet, and every guest log in both harvests but one was
empty, because a force stop leaves the guest's disk unflushed. So the run had no vsock
diagnosis at all.

Changes:

- E1 waits, per arm, for all five frames (`ready-system`, `ready-login`,
  `guest-probe-root.json`, `guest-probe-user.json`, `kbf-probe-diag.json`) or for the end
  of the VM or the guest timeout. `e1.json` lists each arrived frame with the host time it
  was first seen at (`frames`), the ones that never came (`frames_missing`), and keeps
  `guest_marker` and `guest_marker_ms` for the first. The summary line prints the missing
  frames.
- `probe-diag` also sends its whole report as a `result` frame named
  `kbf-probe-diag.json` (the same five attempts as a probe's answer, no write to the share
  on failure), so the diagnosis reaches the host's results directory without a harvest.
- Both VMs are asked to shut down before they are stopped. The helper reads a `shutdown`
  line on its stdin and calls `VZVirtualMachine.requestStopWithError` (the guest's
  power-button request, not the framework's force stop); the host waits up to a minute for
  the guest to power off and the helper to exit, then closes the helper's stdin (the force
  stop `stopWithCompletionHandler`), then kills the helper. `e1.json` has `stop.how`
  (`guest shut down`, `force-stopped after the guest did not shut down` or `helper
  killed`) and `stop.ms` for each arm. Whether a macOS guest at the login window honours
  the request is not known until a Mac runs it; `stop.how` answers it.

What to read in the next E1 run:

| where | line | meaning |
|---|---|---|
| `e1.json`, each arm | `frames_missing` is `[]` | every probe job ran; each frame's time is in `frames` |
| same | a name in `frames_missing` | that job did not deliver within the guest timeout; its log in the harvest says whether it started |
| `<vm>-results/kbf-probe-diag.json` (the host-only directory) | the diagnosis, including `"vsock": {"round_trip": true}` | the diagnosis arrived without the harvest |
| `e1.json`, each arm | `stop.how` is `guest shut down` | the request worked and the harvest logs are complete |
| same | `force-stopped ...` | the guest ignored the request; the harvest logs may be empty again |

### E1 result, seventh run: the channel is complete; the guest ignores the stop request; the host side hardened

The seventh run (results under `<work dir>/e1-run-b7/results/`) was green on the channel:
both arms had `frames_missing` empty, all five frames over vsock (headless 13.07 to 14.60
s, graphics 15.23 to 16.74 s), and the diagnosis reported `"connected": true,
"round_trip": true`. The guest at the login window ignored `requestStopWithError` on both
arms and was force-stopped after the 60 s wait, so the harvest logs are no longer empty
but the guest does not shut down on request. A guest-side shutdown on the host's request
(a clean `shutdown` run by the guest itself) is a follow-up, not part of this change.

A review of the host side found that the results directory was `<share>/results`, which
the guest can write to as root: it could rename it and put a link to any host directory in
its place, and the host's `create_new` and `rename` followed the link (`O_NOFOLLOW` covers
only the last path component), an arbitrary file write as the host user. Changes:

- frames go to `<work dir>/<vm>-results`, `0700`, outside the share, opened once with
  `O_DIRECTORY | O_NOFOLLOW` and written only through that descriptor (`openat`,
  `renameat`); the sink refuses a directory that is not private;
- E1 reads only that directory. A file a guest puts in the share is listed as
  `share_files` and is never a frame;
- one VM may create at most 64 files and 64 MiB, and the log cap is kept under one lock, so
  two connections cannot pass it together;
- each frame must arrive within 30 seconds (a read timeout alone let a guest holding a slot
  with one byte every 29 seconds), and a connection whose socket timeouts cannot be set is
  refused instead of read without a timeout.

What to read in the next E1 run: `results_files` (the host directory) lists the frames and
`share_files` is empty unless the vsock route failed; `frames_missing` is `[]`; a frame
in `share_files` and not in `results_files` means the guest used the fallback.

Runs 7 and 8 both ended with every VM force-stopped after the 60 s wait, so the stop
request is ignored by a guest at the login window and data the guest had not flushed is
lost. The host now asks over the virtio socket instead: image setup installs a root
LaunchDaemon (`com.kbf.probe.shutdown`, `kbf-guest shutdown-listen`, run from the guest's
disk with KeepAlive) that listens on guest vsock port 5001; on stop the helper connects to
it through `VZVirtioSocketDevice.connectToPort`, sends one `shutdown` frame in the same
codec as the results, and the daemon answers `ack` and runs `/sbin/shutdown -h now` (fixed
argv, no shell; it accepts only that frame, at most 256 bytes, and refuses everything
else). With an ack the host waits up to the shutdown timeout for the VM to stop; with none
within 5 s the helper falls back to `requestStopWithError`, then the force stop, as before.
Nothing of this has run in a guest: that a macOS guest serves `AF_VSOCK=40` to a process,
the `sockaddr_vm` layout, binding on any context id, and that `connectToPort` reaches the
listener are checked on a Studio next. To read in the next E1 run: each arm's `stop.how` is
`guest shut down (vsock)` when it worked (`guest shut down (requestStop)` when only the
fallback did, `force-stopped ...` when neither did), `stop.vsock_acked` and
`stop.vsock_detail` say whether the daemon acked and why not, the arm's trace holds a
`shutdown-command` event, and `/var/log/com.kbf.probe.shutdown.log` in the harvest shows
the listener's `bind` result and each connection.

## E3: Xcode and simulator runtimes in the guest image

The iOS simulator probe needs Xcode and a simulator runtime inside the guest. The base is
sized for it when it is built, and `image setup` stages the two inputs as root while the
guest is off:

```sh
sudo $D/kbf-vmm image base --ipsw <ipsw> --sha256 <digest> --out $W/base \
  --cpus 6 --memory-mib 16384 --disk-gib 200
sudo $D/kbf-vmm image setup --base $W/base --guest-binary $D/kbf-guest \
  --xcode /Applications/Xcode.app --sim-runtime ~/Downloads/iOS_27_Simulator_Runtime.dmg \
  | tee $W/e3-setup.json
```

- `--cpus`, `--memory-mib` and `--disk-gib` default to what they were before (the restore
  image's minimums, at least 2 CPUs and 4096 MiB, and 64 GiB). A value below the restore
  image's minimum is refused. The sizes the install used are written to `sizing.json` in
  the base bundle.
- `--xcode` clones the app into the guest's `/Applications/Xcode.app`. The guest disk is on
  another volume than the app, so in practice this is a file-by-file copy (a tree clone is
  tried first). Everything becomes owned by root:wheel; the setuid, setgid and sticky bits
  are dropped on both paths, and so are group and world write (a source 0775 is staged
  0755, 0666 is 0644; owner bits and the others are kept; a bundle's runtime files are
  fixed at 0644 and its directories at 0755). The file-by-file copy writes the planned modes
  and opens each
  source without following a link, refusing a file that is no longer a regular file. A tree
  clone copies the live tree, so it is walked again afterwards: it is refused unless it
  holds exactly the planned entries (same kinds, link targets and file sizes) and then gets
  the planned modes. Both paths keep extended attributes (the code signature of a file that
  is not a Mach-O lives in `com.apple.cs.*` attributes; resource forks and Finder info can
  matter too) except two. `com.apple.quarantine` would make the guest treat Xcode as
  downloaded. `com.apple.macl` is a per-machine access list that SIP protects (root cannot
  write it) and that means nothing on another machine; an App Store Xcode carries it on the
  app root, so copying it would fail the stage. (The other attributes measured on such an
  Xcode are `com.apple.appstore.metadata`, `.store_cohort`, `.storefront` and
  `.vendor_name`, on the app root; they are copied.) The file-by-file copy copies every
  attribute of a file (between the open files), a directory and a link (the link itself,
  not what it points at) except those two, and the clone, which carries them, has both
  removed from every node; if macOS refuses to remove `com.apple.macl` from the clone, that
  one refusal is ignored. A refusal to remove or write any other attribute fails the stage. An
  attribute list above 1 MiB or a value above 64 MiB fails the stage, naming the path,
  rather than being skipped (a skipped part of a signature would leave an Xcode that fails
  validation). Attributes are macOS-only code; other systems copy none. The new
  copy is built under a staging name and renamed into place, so a failed run never leaves a
  half Xcode.app under the real name; when the build or a check fails, the staging tree is
  removed as well. A rerun removes the earlier copy and then renames the
  new one in (a run that dies between the two leaves no Xcode.app), and it holds both
  copies until then, so the free-space check, which counts the earlier copy as used, needs
  room for both.
- A symbolic link in the app is recreated, never followed, when its target stays inside the
  app: relative with the `..` components first and no more than the link is deep, or an
  absolute path into `/Applications/Xcode.app`. Any other link, a socket, a pipe or a
  device is refused before the disk is touched. `--allow-external-links` copies such links
  as links and lists them in the report.
- `--sim-runtime` (repeatable, at most 8) takes either `<name>.dmg` (a file) or
  `<name>.exportedBundle` (a directory, what `xcodebuild -downloadPlatform iOS -exportPath
  <dir>` writes on current Xcode: `ExportedMetadata.plist` and a `Restore/` directory with
  the cryptex image, its `.trustcache`, `.cryptex_info` and `.root_hash`, and
  `BuildManifest.plist`/`Restore.plist`). The name is letters, digits, `.`, `_`, `-` plus
  the suffix (at most 64 bytes, no `..`, not hidden). An image is copied to
  `/private/var/kbf-sim/runtimes/<name>.dmg` while hashing it. A bundle is planned with the
  same walk as Xcode.app (nothing followed; regular files and directories only; a link only
  when its target stays inside the bundle; a socket, pipe or device refused; not
  `--allow-external-links`), counted in the free-space check, and copied file by file to
  `/private/var/kbf-sim/runtimes/<name>.exportedBundle`, each file hashed while it is
  copied; directories become 0755, files 0644, owner root:wheel. An image is opened without
  following a link and without blocking on a pipe, and must be a regular file. Each runtime
  is built in `/private/var/kbf-sim/.runtimes.kbf-staging` (beside the runtime directory,
  not in it, since the guest lists the directory) and renamed in when complete; if a build
  fails, that directory is removed, so no partial runtime is ever listed. At most the planned
  size of a file is copied (one byte more is read), and a source that grew or shrank fails
  the stage. The runtime directory then holds
  exactly the runtimes of the latest setup (a setup that names none empties it). The report
  and `staged.json` record an image's kind, name, size and SHA-256, and a bundle's kind,
  name, total size and a list of `{path, bytes, sha256}` for each file.
- The free space of the guest's Data volume is read first. If Xcode and the runtimes plus
  4 GiB of headroom do not fit, nothing is written and the error says what is needed, what
  is free and to rebuild the base with a larger `--disk-gib`.
- What was staged is also written into the guest as `/private/var/kbf-sim/staged.json`.
- Every directory the copy writes through on the guest volume must be a plain directory; a
  link or a file in the way is an error, so a guest that ran earlier cannot redirect the
  root copy. A leftover in the way (an earlier Xcode.app, a runtime, a staging name,
  `staged.json`) is removed without following a link, and a removal that fails (a file the
  guest made immutable) stops the stage with an error naming the path. Files are created
  exclusively (`staged.json` included) and their mode is set on the open file; a mode set
  later on an existing path opens it without following a link; ownership is `lchown`. A
  directory the stage creates on the way is given mode 0755 explicitly, whatever the umask.

### The guest's user: setup, prime

A guest built by `image base` and `image setup` has no user account. Its processes run as
`nobody` in the system session, and the simulator's user steps (which need an Aqua session)
have nothing to run in. `image setup --guest-user <name>` and `image prime` give the guest
the one user the design calls for (`docs/design/macos-vm-guests.md`): a standard
(non-admin) account with auto-login, with sleep, the screen saver and its lock, and the
update checks off. The order is always:

1. `image base` (as root: the offline write attaches a disk image; it leaves the base owned by root), then
2. `image setup` (as root) with `--xcode`, `--sim-runtime` and `--guest-user <name>`,
3. `image prime` (as the console user, not root: a guest needs a session; it writes to the base, so
   the console user must be able to write the base directory: hand it over, for example with
   `sudo chown -R $(id -un) $W/base`, after `setup`): boots the **base itself**, once, so that the
   user is created in it,
4. `probe e3` (as the console user): its clones boot directly into the user's session.

`--guest-user` stages a one-shot root launch daemon, `com.kbf.mkuser` (from the guest's own
disk, like the other daemons), and `/private/var/kbf-sim/user-wanted`, which holds the name.
While `user-wanted` exists and `user-ready` does not, `kbf-guest sim` runs nothing and
says to prime (the root steps would only import the runtimes into the base). The name is a
short name: it starts with a lowercase letter and holds lowercase letters, digits, `-` and
`_` (at most 31 bytes); `root`, `nobody`, `daemon`, `admin`, `guest` and `wheel` are refused.
Setup alone does not run the daemon: it needs the `kbf-guest` the guest disk holds (this
setup's `--guest-binary` or an earlier one).

`kbf-guest mkuser --name <name>` runs these steps, one command (or file write) each, in
order, each a frame `mkuser-step-NN-<step>.json` of the same shape as the simulator's
(`step`, `argv`, `status`, `exit`, `ms`, the 8 KiB tails), then `mkuser-summary.json`:

| step | what |
|---|---|
| `add-user` | `sysadminctl -addUser <name> -fullName <name> -password <generated> -home /Users/<name>` (no `-admin`: a standard user). If the user exists already: `set-password`, `dscl . -passwd /Users/<name> <generated>` |
| `home` | `createhomedir -c -u <name>` |
| `kcpassword` | writes `/etc/kcpassword`, mode 0600, root: the password XORed with the fixed 11-byte key, zero-padded to a multiple of 12 bytes |
| `autologin` | `defaults write /Library/Preferences/com.apple.loginwindow autoLoginUser -string <name>` |
| `pmset` | `pmset -a sleep 0 displaysleep 0 disksleep 0` |
| `hw-uuid` | `ioreg -rd1 -c IOPlatformExpertDevice`; the step keeps the `IOPlatformUUID` (8-4-4-4-12 hex digits, else the step fails) |
| `prefs-dirs` | makes `/Users/<name>/Library`, `.../Library/Preferences` and `.../Library/Preferences/ByHost` if missing, each owned by the user, mode 0700; a link or a file in the way is refused (opened without following links) |
| `screensaver-idle`, `screensaver-lock` | the user has never logged in, so no preferences service runs for it and `defaults <domain>` (or `sudo -u <name> defaults`) cannot write. These write the files **by path, as root**: `defaults write /Users/<name>/Library/Preferences/ByHost/com.apple.screensaver.<IOPlatformUUID> idleTime -int 0` and `defaults write /Users/<name>/Library/Preferences/com.apple.screensaver askForPassword -int 0` |
| `screensaver-own` | gives both `.plist` files to the user (its primary group), mode 0600, through the open handle (links refused) |
| `loginwindow-idle` | `defaults write /Library/Preferences/com.apple.screensaver loginWindowIdleTime -int 0` |
| `updates-check`, `updates-download`, `updates-os`, `updates-critical` | `defaults write /Library/Preferences/com.apple.SoftwareUpdate` `AutomaticCheckEnabled`, `AutomaticDownload`, `AutomaticallyInstallMacOSUpdates`, `CriticalUpdateInstall`, each `-bool false` |
| `ready-marker` | writes `/private/var/kbf-sim/user-ready` (the name), only when every other step succeeded |

The password is 11 random bytes from the guest's `/dev/urandom`, as 22 hex digits. It is
never sent to the host and never logged: a frame's `argv` shows `<withheld>` in its place,
and every occurrence of it in a command's output is replaced by `[redacted]` before the tail
is cut (so a cut cannot leave part of it; if the capture's own cap split it, the part at that end is dropped). It is in the
guest-side argument list of `sysadminctl` (or `dscl`) for the moment that command runs, so
another process of the guest could read it then; and it is in `/etc/kcpassword`, which is
an obfuscation (the key is fixed and documented), not encryption: root-only mode 0600 is the
protection. The account belongs to a throwaway guest and holds no credential of the farm.

Whether the guest's unified log records that password is audited inside the guest, because
the harvest of logs to the host is an allowlist, not an archive. Just before the
`ready-marker` (and whatever the earlier steps did) the daemon runs a root step, `log-audit`:
`log show --last 1h --info --debug --style compact --predicate '<sysadminctl or dscl
process, or a message with "-password" or "kcpassword">'`, and counts its entries
(`matched_lines`); then counts, in the whole `log show --last 1h --info --debug` output, the
lines that hold the password (`password_hits`) and the lines that hold any 8-character piece
of it (`window_hits`). The output is read as a stream into those counters and kept nowhere:
its frame, `mkuser-audit.json`, holds the three counts and the step's status and exit code,
never a line of the log, never the password, and no output tail. The step is informational:
it does not decide whether the user was made and does not gate the marker; a `log show` that
fails is reported by status and exit code only. `image prime` prints the frame as
`log_audit` in its JSON. A non-zero `password_hits` or `window_hits` means the log has the
password in it; zero in both, with status `ok`, means this run's log hour does not.

When every step is done the daemon powers the guest off (`/sbin/shutdown -h now`), so the
base's disk is flushed. It does so after a failed run too (the next `prime` starts the steps
again; no marker is left). If `user-ready` names the user already, the daemon does nothing,
sends a summary with `already_ready`, and does not power off: `image prime` then shuts the
guest down itself.

```sh
# as root; $RUNTIME is a <name>.dmg or a <name>.exportedBundle; --sim-app and --sim-tests
# are optional (UI tests in the guest's simulator: see below)
sudo $D/kbf-vmm image setup --base $W/base --guest-binary $D/kbf-guest \
  --xcode /Applications/Xcode.app --sim-runtime $RUNTIME [--sim-app $APP.app] \
  [--sim-tests $DD/Build/Products] --guest-user kbfsim | tee $W/e3-setup.json
# as the console user, from Terminal on the Mac: a few minutes, with a graphics window
$D/kbf-vmm image prime --base $W/base --guest-binary $D/kbf-guest [--deadline-secs 1200] \
  | tee $W/e3-prime.json
```

`image prime` boots the base itself (not a clone) with a graphics device and the vsock
channel, collects the frames into `<base>/prime-results` (a directory only the host user can
read, written through the same checked sink as the probes' results), and waits for the guest
to power off, at most `--deadline-secs` (default 1200). A guest still running then is
stopped and the run is reported as a **timeout** (an error; the base's disk may not be
flushed; run prime again). The report has `outcome` (`powered-off`, `already-ready`,
`timeout`, `vm-ended-early`), every `steps` frame, `problems`, the `summary`, `stop`, the
helper's `trace` and `next`. It is an error unless the summary says `ok` and the guest
powered off (or was ready already). Run `prime` once per base; it changes the base's disk, so
rebuild or re-prime if the user's name changes. A guest still up at the deadline is asked to
shut down and given `--shutdown-secs` (default 60) before it is stopped by force; a forced stop
can leave the base's disk unflushed. Copy a base that cannot be rebuilt (`cp -c`) before priming it.

### Running the simulator probe

```sh
# once, as root, after `image base --cpus --memory-mib --disk-gib` (see above), `image setup`
# with --guest-user and `image prime` (the section above); --sim-app is optional: an app
# built for the simulator (<name>.app); $RUNTIME is a <name>.dmg or a <name>.exportedBundle
sudo $D/kbf-vmm image setup --base $W/base --guest-binary $D/kbf-guest \
  --xcode /Applications/Xcode.app --sim-runtime $RUNTIME [--sim-app $APP.app] --guest-user kbfsim
$D/kbf-vmm image prime --base $W/base --guest-binary $D/kbf-guest
# as the console user, from Terminal on the Mac: hours on a first run
$D/kbf-vmm probe e3 --work-dir $W --guest-binary $D/kbf-guest [--keep] [--graphics both|on|off] | tee $W/e3.json
```

Staging also installs one more launch daemon, `com.kbf.sim`, which runs `kbf-guest sim` as
root once per boot from the copy of `kbf-guest` on the guest's disk: pass `--guest-binary` in
this setup or have passed it in an earlier one, or the daemon has nothing to run. A base that has it runs the simulator steps on every boot, E1 included.
The steps, in order, each one command with its own deadline (the flags of the daemon, with
the defaults shown; a command still running at its deadline is killed with its process
group):

| step | runs as | command | deadline |
|---|---|---|---|
| `xcode-select` | root | `xcode-select -s /Applications/Xcode.app` | 120 s |
| `license` | root | `xcodebuild -license accept` | 120 s |
| `first-launch` | root | `xcodebuild -runFirstLaunch` | 1800 s |
| `runtime-add-<name>.dmg` | root | `xcrun simctl runtime add <dmg>`, one per staged image | 1800 s |
| `runtime-import-<name>.exportedBundle` | root | `xcodebuild -importPlatform <bundle>`, one per staged bundle | 1800 s |
| `list-runtimes` | user | `xcrun simctl list runtimes -j`; the newest available iOS runtime is used | 120 s |
| `list-devicetypes` | user | `xcrun simctl list devicetypes -j`; the last iPhone listed is used | 120 s |
| `create` | user | `xcrun simctl create kbf-sim <device type> <runtime>` | 120 s |
| `boot`, `bootstatus` | user | `simctl boot <udid>`, `simctl bootstatus <udid> -b` | 600 s |
| `app-id`, `install`, `launch` | user | with `--sim-app`: the bundle id from `Info.plist` (letters, digits, `.`, `-`, `_`, at most 255 bytes), `simctl install`, `simctl launch` | 120 s, 600 s, 600 s |
| `screenshot` | user | `simctl io <udid> screenshot --type=jpeg` | 120 s |
| `xctest` | user | with `--sim-tests`: `xcodebuild test-without-building -xctestrun <staged>.xctestrun -destination id=<udid> -resultBundlePath /private/var/kbf-sim/out/tests.xcresult` | 1800 s (`--xctest-secs`) |
| `xctest-summary` | user | `xcrun xcresulttool get test-results summary --path /private/var/kbf-sim/out/tests.xcresult` | 120 s |
| `shutdown` | user | `simctl shutdown <udid>` | 120 s |

"User" means the guest's auto-login user, in that user's session
(`launchctl asuser <uid> sudo -u <name> -H -- <command>`). With `--guest-user` that is the
user recorded in `/private/var/kbf-sim/user-ready`; the steps wait up to 300 s for that
user to be at the console, and fail, saying who is there instead, if it is anyone else. A
guest with no record uses whoever owns the console, and the steps fail, saying so, if
nobody is.

### UI tests in the guest's simulator

`image setup --sim-tests <dir>` stages a build of UI tests, and the `xctest` steps run it on
the device the probe booted. The order, with the repository's fixture
(`tools/spike/macos/fixtures`, scheme `IOSFixture`, whose UI test target is
`IOSFixtureUITests`):

1. On the Mac that builds the base, with the same Xcode as the one cloned into the guest
   (the `.xctestrun` and the built products name the SDK they were built with), build the
   tests without running them. `$DD` is a new directory:

   ```sh
   xcodebuild build-for-testing -project tools/spike/macos/fixtures/IOSFixture.xcodeproj \
     -scheme IOSFixture -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' \
     -derivedDataPath $DD CODE_SIGNING_ALLOWED=NO
   ```

   `$DD/Build/Products` then holds one `*.xctestrun` and `Debug-iphonesimulator/` (the app and
   `IOSFixtureUITests-Runner.app`).
2. As root, stage that directory with the rest (after `image base`, before `image prime`):
   add `--sim-tests $DD/Build/Products` to the `image setup` command above. It is copied by the
   rules of `--sim-app` (nothing followed; a link that leaves the tree, a fifo, socket or
   device is refused; exactly one `*.xctestrun` at the top, with a name of letters, digits,
   `.`, `_` and `-`, at most 64 bytes) to `/private/var/kbf-sim/tests`, counted in the free
   space check, and recorded in `staged.json` with the SHA-256 of every file. The directory
   holds exactly this build: a setup that names no `--sim-tests` removes the earlier one.
3. `image prime`, then `probe e3` as above.

The tests run after `launch` (after `bootstatus` when no app is staged) and after the
`screenshot`; `shutdown` does not wait for them. `xctest` is `failed` when a test fails
(`xcodebuild` exits 65) and `timeout` when it passes its own deadline. `xctest-summary`
runs once `xctest` has run, failed or not, and is `skipped` when `xctest` was skipped, timed
out or could not start. Its step frame carries no stdout. The host gets the counts in
`sim-xctest.json`: `result`, `passed`, `failed`, `skipped`, `expected_failures`, `total`,
and for each failed test (at most 50, then `omitted` counts the rest) its name (at most 256
bytes) and the first failure message (at most 512 bytes). The summary of `xcresulttool`
names the failed tests only, so passed and skipped tests are counted, not named. The
result bundle stays in `/private/var/kbf-sim/out/tests.xcresult` on the clone (`--keep`,
then `image harvest`). `probe e3` prints these counts as `xctest` under each arm and, in
its one line, as `UI tests N passed, N failed, N skipped`. The `xcresulttool` form is the one
Xcode prints JSON for by default; it is not run with `--format json`.

Each step sends the host one frame, `sim-step-NN-<step>.json`, as it ends: the step, its
command, `status` (`ok`, `failed` for a non-zero exit or a signal, `timeout`, `error` for a
command that could not start or whose output named nothing to continue with, `skipped`; when the output named nothing to continue with, the note repeats at most 512 bytes of it and ends with `...[cut]` if it was longer, so a frame stays under the body limit),
the exit code or signal, the milliseconds, and the last 8 KiB of stdout and stderr. A step
runs only when the steps it needs succeeded; a skipped step names the one to blame and its
command is not run. `shutdown` needs only `boot`, and `screenshot` only `bootstatus`, so a
failed install still gets a picture and the device is shut down. The screenshot comes in a
frame of its own (`sim-screenshot.json`, base64, at most 700000 bytes before encoding, so
the frame stays under the 1 MiB body limit); the probe writes it to
`<work>/e3-<arm>-screenshot.jpg`. `sim-summary.json` ends the run. Frames that never reached
the host (it was not listening for 5 tries each) are listed in the summary.

The probe prints, for each guest, `steps` (every frame), `problems` (the steps that were
not `ok`, with how), `summary`, `screenshot`, `stop` and the helper's event `trace`. A guest
that sent no step frame is reported as such: check that the base was set up with the
staging flags and that `/var/log/com.kbf.sim.log` (read with `image harvest` after a
`--keep` run) shows the daemon started. `--graphics both` runs the headless guest, stops it,
then runs the one with a graphics device, so the two never run at once. The guests are
sized as the base was built (`sizing.json`); a base without that record runs with 2 CPUs
and 4 GiB and the report says so.

## What to send back

Everything the commands wrote, including each helper's log (the cause of a VM that did
not start is in `*.helper.log`; the tail of the log is also in the JSON failure text):

```sh
tar -czf kbf-vmm-results.tgz -C $W *.json *.err *.helper.log e1-x-harvest e1-y-harvest 2>/dev/null
```

(`e1-*-harvest` are the E1 copies; the other names exist after the other commands.) Send
`kbf-vmm-results.tgz`, the output of `sw_vers` and `sysctl hw.model hw.memsize`, and
`shasum -a 256 $D/kbf-vmm`.

## What is not known until a Mac runs it

Nothing here has run against a hypervisor. "Stop" is a force stop: the helper asks the
framework to stop the VM at once, so a stopped guest sees an unclean shutdown. The macOS
code type-checks against `aarch64-apple-darwin`; the parts that call
Virtualization.framework (`src/vz/`), the `hdiutil` steps (`src/offline.rs`) and the
guest-side probes (`kbf-guest probe-guest`) are exercised only by a run on a Mac.
`probe-bench` times an idle host, so its numbers are lower bounds for a host that is
compiling; the guest's marker times are good to about 0.6 seconds (the host polls every
100 ms and the guest every 500 ms).
