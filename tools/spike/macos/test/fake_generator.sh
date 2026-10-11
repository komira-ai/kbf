#!/usr/bin/env bash
# A fake of the driver's `action_sandbox` example
# (crates/kbf-driver-native/examples/action_sandbox.rs) for the selftest (run.sh): the
# same flags, files and stdout. Like the real profile, which ends with rules naming
# this user's folders, the fake's names this lease, so no committed copy of the
# driver's profile can match it. $FAKE_STATE/gen_fails makes it fail for a lease whose
# path ends with the file's text;
# $FAKE_STATE/gen_wrong_digest makes it print a digest that is not its profile's.
set -u
while [ $# -gt 0 ]; do
    case $1 in
        --lease) lease=$2 ;;
        --out) out=$2 ;;
        *) exit 2 ;;
    esac
    shift 2
done
if [ -e "$FAKE_STATE/gen_fails" ] && case $lease in *"$(cat "$FAKE_STATE/gen_fails")") true ;; *) false ;; esac then
    echo 'Error: Os { code: 13, kind: PermissionDenied }' >&2
    exit 1
fi
mkdir -p "$lease/home" "$lease/tmp" "$lease/cache/clang/ModuleCache" "$out"
real=$(cd "$lease" && pwd -P)
printf '%s\n' '(version 1)' '(allow default)' '(deny job-creation)' '(deny lsopen)' \
    '(deny user-preference-write)' '(deny file-write*)' \
    '(allow file-write* (subpath (param "KBF_LEASE")) (subpath "/dev"))' \
    "(allow file-write* (regex #\"^$real/T/TemporaryItems/\"))" >"$out/profile.sb"
printf 'KBF_LEASE=%s\n' "$real" >"$out/param"
printf '%s\n' "HOME=$lease/home" "TMPDIR=$lease/tmp" "XDG_CACHE_HOME=$lease/cache" \
    "CLANG_MODULE_CACHE_PATH=$lease/cache/clang/ModuleCache" >"$out/env"
if [ -e "$FAKE_STATE/gen_wrong_digest" ]; then
    echo sha256=0000000000000000000000000000000000000000000000000000000000000000
else
    echo "sha256=$(sha256sum <"$out/profile.sb" | cut -d ' ' -f 1)"
fi
echo user_folders=/var/folders/zz/fake/T
