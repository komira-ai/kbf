#!/usr/bin/env bash
# Tests the macOS P0 probes on Linux (or any host with bash) against fakes of the
# macOS programs they run (fakecmd). Each test sets the fake's state, runs one probe
# and checks its SPIKE lines and the calls the fake logged.
#
#   bash tools/spike/macos/test/run.sh [PROBE_DIR]
#
# PROBE_DIR defaults to tools/spike/macos; mutants.sh passes a mutated copy.
# KBF_TESTS limits the run to the named tests. Prints one line per failed assertion
# and a summary; exits 1 if any test failed.

set -u
HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(cd "${1:-$HERE/..}" && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/kbf-spike-macos-test.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
FAKE_REAL_PATH=$PATH
export FAKE_REAL_PATH
TESTS=0
FAILED=0
NAME=
FAKES="uname sw_vers xcodebuild launchctl stat csrutil pgrep id sudo automationmodetool sysadminctl sandbox-exec xcrun sqlite3 hdiutil cp chown mount"

# ---------------------------------------------------------------- harness

failt() {
    FAILED=$((FAILED + 1))
    printf 'not ok %s: %s\n' "$NAME" "$*"
    sed -n '1,40s/^/    out: /p' "$OUT"
}

# fresh: a new fake Mac (state, bin, results) for the current test.
fresh() {
    T=$WORK/$NAME
    mkdir -p "$T/state" "$T/bin" "$T/results" "$T/shared"
    for f in $FAKES; do ln -s "$HERE/fakecmd" "$T/bin/$f"; done
    : >"$T/state/log"
    OUT=$T/out
}

# probe NAME: runs tools/spike/macos/NAME.sh against the fake Mac.
probe() {
    env -u GITHUB_STEP_SUMMARY PATH="$T/bin:$FAKE_REAL_PATH" FAKE_STATE="$T/state" \
        SPIKE_RESULTS="$T/results" SPIKE_TCC_SYSTEM_DB="$T/state/system.db" \
        SPIKE_TCC_USER_DB="$T/state/user.db" SPIKE_P0_PARENT="$T/shared" ${PROBE_ENV-} \
        bash "$DIR/$1.sh" >"$OUT" 2>&1
}

want() {
    grep -qF -- "$1" "$OUT" || failt "stdout lacks: $1"
}

lacks() {
    if grep -qF -- "$1" "$OUT"; then failt "stdout has: $1"; fi
}

# calls PREFIX N: the fake logged N calls whose line starts with PREFIX (a fixed
# string; a call through the fake sudo logs a `sudo ...` line too, which never counts).
calls() {
    local n
    n=$(awk -v p="$1" 'index($0, p) == 1 { n++ } END { print n + 0 }' "$T/state/log")
    [ "$n" = "$2" ] || failt "$n calls match '$1', want $2"
}

# verdict VALUE: the probe's verdict line starts with VALUE.
verdict_is() {
    grep -q "^SPIKE macos-arm64\.[a-z_]*\.verdict=$1:" "$OUT" || failt "verdict is not $1: $(grep '\.verdict=' "$OUT")"
}

selected() {
    [ -z "${KBF_TESTS-}" ] && return 0
    case " $KBF_TESTS " in *" $1 "*) return 0 ;; esac
    return 1
}

t() {
    selected "$1" || return 0
    NAME=$1
    TESTS=$((TESTS + 1))
    PROBE_ENV=
    fresh
    "$1"
}

# ---------------------------------------------------------------- lib

# A failing arm is recorded and the arms after it still run, under the macos- prefix.
t_lib() {
    cat >"$T/lib_probe.sh" <<EOF
. "$DIR/lib.sh"
PROBE=libtest
a1() { false; echo never; }
a2() { kv a2 ran; }
arm a1 a1
arm a2 a2
verdict PASS done
EOF
    env PATH="$T/bin:$FAKE_REAL_PATH" FAKE_STATE="$T/state" SPIKE_RESULTS="$T/results" \
        bash "$T/lib_probe.sh" >"$OUT" 2>&1
    want 'SPIKE macos-arm64.libtest.a1.status=ERROR(1)'
    want 'SPIKE macos-arm64.libtest.a2=ran'
    lacks never
    want 'SPIKE macos-arm64.libtest.verdict=PASS: done'
    grep -qF 'SPIKE macos-arm64.libtest.a2=ran' "$T/results/spike.txt" || failt "spike.txt lacks the a2 line"
}

