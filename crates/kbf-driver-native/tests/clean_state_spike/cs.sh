# The shell side of the clean-state spike (tests/clean_state_spike). Sourced by bash; the test
# exports the cell's variables first:
#   W work dir, K canary, ID uid, GID gid, U name, H home, F /var/folders entry, L lease dir,
#   M marker, ROW row id, MK marker file (mtime = cell start), LEASES root of the lease dirs,
#   HOMES root of the homes, OU / OID a second pool user, STAGE leased|clean|reset, MODE cleaner,
#   SNAPX extras for the snapshot (btm, lsof).
# Functions named plant_<ROW> run as the pool user (sandboxed or not), probe_<ROW> and the
# cs_* functions as root. A probe prints `PROBE <row> present|absent`; a plant prints
# `PLANTED|DENIED|SKIPPED <row> ...` (via `p`) and `INFO <row> ...` lines.

HOMES=${HOMES:-/private/var/kbf-cs-homes.noindex}
MARK=kbf-cs-spike-mark

now_ms() { perl -MTime::HiRes=time -e 'printf "%d", time*1000'; }
alarm_run() { n=$1; shift; perl -e 'alarm shift; exec @ARGV' "$n" "$@"; }
asu() { launchctl asuser "$ID" /usr/bin/sudo -n -u "$U" -H "$@"; }
asu_other() { launchctl asuser "$OID" /usr/bin/sudo -n -u "$OU" -H "$@"; }
procs_all() { ps -axo pid=,ruid=,uid=,svuid=,stat= | awk -v id="$ID" '($2==id||$3==id||$4==id){print}'; }
procs_of() { ps -axo ruid=,uid=,svuid=,stat=,comm= | awk -v id="$ID" '($1==id||$2==id||$3==id){print}'; }
ipcs_has() { ipcs -a 2>/dev/null | awk -v t="$1" -v u="$U" -v id="$ID" '$1==t && ($5==u||$5==id||$7==u||$7==id){f=1} END{exit f?0:1}'; }
pr() { if eval "$1"; then echo "PROBE $ROW present"; else echo "PROBE $ROW absent"; fi; }
home_nonempty() { [ -n "$(find -x "$H" -mindepth 1 2>/dev/null | head -1)" ]; }
home_attr_bad() { [ "$(stat -f %Sf "$H" 2>/dev/null)" != "-" ] || [ "$(stat -f %Lp "$H" 2>/dev/null)" != 700 ]; }
dates() { sed -E 's/ [A-Z][a-z]{2} +[0-9]+ +([0-9]{2}:[0-9]{2}|[0-9]{4}) / <date> /'; }

# ---------------------------------------------------------------- users

cs_mkuser() { # name uid gid home
  _n=$1; _u=$2; _g=$3; _h=$4
  mkdir -p "$HOMES"; chown 0:0 "$HOMES"; chmod 755 "$HOMES"
  mkdir -p "$_h"; chown "$_u:$_g" "$_h"; chmod 700 "$_h"
  _r=/Users/$_n
  dscl . -create "$_r" || return 1
  dscl . -create "$_r" RealName "$MARK"
  dscl . -create "$_r" UniqueID "$_u"
  dscl . -create "$_r" PrimaryGroupID "$_g"
  dscl . -create "$_r" UserShell /usr/bin/false
  dscl . -create "$_r" NFSHomeDirectory "$_h"
  dscl . -create "$_r" IsHidden 1
  dscl . -create "$_r" Password '*'
  dscacheutil -flushcache
  for _i in $(seq 1 60); do
    [ "$(id -u "$_n" 2>/dev/null)" = "$_u" ] && return 0
    sleep 0.5
  done
  echo "mkuser: $_n never resolved to $_u"; return 1
}

