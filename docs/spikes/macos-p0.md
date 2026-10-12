# Spike: macOS P0 mechanics on GitHub-hosted runners

A UI test on a farm Mac rests on a few macOS mechanics that the design marks
**[A]** (assumed) until a probe settles them: `automationmodetool` run as root
without a prompt and kept across users, simulators driven by `simctl` from the
session an action runs in, and the TCC (privacy) state that decides whether a test
may capture the screen ([fleet-updates.md](../design/fleet-updates.md), L1 notes and
the P0 row; [macos-vms.md](../design/macos-vms.md) §11). This spike runs the part of
those probes that a GitHub-hosted macOS runner can run. It is a measurement, not a
decision.

A hosted runner is a virtual machine with passwordless `sudo` and a logged-in runner
user, built from an image GitHub configures. So a hosted result proves the
**mechanics** (the commands, their exact output, that each control arm goes red); it
does not prove what a farm Mac Studio does on its own macOS build, with FileVault on,
with nobody logged in, or with a fresh standard user. The last section lists what
still needs a Studio.

## How it ran

The four probes below (`automation_mode.sh`, `simctl.sh`, `tcc.sh` and
`volume_owner.sh`), their tests and mutants in the selftest, and the
`.github/workflows/spike-macos.yml` workflow were deleted once the verdicts below
were recorded, so those file names are in the history only. To find the commit that
deleted them, and so the tree to restore a probe from:

```sh
git log --diff-filter=D --oneline -- .github/workflows/spike-macos.yml
```

The workflow ran by hand (`workflow_dispatch`) and on pull requests that changed the
probes. One job per runner label (`macos-26`, `macos-15`) ran every probe, each in
its own step. A Linux job ran the probes' selftest first.