# The watchdog returns 124 for a hang, and the command's own status otherwise.
t_watchdog() {
    cat >"$T/wd.sh" <<EOF
. "$DIR/lib.sh"
PROBE=wd
rc=0; watchdog 1 sleep 20 || rc=\$?; echo "hang=\$rc"
rc=0; watchdog 5 sh -c 'exit 3' || rc=\$?; echo "exit=\$rc"
out=\$(watchdog 5 echo hi); echo "out=\$out"
echo fed | watchdog 5 sh -c 'if read -r l; then echo "stdin=\$l"; else echo stdin=eof; fi'
EOF
    local t0=$SECONDS
    env PATH="$T/bin:$FAKE_REAL_PATH" FAKE_STATE="$T/state" SPIKE_RESULTS="$T/results" \
        bash "$T/wd.sh" >"$OUT" 2>&1
    want hang=124
    want exit=3
    want out=hi
    # stdin is closed: a command that prompts gets EOF, not the caller's input.
    want stdin=eof
    lacks stdin=fed
    [ $((SECONDS - t0)) -lt 10 ] || failt "the watchdog took $((SECONDS - t0))s"
}

# ---------------------------------------------------------------- automation_mode

# Already not_required: the control runs first, then enable once, then the user arm.
t_am_already() {
    echo not_required >"$T/state/auth"
    probe automation_mode
    want 'automation_mode.status_before.auth=not_required'
    want 'automation_mode.status_before.enabled=off'
    want 'automation_mode.after_control.auth=required'
    want 'automation_mode.after_enable.auth=not_required'
    want 'automation_mode.as_new_user.auth=not_required'
    want 'automation_mode.new_user.exists=yes'
    want 'automation_mode.new_user.gone=yes'
    calls 'automationmodetool enable-automationmode-without-authentication' 1
    calls 'automationmodetool disable-automationmode-without-authentication' 1
    verdict_is PASS
}

# Required at first: enable runs once, the control runs last.
t_am_required_first() {
    echo required >"$T/state/auth"
    probe automation_mode
    want 'automation_mode.status_before.auth=required'
    want 'automation_mode.after_enable.auth=not_required'
    want 'automation_mode.after_user_delete.auth=not_required'
    want 'automation_mode.after_control.auth=required'
    calls 'automationmodetool enable-automationmode-without-authentication' 1
    calls 'automationmodetool disable-automationmode-without-authentication' 1
    verdict_is PASS
}

# A control that does not bring authentication back is BROKEN, and enable is never
# run against a device the last reading called not_required.
t_am_control_noop() {
    echo not_required >"$T/state/auth"
    : >"$T/state/disable_noop"
    probe automation_mode
    want 'automation_mode.after_control.auth=not_required'
    want 'automation_mode.enable=skipped: the last status was not_required'
    calls 'automationmodetool enable-automationmode-without-authentication' 0
    verdict_is BROKEN
}

# An enable that hangs is a FAIL after the watchdog, with no second attempt.
t_am_hang() {
    echo required >"$T/state/auth"
    : >"$T/state/hang_enable"
    PROBE_ENV=SPIKE_WATCHDOG=1
    local t0=$SECONDS
    probe automation_mode
    want 'automation_mode.enable=TIMEOUT after 1s'
    calls 'automationmodetool enable-automationmode-without-authentication' 1
    verdict_is FAIL
    [ $((SECONDS - t0)) -lt 15 ] || failt "the probe took $((SECONDS - t0))s"
}

# Unrecognised status text: no enable, a FAIL naming the raw line.
t_am_unknown() {
    echo weird >"$T/state/auth"
    probe automation_mode
    want 'automation_mode.status_before.auth=unknown'
    want 'automation_mode.status_before.raw=Something else entirely. [rc=0]'
    calls 'automationmodetool enable-automationmode-without-authentication' 0
    verdict_is FAIL
}

# The setting lost when the new user is deleted is a FAIL.
t_am_user_drops() {
    echo required >"$T/state/auth"
    : >"$T/state/delete_resets"
    probe automation_mode
    want 'automation_mode.after_user_delete.auth=required'
    verdict_is FAIL
}

# A sysadminctl that hangs is cut by the watchdog; the probe ends with a FAIL.
t_am_adduser_hang() {
    echo required >"$T/state/auth"
    : >"$T/state/hang_adduser"
    PROBE_ENV=SPIKE_WATCHDOG=1
    local t0=$SECONDS
    probe automation_mode
    want 'automation_mode.new_user.add= [rc=124]'
    want 'automation_mode.after_control.auth=required'
    verdict_is FAIL
    [ $((SECONDS - t0)) -lt 20 ] || failt "the probe took $((SECONDS - t0))s"
}

# ---------------------------------------------------------------- simctl

