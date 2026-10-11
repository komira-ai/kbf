#!/usr/bin/env bash
# Plants each defect below in a copy of the macOS probes and runs the tests that must
# catch it; every mutant must turn them red. A mutant whose edit no longer applies
# fails too, so this list cannot silently go stale.
#
#   bash tools/spike/macos/test/mutants.sh

set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ORIG=$(cd "$HERE/.." && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/kbf-spike-macos-mutants.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
FAILED=0
COUNT=0

# mutant NAME FILE TESTS SED: the copy of FILE edited by SED must fail TESTS.
mutant() {
    COUNT=$((COUNT + 1))
    local m=$WORK/m$COUNT
    mkdir -p "$m"
    cp "$ORIG"/*.sh "$m/"
    sed -e "$4" "$ORIG/$2" >"$m/$2"
    if cmp -s "$ORIG/$2" "$m/$2"; then
        printf 'STALE  %s: the edit no longer applies\n' "$1"
        FAILED=$((FAILED + 1))
        return 0
    fi
    if KBF_TESTS=$3 bash "$HERE/run.sh" "$m" >"$m/log" 2>&1; then
        printf 'LIVED  %s (%s)\n' "$1" "$3"
        FAILED=$((FAILED + 1))
    else
        printf 'killed %s (%s)\n' "$1" "$3"
    fi
}

mutant 'an arm runs in the caller, so a failing arm stops the probe' lib.sh t_lib \
    '/^arm() {/,/^}/{s/^    (/    {/;s/^    )$/    }/;}'
mutant 'the arm subshell is the left side of ||, where set -e is ignored' lib.sh t_lib \
    '/^arm() {/,/^}/{s/^    )$/    ) || true/;}'
mutant 'the key lacks the macos- prefix (collides with the Linux arm64 keys)' lib.sh t_lib \
    's/^SPIKE_ARCH=macos-/SPIKE_ARCH=/'
mutant 'the watchdog reports a hang as success' lib.sh t_watchdog \
    's/        return 124/        return 0/'
mutant 'the watchdog leaves CMD on the caller pipe, so an orphaned child holds it open' lib.sh t_am_adduser_hang \
    's/    "$@" <\/dev\/null >"$out" 2>&1 &/    "$@" <\/dev\/null \&/'
mutant 'the watchdog leaves stdin open, so a prompting CMD reads the caller input' lib.sh t_watchdog \
    's/    "$@" <\/dev\/null >"$out" 2>&1 &/    "$@" >"$out" 2>\&1 \&/'
mutant 'the auth parser loses its "does not require" case' \
    automation_mode.sh t_am_already \
    's/\*"does not require user authentication"\*) a=not_required/*"requires user authentication"*) a=required/'
mutant 'the auth parser accepts any "require" as not_required' automation_mode.sh t_am_required_first \
    's/\*"requires user authentication"\*) a=required/*"require"*) a=not_required/'
mutant 'enable runs whatever the last status said' automation_mode.sh t_am_control_noop \
    's/if \[ "$(st_get last)" != required \]; then/if false; then/'
mutant 'enable runs when the status was not recognised' automation_mode.sh t_am_unknown \
    's/if \[ "$(st_get last)" != required \]; then/if [ "$(st_get last)" = not_required ]; then/'
mutant 'the verdict ignores the control arm' automation_mode.sh t_am_control_noop \
    's/elif \[ "$control" != required \]; then/elif false; then/'
mutant 'the verdict treats a hung enable as success' automation_mode.sh t_am_hang \
    's/    if \[ "$rc" -eq 124 \]; then/    if false; then/'
mutant 'the verdict ignores the setting after the user is deleted' automation_mode.sh t_am_user_drops \
    's/ || \[ "$after_delete" != not_required \]//'
mutant 'sysadminctl runs without the watchdog' automation_mode.sh t_am_adduser_hang \
    's/add=$(run_cap watchdog "$WATCHDOG" sudo -n sysadminctl/add=$(run_cap sudo -n sysadminctl/'
mutant 'the simctl verdict ignores the deny control' simctl.sh t_simctl_deny_ignored \
    's/elif \[ "$(st_get deny_boot)" = 0 \]; then/elif false; then/'
mutant 'the screenshot is not checked to be a PNG' simctl.sh t_simctl_bad_png \
    's/    if is_png "$png"; then/    if true; then/'
mutant 'the simctl verdict ignores the (allow default) arm' simctl.sh t_simctl_allow_fails \
    's/elif \[ "$(st_get allow_boot)" != 0 \]; then/elif false; then/'
mutant 'the simctl verdict ignores bootstatus' simctl.sh t_simctl_bootstatus_fails \
    's/elif \[ "$(st_get booted)" != 0 \]; then/elif false; then/'
mutant 'the simctl verdict ignores a failed delete' simctl.sh t_simctl_delete_fails \
    's/elif \[ "$(st_get deleted)" != 0 \]; then/elif false; then/'
mutant 'create takes any non-empty output as a UDID' simctl.sh t_simctl_create_garbage \
    's/\*\[!0-9A-F-\]\* | "")/"")/'
mutant 'the simctl probe boots in the default device set' simctl.sh t_simctl_pass \
    's/^sim() { xcrun simctl --set "$SET" "$@"; }/sim() { xcrun simctl "$@"; }/'
mutant 'the TCC verdict ignores the control row' tcc.sh t_tcc_control_empty \
    's/^if \[ "$(st_get control)" = ok \]; then/if true; then/'
mutant 'the TCC reader never reads as root' tcc.sh t_tcc_pass \
    's/^a_system_root() { tcc_read system.as_root "$SYSTEM_DB" sudo -n || true; }/a_system_root() { tcc_read system.as_root "$SYSTEM_DB" || true; }/'
mutant 'the xcuitest_sandbox verdict ignores the unsandboxed controls' xcuitest_sandbox.sh t_xs_control_fails \
    's/^    elif \[ "$(st_get control.macos.result)" != PASS \] || \[ "$(st_get control.ios.result)" != PASS \]; then/    elif false; then/'
mutant 'a run under a profile falls back to no sandbox' xcuitest_sandbox.sh t_xs_pass \
    's/^    exec env -i "${envs\[@\]}" "$sbx" -D "$param" -p "$profile" "$@"$/    exec env -i "${envs[@]}" "$@"/'
mutant 'the driver runs get a copy of BASE_PROFILE, not the generator'"'"'s profile' xcuitest_sandbox.sh t_xs_pass \
    's/^    tracked "$1" in_lease "$1" "$LEASES\/$1\/sandbox\/profile.sb" \\$/    printf "%s\\n" "(version 1)" "(allow default)" "(deny job-creation)" "(deny lsopen)" "(deny user-preference-write)" "(deny file-write*)" "(allow file-write* (subpath (param \\"KBF_LEASE\\")) (subpath \\"\/dev\\"))" >"$LEASES\/$1\/copy.sb"; tracked "$1" in_lease "$1" "$LEASES\/$1\/copy.sb" \\/'
mutant 'the digest check accepts a profile the generator did not print' xcuitest_sandbox.sh t_xs_digest_mismatch \
    's/^    \[ -n "$want" \] && \[ "$want" = "$used" \]$/    [ -n "$want" ]/'
mutant 'the digest check accepts no digest at all' xcuitest_sandbox.sh t_xs_generator_fails \
    's/^    \[ -n "$want" \] && \[ "$want" = "$used" \]$/    [ "$want" = "$used" ]/'
mutant 'the verdict ignores a (deny default) run with no deny line' xcuitest_sandbox.sh t_xs_deny_default_unreported \
    's/^    elif \[ "$(st_get deny_default.tree)" = 0 \] || \[ -z "$(st_get deny_default.tree)" \]; then/    elif false; then/'
mutant 'the verdict ignores a (deny default) run that passed' xcuitest_sandbox.sh t_xs_deny_default_passes \
    's/^    if \[ "$(st_get deny_default.rc)" = 0 \] || \[ -z "$(st_get deny_default.rc)" \]; then/    if false; then/'
mutant 'the verdict ignores a fixture that failed under the driver profile' xcuitest_sandbox.sh t_xs_driver_fails \
    's/^    elif \[ -n "$failed" \]; then/    elif false; then/'
mutant 'deny lines of every process count as the run'"'"'s' xcuitest_sandbox.sh t_xs_pass \
    's/\$3 == "deny" { print ((\$1 in t) ? "tree" : "other"), \$0 }/$3 == "deny" { print "tree", $0 }/'
mutant 'the run'"'"'s tree is its root alone, without its children' xcuitest_sandbox.sh t_xs_pass \
    's/m = split(kids\[p\], k, " ")/m = 0/'
mutant 'the runs get no lease variables (HOME is not the lease'"'"'s)' xcuitest_sandbox.sh t_xs_pass \
    's/done <"$dir\/sandbox\/env"/done <\/dev\/null/'
mutant 'the runs build the checkout'"'"'s fixtures, not the lease'"'"'s copy' xcuitest_sandbox.sh t_xs_pass \
    's/bash "$RUN" "$2"/bash "$SRC\/$RUN" "$2"/'
mutant 'the test-only arms build under the driver profile too' xcuitest_sandbox.sh t_xs_pass \
    's/tracked "$name.build" in_lease "$name" ""/tracked "$name.build" in_lease "$name" "$LEASES\/$name\/sandbox\/profile.sb"/'
mutant 'the verdict ignores a failed unsandboxed build-for-testing' xcuitest_sandbox.sh t_xs_build_fails \
    's/^    elif \[ "$(st_get driver_tests.macos.build)" != PASS \] || \[ "$(st_get driver_tests.ios.build)" != PASS \]; then/    elif false; then/'
mutant 'the verdict ignores the test-only arms' xcuitest_sandbox.sh t_xs_driver_tests_fail \
    's/^DRIVER_ARMS="driver.macos driver.ios driver_tests.macos driver_tests.ios"$/DRIVER_ARMS="driver.macos driver.ios"/'
mutant 'a driver arm is judged by run.sh'"'"'s checks inside the sandbox' xcuitest_sandbox.sh t_xs_analysis_in_sandbox \
    's/^    if \[ "${exit:-none}" = 0 \] && \[ "$(st_get "$1.report.result")" = PASS \]; then/    if [ "$(sed -n "s\/^SPIKE macos-[^.]*\\.xcuitest\\.$2\\.result=\/\/p" "$SPIKE_RESULTS\/$1.out" | tail -n 1)" = PASS ]; then/'
mutant 'a driver arm passes on the report alone, whatever xcodebuild exited with' xcuitest_sandbox.sh t_xs_xcodebuild_fails \
    's/^    if \[ "${exit:-none}" = 0 \] && \[ "$(st_get "$1.report.result")" = PASS \]; then/    if [ "$(st_get "$1.report.result")" = PASS ]; then/'
mutant 'verdicts.sh lets a BROKEN verdict through' verdicts.sh t_verdicts \
    's/    case "$v" in BROKEN\*) bad=1 ;; esac/    :/'
mutant 'verdicts.sh lets a missing verdict through' verdicts.sh t_verdicts \
    '/v="MISSING: the probe printed no verdict"/{n;s/bad=1/:/;}'

printf '%s mutants, %s not killed\n' "$COUNT" "$FAILED"
[ "$FAILED" -eq 0 ]