cs_rmuser() { # name uid home
  _n=$1; _u=$2; _h=$3
  launchctl bootout "user/$_u" >/dev/null 2>&1
  launchctl bootout "gui/$_u" >/dev/null 2>&1
  for _i in $(seq 1 30); do
    pkill -9 -U "$_u" 2>/dev/null; pkill -9 -u "$_u" 2>/dev/null
    ps -axo ruid=,uid=,svuid= | awk -v id="$_u" '($1==id||$2==id||$3==id){f=1} END{exit f?0:1}' || break
    sleep 0.3
  done
  crontab -r -u "$_n" >/dev/null 2>&1; rm -f "/var/at/tabs/$_n"
  # hdiutil images and mounts under the home go first
  mount | awk -v h="$_h" -v l="/private/var/kbf-cs-leases.noindex" '{ i=index($0," on "); r=substr($0,i+4); j=index(r," ("); mp=substr(r,1,j-1); if (index(mp,h)==1||index(mp,l)==1) print mp }' |
    while IFS= read -r mp; do hdiutil detach -force "$mp" >/dev/null 2>&1 || umount -f "$mp" 2>/dev/null; done
  dscl . -delete "/Users/$_n" >/dev/null 2>&1
  chflags -R nouchg,nouappnd,noschg,nosappnd "$_h" >/dev/null 2>&1
  chmod -RN "$_h" >/dev/null 2>&1; chmod -R u+rwx "$_h" >/dev/null 2>&1
  rm -rf "$_h"
  find /private/var/folders -mindepth 2 -maxdepth 2 -uid "$_u" -exec chflags -R nouchg,nouappnd {} + >/dev/null 2>&1
  find /private/var/folders -mindepth 2 -maxdepth 2 -uid "$_u" -exec rm -rf {} + >/dev/null 2>&1
  rm -rf "$HOMES"/.kbf-recreated-"$_n"-* /private/var/folders/*/.kbf-recreated-"$_n"-* 2>/dev/null
  find /private/tmp /private/var/tmp /Users/Shared /Library/Caches -xdev -uid "$_u" -depth -delete >/dev/null 2>&1
  rm -f "/private/var/db/com.apple.xpc.launchd/disabled.$_u.plist" "/private/var/db/com.apple.xpc.launchd/loginitems.$_u.plist"
  dscacheutil -flushcache
}

cs_sweep() { # every user of the spike (by name, uid block and mark), then the homes dir
  for _n in $(dscl . -list /Users UniqueID | awk '$1 ~ /^kbf-cs-/ {print $1}'); do
    _u=$(dscl . -read "/Users/$_n" UniqueID 2>/dev/null | awk '{print $2}')
    cs_rmuser "$_n" "$_u" "$HOMES/$_n"
  done
  rm -rf "$HOMES"/.kbf-recreated-kbf-cs-* 2>/dev/null
  rmdir "$HOMES" 2>/dev/null
  echo "sweep: users left: $(dscl . -list /Users UniqueID | awk '$1 ~ /^kbf-cs-/' | wc -l | tr -d ' ')"
}

# ---------------------------------------------------------------- snapshot

snap_raw() { # a passive, root-only reading of everything the uid can own or leave
  echo "== home";    find -x "$H" -exec ls -ldeO@ {} + 2>&1 | dates
  echo "== folders"; find -x "$F" -exec ls -ldeO@ {} + 2>&1 | dates
  echo "== shared";  find /private/tmp /private/var/tmp /Users/Shared /Library/Caches -xdev -user "$ID" -exec ls -ldeO@ {} + 2>&1 | dates
  echo "== procs";   ps -axo ruid=,uid=,svuid=,stat=,comm= | awk -v id="$ID" '($1==id||$2==id||$3==id){print $4, $5}' | sort
  echo "== launchd-user"; launchctl print "user/$ID" 2>&1 | awk 'NF>=3 && $1 ~ /^[0-9-]+$/ && $2 ~ /^[0-9-]+$/ {print $3}' | sort
  echo "== launchd-gui";  launchctl print "gui/$ID" 2>&1 | awk 'NF>=3 && $1 ~ /^[0-9-]+$/ && $2 ~ /^[0-9-]+$/ {print $3}' | sort
  echo "== disabled"; launchctl print-disabled "user/$ID" 2>&1 | sort
  echo "== ipcs";    ipcs -a 2>/dev/null | awk -v u="$U" -v id="$ID" '($1=="m"||$1=="q"||$1=="s") && ($5==u||$5==id||$7==u||$7==id){print $1,$3,$4}' | sort
  echo "== mounts";  mount | grep -E "$H|$F|$LEASES" ; hdiutil info 2>/dev/null | grep image-path | grep -E "$H|$F|$LEASES"
  echo "== cron";    ls /var/at/tabs 2>/dev/null | grep -x "$U"; find /var/at/jobs -user "$ID" 2>/dev/null
  echo "== crash";   ls "$H/Library/Logs/DiagnosticReports" 2>/dev/null | sort; find /Library/Logs/DiagnosticReports /cores -newer "$MK" -name 'canary*' 2>/dev/null | sort
  echo "== ds";      dscl . -read "/Users/$U" 2>&1 | shasum
  echo "== tcc";     sqlite3 -readonly "$H/Library/Application Support/com.apple.TCC/TCC.db" 'select count(*) from access' 2>&1 | head -1
  case " $SNAPX " in *" lsof "*) echo "== lsof"; lsof -nP -a -u "$ID" 2>/dev/null | sed 1d | awk '{print $1,$5,$9,$10}' | sort ;; esac
  case " $SNAPX " in *" btm "*)  echo "== btm"; sfltool dumpbtm 2>&1 | grep -c -E "$U|$H" ;; esac
}

snap() { snap_raw | awk '/^== /{s=substr($0,4); next} {print s"|"$0}'; }
sdiff_() { diff "$1" "$2" | grep '^[<>]' | cut -c1-200; }

# ---------------------------------------------------------------- the plant wrapper

prelude() {
  export HOME="$H" USER="$U" LOGNAME="$U"
  T=$(getconf DARWIN_USER_TEMP_DIR); C=$(getconf DARWIN_USER_CACHE_DIR); D=$(getconf DARWIN_USER_DIR)
  export T C D
  cd "$L" || exit 1
}

p() { # p <subrow> <command...>: PLANTED, DENIED or SKIPPED (rc 77), with the first lines of its output
  _r=$1; shift
  ( "$@" ) >"$L/.o.$_r" 2>&1 </dev/null
  _rc=$?
  if [ "$_rc" = 0 ]; then echo "PLANTED $_r"
  elif [ "$_rc" = 77 ]; then echo "SKIPPED $_r $(head -c 160 "$L/.o.$_r" | tr '\n' ' ')"
  else echo "DENIED $_r rc=$_rc $(head -c 200 "$L/.o.$_r" | tr '\n' ' ')"; fi
  head -8 "$L/.o.$_r" 2>/dev/null | cut -c1-200 | sed "s/^/INFO $_r /"
}

bg_ok() { # bg_ok <name> <command...>: start detached, succeed when it printed `CANARY ... ok`
  _n=$1; shift
  ( "$@" >"$L/bg.$_n" 2>&1 </dev/null & )
  for _i in $(seq 1 24); do
    sleep 0.5
    grep -q '^CANARY .* ok' "$L/bg.$_n" 2>/dev/null && { cat "$L/bg.$_n"; return 0; }
  done
  cat "$L/bg.$_n" 2>/dev/null
  return 1
}

plant_run() { prelude; "plant_$ROW"; }

hasfn() { type "$1" >/dev/null 2>&1; }

# ---------------------------------------------------------------- the cleaner P

p_bootout() { alarm_run 20 launchctl bootout "user/$ID" >/dev/null 2>&1; alarm_run 20 launchctl bootout "gui/$ID" >/dev/null 2>&1; }
p_cron() { crontab -r -u "$U" >/dev/null 2>&1; rm -f "/var/at/tabs/$U"; find /var/at/jobs -user "$ID" -delete 2>/dev/null; }
p_kill() {
  KI=0; _e=0
  while [ "$KI" -lt 100 ]; do
    KI=$((KI+1))
    pkill -9 -U "$ID" 2>/dev/null; pkill -9 -u "$ID" 2>/dev/null
    sleep 0.2
    if procs_all | grep -q .; then _e=0; else _e=$((_e+1)); [ "$_e" -ge 2 ] && break; fi
  done
  [ "$_e" -ge 2 ] || echo "PINFO kill-failed after $KI scans"
}
p_unmount() {
  mount | awk -v h="$H" -v f="$F" -v l="$LEASES" '{ i=index($0," on "); r=substr($0,i+4); j=index(r," ("); mp=substr(r,1,j-1); if (index(mp,h)==1||index(mp,f)==1||index(mp,l)==1) print mp }' |
    while IFS= read -r mp; do hdiutil detach -force "$mp" >/dev/null 2>&1 || diskutil unmount force "$mp" >/dev/null 2>&1 || umount -f "$mp" 2>/dev/null; done
}
p_ipc() {
  ipcs -a 2>/dev/null | awk -v u="$U" -v id="$ID" '($1=="m"||$1=="q"||$1=="s") && ($5==u||$5==id||$7==u||$7==id){print $1, $2}' |
    while read -r t i; do case $t in m) ipcrm -m "$i";; q) ipcrm -q "$i";; s) ipcrm -s "$i";; esac; done
}
p_flags() {
  [ "$MODE" = P-noflags ] || chflags -R nouchg,nouappnd,dump,noopaque,nohidden "$H" "$F" 2>/dev/null
  chmod -RN "$H" "$F" 2>/dev/null
  chmod -R u+rwx "$H" "$F" 2>/dev/null
}
p_delete() {
  DE=0
  if [ "$MODE" = P-nounmount ]; then
    rm -rf "$H"/* "$H"/.[!.]* "$H"/..?* 2>"$W/de.$ROW"
  else
    find -x "$H" -mindepth 1 -depth -delete 2>"$W/de.$ROW"
  fi
  # the entry's skeleton (its directories as the system made them, listed before the lease) stays, emptied
  for _d in $(cat "$W/skel.$M" 2>/dev/null || echo "0 C T"); do [ -d "$F/$_d" ] && find -x "$F/$_d" -mindepth 1 -depth -delete 2>>"$W/de.$ROW"; done
  ls -A "$F" 2>/dev/null | while IFS= read -r _e; do case " $(cat "$W/skel.$M" 2>/dev/null || echo "0 C T") " in *" $_e "*) ;; *) rm -rf "$F/$_e" 2>>"$W/de.$ROW";; esac; done
  DE=$(wc -l <"$W/de.$ROW" | tr -d ' ')
}
p_shared() {
  find /private/tmp /private/var/tmp /Users/Shared /Library/Caches -xdev -user "$ID" -depth -delete 2>/dev/null
  rm -f "/private/var/db/com.apple.xpc.launchd/disabled.$ID.plist" "/private/var/db/com.apple.xpc.launchd/loginitems.$ID.plist"
}
p_attrs() { chflags nouchg,nouappnd,nohidden "$H" "$F" 2>/dev/null; chmod 700 "$H"; chown "$ID:$GID" "$H"; }
crash_dirs() { echo /Library/Logs/DiagnosticReports /Library/Logs/DiagnosticReports/Retired /cores "$H/Library/Logs/DiagnosticReports" /private/var/root/Library/Logs/DiagnosticReports /Users/*/Library/Logs/DiagnosticReports /Users/*/Library/Logs/DiagnosticReports/Retired; }
p_crash() { find $(crash_dirs) -newer "$MK" -type f -name 'canary*' -delete 2>/dev/null; }

cs_clean_p() { # MODE: P | P-kill-first | P-rm-first | P-noflags | P-nounmount
  _t0=$(now_ms)
  if [ "$MODE" = P-rm-first ]; then p_flags; p_delete; fi
  if [ "$MODE" = P-kill-first ]; then p_kill; p_bootout; elif [ "$MODE" != P-nokill ]; then p_bootout; fi
  p_cron
  [ "$MODE" = P-kill-first ] || [ "$MODE" = P-nokill ] || p_kill
  [ "$MODE" = P-nounmount ] || p_unmount
  p_ipc
  if [ "$MODE" != P-rm-first ]; then p_flags; p_delete; fi
  p_shared; p_attrs; p_crash
  _t1=$(now_ms)
  echo "PINFO mode=$MODE kill_scans=$KI delete_errors=${DE:-0} took_ms=$((_t1-_t0))"
}

# P as a system LaunchDaemon (no TCC grants of a runner step), for W07
cs_clean_p_daemon() {
  _pl=/Library/LaunchDaemons/kbf.cs.clean.plist
  rm -f "$W/daemon.done" "$W/daemon.out"
  { echo '<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>Label</key><string>kbf.cs.clean</string>'
    echo "<key>ProgramArguments</key><array><string>/bin/bash</string><string>-c</string><string>"
    echo "export W='$W' K='$K' ID='$ID' GID='$GID' U='$U' H='$H' F='$F' L='$L' M='$M' ROW='$ROW' MK='$MK' LEASES='$LEASES' HOMES='$HOMES' MODE=P; . '$W/cs.sh'; cs_clean_p; touch '$W/daemon.done'"
    echo '</string></array><key>RunAtLoad</key><true/><key>StandardOutPath</key><string>'"$W"'/daemon.out</string><key>StandardErrorPath</key><string>'"$W"'/daemon.out</string></dict></plist>'; } >"$_pl"
  chown 0:0 "$_pl"; chmod 644 "$_pl"
  launchctl bootstrap system "$_pl" 2>&1
  for _i in $(seq 1 240); do [ -e "$W/daemon.done" ] && break; sleep 1; done
  cat "$W/daemon.out" 2>/dev/null
  launchctl bootout system/kbf.cs.clean >/dev/null 2>&1; rm -f "$_pl"
  [ -e "$W/daemon.done" ] || echo "PINFO daemon-clean-timeout"
}

cs_cell_end() { hasfn "teardown_$ROW" && "teardown_$ROW"; rm -rf /private/tmp/*"$M"* /private/var/tmp/*"$M"* "$W"/*"$M"* 2>/dev/null; }

# the driver's end of a lease: the lease directory goes (no mount crossed), before the slot is cleaned
cs_lease_end() {
  chflags -R nouchg,nouappnd "$L" 2>/dev/null; chmod -RN "$L" 2>/dev/null; chmod -R u+rwx "$L" 2>/dev/null
  find -x "$L" -mindepth 1 -depth -delete 2>/dev/null; rmdir "$L" 2>/dev/null
  [ -e "$L" ] && echo "LEASE-DIR-LEFT $(ls -A "$L" | head -3 | tr '\n' ' ')"
}
cs_skel() { ls -A "$F" 2>/dev/null | tr '\n' ' ' >"$W/skel.$M"; }

# ================================================================ CHANNEL ROWS (table 1)

do_CH01() { printf %s "$M" >"$L/f"; ln -s /etc/hosts "$L/sl"; mkfifo "$L/ff"; }
plant_CH01() { p CH01 do_CH01; }
probe_CH01() { pr '[ -e "$L/f" ] || [ -L "$L/sl" ] || [ -p "$L/ff" ]'; }

do_CH02() { printf %s "$M" >"$H/f"; mkdir -p "$H/Library/Preferences" "$H/.ssh"; ln -s /nonexistent "$H/dangling"; }
plant_CH02() { p CH02 do_CH02; }
probe_CH02() { pr '[ -e "$H/f" ] || [ -L "$H/dangling" ] || [ -d "$H/Library" ]'; }

do_CH03() { printf x >"${T}$M" && printf x >"${C}$M" && printf x >"${D}$M" && printf x >"${F}/$M"; }
plant_CH03() { p CH03 do_CH03; }
probe_CH03() { echo "INFO CH03 found: $(find -x "$F" -name "$M*" 2>/dev/null | sed "s#^$F#F#" | tr '\n' ';')"; pr '[ -n "$(find -x "$F" -name "$M*" 2>/dev/null | head -1)" ]'; }

put() { printf x >"$1/$M"; }
plant_CH04() { for d in /private/tmp /private/var/tmp /Users/Shared /Library/Caches; do p "CH04$(echo "$d" | tr / _)" put "$d"; done; }
probe_CH04() { echo "INFO CH04 found: $(find /private/tmp /private/var/tmp /Users/Shared /Library/Caches -maxdepth 1 -name "$M*" 2>/dev/null | tr '\n' ';')"; pr 'find /private/tmp /private/var/tmp /Users/Shared /Library/Caches -maxdepth 1 -name "$M*" 2>/dev/null | grep -q .'; }

fixture_CH05() { printf orig >"/private/tmp/fx-$M"; chmod 666 "/private/tmp/fx-$M"; }
do_CH05() { ln "/private/tmp/fx-$M" "$L/hl" && printf X >>"$L/hl"; }
plant_CH05() { p CH05 do_CH05; }
probe_CH05() { pr '[ "$(cat "/private/tmp/fx-$M" 2>/dev/null)" != orig ]'; }

do_CH06() { touch "$H/f" && xattr -w kbf "$M" "$H/f" && chflags nodump "$H/f" && chmod +a 'everyone deny delete' "$H/f"; }
plant_CH06() { p CH06 do_CH06; }
probe_CH06() { pr '[ -e "$H/f" ]'; }

do_CH07a() { hdiutil create -quiet -size 2m -fs HFS+ -volname "$M" "$L/a.dmg" && mkdir -p "$H/mnt" && hdiutil attach -quiet -mountpoint "$H/mnt" "$L/a.dmg"; }
do_CH07b() { mkdir "$L/dev" && mount_devfs devfs "$L/dev"; }
plant_CH07() { p CH07a do_CH07a; p CH07b do_CH07b; }
probe_CH07() { echo "INFO CH07 mounts: $(mount | grep -E "$H|$L" | sed -E 's/\(.*//' | tr '\n' ';' | cut -c1-200)"; pr 'mount | grep -E -q "$H/mnt|$L/dev"'; }

plant_CH08() { p CH08 "$K" sysv-sem; }
probe_CH08() { pr 'ipcs_has s'; }
teardown_CH08() { :; }
plant_CH09() { p CH09 "$K" sysv-shm; }
probe_CH09() { pr 'ipcs_has m'; }
plant_CH10() { p CH10 "$K" sysv-msg; }
probe_CH10() { pr 'ipcs_has q'; }

plant_CH11() { p CH11 "$K" shm-create "/$M"; }
probe_CH11() {
  echo "INFO CH11 root-open: $("$K" shm-open "/$M" 2>&1)"
  echo "INFO CH11 other-uid-open: $(asu_other "$K" shm-open "/$M" 2>&1)"
  echo "INFO CH11 lsof-mentions: $(lsof -nP 2>/dev/null | grep -c "$M")  ipcs-lines: $(ipcs -a 2>/dev/null | grep -ci shm)"
  pr '"$K" shm-open "/$M" 2>&1 | grep -q "ok"'
}
teardown_CH11() { "$K" shm-unlink "/$M" >/dev/null 2>&1; }
plant_CH12() { p CH12 "$K" sem-create "/$M"; }
probe_CH12() {
  echo "INFO CH12 other-uid-open: $(asu_other "$K" sem-open "/$M" 2>&1)"
  pr '"$K" sem-open "/$M" 2>&1 | grep -q "ok"'
}
teardown_CH12() { "$K" sem-unlink "/$M" >/dev/null 2>&1; }

plant_CH13() { p CH13 bg_ok m1 "$K" mach-register "kbf.$M" 300; }
probe_CH13() { pr 'asu "$K" mach-lookup "kbf.$M" 2>&1 | grep -q " ok"'; }

plant_CH14() { p CH14 bg_ok l1 "$K" listen-tcp 300; p CH14u bg_ok l2 "$K" listen-unix "${T}s$M" 300; }
probe_CH14() { pr 'lsof -nP -a -u "$ID" -i 2>/dev/null | grep -q LISTEN || lsof -nP -a -u "$ID" -U 2>/dev/null | grep -q "s$M"'; }

do_CH15() { nohup sleep 3600 >/dev/null 2>&1 & ( "$K" stopped >/dev/null 2>&1 & ); ( "$K" zombie >/dev/null 2>&1 & ); sleep 1; }
plant_CH15() { p CH15 do_CH15; }
probe_CH15() { echo "INFO CH15 procs: $(procs_of | grep -E 'sleep|canary' | tr '\n' ';' | cut -c1-200)"; pr 'procs_of | grep -E "sleep|canary" | grep -q .'; }

do_CH16() { launchctl submit -l "kbf.$M" -- /bin/sh -c "touch /private/tmp/esc-$M; sleep 3600" && sleep 2; }
plant_CH16() { p CH16 do_CH16; }
probe_CH16() { echo "INFO CH16 marker-owner: $(stat -f %Su "/private/tmp/esc-$M" 2>&1)"; pr '[ -e "/private/tmp/esc-$M" ] || launchctl print "user/$ID" 2>/dev/null | grep -q "kbf.$M"'; }

write_plist() { # label, command, extra keys
  mkdir -p "$H/Library/LaunchAgents"
  cat >"$H/Library/LaunchAgents/$1.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>Label</key><string>$1</string>$3<key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>$2</string></array></dict></plist>
EOF
}
boot_plist() { launchctl bootstrap "user/$ID" "$H/Library/LaunchAgents/$1.plist" 2>&1 || launchctl bootstrap "gui/$ID" "$H/Library/LaunchAgents/$1.plist" 2>&1; }
do_CH17() { write_plist "$M" "touch /private/tmp/ka-$M; sleep 1000" '<key>KeepAlive</key><true/><key>RunAtLoad</key><true/><key>ThrottleInterval</key><integer>1</integer>' && boot_plist "$M" && sleep 2; }
plant_CH17() { p CH17 do_CH17; }
probe_CH17() {
  pr 'launchctl print "user/$ID" 2>/dev/null | grep -q "$M" || launchctl print "gui/$ID" 2>/dev/null | grep -q "$M" || procs_of | grep -q "sleep" || [ -e "/private/tmp/ka-$M" ]'
}

do_CH18() { launchctl setenv "KBF_$M" v; }
plant_CH18() { p CH18 do_CH18; }
probe_CH18() { pr '[ -n "$(asu launchctl getenv "KBF_$M" 2>/dev/null)" ]'; }

do_CH19() {
  mkdir -p "$L/x.app/Contents/MacOS"
  printf '#!/bin/sh\ntouch /private/tmp/open-%s\n' "$M" >"$L/x.app/Contents/MacOS/x"; chmod +x "$L/x.app/Contents/MacOS/x"
  cat >"$L/x.app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>CFBundleExecutable</key><string>x</string><key>CFBundleIdentifier</key><string>kbf.cs.x</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleName</key><string>x</string></dict></plist>
EOF
  alarm_run 25 open -g -W "$L/x.app"
}
plant_CH19() { p CH19 do_CH19; }
probe_CH19() { pr '[ -e "/private/tmp/open-$M" ]'; }

do_CH20() { alarm_run 25 osascript -e "tell application \"System Events\" to do shell script \"touch /private/tmp/ae-$M\""; }
plant_CH20() { p CH20 do_CH20; }
probe_CH20() { pr '[ -e "/private/tmp/ae-$M" ]'; }

do_CH21() { write_plist "$M" "/usr/bin/true" '<key>RunAtLoad</key><false/>' && boot_plist "$M"; }
plant_CH21() { p CH21 do_CH21; }
probe_CH21() { _b=$(sfltool dumpbtm 2>&1); echo "INFO CH21 dumpbtm-bytes: $(printf %s "$_b" | wc -c | tr -d ' ') lines-with-user: $(printf %s "$_b" | grep -c "$U")"; pr 'printf %s "$_b" | grep -q "$M"'; }

do_CH22() { printf '* * * * * touch /private/tmp/cron-%s\n' "$M" >"$L/ct" && crontab "$L/ct"; }
plant_CH22() { p CH22 do_CH22; }
probe_CH22() { pr '[ -e "/var/at/tabs/$U" ]'; }

do_CH23() {
  mkdir -p "$H/.ssh"; chmod 700 "$H/.ssh"
  ssh-keygen -q -N '' -f "$H/.ssh/k" && cp "$H/.ssh/k.pub" "$H/.ssh/authorized_keys" && chmod 600 "$H/.ssh/authorized_keys" &&
  ssh -i "$H/.ssh/k" -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile="$H/.ssh/kh" -o ConnectTimeout=8 "$U@127.0.0.1" "touch /private/tmp/ssh-$M"
}
plant_CH23() { p CH23 do_CH23; }
probe_CH23() { echo "INFO CH23 remotelogin: $(systemsetup -getremotelogin 2>&1 | head -1)"; pr '[ -e "/private/tmp/ssh-$M" ]'; }

do_CH24() { defaults write "kbf.$M" k "$M" && defaults write -g "KBF_$M" 1; }
plant_CH24() { p CH24 do_CH24; }
probe_CH24() { pr 'asu defaults read "kbf.$M" k 2>/dev/null | grep -q "$M" || [ -e "$H/Library/Preferences/kbf.$M.plist" ]'; }

do_CH25() {
  mkdir -p "$H/Library/Keychains"
  security create-keychain -p '' "$H/Library/Keychains/$M.keychain-db" &&
  security add-generic-password -a x -s "$M" -w "$M" "$H/Library/Keychains/$M.keychain-db"
}
plant_CH25() { p CH25 do_CH25; }
probe_CH25() { pr '[ -e "$H/Library/Keychains/$M.keychain-db" ]'; }

do_CH26() { _d="$H/Library/Application Support/com.apple.TCC"; mkdir -p "$_d" && sqlite3 "$_d/TCC.db" 'create table kbf(x)'; }
plant_CH26() { p CH26 do_CH26; }
probe_CH26() { pr '[ -e "$H/Library/Application Support/com.apple.TCC/TCC.db" ]'; }

plant_CH27() { p CH27 bg_ok n1 "$K" notify-set "kbf.$M" 42 300; }
probe_CH27() { echo "INFO CH27 get: $("$K" notify-get "kbf.$M" 2>&1)"; pr '"$K" notify-get "kbf.$M" 2>&1 | grep -q "value=42"'; }

do_CH28() { printf %s "$M" | pbcopy; }
plant_CH28() { p CH28 do_CH28; }
probe_CH28() { pr '[ "$(asu pbpaste 2>/dev/null)" = "$M" ]'; }

do_CH29() {
  _ok=1
  dscl . -create "/Users/$U" Comment "$M" 2>&1 && _ok=0
  dseditgroup -o edit -a "$U" -t user admin 2>&1 && _ok=0
  return $_ok
}
plant_CH29() { p CH29 do_CH29; }
probe_CH29() { pr 'dscl . -read "/Users/$U" Comment 2>/dev/null | grep -q "$M" || dseditgroup -o checkmember -m "$U" admin 2>&1 | grep -q "^yes"'; }

do_CH30() { printf x >"$H/spot-$M" && sleep 20; }
plant_CH30() { p CH30 do_CH30; }
probe_CH30() { echo "INFO CH30 mdutil: $(mdutil -s "$H" 2>&1 | tr '\n' ' ' | cut -c1-160)"; pr 'mdfind -name "spot-$M" 2>/dev/null | grep -q .'; }

do_CH31() { cp /usr/bin/true "$L/t-$M" && "$L/t-$M" && arch -x86_64 /usr/bin/true; }
plant_CH31() { p CH31 do_CH31; }
probe_CH31() {
  echo "INFO CH31 execpolicy: $(sqlite3 -readonly /var/db/SystemPolicyConfiguration/ExecPolicy 'select count(*) from policy_scan_cache' 2>&1 | head -1)"
  pr '[ -n "$(find /private/var/db/oah -newer "$MK" 2>/dev/null | head -1)" ]'
}

do_CH32() { "$K" oslog "kbf-spike-$M" && logger "kbf-spike-$M" && sleep 2; }
plant_CH32() { p CH32 do_CH32; }
probe_CH32() {
  echo "INFO CH32 other-uid: $(asu_other log show --last 5m --predicate "eventMessage contains \"kbf-spike-$M\"" 2>&1 | head -3 | tr '\n' ' ' | cut -c1-160)"
  pr 'log show --last 5m --style compact --predicate "eventMessage contains \"kbf-spike-$M\"" 2>/dev/null | grep -q "kbf-spike-$M"'
}

