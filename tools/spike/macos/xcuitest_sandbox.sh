#!/usr/bin/env bash
# P0 probe: the XCUITest fixtures (fixtures/run.sh, macOS and iOS) run as a native
# driver action that asks for the network: under the driver's own sandbox profile, in
# a lease directory, with the lease's variables. Records every deny line the sandbox
# reports for the run's processes.
#
#   bash tools/spike/macos/xcuitest_sandbox.sh GENERATOR [SOURCE_ROOT]
#
# GENERATOR is the driver's `action_sandbox` example
# (crates/kbf-driver-native/examples/action_sandbox.rs): it makes a lease directory
# and writes the profile `Isolation::wrap` builds (BASE_PROFILE and this user's folder
# rules), the `-D KBF_LEASE=` parameter and the lease's variables, and prints the
# profile's SHA-256. SOURCE_ROOT (default: this checkout) holds tools/spike/lib.sh and
# tools/spike/macos/fixtures.
#
# Every run gets a lease of its own under $SPIKE_RESULTS/leases/<arm>. Its root (the
# input root) gets a copy of the fixtures and tools/spike/lib.sh, as the driver
# writes an action's inputs there, so the projects Xcode opens are inside the lease.
# The run is `bash tools/spike/macos/fixtures/run.sh PLATFORM <root>/out [ACTION]`, as
# the driver runs an action: in <lease>/root, with stdin from /dev/null and only the
# lease's variables (HOME, TMPDIR, XDG_CACHE_HOME, CLANG_MODULE_CACHE_PATH) and
# PATH=/usr/bin:/bin:/usr/sbin:/sbin in its environment. The arms, in order:
#
#   deny_default         control: run.sh macos under `(version 1)(deny default)`,
#                        through the same code that applies the driver's profile. It
#                        must fail with at least one deny line from its processes, or
#                        the probe is BROKEN (no profile applied, or the log reader
#                        sees nothing);
#   driver.macos         `xcodebuild test` (build and test) under the driver's profile;
#   driver.ios
#   driver_tests.macos   `xcodebuild build-for-testing` without a sandbox (it must pass,
#   driver_tests.ios     or BROKEN), then only `test-without-building` under the
#                        driver's profile: what the UI tests themselves need, past
#                        what the build needs.
#                        For every driver arm the SHA-256 of the profile handed to
#                        sandbox-exec must be the one GENERATOR printed, or BROKEN (a
#                        profile that is not the driver's). The bundle the run left
#                        is then checked without a sandbox (`run.sh ... report`):
#                        PASS needs xcodebuild's exit 0 under the profile and that
#                        report's PASS. PASS or FAIL is the result;
#   control.macos        `xcodebuild test` without a sandbox, same lease layout and
#   control.ios          variables: it must PASS (run.sh's own check: three tests
#                        passed, three screenshots kept), or the probe is BROKEN.
#
# Deny lines: while a run lasts, the process table is sampled every 0.2 s, so the
# run's tree is the root and every process seen with a parent in it (a process that
# starts and ends between two samples is missed). After the run, `log show` reads the
# sandbox's reports (sender Sandbox) for the run's window: those from the tree are
# the run's, the rest are other processes' (counted only). Each arm records
# <arm>.deny.tree, .other, .ops (operation:count), .distinct and each distinct line
# (process, operation, target), at most 40; every line goes to
# $SPIKE_RESULTS/<arm>-denies.txt.
#
# For the selftest only: SPIKE_PASS_ENV names variables passed into the runs as well;
# SPIKE_LOG_SETTLE (5 s) and SPIKE_LOG_WAIT (30 s) bound the waits for the log.

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=tools/spike/macos/lib.sh
. "$HERE/lib.sh"
PROBE=xcuitest_sandbox
GENERATOR=${1:?usage: xcuitest_sandbox.sh GENERATOR [SOURCE_ROOT]}
SRC=${2:-$(cd "$HERE/../../.." && pwd)}
RUN=tools/spike/macos/fixtures/run.sh
LEASES=$SPIKE_RESULTS/leases
ACTION_PATH=/usr/bin:/bin:/usr/sbin:/sbin
ARM_TIMEOUT=${SPIKE_ARM_TIMEOUT:-1200}
LOG_SETTLE=${SPIKE_LOG_SETTLE:-5}
LOG_WAIT=${SPIKE_LOG_WAIT:-30}
DENY_DEFAULT='(version 1)
(deny default)
'

sha256_of() {
    if command -v shasum >/dev/null 2>&1; then shasum -a 256; else sha256sum; fi | cut -d ' ' -f 1
}

