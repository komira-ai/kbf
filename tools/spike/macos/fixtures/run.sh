#!/usr/bin/env bash
# Builds one fixture and runs its UI tests, then reports what the .xcresult holds.
# macOS only. Usage: run.sh ios|macos OUT_DIR [test|build-for-testing|test-without-building|report]
#
# ACTION is the xcodebuild action, `test` by default. `build-for-testing` only builds
# into OUT_DIR's derived data and reports xcodebuild's exit status as the result;
# `test-without-building` then runs the tests from that build and is checked as
# `test` is. `report` runs no xcodebuild: it checks the bundle an earlier run left in
# OUT_DIR as `test` would (so a probe can read a sandboxed run's bundle outside the
# sandbox).
#
# ios runs on a simulator: the first available iPhone of the newest iOS runtime.
# macos runs in this login session. Prints `SPIKE macos-<arch>.xcuitest.<platform>.*`
# lines (lib.sh): xcodebuild's exit status, each test's result, each KBF-FIXTURE line
# the tests printed, and each attachment exported from the bundle with the test that
# made it. Exits 0 only if xcodebuild exited 0, exactly the three expected tests
# passed and the bundle holds their three screenshots (window, screen,
# window-no-pattern), so a run that tested or kept nothing is a failure.
. "$(dirname "$0")/../../lib.sh"
# shellcheck disable=SC2034 # read by kv in lib.sh
SPIKE_ARCH="macos-$(uname -m)"

here=$(cd "$(dirname "$0")" && pwd)
usage="usage: run.sh ios|macos OUT_DIR [test|build-for-testing|test-without-building|report]"
platform=${1:?$usage}
out=${2:?$usage}
action=${3:-test}
case $action in
test | build-for-testing | test-without-building | report) ;;
*)
    echo "$usage" >&2
    exit 2
    ;;
esac
case $platform in
ios)
    project=IOSFixture
    udid=$(xcrun simctl list devices available -j | python3 -I -c '
import json, sys
d = json.load(sys.stdin)["devices"]
ios = sorted((k for k in d if ".iOS-" in k), key=lambda k: [int(p) for p in k.rsplit(".iOS-", 1)[1].split("-")])
for k in reversed(ios):
    phones = [x for x in d[k] if x["name"].startswith("iPhone")]
    if phones:
        print(phones[0]["udid"], k.rsplit(".", 1)[1], phones[0]["name"].replace(" ", "_"))
        break
')
    read -r udid runtime device <<<"$udid" || true
    if [ -z "${udid:-}" ]; then
        kv "xcuitest.ios.simulator" "none available"
        exit 1
    fi
    kv "xcuitest.ios.simulator" "$device $runtime"
    destination="platform=iOS Simulator,id=$udid"
    ;;
macos)
    project=MacFixture
    destination="platform=macOS,arch=$(uname -m)"
    ;;
*)
    echo "$usage" >&2
    exit 2
    ;;
esac

mkdir -p "$out"
bundle=$out/$platform.xcresult
log=$out/$platform-xcodebuild.log
case $action in
build-for-testing | test-without-building) log=$out/$platform-$action-xcodebuild.log ;;
report)
    # The log of the run that left the bundle (its KBF-FIXTURE lines are read below).
    if [ -f "$out/$platform-test-without-building-xcodebuild.log" ]; then
        log=$out/$platform-test-without-building-xcodebuild.log
    fi
    ;;
esac
start=$(date +%s)
rc=0
if [ "$action" = report ]; then
    :
elif [ "$action" = build-for-testing ]; then
    xcodebuild build-for-testing -project "$here/$project.xcodeproj" -scheme "$project" \
        -destination "$destination" -derivedDataPath "$out/$platform-derived" >"$log" 2>&1 || rc=$?
else
    rm -rf "$bundle"
    xcodebuild "$action" -project "$here/$project.xcodeproj" -scheme "$project" \
        -destination "$destination" -derivedDataPath "$out/$platform-derived" \
        -resultBundlePath "$bundle" >"$log" 2>&1 || rc=$?
fi
if [ "$action" != report ]; then
    kv "xcuitest.$platform.xcodebuild_exit" "$rc"
    kv "xcuitest.$platform.seconds" "$(($(date +%s) - start))"