do_CH33() { "$K" crash-segv; "$K" crash-abort; sleep 40; echo "reports-dir-entries: $(ls /Library/Logs/DiagnosticReports 2>/dev/null | wc -l | tr -d ' ') home-dir: $(ls "$H/Library/Logs/DiagnosticReports" 2>&1 | head -3 | tr '\n' ' ')"; true; }
plant_CH33() { p CH33 do_CH33; }
probe_CH33() {
  echo "INFO CH33 found: $(find $(crash_dirs) -newer "$MK" -type f -name 'canary*' 2>/dev/null | head -4 | tr '\n' ';')"
  pr '[ -n "$(find $(crash_dirs) -newer "$MK" -type f -name "canary*" 2>/dev/null | head -1)" ]'
}

do_CH34() {
  _rt=$(xcrun simctl list runtimes available 2>/dev/null | awk '/iOS/ {print $NF; exit}')
  _dt=$(xcrun simctl list devicetypes 2>/dev/null | awk '/iPhone/ {print $NF; exit}' | tr -d '()')
  [ -n "$_rt" ] && [ -n "$_dt" ] || { echo "no iOS runtime or device type"; return 77; }
  _u=$(xcrun simctl create "$M" "$_dt" "$_rt") && xcrun simctl boot "$_u"
}
plant_CH34() { p CH34 do_CH34; }
probe_CH34() { pr 'asu xcrun simctl list devices 2>/dev/null | grep -q "$M"'; }