# lease NAME: lease NAME, made by GENERATOR, with the sources copied into its root;
# records the profile digest GENERATOR printed.
lease() {
    local dir=$LEASES/$1 out rc=0
    out=$("$GENERATOR" --lease "$dir" --out "$dir/sandbox" 2>&1) || rc=$?
    if [ "$rc" -ne 0 ]; then
        kv "$1.generate" "$(printf '%s' "$out" | head -n 5) [rc=$rc]"
        return 1
    fi
    mkdir -p "$dir/root/tools/spike/macos"
    cp "$SRC/tools/spike/lib.sh" "$dir/root/tools/spike/"
    cp -R "$SRC/tools/spike/macos/fixtures" "$dir/root/tools/spike/macos/"
    st_set "$1.sha256" "$(printf '%s\n' "$out" | sed -n 's/^sha256=//p')"
    kv "$1.sha256" "$(st_get "$1.sha256")"
    kv "$1.user_folders" "$(printf '%s\n' "$out" | sed -n 's/^user_folders=//p')"
}

# in_lease NAME PROFILE_FILE CMD...: execs CMD as the driver runs an action in lease
# NAME: in its root, with only the lease's variables and PATH, under
# `sandbox-exec -D <param> -p <the file's bytes>`, and records the SHA-256 of the
# profile passed. An empty PROFILE_FILE runs CMD without a sandbox.
in_lease() {
    local dir=$LEASES/$1 file=$2 profile param l sbx envs
    shift 2
    envs=("PATH=$ACTION_PATH")
    while IFS= read -r l; do envs+=("$l"); done <"$dir/sandbox/env"
    for l in ${SPIKE_PASS_ENV-}; do envs+=("$l=${!l-}"); done
    sbx=$(command -v sandbox-exec)
    cd "$dir/root"
    if [ -z "$file" ]; then exec env -i "${envs[@]}" "$@"; fi
    profile=$(cat "$file"; printf x)
    profile=${profile%x}
    param=$(cat "$dir/sandbox/param")
    printf '%s' "$profile" | sha256_of >"$dir/used.sha256"
    exec env -i "${envs[@]}" "$sbx" -D "$param" -p "$profile" "$@"
}

# tree_of ROOT SNAPSHOTS: ROOT and every pid the snapshots (`pid ppid` lines) show
# below it.
tree_of() {
    sort -u "$2" | awk -v root="$1" '
        { kids[$2] = kids[$2] " " $1 }
        END {
            q[1] = root; seen[root] = 1; n = 1; i = 1
            while (i <= n) {
                p = q[i++]; print p
                m = split(kids[p], k, " ")
                for (j = 1; j <= m; j++) if (!(k[j] in seen)) { seen[k[j]] = 1; q[++n] = k[j] }
            }
        }'
}

# tracked NAME CMD...: runs CMD in the background (output to $SPIKE_RESULTS/NAME.out),
# sampling the process table until it ends, or until ARM_TIMEOUT, when every process
# seen in its tree is killed. Records NAME.rc and NAME.seconds; keeps the window and
# the tree's pids for `denies`.
tracked() {
    local name=$1 root rc=0 start=$SECONDS snaps=$SPIKE_STATE/$PROBE.$1.ps
    shift
    : >"$snaps"
    st_set "$name.since" "$(date '+%Y-%m-%d %H:%M:%S')"
    "$@" </dev/null >"$SPIKE_RESULTS/$name.out" 2>&1 &
    root=$!
    while kill -0 "$root" 2>/dev/null; do
        ps -A -o pid= -o ppid= >>"$snaps" 2>/dev/null || true
        if [ $((SECONDS - start)) -ge "$ARM_TIMEOUT" ]; then
            kv "$name.timeout" "${ARM_TIMEOUT}s"
            tree_of "$root" "$snaps" | xargs kill -KILL 2>/dev/null || true
            break
        fi
        sleep 0.2
    done
    wait "$root" || rc=$?
    sleep 1
    st_set "$name.until" "$(date '+%Y-%m-%d %H:%M:%S')"
    tree_of "$root" "$snaps" >"$SPIKE_STATE/$PROBE.$name.pids"
    st_set "$name.rc" "$rc"
    kv "$name.rc" "$rc"
    kv "$name.seconds" "$((SECONDS - start))"
}

# parse_denies: the sandbox's report lines on stdin as `pid<TAB>process<TAB>kind<TAB>
# operation<TAB>target` (kind is deny or allow).
parse_denies() {
    awk '{
        i = index($0, "Sandbox: "); if (!i) next
        s = substr($0, i + 9)
        if (!match(s, /\([0-9]+\) (deny|allow)/)) next
        name = substr(s, 1, RSTART - 1)
        rest = substr(s, RSTART + 1)
        pid = substr(rest, 1, index(rest, ")") - 1)
        rest = substr(rest, index(rest, ")") + 2)
        n = split(rest, f, " ")
        kind = f[1]; sub(/\(.*/, "", kind)
        op = f[2]; target = ""
        if (n >= 3) target = substr(rest, index(rest, " " op " ") + length(op) + 2)
        print pid "\t" name "\t" kind "\t" op "\t" target
    }'
}

