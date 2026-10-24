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

Without the daemons, or if the guest cannot mount the share or write into it, the probes
that need code running in the guest report `needs-manual` and list the possible causes.

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
allocation, and looks for the system marker. A stop or error event ends that arm's wait at
once and is reported. `--keep` leaves both clones and their shares in the work directory.
Then `image harvest` reads the guest's own logs off each kept disk.

Run it as the console user that ran `image base` (a root run or an ssh run has no
session and fails to start a guest), then as root for the setup and the harvest. Each
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

### E1 result, first run, and the fix

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