do_CH35() { for _b in crontab at sudo su login; do _o=$(alarm_run 5 /usr/bin/$_b </dev/null 2>&1 | head -c 70 | tr '\n' ' '); echo "setuid-exec $_b first-output: $_o"; done; }
plant_CH35() { p CH35 do_CH35; }
probe_CH35() { pr 'false'; }

do_CH36() {
  _ok=1
  sysctl -w kern.sysv.shmmax=1 >/dev/null 2>&1 && _ok=0
  nvram "kbf=$M" >/dev/null 2>&1 && _ok=0
  date -u 010100002099 >/dev/null 2>&1 && _ok=0
  return $_ok
}
plant_CH36() { p CH36 do_CH36; }
probe_CH36() { pr '[ "$(date -u +%Y)" = 2099 ] || nvram -p 2>/dev/null | grep -q "^kbf"'; }

do_CH37() { dscacheutil -q host -a name "$M.invalid"; true; }
plant_CH37() { p CH37 do_CH37; }
probe_CH37() { pr 'false'; }

# ================================================================ WIPE ROWS (table 2)

do_W01() {
  touch "$H/u" "$H/a" "$H/n" && mkdir "$H/d" && touch "$H/d/f" &&
  chflags uchg "$H/u" && chflags uappnd "$H/a" && chflags nodump,hidden,opaque "$H/n" && chflags uchg "$H/d/f" && chflags uchg "$H/d" || return 1
  for _fl in schg sappnd restricted; do echo "try $_fl: $(chflags "$_fl" "$H/n" 2>&1 | head -1) rc=$?"; done
}
plant_W01() { p W01 do_W01; }
probe_W01() { pr 'home_nonempty'; }

do_W02() {
  mkdir "$H/acl" && touch "$H/acl/f" &&
  chmod +a 'everyone deny delete' "$H/acl/f" &&
  chmod +a 'everyone deny delete_child,add_file,add_subdirectory,file_inherit,directory_inherit' "$H/acl" &&
  chmod +a 'everyone deny write,append,writeattr,writeextattr,chown,writesecurity' "$H/acl/f"
}
plant_W02() { p W02 do_W02; }
probe_W02() { pr 'home_nonempty'; }

do_W03() { mkdir -p "$H/z/y" "$H/go/a/b" && touch "$H/z/y/f" "$H/go/a/b/f" && chmod 000 "$H/z/y" "$H/z" && chmod -R a-w "$H/go"; }
plant_W03() { p W03 do_W03; }
probe_W03() { pr 'home_nonempty'; }

install_job() { # <plist>: root puts a job in the uid's launchd on whichever way works; prints how
  _pl=$1
  _o1=$(launchctl bootstrap "user/$ID" "$_pl" 2>&1 | head -1)
  if launchctl print "user/$ID" 2>/dev/null | grep -q "$M"; then echo "job installed via bootstrap user/"; return 0; fi
  _o2=$(launchctl asuser "$ID" /bin/launchctl load -w "$_pl" 2>&1 | head -1); sleep 1
  if launchctl asuser "$ID" /bin/launchctl list 2>/dev/null | grep -q "$M"; then echo "job installed via asuser launchctl load"; return 0; fi
  _o3=$(launchctl asuser "$ID" /usr/bin/sudo -n -u "$U" /bin/launchctl load "$_pl" 2>&1 | head -1); sleep 1
  if launchctl asuser "$ID" /usr/bin/sudo -n -u "$U" /bin/launchctl list 2>/dev/null | grep -q "$M"; then echo "job installed via asuser sudo launchctl load"; return 0; fi
  echo "NO-JOB bootstrap: $_o1 | asuser load: $_o2 | asuser sudo load: $_o3"; return 1
}

# a respawning job in the user's launchd domain, put there by root (a pool user cannot bootstrap one itself on a
# runner: see CH17), stands for any job launchd keeps alive for the uid
fixture_W04() {
  cat >"$W/$M.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Label</key><string>$M</string><key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>1</integer><key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>echo x >>"$H/respawn-$M"; sleep 2</string></array></dict></plist>
EOF
  chmod 644 "$W/$M.plist"
  echo "W04 fixture: $(install_job "$W/$M.plist")"
  sleep 3
}
do_W04() { launchctl submit -l "kbf.w4.$M" -- /bin/sh -c "echo x >>$H/submit-$M; sleep 2"; echo "submit rc=$?"; sleep 2; }
plant_W04() { p W04 do_W04; }
probe_W04() {
  _any=0
  for _s in 0 2 3; do
    sleep "$_s"
    [ "$(wc -l <"$H/respawn-$M" 2>/dev/null | tr -d ' ')" -gt 1 ] 2>/dev/null && _any=1
    launchctl print "user/$ID" 2>/dev/null | grep -q "$M" && _any=1
    launchctl asuser "$ID" /bin/launchctl list 2>/dev/null | grep -q "$M" && _any=1
  done
  echo "INFO W04 respawn-lines: $(wc -l <"$H/respawn-$M" 2>/dev/null | tr -d ' ') job-listed: $(launchctl print "user/$ID" 2>/dev/null | grep -c "$M")"
  [ "$_any" = 1 ] && echo "PROBE $ROW present" || echo "PROBE $ROW absent"
}

do_W05() { ( "$K" hydra 200 10 >/dev/null 2>&1 & ); ( "$K" stopped >/dev/null 2>&1 & ); ( "$K" zombie >/dev/null 2>&1 & ); sleep 3; echo "canaries: $(procs_of | grep -c canary)"; }
plant_W05() { p W05 do_W05; }
probe_W05() { echo "INFO W05 canaries: $(procs_of | grep -c canary) maxprocperuid: $(sysctl -n kern.maxprocperuid)"; pr 'procs_of | grep -q canary'; }

plant_W06a() { p W06a bg_ok hd "$K" hold-deleted "$L/big" 128; }
probe_W06a() { echo "INFO W06a lsof: $(lsof -a +L1 -u "$ID" 2>/dev/null | sed 1d | awk '{print $1,$2,$9}' | head -4 | tr '\n' ';')"; pr 'lsof -a +L1 -u "$ID" 2>/dev/null | sed 1d | grep -q canary'; }
plant_W06() { plant_W06a; }
probe_W06() { probe_W06a; }

fixture_W06b() { hdiutil create -quiet -size 4m -fs HFS+ -volname "$M" "$W/img-$M.dmg" && chown "$ID:$GID" "$W/img-$M.dmg" && chmod 600 "$W/img-$M.dmg"; ls -l "$W/img-$M.dmg"; }
do_W06b() {
  mkdir -p "$H/mnt" && hdiutil attach -quiet -mountpoint "$H/mnt" "$W/img-$M.dmg" || return 1
  printf sentinel >"$H/mnt/sentinel" || return 1
  sync; sleep 1
  bg_ok hc "$K" hold-cwd "$H/mnt" && bg_ok hl "$K" hold-lock "$H/mnt/f"
}
plant_W06b() { p W06b do_W06b; }
probe_W06b() { pr 'mount | grep -q "$H/mnt"'; }
teardown_W06b() {
  mkdir -p "$W/chk-$M"
  if hdiutil attach -quiet -readonly -mountpoint "$W/chk-$M" "$W/img-$M.dmg" >/dev/null 2>&1; then
    [ "$(cat "$W/chk-$M/sentinel" 2>/dev/null)" = sentinel ] && echo "SENTINEL W06b intact" || echo "SENTINEL W06b destroyed"
    hdiutil detach -force "$W/chk-$M" >/dev/null 2>&1
  else echo "SENTINEL W06b image-not-attachable"; fi
}

plant_W06c() { p W06c bg_ok hl2 "$K" hold-lock "$H/lock"; p W06cu bg_ok ls "$K" listen-unix "${T}s$M" 300; }
probe_W06c() { pr 'home_nonempty || lsof -nP -a -u "$ID" 2>/dev/null | grep -q canary'; }