# denies NAME [MIN]: the deny lines of NAME's window, split into the tree's and the
# rest. With MIN, reads the log again (every 2 s, up to LOG_WAIT s) until at least
# MIN of the tree's show.
denies() {
    local name=$1 min=${2:-0} pids=$SPIKE_STATE/$PROBE.$1.pids file=$SPIKE_RESULTS/$1-denies.txt
    local waited=0 tree
    sleep "$LOG_SETTLE"
    while :; do
        log show --style compact --start "$(st_get "$name.since")" --end "$(st_get "$name.until")" \
            --predicate 'sender == "Sandbox"' 2>/dev/null | parse_denies |
            awk -F '\t' -v OFS='\t' 'NR == FNR { t[$1] = 1; next }
                $3 == "deny" { print (($1 in t) ? "tree" : "other"), $0 }' "$pids" - >"$file" || true
        tree=$(awk -F '\t' '$1 == "tree"' "$file" | wc -l | tr -d ' ')
        if [ "$tree" -ge "$min" ] || [ "$waited" -ge "$LOG_WAIT" ]; then break; fi
        sleep 2
        waited=$((waited + 2))
    done
    st_set "$name.tree" "$tree"
    kv "$name.deny.tree" "$tree"
    kv "$name.deny.other" "$(awk -F '\t' '$1 == "other"' "$file" | wc -l | tr -d ' ')"
    kv "$name.deny.ops" "$(awk -F '\t' '$1 == "tree" { c[$5]++ } END { for (o in c) print o ":" c[o] }' "$file" | sort | tr '\n' ' ' | sed 's/ $//')"
    awk -F '\t' '$1 == "tree" { print $3 " " $5 " " $6 }' "$file" | sort -u >"$file.distinct"
    kv "$name.deny.distinct" "$(wc -l <"$file.distinct" | tr -d ' ')"
    head -n 40 "$file.distinct" | while IFS= read -r l; do kv "$name.deny" "$l"; done
}

# outcome NAME PLATFORM: run.sh's result, its tests and, on a failure, its error lines.
outcome() {
    local out=$SPIKE_RESULTS/$1.out result passed
    result=$(sed -n "s/^SPIKE macos-[^.]*\.xcuitest\.$2\.result=//p" "$out" | tail -n 1)
    passed=$(grep -c "^SPIKE macos-[^.]*\.xcuitest\.$2\.test\..*=Passed$" "$out" || true)
    st_set "$1.result" "${result:-none}"
    kv "$1.result" "${result:-none}"
    kv "$1.tests_passed" "$passed"
    sed -n "s/^SPIKE macos-[^.]*\.xcuitest\.$2\.\(test\..*\)$/\1/p" "$out" |
        while IFS= read -r l; do kv "$1.${l%%=*}" "${l#*=}"; done
    if [ "${result:-none}" != PASS ]; then
        kv "$1.error" "$(grep -v '^SPIKE ' "$out" | head -n 5)"
    fi
}

a_deny_default() {
    lease deny_default
    printf '%s' "$DENY_DEFAULT" >"$LEASES/deny_default/deny-default.sb"
    tracked deny_default in_lease deny_default "$LEASES/deny_default/deny-default.sb" \
        bash "$RUN" macos "$LEASES/deny_default/root/out"
    kv deny_default.output "$(head -n 3 "$SPIKE_RESULTS/deny_default.out")"
    denies deny_default 1
}

# driver_arm NAME PLATFORM [ACTION]: run.sh under the driver's profile in lease NAME,
# then `run.sh PLATFORM OUT report` without a sandbox, which checks the bundle the run
# left: the bundle's checks (xcresulttool) are the probe's, not part of the action, so
# they are not judged under the sandbox. NAME.result is PASS only when xcodebuild
# exited 0 under the profile and the report passed; NAME.in_sandbox is run.sh's own
# result there.
driver_arm() {
    local exit
    tracked "$1" in_lease "$1" "$LEASES/$1/sandbox/profile.sb" \
        bash "$RUN" "$2" "$LEASES/$1/root/out" ${3:+"$3"}
    kv "$1.used_sha256" "$(cat "$LEASES/$1/used.sha256" 2>/dev/null)"
    exit=$(sed -n "s/^SPIKE macos-[^.]*\.xcuitest\.$2\.xcodebuild_exit=//p" "$SPIKE_RESULTS/$1.out" | tail -n 1)
    kv "$1.xcodebuild_exit" "${exit:-none}"
    kv "$1.in_sandbox" "$(sed -n "s/^SPIKE macos-[^.]*\.xcuitest\.$2\.result=//p" "$SPIKE_RESULTS/$1.out" | tail -n 1)"
    tracked "$1.report" in_lease "$1" "" bash "$RUN" "$2" "$LEASES/$1/root/out" report
    outcome "$1.report" "$2"
    if [ "${exit:-none}" = 0 ] && [ "$(st_get "$1.report.result")" = PASS ]; then
        st_set "$1.result" PASS
    else
        st_set "$1.result" FAIL
        kv "$1.error" "$(grep -v '^SPIKE ' "$SPIKE_RESULTS/$1.out" | head -n 5)"
    fi
    kv "$1.result" "$(st_get "$1.result")"
    denies "$1"
}