fi
if [ "$rc" -ne 0 ]; then
    grep -E 'error:|failed|FAIL' "$log" | head -n 20 || true
fi
if [ "$action" = build-for-testing ]; then
    kv "xcuitest.$platform.result" "$([ "$rc" -eq 0 ] && echo PASS || echo FAIL)"
    exit "$rc"
fi
{ grep -o 'KBF-FIXTURE [a-z-]*=.*' "$log" || true; } | sort -u | while read -r _ line; do
    kv "xcuitest.$platform.${line%%=*}" "${line#*=}"
done

if [ ! -d "$bundle" ]; then
    kv "xcuitest.$platform.xcresult" "absent"
    exit 1
fi
passed=$(xcrun xcresulttool get test-results tests --path "$bundle" | python3 -I -c '
import json, sys
def walk(node, out):
    if node.get("nodeType") == "Test Case":
        out.append((node["name"], node.get("result", "?")))
    for c in node.get("children", []):
        walk(c, out)
cases = []
for n in json.load(sys.stdin).get("testNodes", []):
    walk(n, cases)
want = {"testTapChangesLabel()", "testScreenshotShowsPattern()", "testPatternLeftOutIsNotFound()"}
for name, result in cases:
    print("case", name, result)
print("passed", int(len(cases) == 3 and {n for n, r in cases if r == "Passed"} == want))
')
while read -r kind name result; do
    if [ "$kind" = case ]; then kv "xcuitest.$platform.test.$name" "$result"; fi
done <<<"$passed"
all_passed=$(sed -n 's/^passed //p' <<<"$passed")

# Attachments: exported with the manifest that names the test and attachment each
# file came from. Inside the bundle they are content-addressed files under Data/.
# The three screenshots the tests keep must all be there, each exported to a
# non-empty file: without lifetime keepAlways a passing test's screenshots are
# deleted, and the run must fail rather than pass with nothing to look at.
attachments=$out/$platform-attachments
rm -rf "$attachments"
all_attached=0
if xcrun xcresulttool export attachments --path "$bundle" --output-path "$attachments" >/dev/null 2>&1; then
    found=$(python3 -I - "$attachments" <<'PY'
import json, os, re, sys
d = sys.argv[1]
want = {
    ("FixtureUITests/testScreenshotShowsPattern()", "window"),
    ("FixtureUITests/testScreenshotShowsPattern()", "screen"),
    ("FixtureUITests/testPatternLeftOutIsNotFound()", "window-no-pattern"),
}
got = set()
for t in json.load(open(os.path.join(d, "manifest.json"))):
    test = t.get("testIdentifier", "?")
    for a in t.get("attachments", []):
        human = a.get("suggestedHumanReadableName", "?")
        file = a.get("exportedFileName", "?")
        print("attachment", test, human.replace(" ", "_"), "->", file)
        # Xcode names the export "<attachment name>_<index>_<UUID>.<ext>".
        m = re.fullmatch(r"(.+)_\d+_[0-9A-Fa-f-]+\.png", human)
        path = os.path.join(d, file)
        if m and os.path.isfile(path) and os.path.getsize(path) > 0:
            got.add((test, m.group(1)))
for test, name in sorted(want - got):
    print("missing", test, name)
print("attached", int(want <= got))
PY
    ) || found="attached 0"
    while read -r kind rest; do
        case $kind in
        attachment) kv "xcuitest.$platform.attachment" "$rest" ;;
        missing) kv "xcuitest.$platform.attachment_missing" "$rest" ;;
        esac
    done <<<"$found"
    all_attached=$(sed -n 's/^attached //p' <<<"$found")
    kv "xcuitest.$platform.bundle_data_files" "$(find "$bundle/Data" -type f | wc -l | tr -d ' ')"
else
    kv "xcuitest.$platform.attachment" "xcresulttool export attachments failed"
fi
kv "xcuitest.$platform.attachments" "$([ "$all_attached" = 1 ] && echo all-three || echo incomplete)"

ok=0
[ "$rc" -eq 0 ] && [ "$all_passed" = 1 ] && [ "$all_attached" = 1 ] && ok=1
kv "xcuitest.$platform.result" "$([ "$ok" = 1 ] && echo PASS || echo FAIL)"
[ "$ok" = 1 ]