t_simctl_pass() {
    echo 1 >"$T/state/launchd_sim"
    probe simctl
    want 'simctl.runtime=com.apple.CoreSimulator.SimRuntime.iOS-26-0'
    want 'simctl.devicetype=com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro'
    want 'simctl.create=0A1B2C3D-0000-4000-8000-000000000001'
    want 'simctl.sandbox_deny.boot=Unable to locate CoreSimulatorService [rc=163]'
    want 'simctl.sandbox_allow.boot= [rc=0]'
    want 'simctl.screenshot.png=yes'
    want 'simctl.leftover.launchd_sim=1'
    want 'simctl.leftover.set_entries=0'
    want 'simctl.context=os=26.0/25A100 xcode=17A100 user=admin domain=Aqua console=runner'
    calls "xcrun simctl --set $T/results/sims create kbf-probe" 1
    verdict_is PASS
}

# A deny that does not stop the boot makes the probe BROKEN.
t_simctl_deny_ignored() {
    : >"$T/state/deny_ignored"
    probe simctl
    want 'simctl.sandbox_deny.boot= [rc=0]'
    verdict_is BROKEN
}

t_simctl_bad_png() {
    : >"$T/state/bad_png"
    probe simctl
    want 'simctl.screenshot.png=no'
    verdict_is FAIL
}

# A boot that fails under (allow default) alone is a FAIL: sandbox-exec itself, not
# the deny, may be what failed the control.
t_simctl_allow_fails() {
    : >"$T/state/allow_fails"
    probe simctl
    want 'simctl.sandbox_allow.boot=sandbox-exec: sandbox_apply: Operation not permitted [rc=71]'
    want 'simctl.screenshot.png=yes'
    want 'the boot under (allow default) failed'
    verdict_is FAIL
}

# A device that never finishes booting is a FAIL, even when the screenshot works.
t_simctl_bootstatus_fails() {
    : >"$T/state/bootstatus_fails"
    probe simctl
    want 'simctl.bootstatus= [rc=1]'
    want 'simctl.screenshot.png=yes'
    want 'the device did not finish booting'
    verdict_is FAIL
}

# A device that cannot be deleted is a FAIL.
t_simctl_delete_fails() {
    : >"$T/state/delete_fails"
    probe simctl
    want 'simctl.delete=Unable to delete device: busy [rc=149]'
    want 'the device could not be deleted'
    verdict_is FAIL
}

# Error text from a create that exits 0 is not taken as a UDID: nothing is booted.
t_simctl_create_garbage() {
    : >"$T/state/create_garbage"
    probe simctl
    want 'simctl.create=An error was encountered processing the command (code=161).'
    want 'no device was created'
    calls "xcrun simctl --set $T/results/sims boot" 0
    verdict_is FAIL
}

# ---------------------------------------------------------------- tcc

t_tcc_pass() {
    printf '%s\n' 'kTCCServiceAccessibility|com.example.a|2' 'kTCCServiceScreenCapture|com.example.b|0' \
        'kTCCServiceScreenCapture|com.example.c|2' >"$T/state/user.db"
    probe tcc
    want 'tcc.control.rows=kTCCServiceScreenCapture|kbf.probe.control|2'
    want 'tcc.system.as_user.readable=no: Error: unable to open database file'
    want 'tcc.user.as_user.readable=yes'
    want 'tcc.user.as_user.ScreenCapture=2'
    want 'tcc.user.as_user.Accessibility=1'
    want 'tcc.user.as_user.ListenEvent=0'
    want 'tcc.user.as_root.readable=yes'
    want 'tcc.sip=System Integrity Protection status: disabled. [rc=0]'
    calls 'sudo -n sqlite3 -readonly' 2
    calls 'sqlite3 -readonly' 5
    verdict_is PASS
}

# A reader that returns nothing for the planted row makes the probe BROKEN.
t_tcc_control_empty() {
    : >"$T/state/control_empty"
    probe tcc
    verdict_is BROKEN
}

# ---------------------------------------------------------------- volume_owner

# vo_cleaned: the user, the scratch directory and every attach are gone.
vo_cleaned() {
    want 'volume_owner.cleanup.user_gone=yes'
    want 'volume_owner.cleanup.dir_gone=yes'
    [ -z "$(find "$T/shared" -mindepth 1 2>/dev/null)" ] || failt "the scratch directory is left: $(ls "$T/shared")"
}

