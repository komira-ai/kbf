#!/usr/bin/env bash
# Probe P-D0 (docs/design/ios-devices.md, section 11): what `xcrun devicectl` says
# about itself under every Xcode on this Mac, with no device attached. It writes one
# directory per Xcode under OUT_DIR:
#
#   bash tools/spike/macos/devicectl_p0.sh OUT_DIR
#
#   OUT_DIR/xcode-<version>-<build>/
#     xcode.txt              `xcodebuild -version`
#     version.txt            `devicectl --version`
#     help.txt               `devicectl help`
#     help-<verb>.txt        `devicectl help <verb>` for `list`, `device` and `manage`
#     help-list-devices.txt  `devicectl help list devices`
#     list-devices.json      `devicectl list devices --json-output`, with its run
#                            fields replaced (below)
#     list-devices.txt       that command's own output
#
# Each command runs under a 120 s alarm (the runner has no `timeout`); a command
# that fails leaves its output and an `exit <status>` line in its file, since the
# failure is the finding. The Xcode is picked with DEVELOPER_DIR; an Xcode whose
# version and build were already written (a second name for the same app) is skipped.
#
# `list devices --json-output` records the run: the output path it was given, the
# environment and the time. The script replaces those with fixed placeholders so a
# fixture is the same bytes on every run and carries no runner path. The fixtures in
# crates/kbf-driver-native/tests/fixtures/devicectl came from this script on the
# hosted macOS runners; the repository lint in crates/kbf-it/tests/repo_lints.rs
# refuses a fixture there that carries a real-looking device identifier.

set -u
OUT=${1:?usage: devicectl_p0.sh OUT_DIR}
mkdir -p "$OUT"

# cap FILE CMD...: runs CMD under the alarm, its stdout and stderr into FILE, and
# appends `exit <status>` to FILE when the status is not 0.
cap() {
    local file=$1 rc=0
    shift
    perl -e 'alarm 120; exec @ARGV or die "exec: $!\n"' "$@" >"$file" 2>&1 </dev/null || rc=$?
    if [ "$rc" -ne 0 ]; then
        printf 'exit %s\n' "$rc" >>"$file"
    fi
}

# scrub_json IN OUT: the JSON with the run's own fields replaced by placeholders.
scrub_json() {
    python3 -I - "$1" "$2" <<'PY'
import json, sys

src, dst = sys.argv[1], sys.argv[2]
with open(src, encoding="utf-8") as f:
    doc = json.load(f)
info = doc.get("info")
if isinstance(info, dict):
    if "arguments" in info:
        info["arguments"] = ["devicectl", "list", "devices", "--json-output", "<file>"]
    if "environment" in info:
        info["environment"] = {"<name>": "<value>"}
    for key in ("commandUUID", "startTime", "endTime", "timestamp"):
        if key in info:
            info[key] = "<" + key + ">"
with open(dst, "w", encoding="utf-8") as f:
    json.dump(doc, f, indent=2, sort_keys=True)
    f.write("\n")
PY
}

for app in /Applications/Xcode*.app; do
    dev=$app/Contents/Developer
    [ -d "$dev" ] || continue
    xcode=$(DEVELOPER_DIR=$dev xcodebuild -version 2>/dev/null) || continue
    id=$(printf '%s\n' "$xcode" | awk 'NR == 1 { v = $2 } NR == 2 { b = $3 } END { print v "-" b }')
    dir=$OUT/xcode-$id
    [ -e "$dir" ] && continue
    mkdir -p "$dir"
    printf '%s\n' "$xcode" >"$dir/xcode.txt"
    export DEVELOPER_DIR=$dev
    cap "$dir/version.txt" xcrun devicectl --version
    cap "$dir/help.txt" xcrun devicectl help
    for verb in list device manage; do
        cap "$dir/help-$verb.txt" xcrun devicectl help "$verb"
    done
    cap "$dir/help-list-devices.txt" xcrun devicectl help list devices
    raw=$(mktemp "${TMPDIR:-/tmp}/devicectl-p0.XXXXXX")
    cap "$dir/list-devices.txt" xcrun devicectl list devices --json-output "$raw"
    if [ -s "$raw" ]; then
        scrub_json "$raw" "$dir/list-devices.json" || printf 'scrub failed\n' >>"$dir/list-devices.txt"
    fi
    rm -f "$raw"
    unset DEVELOPER_DIR
done
