# Switching the farm's object store

The farm's object store is what the frontend registers (`kbf cluster object-store`, the
`/v1/object-stores/...` routes of [the operator API](../api.md)): a connection (an endpoint and a
key pair) and the `cold` binding of that connection to a bucket and a prefix. A storage node
started with `--cold-store registered` names no store of its own: it takes the registered one
at its start ([storage-node.md](storage-node.md)).

Switching the store is one command on the frontend's host, then a restart of the storage members
one at a time:

- **The command** moves the registration to the new endpoint and raises its generation. It
  changes nothing on a node.
- **The rolling restart** is when each node takes the new store. A running node keeps the store
  it opened; it reports `pending` at the new generation until it is restarted.

What does not change: the key pair (the connection keeps the key it has, and the frontend probes
the new endpoint with it before it writes anything), the bucket and the prefix unless the command
names new ones, and every node that names a store URL in its `--cold-store`: such a node never
follows a repoint (it keeps its `identity_mismatch` behaviour). Move each node to `registered`
first ([below](#moving-a-node-to---cold-store-registered)).

## Before the first switch

- The connection is registered and `cold` is bound to it (`kbf cluster object-store status`
  lists both).
- The connection's key is valid at the new endpoint: the repoint probes the new store with it
  and changes nothing when the probe fails.
- Every storage member was started with `--cold-store registered`.

## The command

On the frontend's host, read the registry's generation, then repoint at it:

```text
kbf cluster object-store status --token-file /etc/kbf/api-token
kbf cluster object-store repoint --connection <connection> --endpoint <new endpoint URL> --expect-generation <generation> --token-file /etc/kbf/api-token
```

- `<generation>` is the `generation` that `status` printed. A write in between makes the
  repoint answer `409 generation_mismatch` and change nothing: read `status` again.
- `<new endpoint URL>` is `http://host[:port]`. `--bucket` and `--prefix` move those too; without
  them the binding's stay.
- The frontend probes the new store with the registered key first; a failed probe answers
  `422 probe_failed` and changes nothing. The answer is `{name, generation, changed, from, to,
  verified}`, and the audit log (`kbf cluster object-store audit --token-file
  /etc/kbf/api-token`) has a row with the old and the new identity. Repointing to the current
  identity is `changed: false` and keeps the generation.

Write down the `generation` of the answer: it is the one every node must apply.

## The rolling restart

Restart the storage members **one at a time**. Blobs are kept on two owners, so two members down
at once leave some blobs with no reachable copy (see [updating a host](update-a-host.md#restarting-a-storage-member)).
For each member, with its node id `<node>` and its member id `<member id>` (`GET /v1/members`):

1. **Safe to stop.** Exit 0 means it may stop now; exit 3 prints why not. Wait and ask again;
   never go on without a 0.

   ```text
   kbf cluster safe-to-stop <node> --token-file /etc/kbf/api-token
   ```

2. **Drain** the node, and wait until `kbf cluster status` shows its placement `drained`. The
   drain has no deadline: it waits for its leases however long they run.

   ```text
   kbf cluster drain <node> --token-file /etc/kbf/api-token
   kbf cluster status
   ```

3. **Plan the restart** of its storage member, so writes go to the other owner at once:

   ```text
   kbf cluster member <member id> --state planned-down --for-secs 900 --token-file /etc/kbf/api-token
   ```

4. **Restart** its unit on the node's host: `sudo systemctl restart <unit>`.
5. **Wait until it is back with the new generation applied**: its member is `active` again in
   `GET /v1/members` (`catching_up` while its index rebuilds), and its row in `status`'s
   `convergence.nodes` has `applied_generation` equal to the generation the repoint answered,
   with `status` `ok`. A row that stays `pending` at the old generation is a node that did not
   restart; `identity_mismatch` is a node that names a store URL.

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
  and its value (`registered` refuses it: the keys come from the frontend);
- look at **every** drop-in of the unit (`systemctl show -P DropInPaths <unit>`, and every file
  in `/etc/systemd/system/<unit>.d/`, loaded or not): a drop-in that names a store (`--cold-store`, an
  `s3://` URI, an `endpoint=`, `AWS_ENDPOINT_URL`) is applied after the unit and wins, so move
  that line out of it;
- `sudo systemctl daemon-reload`, and check the flag once: `systemctl show -P ExecStart <unit>`
  holds `--cold-store registered`.

Then restart (step 4). The node fetches the registration at its start, and with no registration
it waits for one; it never falls back to a store of its own. `/etc/kbf/s3.env` and any other key
file on the node are no longer read by `kbf` once it is registered.

## The held script

The walk above, for engineering to run on the frontend's host: save the block below as
`switch-object-store.sh` and check it with `bash -n switch-object-store.sh`. It is a held step:
it prints every write and runs none of them without `--apply`, it stops at the first thing it
does not expect (a `STOP:` line, exit 1), and it never forces anything (no drain deadline, no
restart of a node `safe-to-stop` refuses, no removal of a drop-in). It reaches each node's host
with `ssh` and `sudo`. Its only flags are the values the walk needs; it holds no endpoint and no
key.

```text
switch-object-store.sh --node NODE,MEMBER,HOST [--node ...] [--connection NAME --endpoint URL --expect-generation G] [--migrate] [--apply]
```

- `--node NODE,MEMBER,HOST`, once per storage member, in the order to walk them: the node id, the
  member id and the host `ssh` reaches it at.
- `--connection`, `--endpoint`, `--expect-generation`: repoint first, then walk to the generation
  the repoint answered. Without them there is no repoint, and the walk restarts the members onto
  the registry's current generation.
- `--migrate`: move each node's unit to `--cold-store registered` in its step (the section above).
  Without it, a node whose unit names a store URL stops the walk before anything is written.
- `--unit` (default `kbf-storage-and-daemon.service`), `--token-file` (default
  `/etc/kbf/api-token`), `--api` (default `127.0.0.1:8982`), `--poll-secs` (default 10).

Before any write it checks every node (the unit's `--cold-store`, every drop-in) and the
registry's generation, so a problem stops the walk before the repoint. Waits have no deadline;
Ctrl-C stops the walk, and the node it was on stays drained until `kbf cluster undrain`. To go
on after a stop once the repoint is done, run it again without the repoint flags: it walks every
listed node to the registry's current generation (a node already on it is restarted once more).
`ssh` must reach each host without a prompt, and `sudo` there must not ask for a password.

```bash
#!/usr/bin/env bash
# Held: switch the farm's object store and restart the storage members one at a time
# (docs/deploy/switch-object-store.md). Dry run by default; --apply runs the writes.
set -euo pipefail

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
NODES=()

stop() { echo "STOP: $*" >&2; exit 1; }
say() { echo "== $*"; }

usage() {
  echo "usage: $0 --node NODE,MEMBER,HOST [--node ...] [--connection NAME --endpoint URL --expect-generation G] [--migrate] [--unit UNIT] [--token-file FILE] [--api ADDR] [--poll-secs N] [--apply]" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --node) [ $# -ge 2 ] || usage; NODES+=("$2"); shift 2 ;;
    --connection) [ $# -ge 2 ] || usage; CONNECTION=$2; shift 2 ;;
    --endpoint) [ $# -ge 2 ] || usage; ENDPOINT=$2; shift 2 ;;
    --expect-generation) [ $# -ge 2 ] || usage; EXPECT_GENERATION=$2; shift 2 ;;
    --unit) [ $# -ge 2 ] || usage; UNIT=$2; shift 2 ;;
    --token-file) [ $# -ge 2 ] || usage; TOKEN_FILE=$2; shift 2 ;;
    --api) [ $# -ge 2 ] || usage; API=$2; shift 2 ;;
    --poll-secs) [ $# -ge 2 ] || usage; POLL_SECS=$2; shift 2 ;;
    --migrate) MIGRATE=1; shift ;;
    --apply) APPLY=1; shift ;;
    *) usage ;;
  esac
done

[ ${#NODES[@]} -gt 0 ] || usage
REPOINT=0
if [ -n "$CONNECTION$ENDPOINT$EXPECT_GENERATION" ]; then
  [ -n "$CONNECTION" ] && [ -n "$ENDPOINT" ] && [ -n "$EXPECT_GENERATION" ] \
    || stop "--connection, --endpoint and --expect-generation go together"
  REPOINT=1
fi
command -v jq >/dev/null || stop "jq is not installed on this host"
command -v kbf >/dev/null || stop "kbf is not on PATH"
[ -r "$TOKEN_FILE" ] || stop "cannot read the token file $TOKEN_FILE"
if [ "$APPLY" = 1 ]; then say "APPLY: the writes below run"; else say "DRY RUN: no write runs; add --apply"; fi

# A write: printed, and run only with --apply.
run() {
  if [ "$APPLY" = 1 ]; then echo "+ $*"; "$@"; else echo "would run: $*"; fi
}

# A command on a node's host (reads run in a dry run too; writes go through run).
on() { local host=$1; shift; ssh -o BatchMode=yes -- "$host" "$(printf '%q ' "$@")"; }

# Waits, with no deadline, until the command succeeds.
wait_until() {
  local what=$1; shift
  until "$@"; do echo "   waiting: $what"; sleep "$POLL_SECS"; done
  echo "   done: $what"
}

store_status() { kbf cluster --api "$API" object-store status --token-file "$TOKEN_FILE"; }

# The unit file and every drop-in of $UNIT on $host: prints what it finds; returns 0 when
# the unit says `--cold-store registered`, 10 when it names a store URL; stops on anything
# else.
check_unit() {
  local host=$1 fragment flag dropins f
  fragment=$(on "$host" systemctl show -P FragmentPath "$UNIT")
  [ -n "$fragment" ] || stop "$host: systemd has no unit $UNIT"
  case "$fragment" in
    /etc/systemd/system/*) ;;
    *) stop "$host: $UNIT is $fragment, not a hand-written unit under /etc/systemd/system" ;;
  esac
  dropins=$(on "$host" systemctl show -P DropInPaths "$UNIT")
  dropins="$dropins $(on "$host" find "/etc/systemd/system/$UNIT.d" -maxdepth 1 -type f -name '*.conf' 2>/dev/null | tr '\n' ' ' || true)"
  for f in $dropins; do
    if on "$host" grep -nE 'cold-store|s3://|endpoint=|AWS_ENDPOINT' "$f"; then
      stop "$host: the drop-in $f names a store (lines above); move that line out of it by hand"
    fi
  done
  flag=$(on "$host" grep -oE -- '--cold-store(=|[[:space:]]+)[^[:space:]\\]+' "$fragment" || true)
  echo "   $host: $fragment: ${flag:-no --cold-store}"
  case "$flag" in
    "") stop "$host: $fragment has no --cold-store" ;;
    *registered) return 0 ;;
    *) [ "$(printf '%s\n' "$flag" | wc -l)" = 1 ] || stop "$host: $fragment has more than one --cold-store"
       return 10 ;;
  esac
}

# Rewrites the unit to `--cold-store registered`, keeping a copy, and checks the flag once.
migrate_unit() {
  local host=$1 fragment
  fragment=$(on "$host" systemctl show -P FragmentPath "$UNIT")
  run on "$host" sudo cp -p -- "$fragment" "$fragment.before-registered"
  run on "$host" sudo sed -i -E \
    -e 's#--cold-store(=|[[:space:]]+)[^[:space:]\\]+#--cold-store registered#' \
    -e 's#[[:space:]]*--cold-store-credentials-file(=|[[:space:]]+)[^[:space:]\\]+##' \
    -- "$fragment"
  run on "$host" sudo systemctl daemon-reload
  if [ "$APPLY" = 1 ]; then
    on "$host" systemctl show -P ExecStart "$UNIT" | grep -q -- '--cold-store registered' \
      || stop "$host: after the edit ExecStart has no --cold-store registered; the old unit is $fragment.before-registered"
    if on "$host" systemctl show -P ExecStart "$UNIT" | grep -q -- '--cold-store-credentials-file'; then
      stop "$host: after the edit ExecStart still has --cold-store-credentials-file"
    fi
  fi
}

node_drained() {
  [ "$(kbf cluster --api "$API" status | jq -r --arg n "$1" '.nodes[] | select(.node_id == $n) | .placement.state')" = drained ]
}

member_active() {
  [ "$(curl -sS "http://$API/v1/members" | jq -r --argjson m "$1" '.members[] | select(.id == $m) | .state')" = active ]
}

node_applied() {
  store_status | jq -e --arg n "$1" --argjson g "$2" \
    '.convergence.nodes[] | select(.id == $n) | .applied_generation == $g and .status == "ok"' >/dev/null
}

# 1. Check every node before any write.
say "checking every node's unit and drop-ins"
for spec in "${NODES[@]}"; do
  IFS=, read -r node_id member_id host <<<"$spec"
  [ -n "$node_id" ] && [ -n "$member_id" ] && [ -n "$host" ] || stop "--node $spec: give NODE,MEMBER,HOST"
  rc=0; check_unit "$host" || rc=$?
  if [ "$rc" = 10 ] && [ "$MIGRATE" = 0 ]; then
    stop "$host: its unit names a store URL and would not follow; run with --migrate"
  fi
done

# 2. The registry, and the repoint.
current=$(store_status | jq -r .generation)
say "the registry is at generation $current"
if [ "$REPOINT" = 1 ]; then
  [ "$current" = "$EXPECT_GENERATION" ] || stop "the registry is at $current, not --expect-generation $EXPECT_GENERATION"
  if [ "$APPLY" = 1 ]; then
    echo "+ kbf cluster object-store repoint --connection $CONNECTION --endpoint $ENDPOINT --expect-generation $EXPECT_GENERATION --token-file $TOKEN_FILE"
    answer=$(kbf cluster --api "$API" object-store repoint --connection "$CONNECTION" --endpoint "$ENDPOINT" --expect-generation "$EXPECT_GENERATION" --token-file "$TOKEN_FILE") \
      || stop "the repoint was refused (above); nothing changed"
    echo "$answer"
    target=$(printf '%s' "$answer" | jq -r .generation)
  else
    echo "would run: kbf cluster object-store repoint --connection $CONNECTION --endpoint $ENDPOINT --expect-generation $EXPECT_GENERATION --token-file $TOKEN_FILE"
    target="(the repoint's answer)"
  fi
else
  target=$current
fi
say "every node must apply generation $target"

# 3. One member at a time.
for spec in "${NODES[@]}"; do
  IFS=, read -r node_id member_id host <<<"$spec"
  say "$node_id (member $member_id, $host)"
  kbf cluster --api "$API" safe-to-stop "$node_id" --token-file "$TOKEN_FILE" \
    || stop "$node_id: not safe to stop now (above); run the walk again later, without the repoint flags"
  run kbf cluster --api "$API" drain "$node_id" --token-file "$TOKEN_FILE" \
    || stop "$node_id: the drain was refused (above)"
  [ "$APPLY" = 0 ] || wait_until "$node_id drained" node_drained "$node_id"
  run kbf cluster --api "$API" member "$member_id" --state planned-down --for-secs "$FOR_SECS" --token-file "$TOKEN_FILE" \
    || stop "member $member_id: planned-down was refused (above); $node_id stays drained"
  rc=0; check_unit "$host" || rc=$?
  [ "$rc" = 0 ] || migrate_unit "$host"
  run on "$host" sudo systemctl restart "$UNIT" \
    || stop "$host: the restart of $UNIT failed; $node_id stays drained"
  if [ "$APPLY" = 1 ]; then
    wait_until "member $member_id active" member_active "$member_id"
    wait_until "$node_id applied generation $target" node_applied "$node_id" "$target"
  fi
  run kbf cluster --api "$API" undrain "$node_id" --token-file "$TOKEN_FILE" \
    || stop "$node_id: the undrain was refused (above)"
done

# 4. The farm.
if [ "$APPLY" = 1 ]; then
  store_status | jq '.convergence | {converged, expected, applied, unsupported}'
fi
say "walk done"
```
