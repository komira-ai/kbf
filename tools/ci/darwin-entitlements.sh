#!/usr/bin/env bash
# The per-binary entitlement allow list (tools/ci/darwin-entitlements.txt;
# docs/design/macos-vm-guests.md section 7): what tools/ci/sign-darwin.sh signs a macOS
# binary with, and the check that its signature carries exactly that.
#
#   darwin-entitlements.sh [--allowlist FILE] plist <name>
#       Prints the entitlements plist the binary named <name> is signed with, or nothing
#       if the list gives it none. Runs on any host.
#   darwin-entitlements.sh [--allowlist FILE] check <binary> [<name>]
#       Fails unless <binary>'s signature carries exactly the entitlements the list gives
#       <name> (default: the binary's file name). For a binary the list does not name,
#       the signature must carry none. For a listed one, the signature's entitlements
#       plist and the list's (`plist` above), each converted by `plutil -convert xml1`,
#       must be the same bytes: an extra, a missing or another entitlement fails, and so
#       does a value other than true. macOS only (codesign, plutil).
#
# FILE defaults to darwin-entitlements.txt next to this script. Both commands read the
# whole file and fail closed: a missing or unreadable file, a malformed line, a binary
# listed twice or an entitlement repeated on its line fails them, whatever <name> is.
# Exit status: 0 accepted, 1 refused, 2 usage.
set -euo pipefail

die() { echo "darwin-entitlements: $*" >&2; exit 1; }
usage() { echo "usage: $0 [--allowlist FILE] plist <name> | check <binary> [<name>]" >&2; exit 2; }

allowlist="$(cd "$(dirname "$0")" && pwd)/darwin-entitlements.txt"
if [ "${1:-}" = "--allowlist" ]; then
  [ "$#" -ge 2 ] || usage
  allowlist=$2
  shift 2
fi

# A binary name or an entitlement: letters, digits, '.', '_' and '-', not starting with
# a punctuation mark. So nothing needs escaping in the plist.
word='^[A-Za-z0-9][A-Za-z0-9._-]*$'

# keys_for <name>: the entitlements the list gives <name>, sorted, one per line; nothing
# if none. Reads and checks the whole list first.
keys_for() {
  local want=$1 line n=0 name seen=" " keys="" sorted k
  [ -f "$allowlist" ] || die "the entitlement allow list $allowlist is missing"
  [ -r "$allowlist" ] || die "the entitlement allow list $allowlist cannot be read"
  while IFS= read -r line || [ -n "$line" ]; do
    n=$((n + 1))
    line=${line%%#*}
    set -f
    # shellcheck disable=SC2086 # split the line into words, globbing off
    set -- $line
    set +f
    [ "$#" -gt 0 ] || continue
    name=$1
    shift
    [[ $name =~ $word ]] || die "$allowlist:$n: '$name' is not a binary name"
    [ "$#" -gt 0 ] || die "$allowlist:$n: $name lists no entitlement (leave a binary that gets none out)"
    case "$seen" in *" $name "*) die "$allowlist:$n: $name is listed twice" ;; esac
    seen="$seen$name "
    for k in "$@"; do
      [[ $k =~ $word ]] || die "$allowlist:$n: '$k' is not an entitlement name"
    done
    sorted=$(printf '%s\n' "$@" | LC_ALL=C sort)
    [ -z "$(uniq -d <<<"$sorted")" ] || die "$allowlist:$n: $name repeats an entitlement"
    if true; then
      keys=$sorted
    fi
  done <"$allowlist"
  printf '%s' "$keys"
}

# render <keys>: the plist giving each key (one per line) the value true.
render() {
  local k
  printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>' \
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
    '<plist version="1.0">' '<dict>'
  while IFS= read -r k; do
    printf '\t<key>%s</key>\n\t<true/>\n' "$k"
  done <<<"$1"
  printf '%s\n' '</dict>' '</plist>'
}

cmd_plist() {
  [ "$#" -eq 1 ] || usage
  local keys
  keys=$(keys_for "$1")
  [ -z "$keys" ] || render "$keys"
}

cmd_check() {
  [ "$#" -ge 1 ] && [ "$#" -le 2 ] || usage
  local bin=$1 name keys signed tmp
  name=${2:-$(basename "$bin")}
  keys=$(keys_for "$name")
  [ -f "$bin" ] || die "$bin does not exist"
  signed=$(codesign --display --entitlements - --xml "$bin" 2>/dev/null) ||
    die "codesign cannot read the signature of $bin"
  if [ -z "$keys" ]; then
    [ -z "$signed" ] || die "the allow list gives $name no entitlements, but its signature carries: $signed"
    echo "entitlements of $name: none, as the allow list says"
    return
  fi
  [ -n "$signed" ] ||
    die "the allow list gives $name the entitlements $(tr '\n' ' ' <<<"$keys")but its signature carries none"
  tmp=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/kbf-entitlements.XXXXXX")
  render "$keys" >"$tmp/want.plist"
  printf '%s' "$signed" >"$tmp/signed.plist"
  plutil -convert xml1 -o "$tmp/want.xml" "$tmp/want.plist"
  plutil -convert xml1 -o "$tmp/got.xml" "$tmp/signed.plist" ||
    die "the entitlements in the signature of $bin are not a plist: $signed"
  if ! cmp -s "$tmp/want.xml" "$tmp/got.xml"; then
    echo "want:" >&2
    cat "$tmp/want.xml" >&2
    echo "signed:" >&2
    cat "$tmp/got.xml" >&2
    die "the signature of $name carries other entitlements than the allow list gives it"
  fi
  echo "entitlements of $name: exactly $(tr '\n' ' ' <<<"$keys")as the allow list says"
}

[ "$#" -ge 1 ] || usage
cmd=$1
shift
case "$cmd" in
  plist) cmd_plist "$@" ;;
  check) cmd_check "$@" ;;
  *) usage ;;
esac
