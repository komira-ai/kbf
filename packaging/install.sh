#!/bin/sh
# The curl|sh entry of kbf-installer: download the installer payload for this host,
# check it against the release's SHA256SUMS, and place its files where the Debian
# package puts them. It starts nothing, enables nothing and decides nothing else.
#
#   curl -fsSL <release url>/install.sh | sudo sh -s -- [--release-url URL] [--platform P] [--root DIR]
#
# --release-url URL  where SHA256SUMS and the payload are (default: the latest release)
# --platform P       linux-x86_64 or linux-arm64 (default: this host)
# --root DIR         place the files under DIR, not /, and do not require root (for a
#                    staging directory and for tests)
#
# The payload (`cargo xtask installer-tarball`) holds usr/bin/kbf-installer,
# usr/lib/systemd/system/* and usr/share/kbf-installer/trust/*, and no kbf release:
# `sudo kbf-installer setup` installs the first one. macOS has no payload: nodes there
# get the .pkg.
#
# SHA256SUMS comes from the same place as the payload, so the check catches a damaged or
# mixed-up download, not a hostile server: use an https URL, and verify the release's
# attestation (docs/artifacts.md) where that matters.
#
# Every refusal is exit 1 with a line starting `install.sh: `, before a file is placed.
set -eu

release_url=https://github.com/komira-ai/komira-build-farm/releases/latest/download
root=/
unset platform

die() {
  echo "install.sh: $*" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case $1 in
    --release-url | --platform | --root)
      [ $# -ge 2 ] || die "$1 needs a value"
      case $1 in
        --release-url) release_url=$2 ;;
        --platform) platform=$2 ;;
        --root) root=$2 ;;
      esac
      shift 2
      ;;
    *) die "unknown argument '$1' (--release-url, --platform, --root)" ;;
  esac
done

[ -d "$root" ] || die "--root $root is not a directory"
if [ "$root" = / ] && [ "$(id -u)" != 0 ]; then
  die "must run as root (curl ... | sudo sh)"
fi

if [ -z "${platform+x}" ]; then
  host=$(uname -s)-$(uname -m)
  case $host in
    Linux-x86_64) platform=linux-x86_64 ;;
    Linux-aarch64 | Linux-arm64) platform=linux-arm64 ;;
    Darwin-*) die "macOS has no payload here: install the .pkg or the cask" ;;
    *) die "unsupported platform $host (linux-x86_64 and linux-arm64 are built)" ;;
  esac
fi
case $platform in
  linux-x86_64 | linux-arm64) ;;
  *) die "unsupported platform '$platform' (linux-x86_64 and linux-arm64 are built)" ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM

download() {
  curl -fsSL "$release_url/$1" -o "$tmp/$1" || die "cannot download $release_url/$1"
}

download SHA256SUMS

# The one line for this platform's payload.
name=
want=
count=0
while read -r sum file || [ -n "$sum" ]; do
  case $file in
    */*) continue ;;
    kbf-installer-payload-*-"$platform".tar.gz)
      count=$((count + 1))
      want=$sum
      name=$file
      ;;
  esac
done <"$tmp/SHA256SUMS"
[ "$count" -ge 1 ] || die "SHA256SUMS has no kbf-installer-payload line for $platform"
[ "$count" -le 1 ] || die "SHA256SUMS has more than one kbf-installer-payload line for $platform"
case $want in
  *[!0-9a-f]*) die "the SHA256SUMS sum for $name is not a sha256" ;;
esac
[ "${#want}" -eq 64 ] || die "the SHA256SUMS sum for $name is not a sha256"

download "$name"

if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$name")
elif command -v shasum >/dev/null 2>&1; then
  got=$(shasum -a 256 "$tmp/$name")
else
  die "neither sha256sum nor shasum is installed"
fi
got=${got%% *}
[ "$got" = "$want" ] || die "$name has sha256 $got, SHA256SUMS says $want"

# Only regular files, only where the package puts them, none outside the root.
tar -tzf "$tmp/$name" >"$tmp/names" || die "$name is not a tar.gz"
tar -tvzf "$tmp/$name" >"$tmp/types" || die "$name is not a tar.gz"
[ -s "$tmp/names" ] || die "$name is empty"
while read -r entry; do
  case $entry in
    /* | ../* | */../* | */..) die "$name holds the path '$entry'" ;;
    usr/bin/kbf-installer | usr/lib/systemd/system/?* | usr/share/kbf-installer/trust/?*) ;;
    *) die "$name holds '$entry', which the package does not" ;;
  esac
done <"$tmp/names"
while read -r mode _; do
  case $mode in
    -*) ;;
    *) die "$name holds an entry that is not a regular file" ;;
  esac
done <"$tmp/types"

umask 022
tar -xzf "$tmp/$name" -C "$root" || die "cannot place $name under $root"

echo "install.sh: placed $name under $root; nothing is started or enabled."
echo "install.sh: next: sudo kbf-installer setup"
