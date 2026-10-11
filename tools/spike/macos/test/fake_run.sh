#!/usr/bin/env bash
# A fake of fixtures/run.sh for the selftest (run.sh): `fake_run.sh PLATFORM OUT_DIR
# [ACTION]` prints the SPIKE lines run.sh prints and exits 0 only on PASS. The tests
# copy it to tools/spike/macos/fixtures/run.sh of a fake source root. For
# build-for-testing it prints only the result. Under the driver's profile (the fake
# sandbox-exec exports FAKE_SANDBOX=driver) it reports the denies a sandboxed run
# would, two (the same) from itself and one from a child it keeps alive for a second,
# so the probe's sampler sees it. Every test run also reports one deny of a process
# outside its tree (pid 1). It writes its HOME and its own directory to
# $FAKE_STATE/home.<platform>.<action>.<driver|none> and src.<...>. The result
# follows $FAKE_STATE: control_fails (test, no sandbox), build_fails
# (build-for-testing), driver_fails_<platform> (test under the driver's profile),
# driver_tests_fails_<platform> (test-without-building under it).
set -u
p=$1 out=$2 action=${3:-test}
S=$FAKE_STATE
tag=$p.$action.${FAKE_SANDBOX:-none}
mkdir -p "$out"
printf '%s\n' "${HOME-unset}" >"$S/home.$tag"
(cd "$(dirname "$0")" && pwd) >"$S/src.$tag"
if [ "$action" = build-for-testing ]; then
    if [ -e "$S/build_fails" ]; then
        echo "SPIKE macos-arm64.xcuitest.$p.result=FAIL"
        exit 65
    fi
    echo "SPIKE macos-arm64.xcuitest.$p.result=PASS"
    exit 0
fi
report() {
    printf '2026-10-11 12:00:00.000 E  kernel[0:1] (Sandbox) Sandbox: %s(%s) deny(1) %s\n' "$1" "$2" "$3" >>"$S/sandbox.log"
}
result=PASS
if [ "${FAKE_SANDBOX-}" = driver ]; then
    report xcodebuild $$ "file-write-create /Library/Caches/com.apple.dt.xcodebuild"
    report xcodebuild $$ "file-write-create /Library/Caches/com.apple.dt.xcodebuild"
    S=$S bash -c 'printf "%s E  kernel[0:1] (Sandbox) Sandbox: lsregister(%s) deny(1) lsopen\n" "2026-10-11 12:00:00.000" "$$" >>"$S/sandbox.log"; sleep 1'
    case $action in
        test) if [ -e "$S/driver_fails_$p" ]; then result=FAIL; fi ;;
        test-without-building) if [ -e "$S/driver_tests_fails_$p" ]; then result=FAIL; fi ;;
    esac
elif [ -e "$S/control_fails" ]; then
    result=FAIL
fi
report launchservicesd 1 "mach-lookup com.apple.noise"
for t in testTapChangesLabel testScreenshotShowsPattern testPatternLeftOutIsNotFound; do
    r=Passed
    if [ "$result" = FAIL ] && [ "$t" = testTapChangesLabel ]; then
        r=Failed
        echo "error: -[FixtureUITests $t] failed"
    fi
    echo "SPIKE macos-arm64.xcuitest.$p.test.$t()=$r"
done
echo "SPIKE macos-arm64.xcuitest.$p.result=$result"
[ "$result" = PASS ]
