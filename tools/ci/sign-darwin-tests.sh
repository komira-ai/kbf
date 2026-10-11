#!/usr/bin/env bash
# The per-binary entitlement allow list on a macOS runner (tools/ci/darwin-entitlements.txt,
# tools/ci/darwin-entitlements.sh; docs/design/macos-vm-guests.md section 7).
#
# The kbf-vmm stub (crates/kbf-vmm) is the binary the list gives
# com.apple.security.virtualization. This script
#   - signs it with tools/ci/sign-darwin.sh, which must apply the entitlement and pass,
#     and checks the signature itself, apart from the script's own check;
#   - signs a copy named kbf-daemon the same way, which must pass with no entitlements;
#   - packages both and runs tools/ci/check-darwin-asset.sh on them, which must pass;
#   - signs copies by hand with wrong entitlements, and requires `darwin-entitlements.sh
#     check` (and check-darwin-asset.sh, for the first) to refuse each, with the message
#     for that defect;
#   - requires the check to refuse when the allow list file is missing (fail closed).
# Each case says which defect it catches. Every case runs; the script fails at the end
# if any did.
set -euo pipefail

export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-14.0}"
cargo build --locked -p kbf-vmm --bin kbf-vmm
stub=target/debug/kbf-vmm
ents=tools/ci/darwin-entitlements.sh
vz=com.apple.security.virtualization
gta=com.apple.security.get-task-allow
tmp=$(mktemp -d "${RUNNER_TEMP:-/tmp}/kbf-sign-tests.XXXXXX")
failed=0

ok() { echo "ok: $*"; }
bad() { echo "FAIL: $*" >&2; failed=1; }

# copy <case> <name>: a fresh copy of the stub at $tmp/<case>/<name>; prints its path.
copy() {
  mkdir -p "$tmp/$1"
  cp "$stub" "$tmp/$1/$2"
  echo "$tmp/$1/$2"
}

# sign_with <binary> [<entitlement> ...]: signs ad hoc with the hardened runtime and
# exactly these entitlements (all true), bypassing the allow list.
sign_with() {
  local bin=$1 k
  shift
  if [ "$#" -eq 0 ]; then
    codesign --force --sign - --options runtime "$bin"
    return
  fi
  {
    printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>' '<plist version="1.0">' '<dict>'
    for k in "$@"; do printf '<key>%s</key><true/>\n' "$k"; done
    printf '%s\n' '</dict>' '</plist>'
  } >"$bin.plist"
  codesign --force --sign - --options runtime --entitlements "$bin.plist" "$bin"
}

# package <binary>: a tarball holding the binary, as tools/ci/package-asset.sh lays it out.
package() {
  tar -czf "$1.tar.gz" -C "$(dirname "$1")" "$(basename "$1")"
  echo "$1.tar.gz"
}

# refused <case> <message fragment> <command ...>: the command must fail, saying the fragment.
refused() {
  local what=$1 want=$2 out
  shift 2
  if out=$("$@" 2>&1); then
    bad "$what: accepted"
    echo "$out" >&2
  elif [[ $out != *"$want"* ]]; then
    bad "$what: refused, but not with '$want'"
    echo "$out" >&2
  else
    ok "$what"
  fi
}

# signed_entitlements <binary>: the signature's entitlements, as codesign prints them.
signed_entitlements() { codesign --display --entitlements - --xml "$1" 2>/dev/null; }

# 1. The signer applies the list. Catches sign-darwin.sh not passing --entitlements for
# a listed binary (the stub would carry none), and a check that refuses the right set.
vmm=$(copy signed kbf-vmm)
if bash tools/ci/sign-darwin.sh "$vmm"; then
  ok "sign-darwin.sh signs kbf-vmm with its entitlement"
else
  bad "sign-darwin.sh failed for kbf-vmm"
