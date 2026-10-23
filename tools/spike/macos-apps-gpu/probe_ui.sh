#!/bin/bash
# Usage: probe_ui.sh <bindir> <workdir>. Copies the MacFixture, runs its UI tests.
bin=$1; work=$2
mkdir -p "$work" && rm -rf "$work/fx" "$work/dd" "$work/r.xcresult"
cp -R "$bin/fixtures" "$work/fx" || echo "copy failed"
echo "WHO $(id)"
echo "SESSION $(launchctl managername 2>&1)"
echo "AUTOMATION $(automationmodetool 2>&1 | tr '\n' ' ')"
perl -e 'alarm shift; exec @ARGV' 600 /usr/bin/xcodebuild test -project "$work/fx/MacFixture.xcodeproj" -scheme MacFixture \
  -destination 'platform=macOS' -derivedDataPath "$work/dd" -resultBundlePath "$work/r.xcresult" > "$work/xcb.log" 2>&1
echo "XCODEBUILD_EXIT $?"
grep -E "TEST SUCCEEDED|TEST FAILED|error:|Could not|Failed|WindowServer|ession|utomation|uthentication|Executed [0-9]+ test|lsopen|Testing failed|Test Case.*(passed|failed)" "$work/xcb.log" | head -40 | cut -c1-400
echo "XCB_TAIL"; tail -8 "$work/xcb.log" | cut -c1-300