do_W07() {
  while IFS= read -r d; do
    if mkdir -p "$H/$d" 2>/dev/null && touch "$H/$d/f" 2>/dev/null; then echo "create ok: $d"; else echo "create FAILED: $d"; fi
  done <<EOF
Desktop
Documents
Downloads
Library/Safari
Library/Mail
Library/Messages
Library/Calendars
Library/Cookies
Library/Application Support/AddressBook
Library/Application Support/com.apple.TCC
Library/Group Containers/group.com.apple.notes
Library/Containers/com.apple.Safari/Data
EOF
  [ -n "$(find -x "$H" -name f 2>/dev/null | head -1)" ]
}
plant_W07() { p W07 do_W07; }
probe_W07() { echo "INFO W07 left: $(find -x "$H" -mindepth 1 2>/dev/null | head -12 | sed "s#$H/##" | tr '\n' ';')"; pr 'home_nonempty'; }

fixture_W08() { printf s >"/private/tmp/s-$M"; chmod 666 "/private/tmp/s-$M"; }
do_W08() {
  ln -s "/private/tmp/s-$M" "$H/l1"; ln -s / "$H/l2"; ln -s .. "$H/l3"
  ln "/private/tmp/s-$M" "$H/hl" 2>&1 | head -1
  mkdir "$H/deep" && cd "$H/deep" && perl -e '$n="a" x 255; for (1..120) { mkdir($n,0755) or die "mkdir $_: $!"; chdir($n) or die "chdir $_: $!" }'
  cd "$L"
  return 0
}
plant_W08() { p W08 do_W08; }
probe_W08() { pr 'home_nonempty'; }
teardown_W08() { [ "$(cat "/private/tmp/s-$M" 2>/dev/null)" = s ] && echo "SENTINEL W08 intact" || echo "SENTINEL W08 changed-or-gone"; }

do_W09() {
  perl -e 'for $d (0..399) { mkdir "$ENV{H}/h$d"; for $f (0..249) { open(F, ">$ENV{H}/h$d/f$f") or die; close F } }' || return 1
  mkfile -n 5g "$H/sparse"
  touch "$H/nl"$'\n'"x" "$H/ts " "$H/a" "$H/A"
  touch "$H/$(printf '\377\376')" 2>&1 | head -1
  touch "$H/xa" && xattr -w big "$(head -c 60000 /dev/zero | tr '\0' x)" "$H/xa"
  cp /bin/sh "$H/sh" && chmod u+s,g+s "$H/sh"
  mkdir -m 1777 "$H/st"; mkfifo "$H/ff"
  echo "files: $(find -x "$H" -type f 2>/dev/null | wc -l | tr -d ' ')"
}
plant_W09() { p W09 do_W09; }
probe_W09() { pr 'home_nonempty'; }

do_W10() { touch "$H/x"; chflags uchg "$H" && chmod 000 "$H" && chflags uchg "$L"; chflags hidden "$F"; true; }
plant_W10() { p W10 do_W10; }
probe_W10() { echo "INFO W10 home: $(stat -f '%Sp %Sf' "$H") entry: $(stat -f '%Sf' "$F")"; pr 'home_attr_bad || case "$(stat -f %Sf "$F" 2>/dev/null)" in *uchg*|*hidden*|*uappnd*) true;; *) false;; esac || home_nonempty'; }

fixture_W11() {
  cat >"$W/$M.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Label</key><string>$M</string><key>StartInterval</key><integer>5</integer><key>ProgramArguments</key><array><string>/usr/bin/touch</string><string>$H/si-$M</string></array></dict></plist>
EOF
  chmod 644 "$W/$M.plist"
  echo "W11 fixture: $(install_job "$W/$M.plist")"
}
do_W11a() { printf '* * * * * touch %s/cron-%s\n' "$H" "$M" >"$L/ct" && crontab "$L/ct"; }
plant_W11() { p W11a do_W11a; }
probe_W11() {
  [ "$STAGE" = reset ] || sleep 75
  echo "INFO W11 home: $(ls "$H" 2>/dev/null | tr '\n' ' ') crontab: $(ls /var/at/tabs 2>/dev/null | grep -c -x "$U")"
  pr 'ls "$H" 2>/dev/null | grep -q -E "cron-|si-"'
}

plant_W12() { echo "SKIPPED W12 reason=runner-safety"; }
probe_W12() { pr 'false'; }

# ================================================================ BASELINE AND BUILDS

plant_B1() { p B1 /usr/bin/true; }
probe_B1() { echo "PROBE B1 none"; }

cs_prepare_rust() {
  RHOME=$(dscl . -read "/Users/${SUDO_USER:-runner}" NFSHomeDirectory 2>/dev/null | awk '{print $2}')
  [ -n "$RHOME" ] || RHOME=/Users/runner
  chmod a+x "$RHOME"
  chmod -R a+rX "$RHOME/.rustup" "$RHOME/.cargo" 2>/dev/null
  echo "$RHOME" >"$W/rhome"
  echo "rust-home=$RHOME toolchains=$(ls "$RHOME/.rustup/toolchains" 2>&1 | tr '\n' ' ')"
}

# the driver's lease environment (src/home.rs): HOME and CFFIXED_USER_HOME in the lease, TMPDIR, XDG_CACHE_HOME and the clang module cache beside it
lease_env_run() {
  mkdir -p "$L/home" "$L/tmp" "$L/cache/clang/ModuleCache"
  _bp=$(build_path)
  perl -e 'alarm shift; exec @ARGV' "${BT:-0}" env -i HOME="$L/home" CFFIXED_USER_HOME="$L/home" TMPDIR="$L/tmp" XDG_CACHE_HOME="$L/cache" CLANG_MODULE_CACHE_PATH="$L/cache/clang/ModuleCache" CARGO_HOME="$L/cargo" PATH="$_bp" "$@"
}

build_path() { echo "/usr/bin:/bin:/usr/sbin:/sbin:$(ls -d "$(cat "$W/rhome" 2>/dev/null)"/.rustup/toolchains/*/bin 2>/dev/null | head -1)"; }
build_env() {
  _bp=$(build_path)
  if [ "$BENV" = hostile ]; then env -i PATH="$_bp" "$@"
  elif [ "$BENV" = lease ]; then lease_env_run "$@"
  else env -i HOME="$H" TMPDIR="$L/tmp" CARGO_HOME="$L/cargo" PATH="$_bp" "$@"; fi
}

do_BR1() {
  mkdir -p "$L/tmp" "$L/hello1/src" && printf '[package]\nname="hello1"\nversion="0.1.0"\nedition="2021"\n' >"$L/hello1/Cargo.toml" &&
  printf 'fn main(){println!("hi");}\n' >"$L/hello1/src/main.rs" && cd "$L/hello1" && build_env cargo build --offline
}
do_BR2() { mkdir -p "$L/tmp" && printf 'int main(void){return 0;}\n' >"$L/hello.c" && build_env clang -o "$L/hello" "$L/hello.c"; }
do_BR3() {
  mkdir -p "$L/tmp" && printf 'print("hi")\n' >"$L/hello.swift" && build_env swiftc -o "$L/hello3" "$L/hello.swift" || return 1
  mkdir -p "$L/pkg" && cd "$L/pkg" && build_env swift package init --type executable --name pk >/dev/null 2>&1
  build_env swift build >"$L/swiftbuild.log" 2>&1; echo "swift build rc=$? $(tail -c 300 "$L/swiftbuild.log" | tr '\n' ' ')"
}
do_BR4() {
  mkdir -p "$L/tmp" "$L/r" && cd "$L/r" && build_env git init -q . && echo x >f && build_env git add f &&
  build_env git -c user.name=x -c user.email=x@y.z commit -q -m m && build_env git clone -q "$L/r" "$L/r2"
}
do_BR5() { mkdir -p "$L/tmp" && build_env /usr/bin/python3 -m venv "$L/venv" && build_env "$L/venv/bin/python" -c 'import sqlite3,ssl'; }
plant_BR1() { p BR1 do_BR1; }
plant_BR2() { p BR2 do_BR2; }
plant_BR3() { p BR3 do_BR3; }
plant_BR4() { p BR4 do_BR4; }
plant_BR5() { p BR5 do_BR5; }

probe_BR() {
  case "$STAGE" in late*) echo "PROBE $ROW none"; return;; esac
  _start=$(date -r "$(stat -f %m "$MK")" '+%Y-%m-%d %H:%M:%S')
  log show --start "$_start" --style compact --predicate 'sender == "Sandbox" OR subsystem == "com.apple.sandbox.reporting" OR eventMessage BEGINSWITH "Sandbox: " OR eventMessage CONTAINS "deny("' 2>/dev/null |
    grep 'deny(' | sed -E 's/^.*Sandbox: //' | sed -E "s#$H#<H>#g;s#$F#<F>#g;s#$L#<L>#g;s/\\([0-9]+\\)//" | sort | uniq -c | sort -rn | head -30 | sed "s/^/BUILDDENY $ROW $BENV /"
  echo "BUILDWROTE $ROW $BENV home=$(find -x "$H" 2>/dev/null | wc -l | tr -d ' ') folders=$(find -x "$F" 2>/dev/null | wc -l | tr -d ' ') lease=$(find -x "$L" 2>/dev/null | wc -l | tr -d ' ') shared=$(find /private/tmp /private/var/tmp /Users/Shared /Library/Caches -xdev -user "$ID" 2>/dev/null | wc -l | tr -d ' ')"
  find -x "$H" "$F" -mindepth 1 -maxdepth 3 2>/dev/null | sed "s#^$H#H#;s#^$F#F#" | head -30 | sed "s/^/BUILDWRITE $ROW $BENV inside-allowed /"
  find /private/tmp /private/var/tmp /Users/Shared /Library/Caches -xdev -user "$ID" 2>/dev/null | head -10 | sed "s/^/BUILDWRITE $ROW $BENV OUTSIDE-allowed /"
  echo "PROBE $ROW none"
}
probe_BR1() { probe_BR; }
probe_BR2() { probe_BR; }
probe_BR3() { probe_BR; }
probe_BR4() { probe_BR; }
probe_BR5() { probe_BR; }

cs_posctl() {
  _cu=$(stat -f %u /dev/console)
  cat >/Library/LaunchAgents/kbf.cs.btm.plist <<PL
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Label</key><string>kbf.cs.btm</string><key>RunAtLoad</key><false/><key>ProgramArguments</key><array><string>/usr/bin/true</string></array></dict></plist>
PL
  chown 0:0 /Library/LaunchAgents/kbf.cs.btm.plist; chmod 644 /Library/LaunchAgents/kbf.cs.btm.plist
  echo "btm bootstrap into gui/$_cu: $(launchctl bootstrap "gui/$_cu" /Library/LaunchAgents/kbf.cs.btm.plist 2>&1 | head -2 | tr '\n' ' ')"
  sleep 3
  _b=$(sfltool dumpbtm 2>&1)
  _m=$(printf %s "$_b" | grep -c kbf.cs.btm)
  echo "btm bytes=$(printf %s "$_b" | wc -c | tr -d ' ') mentions-label=$_m -> $([ "$_m" -gt 0 ] && echo POSITIVE || echo BLIND)"
  echo "btm head: $(printf %s "$_b" | head -c 160 | tr '\n' ' ')"
  launchctl bootout "gui/$_cu/kbf.cs.btm" >/dev/null 2>&1; rm -f /Library/LaunchAgents/kbf.cs.btm.plist
  ( asu sleep 25 >/dev/null 2>&1 & ); sleep 2
  procs_of | grep -q sleep && echo "ps-as-pool-user -> POSITIVE" || echo "ps-as-pool-user -> BLIND"
  asu "$K" sysv-sem | head -1
  ipcs_has s && echo "ipcs-sysv-sem -> POSITIVE" || echo "ipcs-sysv-sem -> BLIND"
  p_ipc
  ( asu "$K" listen-tcp 15 >/dev/null 2>&1 & ); sleep 2
  lsof -nP -a -u "$ID" -i 2>/dev/null | grep -q LISTEN && echo "lsof-listener -> POSITIVE" || echo "lsof-listener -> BLIND"
  asu "$K" shm-create "/kbfposshm" | head -1
  echo "shm: root-open=$("$K" shm-open /kbfposshm 2>&1) other-uid-open=$(asu_other "$K" shm-open /kbfposshm 2>&1) ipcs-shm-lines=$(ipcs -a 2>/dev/null | grep -ci shm)"
  "$K" shm-unlink /kbfposshm >/dev/null 2>&1
  pkill -9 -U "$ID" 2>/dev/null
  # crash reports: does ReportCrash write anything for a crash at all on this host? (root-run canary)
  touch "$W/mk.crashctl"; "$K" crash-segv >/dev/null 2>&1; "$K" crash-abort >/dev/null 2>&1; sleep 45
  _c=$(find /Library/Logs/DiagnosticReports "$HOME/Library/Logs/DiagnosticReports" /var/root/Library/Logs/DiagnosticReports -newer "$W/mk.crashctl" -type f 2>/dev/null | head -3 | tr '\n' ';')
  echo "crash-report-control (root-run canary crash, 45 s) -> $([ -n "$_c" ] && echo "POSITIVE $_c" || echo "BLIND: no report appeared")"
}

cs_inventory() {
  _l=$(find /usr /bin /sbin /System/Library /Library -xdev \( -perm -4000 -o -perm -2000 \) -type f 2>/dev/null | sort)
  echo "count=$(printf '%s\n' "$_l" | grep -c .) sha=$(printf '%s\n' "$_l" | shasum | cut -c1-16) os=$(sw_vers -productVersion)"
  printf '%s\n' "$_l" | head -40
}

# ================================================================ E1 (experiment round E1; spike only)

# the uid's processes older than the clean's end (X9): OLD = started before the clean ended
cs_oldprocs() { # clean end, epoch seconds
  _e=$1
  ps -axo pid=,lstart=,ruid=,uid=,svuid=,comm= | while read -r _pid _dw _mo _dy _tm _yr _ru _u _sv _cm; do
    [ "$_ru" = "$ID" ] || [ "$_u" = "$ID" ] || [ "$_sv" = "$ID" ] || continue
    _st=$(date -j -f '%a %b %d %T %Y' "$_dw $_mo $_dy $_tm $_yr" +%s 2>/dev/null)
    if [ -n "$_st" ] && [ "$_st" -lt "$_e" ]; then echo "OLD pid=$_pid comm=$_cm start=$_st clean_end=$_e before_end_s=$((_e-_st))"
    else echo "new pid=$_pid comm=$_cm start=${_st:-?} clean_end=$_e"; fi
  done
}

# ---------------------------------------------------------------- X-L: launchd, with LimitLoadToSessionType = Background

