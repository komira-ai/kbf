#!/usr/bin/env bash
# VM probe P0 (docs/design/macos-vm-guests.md, sections 2.2 and 11): can a host user
# without root write a root-owned file into an attached disk image, the way the image
# builder would write the first-boot launch daemon plist into a guest's volume?
#
#   bash tools/spike/macos/volume_owner.sh
#
# Needs passwordless sudo (a hosted runner has it) to make the throwaway user, to run
# the root control and to read the owner back. The arms, in order:
#
#   new_user   adds a fresh standard user ($SPIKE_P0_USER, default kbfp0) with
#              `sysadminctl -addUser` (no -admin); records whether that name existed
#              before (a user that existed is not fresh, so the probe is BROKEN);
#   write      as that user (`sudo -n -H -u`): `hdiutil create` a scratch APFS image,
#              attach it with `-owners on`, copy a plist in, `chown 0:0` it (status and
#              text recorded), read the owner this first attach shows, detach. The
#              writer's own uid and groups are read through the same `sudo -u` the
#              writes use, so a write that ran as the runner (an admin) is caught;
#   reattach   as root: attach the user's image again, read-only with `-owners on`,
#              and read the plist's owner: this, not the first attach's view, is the
#              owner a guest booting the volume would see;
#   control    the same create, attach, copy, chown and detach as root, then the same
#              re-attach read; it must read back uid 0, or the reader (or the mount)
#              cannot show a root owner and the probe is BROKEN;
#   cleanup    force-detaches whatever is still attached, deletes the user and the
#              scratch directory.
#
# PASS: the re-attach reads the standard user's file as uid 0. FAIL: it does not, or
# the user could not create, attach or write (each step's status is recorded). Every
# hdiutil and sysadminctl call runs under the watchdog (SPIKE_WATCHDOG seconds,
# default 60) with stdin closed. The result is mechanics on a hosted runner only; a farm
# Mac still decides (docs/spikes/macos-p0.md).

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=tools/spike/macos/lib.sh
. "$HERE/lib.sh"
PROBE=volume_owner
WATCHDOG=${SPIKE_WATCHDOG:-60}
PROBE_USER=${SPIKE_P0_USER:-kbfp0}
# A directory the new user can reach: the runner's own temp directory sits under its
# home, which other users cannot traverse.
P0_DIR=$(mktemp -d "${SPIKE_P0_PARENT:-/Users/Shared}/kbf-p0.XXXXXX")
chmod 0777 "$P0_DIR"
cd "$P0_DIR" || exit 1
SRC=$P0_DIR/src.plist
FILE=kbf-p0.plist

as_user() { sudo -n -H -u "$PROBE_USER" "$@"; }
as_root() { sudo -n "$@"; }

# rc_of TEXT: the status run_cap appended to TEXT (empty when there is none).
rc_of() { printf '%s' "$1" | sed -n 's/.*\[rc=\([0-9]*\)\]$/\1/p'; }

# step LABEL KEY CMD...: runs CMD under the watchdog, records LABEL.KEY and returns
# CMD's status.
step() {
    local label=$1 key=$2 out
    shift 2
    out=$(run_cap watchdog "$WATCHDOG" "$@")
    kv "$label.$key" "$out"
    [ "$(rc_of "$out")" = 0 ]
}

# attach LABEL RUN MNT IMG [-readonly]: attaches IMG at MNT as RUN (as_user, as_root).
attach() {
    local label=$1 run=$2 mnt=$3 img=$4
    shift 4
    "$run" mkdir -p "$mnt"
    step "$label" attach "$run" hdiutil attach -nobrowse -owners on "$@" -mountpoint "$mnt" "$img" || return 1
    st_set "mounted.${mnt##*/}" "$mnt"
}

# detach LABEL RUN MNT
detach() {
    step "$1" detach "$2" hdiutil detach "$3" || return 1
    st_set "mounted.${3##*/}" ""
}

# write LABEL RUN IMG MNT: create IMG, attach it, copy the plist in, chown it to root,
# read the owner this attach shows, detach. Records LABEL.done=<the last step reached>.
write() {
    local label=$1 run=$2 img=$3 mnt=$4 chown_out first
    st_set "$label.done" none
    step "$label" create "$run" hdiutil create -size 64m -fs APFS -volname KBFP0 "$img" || return 0
    st_set "$label.done" create
    attach "$label" "$run" "$mnt" "$img" || return 0
    st_set "$label.done" attach
    step "$label" copy "$run" cp "$SRC" "$mnt/$FILE" || return 0
    st_set "$label.done" copy
    chown_out=$(run_cap "$run" chown 0:0 "$mnt/$FILE")
    kv "$label.chown" "$chown_out"
    st_set "$label.chown" "$chown_out"
    first=$("$run" stat -f %u:%g "$mnt/$FILE" 2>&1) || first="unread: $first"
    kv "$label.first_owner" "$first"
    st_set "$label.first" "$first"
    detach "$label" "$run" "$mnt" || return 0
    st_set "$label.done" detach
}

