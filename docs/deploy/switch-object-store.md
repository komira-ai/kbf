# Switching the farm's object store

The farm's object store is what the frontend registers (`kbf cluster object-store`, the
`/v1/object-stores/...` routes of [the operator API](../api.md)): a connection (an endpoint and a
key pair) and the `cold` binding of that connection to a bucket and a prefix. A storage node
started with `--cold-store registered` names no store of its own: it takes the registered one
at its start ([storage-node.md](storage-node.md)).

Switching the store is one command on the frontend's host, then a restart of the storage members
one at a time:

- **The command** moves the registration to the new endpoint and raises its generation. It
  changes nothing on a node and copies no object.
- **The rolling restart** is when each node takes the new store. A running node keeps the store
  it opened; it reports `pending` at the new generation until it is restarted.

What does not change: the key pair (the connection keeps the key it has, and the frontend probes
the new endpoint with it before it writes anything), the bucket and the prefix unless the command
names new ones, and every node that names a store URL in its `--cold-store`: such a node never
follows a repoint (it keeps its `identity_mismatch` behaviour). Move each node to `registered`
first ([below](#moving-a-node-to---cold-store-registered)).

## Before the first switch

- **The new endpoint already holds the farm's objects**, under the same bucket and prefix, and
  goes on holding everything written through the old one until the last member has restarted.
  The repoint probes one new object of its own; it neither copies nor checks the objects the
  members' indexes name. A switch to an empty store is accepted and converges, and then every
  read of an older blob fails. Two endpoints in front of one store (a write-through tier and the
  store behind it, say) meet this; two separate stores do not, unless the objects are copied and
  writes reach both for the whole walk.
- **A new bucket** needs the objects copied into it first, as above.
- **A new prefix** is refused by every node that already has a metadata log: the log keeps its
  objects under the prefix it was made with, and a node on `registered` whose log was made under
  another prefix does not start (`the registered cold store's prefix is ..., but this node's
  metadata log was made under ...`). A prefix change is a migration of each node's data (an empty
  `--meta-dir` and the objects copied), not a switch; this page does not cover it.
- The connection is registered and `cold` is bound to it (`kbf cluster object-store status`
  lists both).
- The connection's key is valid at the new endpoint: the repoint probes the new store with it
  and changes nothing when the probe fails.
- Every storage member was started with `--cold-store registered` and `--frontend`. The held
  script below reads and rewrites hand-written units under `/etc/systemd/system` only.
- **One operator at a time.** `safe-to-stop` answers for the moment it is asked; it reserves
  nothing. For the whole walk, nobody else restarts, drains or updates a storage member, and
  nobody else writes the registration (`connection put`, `bind`, `repoint`). Agree it with the
  others who run the farm, and pause the farm's updater for the walk. A registration written
  during the walk raises the generation again: the members already restarted are then `pending`
  at the newer one, and the held script stops (`the registry moved`). Finish by restarting every
  member onto the new generation (the script without the repoint flags).

## The command

On the frontend's host, read the registry's generation, then repoint at it:

```text
kbf cluster object-store status --token-file /etc/kbf/api-token
kbf cluster object-store repoint --connection <connection> --endpoint <new endpoint URL> --expect-generation <generation> --token-file /etc/kbf/api-token
```

- `<generation>` is the `generation` that `status` printed. A write in between makes the
  repoint answer `409 generation_mismatch` and change nothing: read `status` again.
- `<new endpoint URL>` is `http://host[:port]`. `--bucket` moves the bucket too (see above);
  without it the binding's stays. Leave `--prefix` out (see above).
- The frontend probes the new store with the registered key first; a failed probe answers
  `422 probe_failed` and changes nothing. The answer is `{name, generation, changed, from, to,
  verified}`. Repointing to the current identity is `changed: false` and keeps the generation.
  The audit log has a row with the old and the new identity:

  ```text
  kbf cluster object-store audit --limit 5 --token-file /etc/kbf/api-token
  ```

Write down the `generation` of the answer: it is the one every node must apply.

## The rolling restart

Restart the storage members **one at a time**. Blobs are kept on two owners, so two members down
at once leave some blobs with no reachable copy (see [updating a host](update-a-host.md#restarting-a-storage-member)).
Each member has three names, and the walk uses each where it belongs:

| name | where it comes from | used by |
|---|---|---|
| member id `<member id>` | `id` in `GET /v1/members` | `safe-to-stop`, `member` |
| member address | `addr` in `GET /v1/members`: the host's `--advertise-addr`, or the address it binds | the member's row in `status`'s `convergence.nodes` (its `id` is this address) |
| node id `<node>` | the host's `--node-id`, on a host whose unit also runs a daemon (`storage-and-daemon`) | `drain`, `undrain`, `GET /v1/nodes` |

A storage host with no daemon has no node id: skip the drain and the undrain for it.

For each member:

1. **Safe to stop.** Exit 0 means it may stop now; exit 3 prints why not. Wait and ask again;
   never go on without a 0.

   ```text
   kbf cluster safe-to-stop <member id> --token-file /etc/kbf/api-token
   ```

2. **Drain** the daemon node, and wait until `kbf cluster status` shows its placement `drained`.
   The drain has no deadline: it waits for its leases however long they run.

   ```text
   kbf cluster drain <node> --token-file /etc/kbf/api-token
   kbf cluster status
   ```

3. **Plan the restart** of its storage member, so writes go to the other owner at once. The plan
   lasts `--for-secs` (900 here, at most 86400) or until the member registers again:

   ```text
   kbf cluster member <member id> --state planned-down --for-secs 900 --token-file /etc/kbf/api-token
   ```

4. **Restart** its unit on its host: `sudo systemctl restart <unit>`.
5. **Wait until it is back with the new generation applied**: its member is `active` again in
   `GET /v1/members` (`catching_up` while its index rebuilds), and the row of its member address in
   `status`'s `convergence.nodes` has `applied_generation` equal to the generation the repoint
   answered, with `status` `ok`. A row that stays `pending` at the old generation is a node that
   did not restart; `identity_mismatch` is a node that names a store URL; `probe_failed` is a node
   that cannot reach the new store with the key.

   ```text
   kbf cluster object-store status --token-file /etc/kbf/api-token
   ```

6. **Undrain** the node, then go to the next member:

   ```text
   kbf cluster undrain <node> --token-file /etc/kbf/api-token
   ```

When the last member is done, `status` says `converged: true`.

## Moving a node to `--cold-store registered`

A node whose hand-written unit says `--cold-store s3://...` stays on that store. Move it once,
with the same walk as above: between steps 3 and 4,

- in the unit's `ExecStart=` (`systemctl show -P FragmentPath <unit>` names the file), replace
  `--cold-store <uri>` with `--cold-store registered`, and remove `--cold-store-credentials-file`
  and its value (`registered` refuses it: the keys come from the frontend). The unit must also
  have `--frontend`: `registered` without it does not start;
- look at **every** drop-in that applies to the unit, loaded or not: for each of its names
  (`systemctl show -P Names <unit>`, aliases included), `<name>.d/`, the template's
  `<template>@.service.d/` for an instance, the prefix forms (`kbf-.service.d/`, ...; for an
  instance also `kbf-@<instance>.service.d/` and `kbf-@.service.d/`) and `service.d/`, under `/etc/systemd/system`, `/run/systemd/system`,
  `/usr/local/lib/systemd/system`, `/usr/lib/systemd/system` and `/lib/systemd/system`, a
  directory reached through a symbolic link included; and every file `systemctl show -P
  DropInPaths <unit>` lists as loaded. A drop-in that names a store
  (`--cold-store`, an `s3://` URI, an `endpoint=`, `AWS_ENDPOINT_URL`) or sets `ExecStart=` is
  applied after the unit and wins, so move that line out of it;
- `sudo systemctl daemon-reload`, and check the flag once on what systemd will run, not on the
  file: `systemctl show -P ExecStart <unit>` holds `--cold-store registered` and no
  `--cold-store-credentials-file`.

Then restart (step 4). The node fetches the registration at its start, and with no registration
it waits for one; it never falls back to a store of its own. `/etc/kbf/s3.env` and any other key
file on the node are no longer read by `kbf` once it is registered.

## When a walk stops

The held script prints what a stop leaves behind. By that state:

- **Drained, not planned down** (stopped at or before step 3): the member still serves. Undrain the
  node (`kbf cluster undrain <node> --token-file /etc/kbf/api-token`), or run the walk again with
  this member first.
- **Planned down, unit running** (stopped before the restart, or the restart was never made): the
  member is out of the write path. `kbf cluster member <member id> --state active --token-file
  /etc/kbf/api-token` undoes the plan; then undrain the node. If the unit was rewritten, the old
  file is `<fragment>.before-registered` beside it.
- **Restarted but not back** (failed to start, or never applied the generation): the member stays
  `down`. After `--for-secs` the plan lapses, and after `--member-dwell-secs` (300 by default) the
  member leaves the map and its blobs move to other owners. Read the unit's journal, fix the unit
  (or put the `.before-registered` file back and `daemon-reload`), restart it, and wait until the
  member is `active` and its row `ok` at the generation before undrain. Do not run the walk on
  other members until it is back: the script starts only when every listed member is `active`.

Then run the walk again with only the members not yet done and, if the repoint was made, without
the repoint flags: it walks them to the registry's current generation.

## The held script

The walk above, for engineering to run on the frontend's host: save the block below as
`switch-object-store.sh` and check it with `bash -n switch-object-store.sh`. It is a held step:

- it prints every write and runs none of them without `--apply`;
- it stops at the first thing it does not expect: a `STOP:` line naming what the walk left behind,
  exit 1. An exit it did not plan (Ctrl-C, a signal, a failed read) prints a `STOP:` line too. It
  undoes nothing itself;
- it never forces anything: no drain deadline, no restart of a member `safe-to-stop` refuses, no
  removal of a drop-in, no overwrite of an earlier `.before-registered` copy;
- every wait has no deadline, and stops only on an answer that cannot become the one awaited (a
  failed read, a status other than `pending`, the registry moving);
- it reaches each host with `ssh` (no prompt) and `sudo` (no password), and needs `jq` and `curl`
  on the frontend's host and a user that can read the token file;
- its only inputs are its flags; it holds no endpoint and no key.

```text
switch-object-store.sh --member ID,HOST[,NODE] [--member ...] [--connection NAME --endpoint URL --expect-generation G] [--migrate] [--apply]
```

- `--member ID,HOST[,NODE]`, once per storage member, in the order to walk them: the member id,
  the host `ssh` reaches it at, and the node id of the daemon its unit also runs (leave it out for
  a storage host with no daemon).
- `--connection`, `--endpoint`, `--expect-generation`: repoint first, then walk to the generation
  the repoint answered. Without them there is no repoint, and the walk restarts the members onto
  the registry's current generation.
- `--migrate`: move each unit to `--cold-store registered` in its step (the section above).
  Without it, a unit that names a store URL stops the walk before anything is written.
- `--unit` (default `kbf-storage-and-daemon.service`), `--token-file` (default
  `/etc/kbf/api-token`), `--api` (default `127.0.0.1:8982`), `--poll-secs` (1 to 3600, default
  10), `--for-secs` (the planned-down window, 1 to 86400, default 900).

Before any write it checks every listed member, and stops on the first problem:

- the member is `active` in `GET /v1/members`;
- the unit is a hand-written file under `/etc/systemd/system` that systemd has loaded as it is
  (`NeedDaemonReload=no`);
- the unit's effective `ExecStart` (what systemd will run) is the member: its `--advertise-addr`,
  or else its `--storage-listen`, is the member's address, and its `--node-id` is the node given
  (a unit with `--node-id` and no node given is refused: its daemon would be restarted undrained);
- no drop-in names a store or sets `ExecStart=`: neither one systemd loaded (`DropInPaths`) nor
  any file in a drop-in directory of any of the unit's names (the section above);
- its `--cold-store` is `registered`, or with `--migrate` a store URL that the rewrite can take
  (the flag and its value on one line of the file, unquoted, as systemd runs them), with
  `--frontend` present;
- the registry is at `--expect-generation`.

The rewrite keeps a copy of the unit (`<fragment>.before-registered`), and after `daemon-reload`
compares the effective `ExecStart` with the one expected (the old one with `--cold-store
registered` and without `--cold-store-credentials-file`); on any difference it puts the copy back
and stops.

```shell
#!/usr/bin/env bash
# Held: switch the farm's object store and restart the storage members one at a time
# (docs/deploy/switch-object-store.md). Dry run by default; --apply runs the writes.
set -uo pipefail
# Words are split on purpose below (argument lists, file lists); never globbed.
set -f

API=127.0.0.1:8982
TOKEN_FILE=/etc/kbf/api-token
UNIT=kbf-storage-and-daemon.service
POLL_SECS=10
FOR_SECS=900
CONNECTION=
ENDPOINT=
EXPECT_GENERATION=
MIGRATE=0
APPLY=0
SPECS=()

# What a stop leaves behind; empty when the walk has changed nothing it has not undone.
STATE=
STOPPED=0
stop() {
  STOPPED=1
  echo "STOP: $*" >&2
  [ -z "$STATE" ] || echo "STOP: left behind: $STATE" >&2
  exit 1
}
on_exit() {
  local rc=$?
  if [ "$rc" != 0 ] && [ "$STOPPED" = 0 ]; then
    echo "STOP: the walk ended unexpectedly (exit $rc)" >&2
    [ -z "$STATE" ] || echo "STOP: left behind: $STATE" >&2
  fi
}
# After a stop printed in a subshell (a read inside $(...)): end quietly.
fail() { STOPPED=1; exit 1; }
# Records what a write left behind, under --apply only.
left() { [ "$APPLY" = 0 ] || STATE=$1; }
trap on_exit EXIT
trap 'exit 130' INT TERM HUP
say() { echo "== $*"; }

usage() {
  echo "usage: $0 --member ID,HOST[,NODE] [--member ...] [--connection NAME --endpoint URL --expect-generation G] [--migrate] [--unit UNIT] [--token-file FILE] [--api ADDR] [--poll-secs N] [--for-secs N] [--apply]" >&2
  STOPPED=1
  exit 2
}

# A whole number from 0 to 18 digits (well inside u64), with no leading zero.
is_count() { [[ $1 =~ ^(0|[1-9][0-9]{0,17})$ ]]; }

while [ $# -gt 0 ]; do
  case "$1" in
    --member) [ $# -ge 2 ] || usage; SPECS+=("$2"); shift 2 ;;
    --connection) [ $# -ge 2 ] || usage; CONNECTION=$2; shift 2 ;;
    --endpoint) [ $# -ge 2 ] || usage; ENDPOINT=$2; shift 2 ;;
    --expect-generation) [ $# -ge 2 ] || usage; EXPECT_GENERATION=$2; shift 2 ;;
    --unit) [ $# -ge 2 ] || usage; UNIT=$2; shift 2 ;;
    --token-file) [ $# -ge 2 ] || usage; TOKEN_FILE=$2; shift 2 ;;
    --api) [ $# -ge 2 ] || usage; API=$2; shift 2 ;;
    --poll-secs) [ $# -ge 2 ] || usage; POLL_SECS=$2; shift 2 ;;
    --for-secs) [ $# -ge 2 ] || usage; FOR_SECS=$2; shift 2 ;;
    --migrate) MIGRATE=1; shift ;;
    --apply) APPLY=1; shift ;;
    *) usage ;;
  esac
done

[ ${#SPECS[@]} -gt 0 ] || usage
is_count "$POLL_SECS" && [ "$POLL_SECS" -ge 1 ] && [ "$POLL_SECS" -le 3600 ] || stop "--poll-secs $POLL_SECS: give 1 to 3600"
is_count "$FOR_SECS" && [ "$FOR_SECS" -ge 1 ] && [ "$FOR_SECS" -le 86400 ] || stop "--for-secs $FOR_SECS: give 1 to 86400"
[[ $UNIT =~ ^[A-Za-z0-9@_.:-]+\.service$ ]] || stop "--unit $UNIT: give a unit name ending in .service"
REPOINT=0
if [ -n "$CONNECTION$ENDPOINT$EXPECT_GENERATION" ]; then
  [ -n "$CONNECTION" ] && [ -n "$ENDPOINT" ] && [ -n "$EXPECT_GENERATION" ] \
    || stop "--connection, --endpoint and --expect-generation go together"
  is_count "$EXPECT_GENERATION" || stop "--expect-generation $EXPECT_GENERATION: give a whole number"
  REPOINT=1
fi

# The members, parsed: parallel arrays by index.
MEMBERS=() HOSTS=() NODES=()
for spec in "${SPECS[@]}"; do
  [[ $spec =~ ^([^,]+),([^,]+)(,([^,]+))?$ ]] || stop "--member $spec: give ID,HOST or ID,HOST,NODE"
  id=${BASH_REMATCH[1]} host=${BASH_REMATCH[2]} node=${BASH_REMATCH[4]}
  is_count "$id" || stop "--member $spec: the member id $id is not a whole number"
  [[ $host =~ ^[A-Za-z0-9@._:-]+$ ]] || stop "--member $spec: the host $host is not a host name"
  [ -z "$node" ] || [[ $node =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] || stop "--member $spec: the node id $node is not a node id"
  for seen in "${MEMBERS[@]}"; do [ "$seen" != "$id" ] || stop "--member $spec: member $id is listed twice"; done
  MEMBERS+=("$id") HOSTS+=("$host") NODES+=("$node")
done

for tool in jq curl ssh kbf; do
  command -v "$tool" >/dev/null || stop "$tool is not installed on this host"
done
[ -r "$TOKEN_FILE" ] || stop "cannot read the token file $TOKEN_FILE"
if [ "$APPLY" = 1 ]; then say "APPLY: the writes below run"; else say "DRY RUN: no write runs; add --apply"; fi

# A write: printed, and run only with --apply. Its status is the command's.
run() {
  if [ "$APPLY" = 1 ]; then echo "+ $*"; "$@"; else echo "would run: $*"; fi
}

# A command on a host, its words quoted for the remote shell. ssh's own failure is 255.
on() { local host=$1; shift; ssh -o BatchMode=yes -- "$host" "$(printf '%q ' "$@")"; }

# Reads (each stops the walk when it fails; nothing is guessed).
members_json() {
  curl -fsS "http://$API/v1/members" || stop "cannot read GET /v1/members"
}
store_json() {
  kbf cluster --api "$API" object-store status --token-file "$TOKEN_FILE" \
    || stop "cannot read the object-store status"
}
nodes_json() {
  kbf cluster --api "$API" status || stop "cannot read the nodes"
}
member_field() { # member id, field
  local out
  out=$(members_json) || fail
  printf '%s' "$out" | jq -er --argjson m "$1" --arg f "$2" '.members[] | select(.id == $m) | .[$f] // empty' \
    || stop "member $1 is not in GET /v1/members"
}

# The effective ExecStart of $UNIT on a host, one argument a line.
effective_argv() { # host
  local shown
  shown=$(on "$1" systemctl show -P ExecStart "$UNIT") || stop "$1: cannot read the ExecStart of $UNIT"
  [[ $shown == *"argv[]="*" ; ignore_errors="* ]] || stop "$1: $UNIT has no ExecStart"
  [[ $shown != *"argv[]="*"argv[]="* ]] || stop "$1: $UNIT has more than one ExecStart"
  shown=${shown#*argv[]=}
  shown=${shown%% ; ignore_errors=*}
  [[ $shown != *[\"\']* ]] || stop "$1: the ExecStart of $UNIT has quotes; this walk does not read it"
  printf '%s\n' $shown
}

# The values of a flag in an argv (one argument a line): --name=value and --name value.
flag_values() { # argv, name
  local argv=$1 name=$2 prev= a
  while IFS= read -r a; do
    if [ "$prev" = "$name" ]; then printf '%s\n' "$a"
    elif [[ $a == "$name="* ]]; then printf '%s\n' "${a#"$name="}"
    fi
    prev=$a
  done <<<"$argv"
}
the_flag() { # host, argv, name: its one value, empty if absent; stops if given twice
  local values n
  values=$(flag_values "$2" "$3")
  n=$(printf '%s' "$values" | grep -c '^' || true)
  [ "$n" -le 1 ] || stop "$1: $UNIT gives $3 $n times"
  printf '%s' "$values"
}

# Exit status of a remote test: 0 true, 1 false, anything else stops.
remote_test() { # host, test args...
  local host=$1 rc=0; shift
  on "$host" test "$@" || rc=$?
  [ "$rc" -le 1 ] || stop "$host: cannot run test $* (exit $rc)"
  return "$rc"
}

# The drop-in directory names systemd reads for one unit name: <name>.d, the template's
# <template>@.service.d for an instance, each dash-prefix form (kbf-.service.d, ...) and,
# for an instance, its instance and template forms too (kbf-@<instance>.service.d,
# kbf-@.service.d), and service.d. A name too many is only one more directory looked at.
dropin_names() { # unit name
  local base=${1%.service} prefix= part instance=
  [[ $base != *@* ]] || instance=${base#*@}
  echo "$1.d"
  [[ $base != *@?* ]] || echo "${base%%@*}@.service.d"
  IFS=- read -ra parts <<<"${base%%@*}"
  for part in "${parts[@]:0:${#parts[@]}-1}"; do
    prefix+="$part-"
    echo "$prefix.service.d"
    if [[ $base == *@* ]]; then
      echo "$prefix@.service.d"
      [ -z "$instance" ] || echo "$prefix@$instance.service.d"
    fi
  done
  echo service.d
}

# Every drop-in directory systemd reads for $UNIT on a host, under each of its names
# (aliases included), in every unit directory.
dropin_dirs() { # host
  local names name part dir
  names=$(on "$1" systemctl show -P Names "$UNIT") || stop "$1: cannot read the names of $UNIT"
  [ -n "$names" ] || stop "$1: systemd gives no names for $UNIT"
  for name in $names; do
    for part in $(dropin_names "$name"); do
      for dir in /etc/systemd/system /run/systemd/system /usr/local/lib/systemd/system /usr/lib/systemd/system /lib/systemd/system; do
        echo "$dir/$part"
      done
    done
  done
}

# Checks a member's host and unit. Sets FRAGMENT, ARGV, NEEDS_MIGRATION (0 or 1), and the
# forms the rewrite replaces (STORE_FORM, CREDENTIALS_FORM).
check_unit() { # index
  local i=$1 host=${HOSTS[$1]} node=${NODES[$1]} addr dir dirs files found f rc store creds unit_node bound
  addr=$(member_field "${MEMBERS[$i]}" addr) || fail
  FRAGMENT=$(on "$host" systemctl show -P FragmentPath "$UNIT") || stop "$host: cannot read $UNIT"
  case "$FRAGMENT" in
    /etc/systemd/system/*) ;;
    *) stop "$host: $UNIT is '$FRAGMENT', not a hand-written unit under /etc/systemd/system" ;;
  esac
  [ "$(on "$host" systemctl show -P NeedDaemonReload "$UNIT")" = no ] \
    || stop "$host: $UNIT changed on disk since systemd read it; daemon-reload first"
  # The drop-ins systemd loaded, and every file in a drop-in directory it would read
  # (through a symbolic link too), loaded or not.
  files=$(on "$host" systemctl show -P DropInPaths "$UNIT") || stop "$host: cannot read the drop-ins of $UNIT"
  dirs=$(dropin_dirs "$host") || fail
  for dir in $dirs; do
    remote_test "$host" -d "$dir" || continue
    found=$(on "$host" find -L "$dir" -maxdepth 1 -name '*.conf') || stop "$host: cannot list $dir"
    files="$files $found"
  done
  for f in $files; do
    rc=0; on "$host" grep -nE 'ExecStart|cold-store|s3://|endpoint=|AWS_ENDPOINT' "$f" || rc=$?
    case "$rc" in
      0) stop "$host: the drop-in $f names a store or sets ExecStart (lines above); move that out of it by hand" ;;
      1) ;;
      *) stop "$host: cannot read the drop-in $f (exit $rc)" ;;
    esac
  done
  ARGV=$(effective_argv "$host") || fail
  bound=$(the_flag "$host" "$ARGV" --advertise-addr) || fail
  [ -n "$bound" ] || { bound=$(the_flag "$host" "$ARGV" --storage-listen) || fail; }
  [ "$bound" = "$addr" ] || stop "$host: $UNIT serves '$bound', but member ${MEMBERS[$i]} is $addr: wrong host or member"
  unit_node=$(the_flag "$host" "$ARGV" --node-id) || fail
  [ "$unit_node" = "$node" ] || stop "$host: $UNIT runs node '${unit_node:-(none)}', but '${node:-(none)}' was given"
  store=$(the_flag "$host" "$ARGV" --cold-store) || fail
  creds=$(the_flag "$host" "$ARGV" --cold-store-credentials-file) || fail
  [ -n "$(the_flag "$host" "$ARGV" --frontend)" ] || stop "$host: $UNIT has no --frontend; registered needs it"
  echo "   $host: member ${MEMBERS[$i]} at $addr, node ${node:-(none)}, --cold-store ${store:-(none)}"
  case "$store" in
    registered) NEEDS_MIGRATION=0
       [ -z "$creds" ] || stop "$host: $UNIT has --cold-store registered and --cold-store-credentials-file" ;;
    "") stop "$host: $UNIT has no --cold-store" ;;
    *) NEEDS_MIGRATION=1
       STORE_FORM=$(unit_form "$host" --cold-store "$store") || fail
       CREDENTIALS_FORM=
       [ -z "$creds" ] || { CREDENTIALS_FORM=$(unit_form "$host" --cold-store-credentials-file "$creds") || fail; } ;;
  esac
}

# How the unit file writes a flag and its value: `--name=value` or `--name value`, unquoted on
# one line, exactly once. Anything else stops: the rewrite does not take it.
unit_form() { # host, name, value
  local form found=
  for form in "$2=$3" "$2 $3"; do
    local rc=0 n
    n=$(on "$1" grep -cF -e "$form" "$FRAGMENT") || rc=$?
    [ "$rc" -le 1 ] || stop "$1: cannot read $FRAGMENT (exit $rc)"
    [ "$n" = 0 ] || { [ "$n" = 1 ] && [ -z "$found" ]; } || stop "$1: $FRAGMENT writes $2 more than once"
    [ "$n" = 0 ] || found=$form
  done
  [ -n "$found" ] || stop "$1: $FRAGMENT writes $2 quoted or across lines; move it to --cold-store registered by hand"
  printf '%s' "$found"
}

# The argv with --cold-store registered and without --cold-store-credentials-file.
expected_argv() {
  local prev= a
  while IFS= read -r a; do
    if [ "$prev" = --cold-store ]; then echo registered
    elif [ "$prev" = --cold-store-credentials-file ]; then :
    elif [[ $a == --cold-store=* ]]; then echo --cold-store=registered
    elif [[ $a == --cold-store-credentials-file* ]]; then :
    else echo "$a"
    fi
    prev=$a
  done <<<"$ARGV"
}

sed_literal() { printf '%s' "$1" | sed -e 's/[][\.*^$/]/\\&/g'; }

# Rewrites the unit to --cold-store registered, keeping a copy, and checks what systemd will run.
migrate_unit() { # index
  local host=${HOSTS[$1]} backup="$FRAGMENT.before-registered" want got
  [ "$MIGRATE" = 1 ] || stop "$host: $UNIT names a store URL and --migrate was not given"
  want=$(expected_argv)
  local to="--cold-store registered"
  [[ $STORE_FORM != --cold-store=* ]] || to=--cold-store=registered
  local edits=(-e "s/$(sed_literal "$STORE_FORM")/$to/")
  [ -z "$CREDENTIALS_FORM" ] || edits+=(-e "s/[[:space:]]*$(sed_literal "$CREDENTIALS_FORM")//")
  if [ "$APPLY" = 0 ]; then
    echo "would run: on $host: sudo cp -p $FRAGMENT $backup; sudo sed -i ${edits[*]} $FRAGMENT; sudo systemctl daemon-reload"
    return
  fi
  if remote_test "$host" -e "$backup"; then stop "$host: $backup already exists; look at it and move it away first"; fi
  echo "+ on $host: sudo cp -p -- $FRAGMENT $backup"
  on "$host" sudo cp -p -- "$FRAGMENT" "$backup" || stop "$host: the copy of $FRAGMENT failed; the unit is unchanged"
  STATE="$STATE; $FRAGMENT copied to $backup"
  echo "+ on $host: sudo sed -i ${edits[*]} -- $FRAGMENT"
  on "$host" sudo sed -i "${edits[@]}" -- "$FRAGMENT" || stop "$host: the edit of $FRAGMENT failed; put $backup back"
  STATE="$STATE; $FRAGMENT rewritten"
  on "$host" sudo systemctl daemon-reload || stop "$host: daemon-reload failed; put $backup back"
  got=$(effective_argv "$host") || fail
  if [ "$got" != "$want" ]; then
    echo "   expected ExecStart:"; printf '      %s\n' $want
    echo "   systemd has:"; printf '      %s\n' $got
    on "$host" sudo cp -p -- "$backup" "$FRAGMENT" && on "$host" sudo systemctl daemon-reload \
      || stop "$host: the rewrite gave another ExecStart, and putting $backup back failed"
    stop "$host: the rewrite gave another ExecStart than expected (above); $backup is back in place"
  fi
  echo "   $host: ExecStart has --cold-store registered and no --cold-store-credentials-file"
}

# Waits, with no deadline, while the check says `wait`; the check stops the walk itself.
wait_until() { # what, check...
  local what=$1 answer; shift
  while :; do
    answer=$("$@") || fail
    [ "$answer" = wait ] || break
    echo "   waiting: $what"
    sleep "$POLL_SECS"
  done
  echo "   done: $what"
}

drained() { # node
  local out state
  out=$(nodes_json) || fail
  state=$(printf '%s' "$out" | jq -r --arg n "$1" '.nodes[] | select(.node_id == $n) | .placement.state') \
    || stop "the nodes answer does not parse"
  case "$state" in
    drained) echo done ;;
    draining | drain_paused) echo wait ;;
    *) stop "node $1 is '${state:-absent}', not draining" ;;
  esac
}

member_back() { # member id
  local state
  state=$(member_field "$1" state) || fail
  case "$state" in
    active) echo done ;;
    down | catching_up | joining) echo wait ;;
    *) stop "member $1 is '$state'" ;;
  esac
}

applied() { # member address, generation
  local out generation row status got
  out=$(store_json) || fail
  generation=$(printf '%s' "$out" | jq -er .generation) || stop "the object-store status does not parse"
  [ "$generation" = "$2" ] || stop "the registry moved to generation $generation during the walk (want $2)"
  row=$(printf '%s' "$out" | jq -c --arg a "$1" '.convergence.nodes[] | select(.id == $a)') \
    || stop "the object-store status does not parse"
  [ -n "$row" ] || { echo wait; return; }
  status=$(printf '%s' "$row" | jq -r .status)
  got=$(printf '%s' "$row" | jq -r '.applied_generation // "none"')
  case "$status" in
    ok) if [ "$got" = "$2" ]; then echo done; else echo wait; fi ;;
    pending) echo wait ;;
    *) stop "member at $1 reports $status at generation $got" ;;
  esac
}

# 1. Check every member before any write.
say "checking every member"
for i in "${!MEMBERS[@]}"; do
  state=$(member_field "${MEMBERS[$i]}" state) || fail
  [ "$state" = active ] || stop "member ${MEMBERS[$i]} is '$state', not active"
  check_unit "$i"
  if [ "$NEEDS_MIGRATION" = 1 ] && [ "$MIGRATE" = 0 ]; then
    stop "${HOSTS[$i]}: $UNIT names a store URL and would not follow; run with --migrate"
  fi
  if [ -n "${NODES[$i]}" ]; then
    out=$(nodes_json) || fail
    printf '%s' "$out" | jq -e --arg n "${NODES[$i]}" 'any(.nodes[]; .node_id == $n)' >/dev/null \
      || stop "node ${NODES[$i]} is not in GET /v1/nodes"
  fi
done

# 2. The registry, and the repoint.
out=$(store_json) || fail
current=$(printf '%s' "$out" | jq -er .generation) || stop "the object-store status does not parse"
say "the registry is at generation $current"
target=$current
if [ "$REPOINT" = 1 ]; then
  [ "$current" = "$EXPECT_GENERATION" ] || stop "the registry is at $current, not --expect-generation $EXPECT_GENERATION"
  if [ "$APPLY" = 1 ]; then
    say "repoint $CONNECTION to $ENDPOINT"
    answer=$(kbf cluster --api "$API" object-store repoint --connection "$CONNECTION" --endpoint "$ENDPOINT" --expect-generation "$EXPECT_GENERATION" --token-file "$TOKEN_FILE") \
      || stop "the repoint was refused (above); nothing changed"
    echo "$answer"
    target=$(printf '%s' "$answer" | jq -er .generation) || stop "the repoint answer does not parse; read the status"
  else
    echo "would run: repoint --connection $CONNECTION --endpoint $ENDPOINT --expect-generation $EXPECT_GENERATION"
    target="(the repoint's answer)"
  fi
fi
say "every member must apply generation $target"

# 3. One member at a time.
for i in "${!MEMBERS[@]}"; do
  member_id=${MEMBERS[$i]} host=${HOSTS[$i]} node_id=${NODES[$i]}
  addr=$(member_field "$member_id" addr) || fail
  say "member $member_id at $addr ($host${node_id:+, node $node_id})"
  if ! kbf cluster --api "$API" safe-to-stop "$member_id" --token-file "$TOKEN_FILE"; then
    stop "member $member_id: not safe to stop now, or the call failed (above); run the walk again later with the members not yet done"
  fi
  if [ -n "$node_id" ]; then
    run kbf cluster --api "$API" drain "$node_id" --token-file "$TOKEN_FILE" \
      || stop "node $node_id: the drain was refused (above)"
    left "node $node_id drained: undrain it"
    [ "$APPLY" = 0 ] || wait_until "node $node_id drained" drained "$node_id"
  fi
  run kbf cluster --api "$API" member "$member_id" --state planned-down --for-secs "$FOR_SECS" --token-file "$TOKEN_FILE" \
    || stop "member $member_id: planned-down was refused (above)"
  left "member $member_id planned down for $FOR_SECS s, unit not yet restarted${node_id:+; node $node_id drained}"
  check_unit "$i"
  [ "$NEEDS_MIGRATION" = 0 ] || migrate_unit "$i"
  run on "$host" sudo systemctl restart "$UNIT" || stop "$host: the restart of $UNIT failed (see its journal)"
  left "member $member_id restarted, not yet back${node_id:+; node $node_id drained}"
  if [ "$APPLY" = 1 ]; then
    wait_until "member $member_id active" member_back "$member_id"
    wait_until "member $member_id applied generation $target" applied "$addr" "$target"
  fi
  if [ -n "$node_id" ]; then
    run kbf cluster --api "$API" undrain "$node_id" --token-file "$TOKEN_FILE" \
      || stop "node $node_id: the undrain was refused (above)"
  fi
  STATE=
done

# 4. The farm.
if [ "$APPLY" = 1 ]; then
  out=$(store_json) || fail
  printf '%s' "$out" | jq '.convergence | {converged, expected, applied, unsupported}'
fi
say "walk done"
```