agent_plist() { # path label command extra-keys   (SESS: session type, default Background)
  cat >"$1" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>Label</key><string>$2</string><key>LimitLoadToSessionType</key><string>${SESS:-Background}</string>$4<key>ProgramArguments</key><array><string>/bin/sh</string><string>-c</string><string>$3</string></array></dict></plist>
EOF
}
xl_boot() { # label plist: bootstrap into user/$ID as the pool user (what tests/slot_clean.rs does as the user), print what launchctl said
  _o=$(launchctl bootstrap "user/$ID" "$2" 2>&1); _rc=$?
  echo "bootstrap user/$ID rc=$_rc: $_o"
  if [ "$_rc" != 0 ]; then
    _o2=$(launchctl bootstrap "gui/$ID" "$2" 2>&1); _rc2=$?
    echo "bootstrap gui/$ID rc=$_rc2: $_o2"
    [ "$_rc2" = 0 ] && return 0
    return 1
  fi
  if launchctl print "user/$ID/$1" >/dev/null 2>&1; then echo "print-as-user: listed"; else echo "print-as-user: not-listed"; fi
  return 0
}
xl_listed() { launchctl print "user/$ID/$M" >/dev/null 2>&1; }
xl_pid() { launchctl print "user/$ID/$M" 2>/dev/null | grep -E '^[[:space:]]+pid = ' | head -1 | awk '{print $3}'; }

do_XL_ka() {
  mkdir -p "$H/Library/LaunchAgents"
  agent_plist "$H/Library/LaunchAgents/$M.plist" "$M" "touch /private/tmp/ka-$M; sleep 1000" '<key>KeepAlive</key><true/><key>RunAtLoad</key><true/><key>ThrottleInterval</key><integer>1</integer>'
  xl_boot "$M" "$H/Library/LaunchAgents/$M.plist" || return 1
  sleep 3
}
plant_XL_ka() { p XL_ka do_XL_ka; }
probe_XL_ka() {
  echo "INFO XL_ka listed=$(xl_listed && echo y || echo n) pid=$(xl_pid) escape-marker=$([ -e "/private/tmp/ka-$M" ] && echo "y owner=$(stat -f %Su "/private/tmp/ka-$M")" || echo n) sleepers=$(procs_of | grep -c sleep)"
  pr 'xl_listed || [ -e "/private/tmp/ka-$M" ] || procs_of | grep -q sleep'
}
teardown_XL_ka() { launchctl bootout "user/$ID/$M" >/dev/null 2>&1; rm -f "/private/tmp/ka-$M"; }

do_XL_sub() { launchctl submit -l "kbf.$M" -- /bin/sh -c "touch /private/tmp/esc-$M; sleep 3600" && sleep 2; }
plant_XL_sub() { p XL_sub do_XL_sub; }
probe_XL_sub() {
  echo "INFO XL_sub listed=$(launchctl print "user/$ID/kbf.$M" >/dev/null 2>&1 && echo y || echo n) escape-marker=$([ -e "/private/tmp/esc-$M" ] && echo "y owner=$(stat -f %Su "/private/tmp/esc-$M")" || echo n)"
  pr '[ -e "/private/tmp/esc-$M" ] || launchctl print "user/$ID/kbf.$M" >/dev/null 2>&1'
}
teardown_XL_sub() { launchctl bootout "user/$ID/kbf.$M" >/dev/null 2>&1; rm -f "/private/tmp/esc-$M"; }

do_XL_load() {
  mkdir -p "$H/Library/LaunchAgents"
  agent_plist "$H/Library/LaunchAgents/$M.plist" "$M" "touch /private/tmp/ld-$M; sleep 1000" '<key>RunAtLoad</key><true/>'
  launchctl load -w "$H/Library/LaunchAgents/$M.plist"; _rc=$?
  echo "launchctl load -w rc=$_rc"; sleep 3; return $_rc
}
plant_XL_load() { p XL_load do_XL_load; }
probe_XL_load() {
  echo "INFO XL_load listed=$(xl_listed && echo y || echo n) escape-marker=$([ -e "/private/tmp/ld-$M" ] && echo y || echo n)"
  pr 'xl_listed || [ -e "/private/tmp/ld-$M" ]'
}
teardown_XL_load() { launchctl bootout "user/$ID/$M" >/dev/null 2>&1; rm -f "/private/tmp/ld-$M"; }

do_XL_btm() {
  mkdir -p "$H/Library/LaunchAgents"
  agent_plist "$H/Library/LaunchAgents/$M.plist" "$M" /usr/bin/true '<key>RunAtLoad</key><false/>'
  xl_boot "$M" "$H/Library/LaunchAgents/$M.plist" || return 1
  sleep 3
}
plant_XL_btm() { p XL_btm do_XL_btm; }
probe_XL_btm() {
  _b=$(sfltool dumpbtm 2>&1)
  echo "INFO XL_btm dumpbtm-bytes=$(printf %s "$_b" | wc -c | tr -d ' ') mentions-label=$(printf %s "$_b" | grep -c "$M") listed=$(xl_listed && echo y || echo n)"
  pr 'printf %s "$_b" | grep -q "$M" || xl_listed'
}
teardown_XL_btm() { launchctl bootout "user/$ID/$M" >/dev/null 2>&1; }

# W04 again: a respawning job and a submitted one, writing in the home
do_W04x() {
  mkdir -p "$H/Library/LaunchAgents"
  agent_plist "$H/Library/LaunchAgents/$M.plist" "$M" "echo x | /usr/bin/tee -a $H/respawn-$M; sleep 2" '<key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>1</integer>'
  xl_boot "$M" "$H/Library/LaunchAgents/$M.plist" || return 1
  launchctl submit -l "kbf.w4.$M" -- /bin/sh -c "echo x | /usr/bin/tee -a $H/submit-$M >/dev/null; sleep 2"
  sleep 4
}
plant_W04x() { p W04x do_W04x; }
probe_W04x() {
  _any=0
  for _s in 0 2 3; do
    sleep "$_s"
    [ "$(wc -l <"$H/respawn-$M" 2>/dev/null | tr -d ' ')" -gt 1 ] 2>/dev/null && _any=1
    xl_listed && _any=1
  done
  echo "INFO W04x respawn-lines=$(wc -l <"$H/respawn-$M" 2>/dev/null | tr -d ' ') job-listed=$(xl_listed && echo y || echo n) submit-listed=$(launchctl print "user/$ID/kbf.w4.$M" >/dev/null 2>&1 && echo y || echo n)"
  [ "$_any" = 1 ] && echo "PROBE $ROW present" || echo "PROBE $ROW absent"
}
teardown_W04x() { launchctl bootout "user/$ID/$M" >/dev/null 2>&1; launchctl bootout "user/$ID/kbf.w4.$M" >/dev/null 2>&1; }

# W11 launchd half: a StartInterval job writing in the home
do_W11x() {
  mkdir -p "$H/Library/LaunchAgents"
  agent_plist "$H/Library/LaunchAgents/$M.plist" "$M" "/usr/bin/touch $H/si-$M" '<key>StartInterval</key><integer>5</integer>'
  xl_boot "$M" "$H/Library/LaunchAgents/$M.plist" || return 1
}
plant_W11x() { p W11x do_W11x; }
probe_W11x() {
  sleep 12
  echo "INFO W11x home: $(ls "$H" 2>/dev/null | tr '\n' ' ') listed=$(xl_listed && echo y || echo n)"
  pr 'ls "$H" 2>/dev/null | grep -q "si-" || xl_listed'
}
teardown_W11x() { launchctl bootout "user/$ID/$M" >/dev/null 2>&1; }

# respawn after kill: root kills the job's process while leased; the job must come back
do_XL_resp() { do_XL_ka; }
plant_XL_resp() { p XL_resp do_XL_resp; }
midplant_XL_resp() {
  _p1=$(xl_pid); echo "pid-before=${_p1:-none}"
  [ -n "$_p1" ] && kill -9 "$_p1"
  sleep 4
  _p2=$(xl_pid); echo "pid-after=${_p2:-none} respawned=$([ -n "$_p2" ] && [ "$_p2" != "$_p1" ] && echo y || echo n)"
}
probe_XL_resp() { probe_XL_ka; }
teardown_XL_resp() { teardown_XL_ka; }

# X8: Background Task Management by FILE, no job
do_X8() {
  mkdir -p "$H/Library/LaunchAgents" "$H/Library/LaunchDaemons" "$L/la"
  agent_plist "$H/Library/LaunchAgents/$M.agent.plist" "$M.agent" /usr/bin/true '<key>RunAtLoad</key><false/>'
  agent_plist "$H/Library/LaunchDaemons/$M.daemon.plist" "$M.daemon" /usr/bin/true ''
  agent_plist "$L/la/$M.lease.plist" "$M.lease" /usr/bin/true ''
  sleep 12
}
plant_X8() { p X8 do_X8; }
probe_X8() {
  _b=$(sfltool dumpbtm 2>&1)
  echo "INFO X8 dumpbtm-bytes=$(printf %s "$_b" | wc -c | tr -d ' ') mentions-marker=$(printf %s "$_b" | grep -c "$M") mentions-uid-home=$(printf %s "$_b" | grep -c "$H")"
  pr 'printf %s "$_b" | grep -q "$M"'
}

# positive controls for the BTM probe: (a) a job the console user's gui domain loads (the spike's B3), (b) a plist FILE only, in the console
# user's LaunchAgents and in /Library/LaunchAgents (what X8 plants in a pool home)
cs_btmctl() {
  _cu=$(stat -f %u /dev/console); _cn=$(stat -f %Su /dev/console); _ch=$(dscl . -read "/Users/$_cn" NFSHomeDirectory | awk '{print $2}')
  echo "btmctl console=$_cn uid=$_cu home=$_ch"
  _pl=/Library/LaunchAgents/kbf.cs.btm.plist
  SESS=Aqua agent_plist "$_pl" kbf.cs.btm /usr/bin/true '<key>RunAtLoad</key><false/>'
  chown 0:0 "$_pl"; chmod 644 "$_pl"
  echo "btm-job bootstrap gui/$_cu: $(launchctl bootstrap "gui/$_cu" "$_pl" 2>&1 | head -2 | tr '\n' ' ')"
  sleep 4; _b=$(sfltool dumpbtm 2>&1); _m=$(printf %s "$_b" | grep -c kbf.cs.btm)
  echo "BTMCTL job-in-gui-domain: mentions=$_m -> $([ "$_m" -gt 0 ] && echo POSITIVE || echo BLIND)"
  launchctl bootout "gui/$_cu/kbf.cs.btm" >/dev/null 2>&1; rm -f "$_pl"
  _pf="$_ch/Library/LaunchAgents/kbf.cs.btmfile.plist"
  mkdir -p "$_ch/Library/LaunchAgents"
  SESS=Aqua agent_plist "$_pf" kbf.cs.btmfile /usr/bin/true '<key>RunAtLoad</key><false/>'
  chown "$_cu" "$_pf" 2>/dev/null
  _pg=/Library/LaunchAgents/kbf.cs.btmfile2.plist
  SESS=Aqua agent_plist "$_pg" kbf.cs.btmfile2 /usr/bin/true '<key>RunAtLoad</key><false/>'
  chown 0:0 "$_pg"; chmod 644 "$_pg"
  sleep 15; _b=$(sfltool dumpbtm 2>&1)
  _m1=$(printf %s "$_b" | grep -c kbf.cs.btmfile.plist); _m2=$(printf %s "$_b" | grep -c kbf.cs.btmfile2)
  echo "BTMCTL file-in-console-user-LaunchAgents: mentions=$_m1 -> $([ "$_m1" -gt 0 ] && echo POSITIVE || echo BLIND)"
  echo "BTMCTL file-in-/Library/LaunchAgents: mentions=$_m2 -> $([ "$_m2" -gt 0 ] && echo POSITIVE || echo BLIND)"
  rm -f "$_pf" "$_pg"
}