# The common case: the standard user's chown is refused, the re-attach reads its own
# uid, the root control reads root. Catches writes that run as the runner (an admin)
# instead of the fresh standard user, and a cleanup that leaves the user or the files.
t_vo_fail() {
    probe volume_owner
    want 'volume_owner.new_user.existed_before=no'
    want 'volume_owner.write.writer=uid=502 kind=standard groups=staff everyone localaccounts'
    want 'volume_owner.write.chown=chown: '
    want 'Operation not permitted [rc=1]'
    want 'volume_owner.reattach.owner=502:20'
    want 'volume_owner.write.copy_keep= [rc=0]'
    want 'volume_owner.reattach.keep_owner=502:20'
    want 'volume_owner.control_reattach.owner=0:0'
    calls 'hdiutil create -size 64m -fs APFS' 2
    calls 'sudo -n -H -u kbfp0 hdiutil create' 1
    calls 'sudo -n -H -u kbfp0 hdiutil attach -nobrowse -owners on -mountpoint' 1
    calls 'sudo -n -H -u kbfp0 chown 0:0' 1
    want 'volume_owner.write.mount=/dev/disk4s1 on '
    want 'm-user-1 (apfs, local, nodev, nosuid, journaled, nobrowse, mounted by kbfp0)'
    want 'm-user-2 (apfs, local, nodev, nosuid, read-only, journaled, nobrowse)'
    calls 'sudo -n hdiutil attach -nobrowse -owners on -readonly -mountpoint' 2
    vo_cleaned
    verdict_is FAIL
}

# A volume that stores the standard user's chown: PASS. Catches a verdict that cannot
# say PASS (an owner compared in the wrong form), and a keep owner read from the
# chowned file instead of the one the user left at its uid.
t_vo_pass() {
    : >"$T/state/owners_writable"
    probe volume_owner
    want 'volume_owner.write.chown= [rc=0]'
    want 'volume_owner.reattach.owner=0:0'
    want 'volume_owner.reattach.keep_owner=502:20'
    verdict_is PASS
    vo_cleaned
}

# A chown the first attach shows but never stores: the first attach says 0:0, the
# re-attach says 502:20, and the verdict follows the re-attach. Catches an owner read
# from the first attach.
t_vo_chown_lies() {
    : >"$T/state/chown_lies"
    probe volume_owner
    want 'volume_owner.write.first_owner=0:0'
    want 'volume_owner.reattach.owner=502:20'
    verdict_is FAIL
}

# A root control that does not read back as root is BROKEN, even when the user's file
# reads as root. Catches a verdict that ignores the control.
t_vo_control_lost() {
    : >"$T/state/owners_writable"
    : >"$T/state/root_owner_lost"
    probe volume_owner
    want 'volume_owner.reattach.owner=0:0'
    want 'volume_owner.control_reattach.owner=99:99'
    verdict_is BROKEN
}

# The verifier's false PASS: root's re-attach of the user's image comes up noowners
# and shows root as every owner, so the refused chown's file reads 0:0. BROKEN, for
# the mount flags. Catches a verdict that never reads the re-attach's mount line. The
# test's own directory puts the word noowners in every mount path, so it also catches
# a check that searches the path instead of the flag list (the control's line would
# match first).
t_vo_noowners() {
    : >"$T/state/noowners_m-user-2"
    : >"$T/state/shows_caller_m-user-2"
    probe volume_owner
    want 'volume_owner.write.chown=chown: '
    want 'm-user-2 (apfs, local, nodev, nosuid, read-only, journaled, noowners, nobrowse)'
    want 'volume_owner.reattach.owner=0:0'
    want 'volume_owner.reattach.keep_owner=0:0'
    want "the re-attach of the user's image did not honour owners"
    vo_cleaned
    verdict_is BROKEN
}

# A re-attach that shows root as every owner with no noowners in its mount line: the
# keep file reads 0:0, not the user's 502, so BROKEN. Catches a verdict whose only
# evidence that the reader can show a non-root owner is the mount line.
t_vo_shows_caller() {
    : >"$T/state/shows_caller_m-user-2"
    probe volume_owner
    want 'm-user-2 (apfs, local, nodev, nosuid, read-only, journaled, nobrowse)'
    want 'volume_owner.reattach.owner=0:0'
    want "the re-attach read the file the user left at uid 502 as '0:0'"
    verdict_is BROKEN
}

# The control's re-attach comes up noowners: its 0:0 is root shown as every owner,
# not the stored owner (here lost to 99:99), so BROKEN even though the user's file
# reads 0:0 and the keep file 502:20. Catches a verdict that checks only the user
# re-attach's mount line.
t_vo_control_noowners() {
    : >"$T/state/owners_writable"
    : >"$T/state/root_owner_lost"
    : >"$T/state/noowners_m-control-2"
    : >"$T/state/shows_caller_m-control-2"
    probe volume_owner
    want 'volume_owner.reattach.owner=0:0'
    want 'volume_owner.reattach.keep_owner=502:20'
    want 'volume_owner.control_reattach.owner=0:0'
    want "the root control's re-attach did not honour owners"
    verdict_is BROKEN
}