Each probe printed `SPIKE macos-<arch>.<probe>.<arm>=<value>` lines (the `macos-`
prefix keeps them apart from the Linux spike's `<arch>.<key>` lines), one `context`
line (macOS and Xcode builds, user kind, launchd domain, console user) and one
`verdict`:

- **PASS**: the mechanics worked and the control arm went red;
- **FAIL**: the mechanics did not work; the reason follows;
- **BROKEN**: the control arm did not go red, so the probe proved nothing.

Each arm ran in a subshell, so one failed arm was recorded as
`<arm>.status=ERROR(<rc>)` and did not hide the others. The job's last step
(`verdicts.sh`) failed the job on a BROKEN verdict or a missing one, never on a FAIL,
which is a result. The XCUITest sandbox probe (below) still prints these lines and
ends with the same step.

## The probes

| Probe | Arms | Control arm |
|---|---|---|
| `automation_mode.sh` | status as the runner user; `enable-automationmode-without-authentication` under a 60 s watchdog, only while the status says authentication is required; a new standard user added, the status read as that user, the user deleted, the status read again | `disable-automationmode-without-authentication` must bring "requires authentication" back. It runs first when the image starts out not requiring authentication, last otherwise, so both transitions are seen |
| `simctl.sh` (the runner's own session) | newest available iOS runtime and an iPhone device type; `simctl --set <dir>` create, boot, `bootstatus -b` (10 min watchdog), `io screenshot` (must be a PNG), shutdown, delete; then leftover `launchd_sim` processes and device-set entries | a boot under a sandbox profile that denies the lookup of `com.apple.CoreSimulator.CoreSimulatorService` must fail, and the same boot under `(allow default)` alone must succeed |
| `volume_owner.sh` (VM probe P0 of [macos-vm-guests.md](../design/macos-vm-guests.md#11-probes-later-prs-must-run)) | a fresh standard user, added with `sysadminctl -addUser`, runs (through `sudo -n -H -u`) `hdiutil create` of a 64 MiB APFS image, `hdiutil attach -owners on` (its mount line recorded), two copies of a small plist into it, `chown 0:0` on the first (status and text recorded) while the second (the keep file) stays at the user's uid, a read of the owner this first attach shows, and a detach; then root re-attaches the image read-only with `-owners on` and reads both owners. The verdict follows that re-attach, not the first attach's view. The writer's uid and groups are read through the same `sudo -u` the writes use; then the scratch directory is deleted, and the user too, but only if this run's creation marker (the uid `sysadminctl` gave it) is there and the name still has that uid. If the probe user exists before the probe, the probe does not start: it may be a real account, so the probe runs no `sysadminctl` or `hdiutil`, leaves the user and says BROKEN | the same create, attach, copy, chown and detach as root, and the same re-attach read, must give uid 0; the user's keep file must read back as the user's uid, so the re-attach is shown able to read a non-root owner too (macOS reports a `noowners` volume's files as owned by the reader, which for root is `0:0`); and both re-attaches must be in the mount table without `noowners`. A writer that is admin or root is BROKEN too: the probe would not be asking about a fresh standard user |
| `tcc.sh` (instrument for the Screen Recording and Accessibility questions) | SIP status; the ScreenCapture, Accessibility, ListenEvent and PostEvent rows of the system and the user TCC database, each read as the runner user and as root | the same reader over a planted database must return exactly its one known row |

The status text of `automationmodetool` was parsed, case-insensitively, as two fields:
`enabled` (on, off) and `auth` (required, not required), matching the wording the
hosted runners printed (Results). Every reading also recorded the raw text, and text
the parser did not recognise read as `unknown`, which never triggered enable and
gave a FAIL. Every `automationmodetool` and `sysadminctl` call ran under the
watchdog with stdin closed; the quick local reads (`id`, the context line) did not.

The selftest (`tools/spike/macos/test/run.sh`) drove each probe against fakes of the
macOS programs. `test/mutants.sh` planted defects that each had to turn a test red,
among them: an arm run outside its subshell; a watchdog that leaves the command on
the caller's pipe; `sysadminctl` run without the watchdog; the key without the `macos-` prefix; a
watchdog that reports a hang as success; a status parser that accepts any "require"
as not required; enable run whatever the last status said; a verdict that ignores
the control arm (each probe); a watchdog that leaves stdin open; a simctl verdict
that ignores the `(allow default)` boot, `bootstatus` or a failed delete; error text
from create taken as a UDID; a screenshot not checked to be a PNG; a simulator
booted in the default device set; for P0, a verdict that ignores the root control,
the owner taken from the first attach instead of the re-attach, the writes run as the
runner (an admin) instead of the fresh standard user, a verdict that ignores an admin
writer or the step where the write stopped, a verdict that ignores `noowners` on
either re-attach (or takes a re-attach missing from the mount table as owners
honoured, or finds the word in the mount point's path), a verdict that ignores the
keep file or reads it from the chowned copy, an owner compared on its gid, a failed
detach recorded as done, a cleanup that leaves the user or skips (or does not force)
the detach of a mount left behind; and, so that the probe never deletes an account it
did not make, a probe that skips the check for a user that existed before, a cleanup
that ignores the creation marker or does not compare its uid, and a marker kept from
an earlier run. The selftest's tests and mutants of the XCUITest sandbox probe
(below), of `lib.sh` and of `verdicts.sh` are still in that directory, and run in the
`selftest` job of `.github/workflows/spike-xcuitest.yml`.

## Results

All numbers come from run `37987149882` (commit `95505ed`). Every probe's verdict was
PASS on both labels. Run `37988450842` (commit `51edbb0`, which changed only this
document) gave the same verdicts and the same values. Run `37995044432` (commit
`8909f87`) gave the same verdicts; two `simctl` values differed, noted below.

| | `macos-26` job | `macos-15` job |
|---|---|---|
| macOS | 26.6.2 (25G83), arm64 | 15.7.9 (24G830), arm64 |
| Default Xcode | 26.6 (17F113) | 16.4 (16F6) |
| Context | admin user, launchd domain `Aqua`, console user = the runner user | same |
| SIP | disabled | disabled |

**automationmodetool.** The status prints two lines. The first is `Automation Mode is
disabled.` on every reading. Inferred, not measured: it reports whether a test session
holds Automation Mode now, not the device setting (no reading here ran while a test
session held it). The second is the setting: `This device DOES NOT REQUIRE
user authentication to enable Automation Mode.` or `This device requires user
authentication to enable Automation Mode.` (exact case as printed). Both images start
out not requiring authentication, so the image already ran enable. The control
(`disable-automationmode-without-authentication` via `sudo -n`, stdin closed) brought
`requires` back; `enable-automationmode-without-authentication` the same way took it
to `DOES NOT REQUIRE` again, with exit status 0, no prompt, and well inside the 60 s
watchdog. The first line still said `Automation Mode is disabled.` afterwards: the verb
clears the authentication requirement, it does not turn Automation Mode on. A new standard user, made with `sysadminctl -addUser`, read the same
not-required status, and so did a reading after that user was deleted.
`sysadminctl` warned that the new user cannot use FileVault (no secure token).

The first run of this probe (run `37984178027`, cancelled) ran `sysadminctl` without
the watchdog and with the step's stdin open; neither job got past adding the user in
15 minutes. Since then every `automationmodetool` and `sysadminctl` call runs under the
watchdog with stdin closed, and
the add took about 4 s. What it waited on in that run was not recorded.

**simctl.** The newest available iOS runtime (iOS 26.5 on `macos-26`, iOS 26.2 on
`macos-15`) and an iPhone 17 Pro device type. In a private `--set` device set, create,
boot, `bootstatus -b`, a PNG screenshot, shutdown and delete all succeeded in the
runner's session. Two values vary from run to run. The screenshot was about 2.9 MB
in runs `37987149882` and `37988450842` but 197564 bytes on `macos-26` in run
`37995044432`. In the first two runs `bootstatus` spent its time waiting on data
migration; in run `37995044432` on `macos-15` it printed `Device already booted,
nothing to do.` The control held: under the CoreSimulatorService deny, `simctl` reported
"CoreSimulatorService connection became invalid" and the boot failed, while the same
boot under `(allow default)` succeeded. After the delete: 0 `launchd_sim` processes, 0
entries left in the device set.

**TCC as seeded.** Both TCC databases were readable as the runner user (SIP is off)
and as root. Both images seed the same grants, all `auth_value` 2 (allowed). System
database: ScreenCapture 3 rows, Accessibility 9, PostEvent 3, ListenEvent 0. User
database: ScreenCapture 5, Accessibility 3, PostEvent 1, ListenEvent 0. The granted
clients include `/bin/bash`, `/usr/bin/osascript`, `com.apple.Terminal`,
`com.apple.dt.Xcode-Helper` and the image's own runner and provisioning agents. So on
a hosted runner a shell-launched UI test inherits Screen Recording and Accessibility
grants that a fresh farm user will not have. A hosted UI-test result says nothing
about prompts.

**VM probe P0 (`volume_owner.sh`).** Runs `38120894541` (commit `3572c58`) and
`38121390097` (commit `fa2e91e`, which added the mount lines), both labels, gave the
same verdict: FAIL. A fresh standard user (uid 502; groups `staff`, `everyone`,
`localaccounts` and three the image adds, no `admin`) created a 64 MiB APFS image,
attached it and copied the plist in, all with status 0. Its mount line was `apfs,
local, nodev, nosuid, journaled, nobrowse, mounted by kbfp0`: no `noowners`, so
`-owners on` took. Its `chown 0:0` failed with `Operation not permitted`. The first
attach showed `502:20`, and so did root's read-only re-attach. The root control
(same steps via `sudo -n`) read back `0:0` from its re-attach, so the reader can show
a root owner. Cleanup deleted the user and the scratch directory on every run.
Run `38126351622` (commit `6f2e739`, which added the keep file and the mount-flag
checks) gave the same FAIL on both labels: neither re-attach was `noowners`, and the
keep file the user left at its uid read back `502:20`, so the re-attach shows a
non-root owner as stored.
`sysadminctl` printed the same FileVault warning as in the automation-mode probe.

So on hosted macOS 15.7.9 and 26.6.2 a standard user cannot make a root-owned file
in a disk image it attached itself, even with owners honoured. Inferred, not
measured: this is the usual rule that only root may give a file away, not something
particular to disk images, so other ways of writing the file as that user (an
archive extracted with owners kept, a copy that keeps owners) would fail the same
way. Those variants were not run.

## XCUITest under the native driver's sandbox

The probe `tools/spike/macos/xcuitest_sandbox.sh` runs the XCUITest fixtures
(`tools/spike/macos/fixtures`, three UI tests each for a macOS app and an iOS app on a
simulator) as the native driver runs an action that asks for the network. The job is
`xcuitest-sandbox` in `.github/workflows/spike-xcuitest.yml` (`macos-15`). The profile
is not a copy: the driver's example `action_sandbox`
(`crates/kbf-driver-native/examples/action_sandbox.rs`) writes the profile
`Isolation::wrap` builds, `BASE_PROFILE`, then `NO_DEVICE_RULES`, then the rules for
the runner user's temporary folder, and prints its SHA-256. Each run gets a lease directory of its own,
whose `root` (the input root) gets a copy of the fixtures, so the projects Xcode opens
and writes into are inside the lease, as an action's sources are. It runs in that
`root`, with stdin from `/dev/null` and only the lease's variables (`HOME`, `TMPDIR`,
`XDG_CACHE_HOME`, `CLANG_MODULE_CACHE_PATH`) and `PATH=/usr/bin:/bin:/usr/sbin:/sbin`,
as the driver runs an action whose Command sets only `PATH`.

| Arm | What runs | Must |
|---|---|---|
| `deny_default` | `run.sh macos` under `(version 1)(deny default)`, through the same code that applies the driver's profile | fail, with at least one deny line from its processes, or the verdict is BROKEN (no profile applied, or a log reader that sees nothing) |
| `driver.macos`, `driver.ios` | `run.sh` (`xcodebuild test`: build and test) under the driver's profile | hand `sandbox-exec` a profile whose SHA-256 is the one `action_sandbox` printed, or BROKEN; PASS or FAIL is the result |
| `driver_tests.macos`, `driver_tests.ios` | `run.sh ... build-for-testing` without a sandbox, then `run.sh ... test-without-building` under the driver's profile in the same lease: what the UI tests need past the build | the unsandboxed build passes, or BROKEN; then as `driver.*` |
| `control.macos`, `control.ios` | `run.sh` the same way, no sandbox | pass (three tests passed, three screenshots kept), or BROKEN |

Deny lines: while a run lasts the probe samples the process table every 0.2 s; the
run's processes are its root and every process seen with a parent among them (one that
starts and ends between two samples is missed). After the run, `log show` reads the
sandbox's reports (sender `Sandbox`) for the run's time window. Those from the run's
processes are recorded: a count, a count per operation, and each distinct
(process, operation, target), the first 40 as `SPIKE` lines and all of them in
`<arm>-denies.txt` (uploaded). Reports from other processes in the window are only
counted. The profile allows every mach and XPC lookup but those `NO_DEVICE_RULES`
denies (the device services), so a run names no other mach-lookup; which names a UI
test looks up is not measured here.

A driver arm passes when `xcodebuild` exits 0 under the profile and the bundle it left
passes `run.sh ... report`, run afterwards without a sandbox: reading the bundle
(`xcresulttool`) is the probe's work, not the action's, and under the profile
`xcresulttool` cannot write its temporary file (Results). run.sh's own result inside
the sandbox is recorded as `<arm>.in_sandbox`.

The verdict is BROKEN when a control does not hold, FAIL when a fixture run fails
under the driver's profile, and PASS when every one passes there. The selftest (`test/run.sh`, the
`t_xs_*` tests, against a fake generator, a fake `run.sh` and fakes of `sandbox-exec`
and `log`) and `test/mutants.sh` cover the verdict: among the mutants are a verdict
that ignores the unsandboxed controls, a run that falls back to no sandbox, driver
runs handed a copy of `BASE_PROFILE` instead of the generated profile, a digest check
that accepts a mismatch or no digest, a verdict that ignores a `(deny default)` run
that passed or showed no deny line, deny lines counted from every process, a process
tree without the run's children, runs without the lease's variables, runs of the
checkout's fixtures instead of the lease's copy, test-only arms that build under the
sandbox, a verdict that ignores the test-only arms or their unsandboxed build, and a
driver arm judged by run.sh's checks inside the sandbox or by the report alone.

### Results

Run `38140794202` (commit `989bdd8`), job `xcuitest-sandbox` on `macos-15`
(macOS 15.7.9, Xcode 16.4 16F6, the runner's admin user in its Aqua session), with the
profile as `main` builds it: `BASE_PROFILE`, `NO_DEVICE_RULES`, the user-folder rules.
The verdict was FAIL: the controls held and three of the four driver arms failed. Run
`38138189630` (commit `3ec0969`, before `NO_DEVICE_RULES` was on `main`) gave the same
verdict and the same arm results, and the same distinct denies but for the two device
rules' (`CoreDeviceService`, `usbmuxd`), which it did not have, and the `xcresulttool`
one, which showed in a different three arms.

| Arm | xcodebuild exit under the profile | Report (outside) | Deny lines from the run |
|---|---|---|---|
| `deny_default` | not run: `sandbox-exec: execvp() of 'bash' failed` (rc 71) | | 2: `file-read-metadata` and `process-exec*` of `/bin/bash` |
| `driver.macos` | 65: the build fails | FAIL, no tests ran | 24 |
| `driver.ios` | 65: the build fails | FAIL, no tests ran | 15 |
| `driver_tests.macos` | 65: "Could not launch MacFixtureUITests" | FAIL, no tests ran | 20 |
| `driver_tests.ios` | 0 | PASS: 3 of 3 tests, the three screenshots kept | 20 |
| `control.macos`, `control.ios` | 0, no sandbox | PASS, 3 of 3 | |

Each distinct deny from the driver arms, and what it did. `T/` and `C/` are the
runner user's temporary and cache folders under `/var/folders` (the ones
`kbf_driver_native::user_folders` names); `~` is that user's home as the user database
gives it, not the lease's `HOME`.

| Operation and target | Process | Arms | Effect |
|---|---|---|---|
| `file-write-create T/swbuild.tmp.<random>` | `SWBBuildService` | `driver.macos`, `driver.ios` | fatal: the build stops at `mkdtemp(...T/swbuild.tmp...): Operation not permitted`, before any test. The driver's folder rules open `T/TemporaryItems/` and `T/xcrun_db*` only |
| `lsopen` | `xcodebuild` | `driver_tests.macos` | fatal: the macOS UI test runner is launched through Launch Services, and xcodebuild reports error -54, "The sandbox profile of this process is missing "(allow lsopen)", so it cannot invoke Launch Services' open API" |
| `mach-lookup com.apple.CoreDevice.CoreDeviceService` (XPC) and `network-outbound /private/var/run/usbmuxd` (`NO_DEVICE_RULES`) | `xcodebuild` | all four | not fatal for the iOS tests on a simulator |
| `job-creation` | `xcodebuild` | all four | not fatal for the iOS tests (`driver_tests.ios` passed with it denied) |
| `user-preference-write` of `com.apple.dt.xcodebuild`, `com.apple.coresimulator`, `com.apple.crashreporter` | `xcodebuild` | all four (the domain varies) | not fatal for the iOS tests |
| `file-write-*` under `~/Library/Caches/org.swift.swiftpm`, `~/Library/org.swift.swiftpm`, `~/.swiftpm/security` | `xcodebuild` | all four | not fatal for the iOS tests; SwiftPM finds the home from the user database, as the driver's documentation already says |
| `file-write-*` of `C/com.apple.DeveloperTools/16.4-16F6/Xcode/PlugInCache-xcodebuild.xcplugincache` and `C/com.apple.DTServiceHub` | `xcodebuild`, `DTServiceHub` | all four (`DTServiceHub` in all but `driver.ios`) | not fatal for the iOS tests |
| `file-write-unlink /private/var/tmp/test-session-systemlogs-<time>.logarchive` | `xcodebuild` | `driver_tests.macos` | not known: that run had already failed on `lsopen` |
| `file-write-create T/<UUID>` | `xcresulttool` | `driver.macos`, `driver_tests.macos`, `driver_tests.ios` | fails the probe's own reading of the bundle inside the sandbox (run.sh stops before its result line, `in_sandbox=none`), which is why the arms read it outside |

So, under the driver's profile as it stands: UI tests of an iOS app on a simulator
run and pass once built, a build of either fixture with `xcodebuild` fails on Swift
Build's temporary directory, and UI tests of a macOS app fail on `(deny lsopen)`.
Which operations and names a change of the profile would open is a separate
decision; this spike changes no profile (the next subsection has the one change made
since). Apart from the device services, the profile
allows every mach-lookup, so the run says nothing about which mach or XPC names a UI
test looks up. `deny.other` counted 1 to 48 reports from other processes in each
driver arm's window; they are not the run's.

### After the Swift Build rule (PR #425)

PR #425 lets an action make `T/swbuild.tmp.<random>` and write below it
(`kbf_driver_native::user_folders`) and keeps `(deny lsopen)`. Run `38148039779`
(job `xcuitest-sandbox`, `macos-15`, macOS 15.7.9, Xcode 16.4, the same image as
above), with that profile:

| Arm | xcodebuild exit under the profile | Report (outside) |
|---|---|---|
| `driver.ios` | 0: built and tested under the profile | PASS, 3 of 3, the three screenshots kept |
| `driver_tests.ios` | 0 | PASS, 3 of 3 |
| `driver.macos` | 65: the build passes, then "Could not launch MacFixtureUITests" | FAIL, no tests ran |
| `driver_tests.macos` | 65: "Could not launch MacFixtureUITests" | FAIL, no tests ran |
| `control.macos`, `control.ios` | 0, no sandbox | PASS, 3 of 3 |

No arm showed a `swbuild.tmp` deny. Both macOS arms showed the `lsopen` deny of the
first run; the other denies were those of the table above.

A mutant of that profile without `(deny lsopen)` (draft PR #427, run `38148073028`,
not merged) measured what macOS UI tests meet next: no `lsopen` deny, and xcodebuild
launched the runner, which "exited with code 1 before checking in" ("Early
unexpected exit, operation never finished bootstrapping"), so both macOS arms still
failed with no test run; the iOS arms passed as above. The runner, started by
Launch Services, is not in the run's process tree, so its own denies, if it had any,
are among the reports this probe only counts (`deny.other`: 21 and 3 in the two
macOS arms). The same mutant turned `tests/launchd.rs`
(`no_action_can_open_an_application`) red on the `native-macos` job: an action
opened Calculator. So `(deny lsopen)` stays, and macOS UI tests do not run under
the native driver's profile.

## Not covered here (planned)

- **The TCC log reader** (`log show` for `com.apple.TCC`, naming the responsible
  client) beside the UI-test fixtures.
- **simctl from other contexts.** Directly under kbf's sandbox profiles (the
  fixtures above reach the simulator only through `xcodebuild`, and only under
  `BASE_PROFILE`, not `NO_NETWORK_PROFILE`), from a system LaunchDaemon with no
  session, and through `launchctl asuser`.
- **The §3.2 probes and `DEVELOPER_DIR` through the driver's `xcode` property.**

## Hosted-runner facts against what needs a Studio

| Question | What a hosted run can settle | What still needs a Mac Studio |
|---|---|---|
| `automationmodetool` as root without a prompt | Settled for hosted macOS 15 and 26: the verbs, the exact status text, that root via `sudo -n` with stdin closed clears the authentication requirement with no prompt (the status still says disabled), and that the control brings authentication back | The same on the Studio's macOS build, run as root at its console or by the provisioning profile, on a Mac where nothing enabled it before |
| The setting across user deletion | Settled as a status reading: a new standard user, and a reading after that user is deleted, still see not required | That a UI test in a fresh standard user's **console session** starts without an authentication sheet: that needs a person to log the user in (FileVault on rules out auto-login) and watch |
| Simulators outside a logged-in session | Settled for the runner's own Aqua session: `simctl` works with a private device set, cleans up after delete, and a sandbox deny of CoreSimulatorService stops it | Whether it works from a LaunchDaemon with nobody logged in, on the Studio's build, with the runtimes installed there |
| Screen Recording need of XCUITest | Only what the image seeded: SIP is off and ScreenCapture is pre-granted to the shell and the runner's agents, so a hosted run cannot answer it | Whether a UI test needs the grant at all, whether a grant persists across users and runner rebuilds: arms in fresh standard users' sessions, with a person to see and click prompts |
| VM probe P0: a root-owned file in an attached image without root | Settled for hosted macOS 15 and 26: no. A fresh standard user's `chown 0:0` in an image it attached with owners honoured is refused, and a root re-attach reads the user's own uid; the root control reads root | The same on the Studio's macOS build, with the account the image builder will run as. This result is mechanics only; the Studio still decides, and the root helper verb or the MDM route of [macos-vm-guests.md](../design/macos-vm-guests.md#10-open-decisions) is then chosen |
| Accessibility | As above, the seeded rows only | The same Studio arms, and the profile route once a Studio is enrolled in device management |
