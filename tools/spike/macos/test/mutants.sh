#!/usr/bin/env bash
# Plants each defect below in a copy of the macOS probes and runs the tests that must
# catch it; every mutant must turn them red. A mutant whose edit no longer applies
# fails too, so this list cannot silently go stale.
#
#   bash tools/spike/macos/test/mutants.sh

set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ORIG=$(cd "$HERE/.." && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/kbf-spike-macos-mutants.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
FAILED=0
COUNT=0

# mutant NAME FILE TESTS SED: the copy of FILE edited by SED must fail TESTS.
mutant() {
    COUNT=$((COUNT + 1))
    local m=$WORK/m$COUNT
    mkdir -p "$m"
    cp "$ORIG"/*.sh "$m/"
    sed -e "$4" "$ORIG/$2" >"$m/$2"
    if cmp -s "$ORIG/$2" "$m/$2"; then
        printf 'STALE  %s: the edit no longer applies\n' "$1"
        FAILED=$((FAILED + 1))
        return 0
    fi
    if KBF_TESTS=$3 bash "$HERE/run.sh" "$m" >"$m/log" 2>&1; then
        printf 'LIVED  %s (%s)\n' "$1" "$3"
        FAILED=$((FAILED + 1))
    else
        printf 'killed %s (%s)\n' "$1" "$3"
    fi
}

mutant 'an arm runs in the caller, so a failing arm stops the probe' lib.sh t_lib \
    '/^arm() {/,/^}/{s/^    (/    {/;s/^    )$/    }/;}'
mutant 'the arm subshell is the left side of ||, where set -e is ignored' lib.sh t_lib \
    '/^arm() {/,/^}/{s/^    )$/    ) || true/;}'
mutant 'the key lacks the macos- prefix (collides with the Linux arm64 keys)' lib.sh t_lib \
    's/^SPIKE_ARCH=macos-/SPIKE_ARCH=/'
mutant 'the watchdog reports a hang as success' lib.sh t_watchdog \
    's/        return 124/        return 0/'
mutant 'the watchdog leaves CMD on the caller pipe, so an orphaned child holds it open' lib.sh t_am_adduser_hang \
    's/    "$@" <\/dev\/null >"$out" 2>&1 &/    "$@" <\/dev\/null \&/'
mutant 'the watchdog leaves stdin open, so a prompting CMD reads the caller input' lib.sh t_watchdog \
    's/    "$@" <\/dev\/null >"$out" 2>&1 &/    "$@" >"$out" 2>\&1 \&/'
mutant 'the auth parser loses its "does not require" case' \
    automation_mode.sh t_am_already \
    's/\*"does not require user authentication"\*) a=not_required/*"requires user authentication"*) a=required/'
mutant 'the auth parser accepts any "require" as not_required' automation_mode.sh t_am_required_first \
    's/\*"requires user authentication"\*) a=required/*"require"*) a=not_required/'
mutant 'enable runs whatever the last status said' automation_mode.sh t_am_control_noop \
    's/if \[ "$(st_get last)" != required \]; then/if false; then/'
mutant 'enable runs when the status was not recognised' automation_mode.sh t_am_unknown \
    's/if \[ "$(st_get last)" != required \]; then/if [ "$(st_get last)" = not_required ]; then/'
mutant 'the verdict ignores the control arm' automation_mode.sh t_am_control_noop \
    's/elif \[ "$control" != required \]; then/elif false; then/'
mutant 'the verdict treats a hung enable as success' automation_mode.sh t_am_hang \
    's/    if \[ "$rc" -eq 124 \]; then/    if false; then/'
mutant 'the verdict ignores the setting after the user is deleted' automation_mode.sh t_am_user_drops \
    's/ || \[ "$after_delete" != not_required \]//'
mutant 'sysadminctl runs without the watchdog' automation_mode.sh t_am_adduser_hang \
    's/add=$(run_cap watchdog "$WATCHDOG" sudo -n sysadminctl/add=$(run_cap sudo -n sysadminctl/'
mutant 'the simctl verdict ignores the deny control' simctl.sh t_simctl_deny_ignored \
    's/elif \[ "$(st_get deny_boot)" = 0 \]; then/elif false; then/'
mutant 'the screenshot is not checked to be a PNG' simctl.sh t_simctl_bad_png \
    's/    if is_png "$png"; then/    if true; then/'
mutant 'the simctl verdict ignores the (allow default) arm' simctl.sh t_simctl_allow_fails \
    's/elif \[ "$(st_get allow_boot)" != 0 \]; then/elif false; then/'
mutant 'the simctl verdict ignores bootstatus' simctl.sh t_simctl_bootstatus_fails \
    's/elif \[ "$(st_get booted)" != 0 \]; then/elif false; then/'
mutant 'the simctl verdict ignores a failed delete' simctl.sh t_simctl_delete_fails \
    's/elif \[ "$(st_get deleted)" != 0 \]; then/elif false; then/'
mutant 'create takes any non-empty output as a UDID' simctl.sh t_simctl_create_garbage \
    's/\*\[!0-9A-F-\]\* | "")/"")/'
mutant 'the simctl probe boots in the default device set' simctl.sh t_simctl_pass \
    's/^sim() { xcrun simctl --set "$SET" "$@"; }/sim() { xcrun simctl "$@"; }/'
mutant 'the TCC verdict ignores the control row' tcc.sh t_tcc_control_empty \
    's/^if \[ "$(st_get control)" = ok \]; then/if true; then/'
mutant 'the TCC reader never reads as root' tcc.sh t_tcc_pass \
    's/^a_system_root() { tcc_read system.as_root "$SYSTEM_DB" sudo -n || true; }/a_system_root() { tcc_read system.as_root "$SYSTEM_DB" || true; }/'
mutant 'the P0 verdict ignores the root control' volume_owner.sh t_vo_control_lost \
    's/elif \[ "${control%%:\*}" != 0 \]; then/elif false; then/'
mutant 'the P0 owner is read from the first attach, not a re-attach' volume_owner.sh t_vo_chown_lies \
    's/owner=$(st_get reattach)/owner=$(st_get write.first)/'
mutant 'the P0 writes run as the runner (an admin), not the fresh standard user' volume_owner.sh t_vo_fail \
    's/^as_user() { sudo -n -H -u "$PROBE_USER" "$@"; }/as_user() { "$@"; }/'
mutant 'the P0 verdict ignores an admin writer' volume_owner.sh t_vo_admin_writer \
    's/if \[ "$writer" = admin \] || \[ "$writer" = root \]; then/if false; then/'
mutant 'the P0 probe skips the check for a user that existed before' volume_owner.sh t_vo_user_existed \
    's/^if id "$PROBE_USER" >\/dev\/null 2>&1; then$/if false; then/'
mutant "the P0 cleanup ignores the creation marker (takes the name's current uid)" volume_owner.sh t_vo_user_existed \
    's/^    created=$(st_get created_uid)$/    created=$(id -u "$PROBE_USER" 2>\/dev\/null || true)/'
mutant 'the P0 cleanup deletes by name without comparing the recorded uid' volume_owner.sh t_vo_user_recreated \
    's/^    elif \[ "$now" != "$created" \]; then$/    elif false; then/'
mutant 'the P0 probe keeps a creation marker from an earlier run' volume_owner.sh t_vo_stale_marker \
    '/^st_set created_uid ""$/d'
mutant 'the P0 cleanup skips the force-detach of a mount left behind' volume_owner.sh t_vo_detach_busy \
    's/^        \[ -n "$m" \] || continue$/        continue/'
mutant 'the P0 cleanup detaches without -force' volume_owner.sh t_vo_detach_busy \
    's/as_root hdiutil detach -force "$m"/as_root hdiutil detach "$m"/'
mutant 'the P0 write records a failed detach as done' volume_owner.sh t_vo_detach_busy \
    's/^    detach "$label" "$run" "$mnt" || return 0$/    detach "$label" "$run" "$mnt" || true/'
mutant 'the P0 verdict compares the owner on its gid' volume_owner.sh t_vo_group_root \
    's/elif \[ "${owner%%:\*}" = 0 \]; then/elif [ "${owner##*:}" = 0 ]; then/'
mutant 'the P0 verdict ignores where the write stopped' volume_owner.sh t_vo_attach_fails \
    's/elif \[ "$reached" != detach \]; then/elif false; then/'
mutant 'the P0 verdict ignores noowners on a re-attach (the false PASS)' volume_owner.sh 't_vo_noowners t_vo_control_noowners' \
    '/^        \*", noowners,"\*) return 1 ;;$/d'
mutant 'the P0 noowners check reads the whole mount line, path included' volume_owner.sh t_vo_noowners \
    's/^    case ", $flags," in$/    case "$line" in/;s/^        \*", noowners,"\*) return 1 ;;$/        *noowners*) return 1 ;;/'
mutant 'the P0 verdict takes a re-attach missing from the mount table as owners on' volume_owner.sh t_vo_mount_unlisted \
    's/^        "" | "not in the mount table") return 1 ;;$/        "") return 1 ;;/'
mutant "the P0 verdict ignores the user re-attach's mount flags" volume_owner.sh 't_vo_noowners t_vo_mount_unlisted' \
    's/^    elif ! owners_on reattach; then$/    elif false; then/'
mutant "the P0 verdict ignores the control re-attach's mount flags" volume_owner.sh t_vo_control_noowners \
    's/^    elif ! owners_on control_reattach; then$/    elif false; then/'
mutant 'the P0 verdict ignores the keep file left at the user uid' volume_owner.sh t_vo_shows_caller \
    's/^    elif \[ "${keep%%:\*}" != "$uid" \]; then$/    elif false; then/'
mutant 'the P0 keep owner is read from the chowned file' volume_owner.sh t_vo_pass \
    's/keep=$(as_root stat -f %u:%g "$mnt\/$KEEP" 2>&1)/keep=$(as_root stat -f %u:%g "$mnt\/$FILE" 2>\&1)/'
mutant 'the P0 cleanup leaves the user' volume_owner.sh t_vo_fail \
    's/kv cleanup.delete_user "$(run_cap watchdog "$WATCHDOG" as_root sysadminctl -deleteUser "$PROBE_USER")"/:/'
mutant 'verdicts.sh lets a BROKEN verdict through' verdicts.sh t_verdicts \
    's/    case "$v" in BROKEN\*) bad=1 ;; esac/    :/'
mutant 'verdicts.sh lets a missing verdict through' verdicts.sh t_verdicts \
    '/v="MISSING: the probe printed no verdict"/{n;s/bad=1/:/;}'

printf '%s mutants, %s not killed\n' "$COUNT" "$FAILED"
[ "$FAILED" -eq 0 ]