# X1: the same doors as the console user (uid 501 has real domains and a GUI session), where open/lsopen can be seen working
plant_X1() {
  export SESS=Aqua
  p x1_open do_CH19
  p x1_submit launchctl submit -l "kbf.x1.$M" -- /bin/sh -c "touch /private/tmp/x1s-$M; sleep 30"
  p x1_gui do_x1_gui
  p x1_osa do_CH20
  SESS=Background p x1_user do_x1_user
}
do_x1_gui() {
  agent_plist "$L/g.plist" "kbf.xg.$M" "touch /private/tmp/xg-$M; sleep 30" '<key>RunAtLoad</key><true/>'
  launchctl bootstrap "gui/$ID" "$L/g.plist"
}
do_x1_user() {
  agent_plist "$L/u.plist" "kbf.xu.$M" "touch /private/tmp/xu-$M; sleep 30" '<key>RunAtLoad</key><true/>'
  launchctl bootstrap "user/$ID" "$L/u.plist"
}
probe_X1() {
  echo "INFO X1 markers: $(ls /private/tmp 2>/dev/null | grep -E "^(open|ae|x1s|xg|xu)-$M" | tr '\n' ' ')"
  pr 'ls /private/tmp 2>/dev/null | grep -q -E "^(open|ae|x1s|xg|xu)-$M"'
}
teardown_X1() {
  launchctl bootout "gui/$ID/kbf.xg.$M" >/dev/null 2>&1; launchctl bootout "user/$ID/kbf.xu.$M" >/dev/null 2>&1
  launchctl asuser "$ID" /bin/launchctl remove "kbf.x1.$M" >/dev/null 2>&1
  rm -f "/private/tmp/open-$M" "/private/tmp/ae-$M" "/private/tmp/x1s-$M" "/private/tmp/xg-$M" "/private/tmp/xu-$M"
}

# ---------------------------------------------------------------- X-B': the v2 denies, one canary each

fixture_XCAN() { printf orig >"/private/tmp/fx-$M"; chmod 666 "/private/tmp/fx-$M"; }
do_canary_boot() {
  mkdir -p "$L/la"
  agent_plist "$L/la/$M.plist" "kbf.cb.$M" /usr/bin/true '<key>RunAtLoad</key><false/>'
  launchctl bootstrap "user/$ID" "$L/la/$M.plist"
}
plant_XCAN() {
  p c_write put /private/tmp
  p c_hlink ln "/private/tmp/fx-$M" "$L/hl"
  p c_shm "$K" shm-create "/c$M"
  p c_sem "$K" sem-create "/c$M"
  p c_mach bg_ok cm "$K" mach-register "kbf.c$M" 20
  p c_job_submit launchctl submit -l "kbf.c.$M" -- /bin/sleep 20
  p c_job_boot do_canary_boot
  p c_lsopen do_CH19
  p c_mount_img do_CH07a
  p c_mount_dev do_CH07b
  p c_userpref defaults write "kbf.c.$M" k v
}
probe_XCAN() {
  echo "INFO XCAN shm-open-by-root: $("$K" shm-open "/c$M" 2>&1 | head -1) sem-open-by-root: $("$K" sem-open "/c$M" 2>&1 | head -1)"
  pr '"$K" shm-open "/c$M" 2>&1 | grep -q ok || "$K" sem-open "/c$M" 2>&1 | grep -q ok'
}
teardown_XCAN() {
  "$K" shm-unlink "/c$M" >/dev/null 2>&1; "$K" sem-unlink "/c$M" >/dev/null 2>&1
  rm -f "/private/tmp/fx-$M" "/private/tmp/$M"
  launchctl bootout "user/$ID/kbf.cb.$M" >/dev/null 2>&1; launchctl remove "kbf.c.$M" >/dev/null 2>&1
}

# ---------------------------------------------------------------- X-B': more builds, with the lease environment

do_BR6() {
  mkdir -p "$L/pk6" && cd "$L/pk6" && build_env swift package init --type executable --name pk6 >/dev/null 2>&1 || { echo "swift package init failed"; return 1; }
  BT=600 build_env swift build --disable-sandbox >"$L/sb6.log" 2>&1; _rc=$?
  echo "swift build --disable-sandbox rc=$_rc $(tail -c 300 "$L/sb6.log" | tr '\n' ' ')"
  [ "$_rc" = 0 ]
}
plant_BR6() { p BR6 do_BR6; }
probe_BR6() { probe_BR; }

write_mp() {
  cat >"$L/mp.py" <<'PY'
import sys, multiprocessing as mp
def work(q): q.put("child-ok")
def sq(x): return x * x
def main(which):
    ctx = mp.get_context("spawn")
    if which == "lock":
        l = ctx.Lock(); l.acquire(); l.release(); print("lock-ok")
    elif which == "queue":
        q = ctx.Queue(); p = ctx.Process(target=work, args=(q,)); p.start(); print(q.get(timeout=30)); p.join()
    elif which == "pool":
        with ctx.Pool(2) as pl: print(pl.map(sq, [1, 2, 3]))
    elif which == "shm":
        from multiprocessing import shared_memory
        s = shared_memory.SharedMemory(create=True, size=64); s.close(); s.unlink(); print("shm-ok")
    elif which == "executor":
        from concurrent.futures import ProcessPoolExecutor
        with ProcessPoolExecutor(2, mp_context=ctx) as ex: print(list(ex.map(sq, [1, 2, 3])))
if __name__ == "__main__": main(sys.argv[1])
PY
}
do_mp() { # which, interpreter
  BT=90 build_env "$2" "$L/mp.py" "$1" 2>&1
}
plant_BR7() {
  mkdir -p "$L/tmp"
  write_mp
  for _w in lock queue pool shm executor; do p "BR7_sys_$_w" do_mp "$_w" /usr/bin/python3; done
  if [ -x /opt/homebrew/bin/python3 ]; then for _w in lock queue pool shm executor; do p "BR7_brew_$_w" do_mp "$_w" /opt/homebrew/bin/python3; done; fi
}
probe_BR7() { probe_BR; }

mkpk8() {
  mkdir -p "$L/pk8" && cd "$L/pk8" && build_env swift package init --type library --name Pk8 >/dev/null 2>&1 || { echo "swift package init failed"; return 1; }
}
do_BR8() {
  mkpk8 || return 1
  BT=900 build_env xcodebuild -scheme Pk8 -destination 'platform=macOS' -derivedDataPath "$L/dd8" -skipPackagePluginValidation -IDEPackageSupportDisableManifestSandbox YES -IDEPackageSupportDisablePluginExecutionSandbox YES build >"$L/xb.log" 2>&1; _b=$?
  echo "xcodebuild build rc=$_b $(grep -E 'BUILD (SUCCEEDED|FAILED)|error:|Could not|fatal|denied|not permitted|failed' "$L/xb.log" | head -n 8 | cut -c1-260 | tr '\n' '|')"
  [ "$_b" = 0 ] || return 1
  BT=900 build_env xcodebuild -scheme Pk8 -destination 'platform=macOS' -derivedDataPath "$L/dd8" -skipPackagePluginValidation -IDEPackageSupportDisableManifestSandbox YES -IDEPackageSupportDisablePluginExecutionSandbox YES test >"$L/xt.log" 2>&1; _t=$?
  echo "xcodebuild test rc=$_t $(grep -E 'Executed|Test run with|TEST (SUCCEEDED|FAILED)|error:|Could not|fatal|denied|not permitted|failed' "$L/xt.log" | head -n 8 | cut -c1-260 | tr '\n' '|')"
  [ "$_t" = 0 ]
}
plant_BR8() { p BR8 do_BR8; }
probe_BR8() { probe_BR; }
do_BR8i() {
  mkpk8 || return 1
  _rt=$(xcrun simctl list runtimes available 2>/dev/null | awk '/iOS/ {print $NF; exit}')
  _dt=$(xcrun simctl list devicetypes 2>/dev/null | awk '/iPhone/ {print $NF; exit}' | tr -d '()')
  [ -n "$_rt" ] && [ -n "$_dt" ] || { echo "no iOS runtime or device type"; return 77; }
  _u=$(build_env xcrun simctl create "$M" "$_dt" "$_rt") || { echo "simctl create failed"; return 1; }
  build_env xcrun simctl boot "$_u" 2>&1 | head -2
  BT=1200 build_env xcodebuild -scheme Pk8 -destination "platform=iOS Simulator,id=$_u" -derivedDataPath "$L/ddi" -skipPackagePluginValidation -IDEPackageSupportDisableManifestSandbox YES -IDEPackageSupportDisablePluginExecutionSandbox YES test >"$L/xi.log" 2>&1; _t=$?
  echo "xcodebuild test (iOS simulator) rc=$_t $(grep -E 'Executed|Test run with|TEST (SUCCEEDED|FAILED)|error:|Could not|fatal|denied|not permitted|failed' "$L/xi.log" | head -n 8 | cut -c1-260 | tr '\n' '|')"
  [ "$_t" = 0 ]
}
plant_BR8i() { p BR8i do_BR8i; }
probe_BR8i() { probe_BR; }

# ---------------------------------------------------------------- X5: mounts the clean might not get past