# A re-attach whose mount point is not in the mount table: its flags are unknown, so
# BROKEN even when the owners read right. Catches a missing line taken as owners on.
t_vo_mount_unlisted() {
    : >"$T/state/owners_writable"
    : >"$T/state/unlisted_m-user-2"
    probe volume_owner
    want 'volume_owner.reattach.mount=not in the mount table'
    want 'volume_owner.reattach.owner=0:0'
    want "the re-attach of the user's image did not honour owners (not in the mount table)"
    verdict_is BROKEN
}

# A writer in the admin group is BROKEN. Catches a verdict that ignores the writer's
# kind.
t_vo_admin_writer() {
    : >"$T/state/probe_user_admin"
    : >"$T/state/owners_writable"
    probe volume_owner
    want 'volume_owner.write.writer=uid=502 kind=admin'
    verdict_is BROKEN
}

# A probe user that existed before the probe is not fresh: BROKEN. Catches a verdict
# that ignores the pre-existing user.
t_vo_user_existed() {
    : >"$T/state/user_kbfp0"
    : >"$T/state/owners_writable"
    probe volume_owner
    want 'volume_owner.new_user.existed_before=yes'
    verdict_is BROKEN
}

# An attach the standard user cannot make is a FAIL that names the step; the control
# and the cleanup still run. Catches a verdict that ignores where the write stopped.
t_vo_attach_fails() {
    : >"$T/state/attach_fails"
    probe volume_owner
    want 'volume_owner.write.attach=hdiutil: attach failed - Device not configured [rc=1]'
    want 'volume_owner.control_reattach.owner=0:0'
    want "the standard user's write stopped after 'create'"
    vo_cleaned
    verdict_is FAIL
}

# ---------------------------------------------------------------- verdicts

# verdicts.sh is red for a BROKEN or a missing verdict and green for PASS and FAIL.
t_verdicts() {
    local f=$T/results/spike.txt rc
    printf '%s\n' 'SPIKE macos-arm64.a.verdict=PASS: ok' 'SPIKE macos-arm64.b.verdict=FAIL: measured' \
        'SPIKE macos-arm64.b.x.status=ERROR(2)' >"$f"
    rc=0
    env -u GITHUB_STEP_SUMMARY SPIKE_RESULTS="$T/results" bash "$DIR/verdicts.sh" a b >"$OUT" 2>&1 || rc=$?
    [ "$rc" = 0 ] || failt "PASS and FAIL exit $rc, want 0"
    want '| b | FAIL: measured |'
    want 'macos-arm64.b.x.status=ERROR(2)'
    rc=0
    env -u GITHUB_STEP_SUMMARY SPIKE_RESULTS="$T/results" bash "$DIR/verdicts.sh" a b c >"$OUT" 2>&1 || rc=$?
    [ "$rc" = 1 ] || failt "a missing verdict exits $rc, want 1"
    want '| c | MISSING: the probe printed no verdict |'
    echo 'SPIKE macos-arm64.c.verdict=BROKEN: control lived' >>"$f"
    rc=0
    env -u GITHUB_STEP_SUMMARY SPIKE_RESULTS="$T/results" bash "$DIR/verdicts.sh" a c >"$OUT" 2>&1 || rc=$?
    [ "$rc" = 1 ] || failt "a BROKEN verdict exits $rc, want 1"
}

# ---------------------------------------------------------------- run

for test in t_lib t_watchdog t_am_already t_am_required_first t_am_control_noop t_am_hang \
    t_am_unknown t_am_user_drops t_am_adduser_hang t_simctl_pass t_simctl_deny_ignored t_simctl_bad_png \
    t_simctl_allow_fails t_simctl_bootstatus_fails t_simctl_delete_fails t_simctl_create_garbage \
    t_tcc_pass t_tcc_control_empty t_vo_fail t_vo_pass t_vo_chown_lies t_vo_control_lost \
    t_vo_noowners t_vo_shows_caller t_vo_control_noowners t_vo_mount_unlisted \
    t_vo_admin_writer t_vo_user_existed t_vo_attach_fails t_verdicts; do
    t "$test"
done
printf '%s tests, %s failed assertions\n' "$TESTS" "$FAILED"
[ "$TESTS" -gt 0 ] && [ "$FAILED" -eq 0 ]