fi
got=$(signed_entitlements "$vmm")
if [[ $got == *"<key>$vz</key>"* ]] && [[ $got != *"$gta"* ]]; then
  ok "kbf-vmm's signature carries $vz and no $gta"
else
  bad "kbf-vmm's signature carries: $got"
fi
if bash tools/ci/check-darwin-asset.sh "$(package "$vmm")" kbf-vmm; then
  ok "check-darwin-asset.sh accepts the packaged kbf-vmm"
else
  bad "check-darwin-asset.sh refused the packaged kbf-vmm"
fi

# 2. Every other binary gets none. Catches a signer that gives an unlisted binary the
# list's entitlement (a lookup that ignores the name signs and passes it with one).
daemon=$(copy signed kbf-daemon)
if bash tools/ci/sign-darwin.sh "$daemon"; then
  ok "sign-darwin.sh signs kbf-daemon"
else
  bad "sign-darwin.sh failed for kbf-daemon"
fi
got=$(signed_entitlements "$daemon")
if [ -z "$got" ]; then
  ok "kbf-daemon's signature carries no entitlements"
else
  bad "kbf-daemon's signature carries: $got"
fi
if bash tools/ci/check-darwin-asset.sh "$(package "$daemon")" kbf-daemon; then
  ok "check-darwin-asset.sh accepts the packaged kbf-daemon"
else
  bad "check-darwin-asset.sh refused the packaged kbf-daemon"
fi

# 3. An unlisted binary carrying an entitlement. Catches a lookup that ignores the
# binary's name (it would find kbf-vmm's set for kbf-daemon) and a check that lets any
# entitlement through.
bin=$(copy daemon-vz kbf-daemon)
sign_with "$bin" "$vz"
refused "kbf-daemon with $vz" "gives kbf-daemon no entitlements" bash "$ents" check "$bin"
refused "packaged kbf-daemon with $vz" "gives kbf-daemon no entitlements" \
  bash tools/ci/check-darwin-asset.sh "$(package "$bin")" kbf-daemon
bin=$(copy session-gta kbf-mac-session)
sign_with "$bin" "$gta"
refused "kbf-mac-session with $gta" "gives kbf-mac-session no entitlements" bash "$ents" check "$bin"

# 4. kbf-vmm with one entitlement more. Catches a subset compare (every listed key
# present) that accepts extra keys, get-task-allow among them.
bin=$(copy vmm-extra kbf-vmm)
sign_with "$bin" "$vz" "$gta"
refused "kbf-vmm with $vz and $gta" "other entitlements than the allow list gives it" bash "$ents" check "$bin"

# 5. kbf-vmm with none. Catches a check that treats "no entitlements" as always fine.
bin=$(copy vmm-none kbf-vmm)
sign_with "$bin"
refused "kbf-vmm with no entitlements" "its signature carries none" bash "$ents" check "$bin"

# 6. kbf-vmm with as many entitlements as listed, but another one. Catches a compare of
# the count only, or of the signature being non-empty.
bin=$(copy vmm-other kbf-vmm)
sign_with "$bin" "$gta"
refused "kbf-vmm with $gta instead of $vz" "other entitlements than the allow list gives it" bash "$ents" check "$bin"

# 7. The allow list missing. Catches a check that skips itself, or reads "no list" as
# "no entitlements", when the file is not there: it must fail closed, for an unlisted
# binary with no entitlements (which would otherwise pass) and for kbf-vmm.
refused "a missing allow list (kbf-daemon, no entitlements)" "allow list $tmp/no-such-list is missing" \
  bash "$ents" --allowlist "$tmp/no-such-list" check "$daemon"
refused "a missing allow list (kbf-vmm)" "allow list $tmp/no-such-list is missing" \
  bash "$ents" --allowlist "$tmp/no-such-list" check "$vmm"

if [ "$failed" -ne 0 ]; then
  echo "sign-darwin-tests: a case failed (see FAIL above)" >&2
  exit 1
fi
echo "sign-darwin-tests: every case passed"