a_driver_macos() {
    lease driver.macos
    driver_arm driver.macos macos
}
a_driver_ios() {
    lease driver.ios
    driver_arm driver.ios ios
}

# tests_arm PLATFORM: build-for-testing without a sandbox, then test-without-building
# under the driver's profile, in one lease.
tests_arm() {
    local name=driver_tests.$1
    lease "$name"
    tracked "$name.build" in_lease "$name" "" bash "$RUN" "$1" "$LEASES/$name/root/out" build-for-testing
    st_set "$name.build" "$(sed -n "s/^SPIKE macos-[^.]*\.xcuitest\.$1\.result=//p" "$SPIKE_RESULTS/$name.build.out" | tail -n 1)"
    kv "$name.build.result" "$(st_get "$name.build")"
    [ "$(st_get "$name.build")" = PASS ]
    driver_arm "$name" "$1" test-without-building
}
a_driver_tests_macos() { tests_arm macos; }
a_driver_tests_ios() { tests_arm ios; }

control_arm() {
    lease "control.$1"
    tracked "control.$1" in_lease "control.$1" "" bash "$RUN" "$1" "$LEASES/control.$1/root/out"
    outcome "control.$1" "$1"
}

a_control_macos() { control_arm macos; }
a_control_ios() { control_arm ios; }

# same_profile NAME: the profile driver arm NAME handed sandbox-exec is the one
# GENERATOR printed (both digests present and equal).
same_profile() {
    local want used
    want=$(st_get "$1.sha256")
    used=$(cat "$LEASES/$1/used.sha256" 2>/dev/null)
    [ -n "$want" ] && [ "$want" = "$used" ]
}

DRIVER_ARMS="driver.macos driver.ios driver_tests.macos driver_tests.ios"

decide() {
    local a failed='' mismatch=''
    for a in $DRIVER_ARMS; do
        if [ "$(st_get "$a.result")" != PASS ]; then
            failed="$failed $a=$(st_get "$a.result") ($(st_get "$a.tree") deny lines)"
        fi
        same_profile "$a" || mismatch="$mismatch $a"
    done
    if [ "$(st_get deny_default.rc)" = 0 ] || [ -z "$(st_get deny_default.rc)" ]; then
        verdict BROKEN "the run under (deny default) did not fail: no sandbox was applied"
    elif [ "$(st_get deny_default.tree)" = 0 ] || [ -z "$(st_get deny_default.tree)" ]; then
        verdict BROKEN "no deny line from the run under (deny default): the sandbox was not applied, or the log shows nothing"
    elif [ "$(st_get control.macos.result)" != PASS ] || [ "$(st_get control.ios.result)" != PASS ]; then
        verdict BROKEN "an unsandboxed control did not pass: control.macos=$(st_get control.macos.result) control.ios=$(st_get control.ios.result)"
    elif [ "$(st_get driver_tests.macos.build)" != PASS ] || [ "$(st_get driver_tests.ios.build)" != PASS ]; then
        verdict BROKEN "an unsandboxed build-for-testing did not pass: macos=$(st_get driver_tests.macos.build) ios=$(st_get driver_tests.ios.build)"
    elif [ -n "$mismatch" ]; then
        verdict BROKEN "the profile handed to sandbox-exec is not the one the driver's generator printed:$mismatch"
    elif [ -n "$failed" ]; then
        verdict FAIL "under the driver's profile:$failed"
    else
        verdict PASS "every fixture run passed under the driver's profile; (deny default) failed the run and the controls passed"
    fi
}

spike_context
arm deny_default a_deny_default
arm driver.macos a_driver_macos
arm driver.ios a_driver_ios
arm driver_tests.macos a_driver_tests_macos
arm driver_tests.ios a_driver_tests_ios
arm control.macos a_control_macos
arm control.ios a_control_ios
decide
