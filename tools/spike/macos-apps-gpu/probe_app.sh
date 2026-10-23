#!/bin/bash
# Usage: probe_app.sh <bindir> <workdir> <label>. Opens the ad hoc signed Hello.app through Launch Services.
bin=$1; work=$2; label=$3
mkdir -p "$work"; app="$bin/Hello.app"
marker="$work/marker-$label"; escape="/private/tmp/kbf-spike-escape-$label"
rm -f "$marker" "$escape"
echo "WHO $(id)"
echo "SESSION $(launchctl managername 2>&1)"
codesign -dv "$app" 2>&1 | grep -E "Identifier|Signature|flags" | tr '\n' ' '; echo
codesign --verify --strict "$app" && echo CODESIGN_VERIFY_OK
spctl --assess --type execute -v "$app" 2>&1 | head -2 | sed 's/^/SPCTL /'
perl -e 'alarm shift; exec @ARGV' 60 /usr/bin/open -n -W "$app" --args "$marker" "$escape" 2>&1 | sed 's/^/OPEN /'
echo "OPEN_EXIT ${PIPESTATUS[0]}"
sleep 2
if [ -f "$marker" ]; then echo "MARKER $(tr '\n' ' ' < "$marker")"; else echo "MARKER none"; fi
if [ -f "$escape" ]; then echo "ESCAPE_FILE_EXISTS $(ls -l "$escape" | awk '{print $3}')"; else echo "ESCAPE_FILE none"; fi
echo "DIRECT_EXEC (not via open):"; perl -e 'alarm shift; exec @ARGV' 20 "$app/Contents/MacOS/hello" "$work/direct-$label" "/private/tmp/kbf-spike-escape-direct-$label" 2>&1 | head -3 | sed 's/^/DIRECT /'
rm -f "$escape" "/private/tmp/kbf-spike-escape-direct-$label"