# reread LABEL IMG MNT: as root, attach IMG read-only with owners on and record the
# plist's owner as LABEL.owner (state: LABEL).
reread() {
    local label=$1 img=$2 mnt=$3 owner
    st_set "$label" ""
    attach "$label" as_root "$mnt" "$img" -readonly || return 0
    owner=$(as_root stat -f %u:%g "$mnt/$FILE" 2>&1) || owner="unread: $owner"
    kv "$label.owner" "$owner"
    st_set "$label" "$owner"
    detach "$label" as_root "$mnt" || true
}

a_new_user() {
    local pw
    if id "$PROBE_USER" >/dev/null 2>&1; then st_set existed yes; else st_set existed no; fi
    kv new_user.existed_before "$(st_get existed)"
    # A throwaway password for a throwaway user that is deleted in cleanup.
    pw=$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')
    step new_user add as_root sysadminctl -addUser "$PROBE_USER" -password "$pw" || true
    if id "$PROBE_USER" >/dev/null 2>&1; then kv new_user.exists yes; else kv new_user.exists no; fi
}

a_write() {
    local uid groups kind
    printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>' \
        '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
        '<plist version="1.0"><dict><key>Label</key><string>kbf.p0</string></dict></plist>' >"$SRC"
    chmod 0644 "$SRC"
    # The writer, through the same wrapper as the writes.
    uid=$(as_user id -u 2>/dev/null || true)
    groups=$(as_user id -Gn 2>/dev/null || true)
    if [ -z "$uid" ]; then
        kind=none
    elif [ "$uid" = 0 ]; then
        kind=root
    else
        case " $groups " in *" admin "*) kind=admin ;; *) kind=standard ;; esac
    fi
    kv write.writer "uid=${uid:-none} kind=$kind groups=$groups"
    st_set writer "$kind"
    write write as_user "$P0_DIR/user.dmg" "$P0_DIR/m-user-1"
}

a_reattach() { reread reattach "$P0_DIR/user.dmg" "$P0_DIR/m-user-2"; }

a_control() {
    write control as_root "$P0_DIR/control.dmg" "$P0_DIR/m-control-1"
    reread control_reattach "$P0_DIR/control.dmg" "$P0_DIR/m-control-2"
}

a_cleanup() {
    local f m
    for f in "$SPIKE_STATE/$PROBE".mounted.*; do
        [ -e "$f" ] || continue
        m=$(cat "$f")
        [ -n "$m" ] || continue
        kv "cleanup.detach.${m##*/}" "$(run_cap watchdog "$WATCHDOG" as_root hdiutil detach -force "$m")"
    done
    kv cleanup.delete_user "$(run_cap watchdog "$WATCHDOG" as_root sysadminctl -deleteUser "$PROBE_USER")"
    if id "$PROBE_USER" >/dev/null 2>&1; then kv cleanup.user_gone no; else kv cleanup.user_gone yes; fi
    cd / && as_root rm -rf "$P0_DIR"
    if [ -e "$P0_DIR" ]; then kv cleanup.dir_gone no; else kv cleanup.dir_gone yes; fi
}

decide() {
    local writer owner control reached
    writer=$(st_get writer)
    owner=$(st_get reattach)
    control=$(st_get control_reattach)
    reached=$(st_get write.done)
    if [ "$(st_get existed)" = yes ]; then
        verdict BROKEN "user $PROBE_USER existed before the probe, so the writer is not a fresh user"
    elif [ "$writer" = admin ] || [ "$writer" = root ]; then
        verdict BROKEN "the writes ran as $writer, not as a fresh standard user"
    elif [ "${control%%:*}" != 0 ]; then
        verdict BROKEN "the root control read back as '${control:-unread}', not uid 0, so the reader cannot show a root owner"
    elif [ "$writer" != standard ]; then
        verdict FAIL "nothing ran as the new user (writer: $writer)"
    elif [ "$reached" != detach ]; then
        verdict FAIL "the standard user's write stopped after '$reached' (see the write.* lines)"
    elif [ -z "$owner" ]; then
        verdict FAIL "the re-attach of the user's image did not read the file"
    elif [ "${owner%%:*}" = 0 ]; then
        verdict PASS "a fresh standard user wrote a file a re-attach reads as $owner; the root control read back as $control"
    else
        verdict FAIL "a re-attach reads the standard user's file as $owner, not root (chown: $(st_get write.chown)); the root control read back as $control"
    fi
}

spike_context
arm new_user a_new_user
arm write a_write
arm reattach a_reattach
arm control a_control
arm cleanup a_cleanup
decide
