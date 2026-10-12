#!/usr/bin/env bash
# Tests the XCUITest sandbox probe (xcuitest_sandbox.sh), the helpers it sources
# (lib.sh) and the verdict step (verdicts.sh) on Linux (or any host with bash),
# against fakes of the macOS programs they run (fakecmd). Each test sets the fake's
# state, runs the probe and checks its SPIKE lines.
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
FAKES="uname sw_vers xcodebuild launchctl stat id sandbox-exec log"

# ---------------------------------------------------------------- harness

failt() {
    FAILED=$((FAILED + 1))
    printf 'not ok %s: %s\n' "$NAME" "$*"
    sed -n '1,40s/^/    out: /p' "$OUT"
}

# fresh: a new fake Mac (state, bin, results) for the current test.
fresh() {
    T=$WORK/$NAME
    mkdir -p "$T/state" "$T/bin" "$T/results"
    for f in $FAKES; do ln -s "$HERE/fakecmd" "$T/bin/$f"; done
    : >"$T/state/log"
    OUT=$T/out
}

want() {
    grep -qF -- "$1" "$OUT" || failt "stdout lacks: $1"
}

lacks() {
    if grep -qF -- "$1" "$OUT"; then failt "stdout has: $1"; fi
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

# ---------------------------------------------------------------- xcuitest_sandbox

# xs: runs the xcuitest_sandbox probe against the fake Mac, with the fake generator
# and a source root holding the fake run.sh; the runs get FAKE_STATE and
# FAKE_REAL_PATH besides the lease's variables, so the fakes inside them work.
xs() {
    mkdir -p "$T/src/tools/spike/macos/fixtures"
    : >"$T/src/tools/spike/lib.sh"
    cp "$HERE/fake_run.sh" "$T/src/tools/spike/macos/fixtures/run.sh"
    env -u GITHUB_STEP_SUMMARY PATH="$T/bin:$FAKE_REAL_PATH" FAKE_STATE="$T/state" \
        SPIKE_RESULTS="$T/results" SPIKE_PASS_ENV="FAKE_STATE FAKE_REAL_PATH" \
        SPIKE_LOG_SETTLE=0 SPIKE_LOG_WAIT=2 \
        bash "$DIR/xcuitest_sandbox.sh" "$HERE/fake_generator.sh" "$T/src" >"$OUT" 2>&1
}

# Everything works: PASS. Catches a profile that is not the generator's (a copy: the
# bytes sandbox-exec got are compared with the lease's profile.sb), a run that falls
# back to no sandbox (the (deny default) run would pass), the runner's environment in
# place of the lease's (HOME), a run of the checkout's fixtures rather than the
# lease's copy, deny lines counted from processes outside the run's tree (pid 1's),
# a tree without the run's children (the child's lsopen), and a test-only arm that
# builds under the sandbox or tests outside it.
t_xs_pass() {
    local L=$T/results/leases
    xs
    want 'xcuitest_sandbox.deny_default.rc=71'
    want 'xcuitest_sandbox.deny_default.deny.tree=1'
    want 'xcuitest_sandbox.deny_default.deny=sandbox-exec process-exec* bash'
    want 'xcuitest_sandbox.driver.macos.result=PASS'
    want 'xcuitest_sandbox.driver.macos.report.tests_passed=3'
    want 'xcuitest_sandbox.driver.macos.deny.tree=3'
    want 'xcuitest_sandbox.driver.macos.deny.ops=file-write-create:2 lsopen:1'
    want 'xcuitest_sandbox.driver.macos.deny.distinct=2'
    want 'xcuitest_sandbox.driver.macos.deny=lsregister lsopen'
    want 'xcuitest_sandbox.driver.macos.deny=xcodebuild file-write-create /Library/Caches/com.apple.dt.xcodebuild'
    want 'xcuitest_sandbox.driver.ios.deny.tree=3'
    lacks 'xcuitest_sandbox.driver.macos.deny.other=0'
    want 'xcuitest_sandbox.control.ios.result=PASS'
    want 'xcuitest_sandbox.control.ios.tests_passed=3'
    want 'xcuitest_sandbox.control.ios.test.testTapChangesLabel()=Passed'
    want 'xcuitest_sandbox.driver.macos.user_folders=/var/folders/zz/fake/T'
    want 'xcuitest_sandbox.driver_tests.ios.build.result=PASS'
    want 'xcuitest_sandbox.driver_tests.ios.result=PASS'
    want 'xcuitest_sandbox.driver_tests.ios.deny.tree=3'
    local i=0 a
    for a in driver.macos driver.ios driver_tests.macos driver_tests.ios; do
        cmp -s "$T/state/driver_profile.$i" "$L/$a/sandbox/profile.sb" ||
            failt "sandbox-exec did not get $a's generated profile"
        i=$((i + 1))
    done
    grep -qxF "KBF_LEASE=$(cd "$L/driver.macos" && pwd -P)" "$T/state/sandbox_params" ||
        failt "sandbox-exec did not get driver.macos's lease parameter"
    [ "$(cat "$T/state/home.macos.test.driver")" = "$L/driver.macos/home" ] ||
        failt "driver.macos ran with HOME=$(cat "$T/state/home.macos.test.driver")"
    [ "$(cat "$T/state/home.ios.test.none")" = "$L/control.ios/home" ] ||
        failt "control.ios ran with HOME=$(cat "$T/state/home.ios.test.none")"
    [ "$(cat "$T/state/src.macos.test.driver")" = "$L/driver.macos/root/tools/spike/macos/fixtures" ] ||
        failt "driver.macos ran the fixtures in $(cat "$T/state/src.macos.test.driver")"
    [ -e "$T/state/src.ios.build-for-testing.none" ] || failt "driver_tests.ios built under a sandbox"
    [ -e "$T/state/src.ios.test-without-building.driver" ] ||
        failt "driver_tests.ios did not test under the driver's profile"
    verdict_is PASS
}

# A fixture that fails under the driver's profile is a FAIL (a result), naming it.
t_xs_driver_fails() {
    : >"$T/state/driver_fails_ios"
    xs
    want 'xcuitest_sandbox.driver.ios.result=FAIL'
    want 'xcuitest_sandbox.driver.ios.xcodebuild_exit=65'
    want 'xcuitest_sandbox.driver.ios.in_sandbox=FAIL'
    want 'xcuitest_sandbox.driver.ios.report.tests_passed=2'
    want 'xcuitest_sandbox.driver.ios.error=error: -[FixtureUITests testTapChangesLabel] failed'
    want 'xcuitest_sandbox.driver_tests.ios.result=PASS'
    want 'under the driver'"'"'s profile: driver.ios=FAIL (3 deny lines)'
    lacks 'driver.macos=FAIL'
    verdict_is FAIL
}

# run.sh's own checks failing inside the sandbox (xcresulttool there) do not fail an
# arm whose xcodebuild passed: the bundle is read again outside, and that report
# decides. Catches a driver arm judged by run.sh's in-sandbox result.
t_xs_analysis_in_sandbox() {
    : >"$T/state/analysis_fails_in_sandbox"
    xs
    want 'xcuitest_sandbox.driver.macos.in_sandbox=FAIL'
    want 'xcuitest_sandbox.driver.macos.xcodebuild_exit=0'
    want 'xcuitest_sandbox.driver.macos.report.result=PASS'
    want 'xcuitest_sandbox.driver.macos.result=PASS'
    verdict_is PASS
}

# An xcodebuild that fails under the profile fails the arm even when the bundle's
# tests passed.
t_xs_xcodebuild_fails() {
    : >"$T/state/xcodebuild_fails_after_tests"
    xs
    want 'xcuitest_sandbox.driver.macos.xcodebuild_exit=65'
    want 'xcuitest_sandbox.driver.macos.report.result=PASS'
    want 'xcuitest_sandbox.driver.macos.result=FAIL'
    verdict_is FAIL
}

# Tests that fail under the driver's profile after an unsandboxed build: a FAIL.
t_xs_driver_tests_fail() {
    : >"$T/state/driver_tests_fails_macos"
    xs
    want 'xcuitest_sandbox.driver.macos.result=PASS'
    want 'xcuitest_sandbox.driver_tests.macos.result=FAIL'
    want 'under the driver'"'"'s profile: driver_tests.macos=FAIL (3 deny lines)'
    verdict_is FAIL
}

# An unsandboxed build-for-testing that fails makes the probe BROKEN.
t_xs_build_fails() {
    : >"$T/state/build_fails"
    xs
    want 'xcuitest_sandbox.driver_tests.macos.build.result=FAIL'
    want 'an unsandboxed build-for-testing did not pass'
    verdict_is BROKEN
}

# An unsandboxed control that fails makes the probe BROKEN.
t_xs_control_fails() {
    : >"$T/state/control_fails"
    xs
    want 'xcuitest_sandbox.control.macos.result=FAIL'
    verdict_is BROKEN
}

# A run under (deny default) that passes makes the probe BROKEN, even though the
# sandbox reported a deny for it.
t_xs_deny_default_passes() {
    : >"$T/state/deny_default_ignored"
    xs
    want 'xcuitest_sandbox.deny_default.rc=0'
    want 'xcuitest_sandbox.deny_default.deny.tree=1'
    want 'the run under (deny default) did not fail'
    verdict_is BROKEN
}

# A (deny default) run that fails but shows no deny line makes the probe BROKEN: the
# failure may not be the sandbox's, or the log reader sees nothing.
t_xs_deny_default_unreported() {
    : >"$T/state/deny_default_unreported"
    xs
    want 'xcuitest_sandbox.deny_default.rc=71'
    want 'xcuitest_sandbox.deny_default.deny.tree=0'
    want 'no deny line from the run under (deny default)'
    verdict_is BROKEN
}

# A generator digest that is not the profile's makes the probe BROKEN.
t_xs_digest_mismatch() {
    : >"$T/state/gen_wrong_digest"
    xs
    want 'xcuitest_sandbox.driver.macos.sha256=0000000000000000000000000000000000000000000000000000000000000000'
    want 'not the one the driver'"'"'s generator printed'
    verdict_is BROKEN
}

# A generator that fails for one lease: no digest and no run, BROKEN, not a FAIL.
t_xs_generator_fails() {
    echo driver.ios >"$T/state/gen_fails"
    xs
    want 'xcuitest_sandbox.driver.ios.generate=Error: Os { code: 13, kind: PermissionDenied } [rc=1]'
    want 'xcuitest_sandbox.driver.ios.status=ERROR(1)'
    verdict_is BROKEN
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

for test in t_lib t_xs_pass t_xs_driver_fails t_xs_analysis_in_sandbox t_xs_xcodebuild_fails \
    t_xs_driver_tests_fail t_xs_build_fails t_xs_control_fails t_xs_deny_default_passes \
    t_xs_deny_default_unreported t_xs_digest_mismatch t_xs_generator_fails t_verdicts; do
    t "$test"
done
printf '%s tests, %s failed assertions\n' "$TESTS" "$FAILED"
[ "$TESTS" -gt 0 ] && [ "$FAILED" -eq 0 ]