X5_PORT=18081
write_dav() {
  cat >"$W/dav.py" <<'PY'
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
BODY = ('<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/</D:href><D:propstat><D:prop>'
        '<D:resourcetype><D:collection/></D:resourcetype><D:getlastmodified>Mon, 01 Jan 2029 00:00:00 GMT</D:getlastmodified>'
        '<D:creationdate>2029-01-01T00:00:00Z</D:creationdate></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>').encode()
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_OPTIONS(self):
        self.send_response(200); self.send_header("DAV", "1,2"); self.send_header("Allow", "OPTIONS,PROPFIND,GET,HEAD"); self.send_header("Content-Length", "0"); self.end_headers()
    def do_PROPFIND(self):
        self.send_response(207); self.send_header("Content-Type", "application/xml; charset=utf-8"); self.send_header("Content-Length", str(len(BODY))); self.end_headers(); self.wfile.write(BODY)
    def do_GET(self):
        self.send_response(200); self.send_header("Content-Length", "0"); self.end_headers()
    do_HEAD = do_GET
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
  chmod 644 "$W/dav.py"
}
fixture_X5a() { write_dav; echo "dav server script written"; }
do_X5a() {
  mkdir -p "$L/m" || return 1
  ( /usr/bin/python3 "$W/dav.py" "$X5_PORT" >"$L/dav.log" 2>&1 & echo $! >"$L/dav.pid" )
  sleep 2
  _o=$(alarm_run 40 mount_webdav -S "http://127.0.0.1:$X5_PORT/" "$L/m" 2>&1); _rc=$?
  echo "mount_webdav rc=$_rc: $_o"
  mount | grep -q "$L/m" || { echo "no webdav mount listed: server-log: $(head -c 200 "$L/dav.log")"; return 1; }
  echo "mounted: $(mount | grep "$L/m" | cut -c1-140)"
  kill -STOP "$(cat "$L/dav.pid")"
  ( ls "$L/m" >/dev/null 2>&1 & )
  echo "server stopped; a reader holds the mount"
  return 0
}
plant_X5a() { p X5a do_X5a; }
probe_X5a() {
  echo "INFO X5a mounts: $(mount | grep -c "$L/m") ls-5s: $(alarm_run 5 /bin/ls "$L/m" 2>&1 | head -c 80)"
  pr 'mount | grep -q "$L/m"'
}
plant_X5b() { echo "SKIPPED X5b reason=no-smb-server-on-the-runner(mount_smbfs-not-measured)"; }
probe_X5b() { echo "PROBE X5b none"; }
do_X5c() {
  hdiutil create -quiet -size 2m -fs HFS+ -volname "c$M" "$L/c.dmg" || return 1
  _o=$(hdiutil attach "$L/c.dmg" 2>&1); _rc=$?
  echo "attach (no -mountpoint) rc=$_rc: $(printf %s "$_o" | tr '\n' ' ' | cut -c1-200)"
  mount | grep "c$M" | cut -c1-140
  [ "$_rc" = 0 ] && mount | grep -q "c$M"
}
plant_X5c() { p X5c do_X5c; }
probe_X5c() {
  echo "INFO X5c mounts: $(mount | grep "c$M" | sed -E 's/\(.*//' | tr '\n' ';') /Volumes: $(ls /Volumes | tr '\n' ' ')"
  pr 'mount | grep -q "c$M"'
}
teardown_X5c() { for mp in $(mount | grep "c$M" | sed -E 's/^.* on (.*) \(.*$/\1/'); do hdiutil detach -force "$mp" >/dev/null 2>&1; done; }
do_X5d() { do_CH07a || return 1; sleep 1; }
plant_X5d() { p X5d do_X5d; }
midplant_X5d() { # SIGKILL what serves the image: processes whose command line names this lease
  for _pid in $(ps -axo pid=,ruid=,command= | awk -v id="$ID" '$2==id && /diskimages/ {print $1}'); do
    echo "kill -9 $_pid: $(ps -o ruid=,comm= -p "$_pid" | tr -s ' ' | head -1)"; kill -9 "$_pid" 2>/dev/null
  done
  sleep 2; echo "after-kill mounts: $(mount | grep -c "$H/mnt")"
}
probe_X5d() { echo "INFO X5d mounts: $(mount | grep "$H/mnt" | sed -E 's/\(.*//' | tr '\n' ';')"; pr 'mount | grep -q "$H/mnt"'; }

# ---------------------------------------------------------------- X6: cores, resource reports, crash attribution

fixture_X6() { # root-run crash, the positive control for the report probe
  sh -c 'echo "ROOT-CRASH-PID $$"; exec "$K" crash-segv' 2>&1 | head -2
  printf x >"/cores/core.ctl-$M" 2>/dev/null && chown "$ID" "/cores/core.ctl-$M" && echo "core-control planted in /cores owned by the uid"
  echo "coredump=$(sysctl -n kern.coredump 2>&1) corefile=$(sysctl -n kern.corefile 2>&1) cores-dir=$(ls -ld /cores 2>&1)"
}
do_X6() {
  ulimit -c unlimited 2>&1; echo "ulimit-c=$(ulimit -c)"
  sh -c 'echo "USER-CRASH-PID $$"; exec "$K" crash-segv' 2>&1 | head -2
  sh -c 'echo "USER-ABORT-PID $$"; exec "$K" crash-abort' 2>&1 | head -2
  ( /usr/bin/perl -e '$e=time+215; while(time<$e){ $x++ }' >/dev/null 2>&1 & )
  dd if=/dev/zero of="$L/big" bs=1m count=3072 2>&1 | tail -1
  sleep 225
  return 0
}
plant_X6() { p X6 do_X6; }
x6_reports() {
  for f in $(find /Library/Logs/DiagnosticReports /cores /private/var/root/Library/Logs/DiagnosticReports /Users/*/Library/Logs/DiagnosticReports "$H/Library/Logs/DiagnosticReports" -newer "$MK" -type f 2>/dev/null | head -30); do
    echo "INFO X6 REPORT $f owner=$(stat -f %Su:%Sg "$f") mode=$(stat -f %Lp "$f") $(head -c 6000 "$f" 2>/dev/null | grep -o -E '"(userID|uid|procName|pid|name|app_name)" *: *("[^"]*"|[0-9]+)' | head -6 | tr '\n' ' ')"
  done
}
probe_X6() {
  x6_reports
  echo "INFO X6 cores-owned-by-uid: $(find /cores -user "$ID" 2>/dev/null | head -3 | tr '\n' ' ')"
  pr 'find /cores /Library/Logs/DiagnosticReports -newer "$MK" -type f \( -user "$ID" -o -name "canary*" -o -name "*.diag" \) 2>/dev/null | grep -q .'
}

# ---------------------------------------------------------------- X-TCC

cs_tcc_vol() { # an APFS volume in an image at /Volumes/kbfpool, ownership honoured: the farm's home is on /Volumes/<volume>/home
  rm -f "$W/pool.dmg"
  hdiutil create -quiet -size 600m -fs APFS -volname kbfpool "$W/pool.dmg" 2>&1 | head -2
  hdiutil attach -quiet -owners on -mountpoint /Volumes/kbfpool "$W/pool.dmg" 2>&1 | head -2
  echo "TCCVOL mount: $(mount | grep kbfpool | cut -c1-160)"
  echo "TCCVOL ls: $(ls -ld /Volumes/kbfpool 2>&1)"
  mkdir -p /Volumes/kbfpool/home && chown 0:0 /Volumes/kbfpool/home && chmod 755 /Volumes/kbfpool/home
  echo "TCCVOL fs: $(diskutil info /Volumes/kbfpool 2>&1 | grep -E 'File System|Ownership|Owners' | tr -s ' ' | tr '\n' ';')"
}
cs_tcc_vol_end() { hdiutil detach -force /Volumes/kbfpool 2>&1 | head -1; rm -f "$W/pool.dmg"; }

cs_daemon_sh() { # script file, interpreter: run it in a system LaunchDaemon (no grants of the runner step); output in $W/dsh.out
  _sh=${2:-/bin/bash}
  _pl=/Library/LaunchDaemons/kbf.cs.dsh.plist
  rm -f "$W/dsh.done" "$W/dsh.out"
  cat >"$_pl" <<PL
<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>Label</key><string>kbf.cs.dsh</string>
<key>ProgramArguments</key><array><string>$_sh</string><string>-c</string><string>$_sh $1; touch $W/dsh.done</string></array>
<key>StandardOutPath</key><string>$W/dsh.out</string><key>StandardErrorPath</key><string>$W/dsh.out</string>
<key>RunAtLoad</key><true/></dict></plist>
PL
  chown 0:0 "$_pl"; chmod 644 "$_pl"
  launchctl bootstrap system "$_pl" 2>&1 | head -2
  for _i in $(seq 1 120); do [ -e "$W/dsh.done" ] && break; sleep 1; done
  cat "$W/dsh.out" 2>/dev/null
  [ -e "$W/dsh.done" ] || echo "TCCDAEMON timeout"
  launchctl bootout system/kbf.cs.dsh >/dev/null 2>&1; rm -f "$_pl"
}
cs_tcc_script() {
  _cn=$(stat -f %Su /dev/console); _ch=$(dscl . -read "/Users/$_cn" NFSHomeDirectory 2>/dev/null | awk '{print $2}')
  { printf 'H=%q\nRH=%q\nERRF=%q\n' "$H" "${_ch:-/Users/runner}" "$W/tcc-rm.err"; cat <<'SH'
echo "TCCCTX uid=$(id -u) console-home=$RH pool-home=$H"
sqlite3 -readonly '/Library/Application Support/com.apple.TCC/TCC.db' "select service||':'||client||':'||auth_value from access where service in ('kTCCServiceSystemPolicyAllFiles','kTCCServiceSystemPolicyDesktopFolder','kTCCServiceSystemPolicyDocumentsFolder')" 2>&1 | head -30 | sed 's/^/TCCDB-sys /' 
echo "TCCDB-user: $(sqlite3 -readonly "$RH/Library/Application Support/com.apple.TCC/TCC.db" "select service||':'||client||':'||auth_value from access" 2>&1 | head -8 | tr '\n' ' ')"
echo "TCCSIP: $(csrutil status 2>&1 | head -1)"
while IFS= read -r d; do
  for pair in "ctl:$RH" "pool:$H"; do
    tag=${pair%%:*}; base=${pair#*:}
    if [ ! -e "$base/$d" ]; then echo "TCCPROBE $tag $d absent"; continue; fi
    o=$(ls -A "$base/$d" 2>&1 >/dev/null | head -c 70)
    r=""
    if [ "$tag" = pool ]; then rm -f "${base:?}/${d:?}/f" 2>"$ERRF"; r=" rm-rc=$? $(head -c 70 "$ERRF" 2>/dev/null)"; fi
    echo "TCCPROBE $tag $d ls-err=[$o]$r"
  done
done <<'EOF'
Desktop
Documents
Downloads
Library/Safari
Library/Mail
Library/Messages
Library/Calendars
Library/Cookies
Library/Application Support/AddressBook
Library/Group Containers/group.com.apple.notes
Library/Containers/com.apple.Safari/Data
EOF
SH
  } >"$W/tcc.sh"
  chmod 644 "$W/tcc.sh"
}
midplant_TCC() {
  cs_tcc_script
  echo "TCC step-context:"; bash "$W/tcc.sh" 2>&1 | head -40
  echo "TCC daemon-context(bash):"; cs_daemon_sh "$W/tcc.sh" /bin/bash | head -40
  echo "TCC daemon-context(zsh):"; cs_daemon_sh "$W/tcc.sh" /bin/zsh | head -40
}
plant_TCC() { p TCC do_W07; }
probe_TCC() { probe_W07; }

# ---------------------------------------------------------------- X7, X9 controls and X-base

fixture_LATEC() { # root-planted late arrivals, at +25 s and +70 s from now: the late looks must see them
  ( sleep 25; printf x >"$H/late25-$M"; chown "$ID:$GID" "$H/late25-$M" ) >/dev/null 2>&1 &
  ( sleep 70; printf x >"$H/late70-$M"; chown "$ID:$GID" "$H/late70-$M" ) >/dev/null 2>&1 &
}
plant_LATEC() { p LATEC true; }
probe_LATEC() { pr 'ls "$H" 2>/dev/null | grep -q late'; }
do_LATEK() { nohup sleep 3600 >/dev/null 2>&1 & sleep 1; }
plant_LATEK() { p LATEK do_LATEK; }
probe_LATEK() { pr 'procs_of | grep -q sleep'; }
do_B1k() { nohup sleep 3600 >/dev/null 2>&1 & sleep 1; }
plant_B1k() { p B1k do_B1k; }
probe_B1k() { echo "PROBE B1k none"; }
teardown_X6() { rm -f "/cores/core.ctl-$M"; find /cores -user "$ID" -delete 2>/dev/null; }
