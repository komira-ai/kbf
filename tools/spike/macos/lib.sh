# shellcheck shell=bash
# Shared helpers of the XCUITest sandbox probe, xcuitest_sandbox.sh (sourced, not
# run). Findings: docs/spikes/macos-p0.md.
#
# Each measurement is one line:
#
#   SPIKE macos-<arch>.<probe>.<arm>=<value>
#
# The `macos-` prefix keeps these keys apart from the Linux spike's `<arch>.<key>`
# lines (tools/spike/lib.sh), whose arm64 keys would otherwise share a namespace.
# Every probe ends with one `<probe>.verdict` line whose value starts with one of:
#
#   PASS    the mechanics worked and the control arm went red as it must;
#   FAIL    the mechanics did not work (the reason follows);
#   BROKEN  the control arm did not go red, so the probe proves nothing.
#
# Lines go to stdout, to $SPIKE_RESULTS/spike.txt (uploaded by the workflow) and to
# the job summary. Each arm runs in its own subshell under `set -e`, so a failing arm
# is recorded as `<arm>.status=ERROR(<rc>)` and the arms after it still run. Arms
# share state through files under $SPIKE_RESULTS/state, not shell variables.
#
# This file is bash 3.2 clean (macOS's /bin/bash): no associative arrays, no
# `${v,,}`, no `mapfile`.

set -u -o pipefail

SPIKE_ARCH=macos-$(uname -m)
# Each probe sets PROBE (its key segment) after sourcing this file.
PROBE=${PROBE:-unset}
SPIKE_RESULTS=${SPIKE_RESULTS:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/kbf-spike-macos}
SPIKE_STATE=$SPIKE_RESULTS/state
mkdir -p "$SPIKE_STATE"

# kv KEY VALUE: one measurement line under the current $PROBE (newlines in VALUE are
# folded to " | ").
kv() {
    local v=${2//$'\n'/ | }
    local line="SPIKE $SPIKE_ARCH.$PROBE.$1=$v"
    printf '%s\n' "$line"
    printf '%s\n' "$line" >>"$SPIKE_RESULTS/spike.txt"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        printf -- '- `%s.%s.%s` = `%s`\n' "$SPIKE_ARCH" "$PROBE" "$1" "$v" >>"$GITHUB_STEP_SUMMARY"
    fi
}

# st_set KEY VALUE / st_get KEY: state shared between arms (st_get prints "" when unset).
st_set() { printf '%s' "$2" >"$SPIKE_STATE/$PROBE.$1"; }
st_get() { cat "$SPIKE_STATE/$PROBE.$1" 2>/dev/null || true; }

# arm NAME FUNCTION: runs FUNCTION in a subshell under `set -e`. A non-zero exit is
# recorded as NAME.status=ERROR(<rc>) and never stops the caller. The subshell is a
# statement of its own, not the left side of `||` or `&&`: bash ignores `set -e`
# anywhere inside such a list, subshells included.
arm() {
    local name=$1 fn=$2 rc flags=$-
    set +e
    (
        set -e
        "$fn"
    )
    rc=$?
    case "$flags" in *e*) set -e ;; esac
    if [ "$rc" -ne 0 ]; then kv "$name.status" "ERROR($rc)"; fi
    return 0
}

# user_kind: root, admin or standard, for the user running the probe.
user_kind() {
    if [ "$(id -u)" = 0 ]; then
        echo root
    else
        case " $(id -Gn) " in
            *" admin "*) echo admin ;;
            *) echo standard ;;
        esac
    fi
}

# spike_context: the one context line every probe records: macOS and Xcode builds,
# the user kind, the launchd domain this shell runs in and the console user.
spike_context() {
    local xb
    xb=$(xcodebuild -version 2>/dev/null | sed -n 's/^Build version //p')
    kv context "os=$(sw_vers -productVersion 2>/dev/null)/$(sw_vers -buildVersion 2>/dev/null) xcode=${xb:-none} user=$(user_kind) domain=$(launchctl managername 2>/dev/null || echo unknown) console=$(stat -f %Su /dev/console 2>/dev/null || echo unknown)"
}

# verdict VALUE REASON: the probe's last line.
verdict() {
    kv verdict "$1: $2"
}
