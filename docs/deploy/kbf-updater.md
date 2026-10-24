# kbf-updater: rolling a version over a farm

`kbf-updater` rolls one version of the `kbf` package over the hosts of a farm, one host at a
time, behind kbf-proxy's local socket. It runs on the frontend host. It installs through the
forced command of [`kbf-host-install`](host-install.md) over ssh, asks the frontend's loopback
operator API ([the API](../api.md)) whether a host may be stopped and whether it runs the target,
and writes its progress to `roll.json` in its state directory before each step, so a restart
of the updater finds a roll where it was.

## The inventory

`--inventory` names a root-owned file (and a root-owned directory), one host per line:

```text
# alias  address               role        host key (type and base64)                 [node id]
fe-a     fe.farm.example       frontend    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  instance=a base=18000
fe-b     fe.farm.example       frontend    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  instance=b base=18010
st1      st1.farm.example      storage     ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample
dm1      dm1.farm.example      daemon      ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  dm1
```

The roles are `frontend`, `storage`, `daemon` and `mac-daemon`. A daemon host lists the node id
of its kbf configuration as the sixth word.

### A frontend that is a member of the kbf-proxy pool

A `frontend` line with two more words after the key is one member of a pool that kbf-proxy
balances over (pool mode, the `[[proxy.frontend]]` tables of [the proxy's API](../design/kbf-proxy-api.md)):

| word | meaning |
|---|---|
| `instance=<name>` | the name the proxy knows the member by (`name` of its `[[proxy.frontend]]` table) and the unit instance the host restarts for it: `kbf-frontend@<name>.service`. `-` is the plain unit `kbf-frontend.service`. Otherwise `[a-z][a-z0-9-]{0,31}` |
| `base=<port>` | the first of the member's five ports (`base_port` of the table), 1024 to 65531. The member's operator API is at `base` plus 2 on loopback |

- There is one line per member, and every line lists the same address and host key: every
  member runs on the host the updater runs on, and the retire and the commit go to the member
  on loopback while the install and the restart go over ssh to the address, so a member that
  lists another address or host key than an earlier one is refused.
- No two members share an `instance`, and their bases are at least 5 apart (a member uses
  `base` to `base` plus 4), or the file is refused.
- A pool of members and a `frontend` line without these words cannot be in one file, and a
  second `frontend` line without them is refused as before. A `frontend` line without them is
  not in a pool, nothing can move traffic away from it, and a roll that reaches it fails
  there (`swap_abandoned`) before any ssh call.
- `instance=` and `base=` are accepted on a `frontend` line only.

## The flags

All are flags; none is read from the environment.

| flag | |
|---|---|
| `--state-dir`, `--inventory`, `--proxy-uid`, `--token-file`, `--ssh-key-file`, `--ssh-user`, `--channel` | required |
| `--operator-api http://<loopback ip>:<port>` | required: the frontend's operator API, for safe-to-stop and the health of storage and daemon hosts. Not a loopback address is refused |
| `--operator-proxy-auth-file` | the proxy's per-boot secret, for a frontend that runs behind kbf-proxy |
| `--proxy-admin http://<loopback ip>:<port>` | the admin API of the kbf-proxy in pool mode (`admin_listen`). Required when the inventory lists pool members; refused unless it is a loopback address. It asks for no credential, and none is sent |
| `--socket` | the roll API's socket, for tests and development; in production systemd passes it |
| `--ssh-connect-timeout-secs`, `--ssh-command-timeout-secs`, `--drain-deadline-secs`, `--apply-poll-secs`, `--apply-deadline-secs` | optional bounds in seconds; they default to 10, 900, 600, 5 and 600 |

The updater reaches each member's operator API on loopback at `127.0.0.1:<base + 2>`, with the
same token (`--token-file`) and proxy secret as `--operator-api`.

## What a roll does with a frontend pool member

A roll takes the members before every other host, one member at a time, and does not begin one
until the one before it is back in the pool. For a member the turn is:

1. **Check.** The proxy's list (`GET /v1/proxy/upstreams`) must show some other member up and
   ready. If none is, the roll fails with `unsafe_to_stop` and nothing has been changed: the last
   member that is up is never taken out. The member itself need not be up: one that is drained
   or not ready is taken out of nothing, and is rolled while one other member serves.
   (`unknown_member` if the proxy does not list the member, `safe_to_stop_unavailable` if the
   list cannot be read.)
2. **Drain** (`draining`). `POST /v1/proxy/upstreams/<instance>:drain`: the proxy gives the member
   no new stream. The roll then waits until the REAPI and operator streams the proxy shows open
   on the member are 0, for at most 900 seconds (`drain_failed` if they are not).
3. **Retire** (`retiring`). `POST /v1/frontend/retire` on the member's operator API: the member
   hands its work to the other frontends, and the roll waits until the member reports no stream
   open, for at most 900 seconds (`retire_failed`). The call is answered when the retire has
   ended, so it is given 120 seconds to be answered.
4. **Install** (`installing`). The package is installed on the host through `kbf-host-install`,
   unless the host already has the target. An install the host reports as in progress is waited
   for, for at most 900 seconds (`install_failed`), and not run a second time beside it. The
   package covers every member of the host, so this happens once per host, at the first member.
5. **Restart** (`restarting`). `restart frontend=<instance>` through `kbf-host-install`
   (`restart_failed`).
6. **Ready** (`waiting_healthy`). The roll waits until the proxy lists the member ready and the
   member's `GET /healthz` reports a `commit` that the target's version names, for at most the
   frontend health bound, 300 seconds (`health_timeout`; `swap_abandoned` if the version names no
   commit or the proxy drops the member).
7. **Undrain** (`undraining`). `POST /v1/proxy/upstreams/<instance>:undrain` (`undrain_failed`).

Every failure after the check stops the roll. One after the proxy has drained the member **leaves
it drained**: the detail names it (`frontend member a (fe-a) is left drained`). A drain the proxy
refuses says only that, and the member may not be drained. The updater never undrains a member it
could not show to be healthy. An operator looks at the member, and either fixes it and starts
another roll (a member found drained is rolled again as long as another member is up) or puts it
back by hand:

- A failure before the retire (the check, the drain, the wait for streams) leaves a member that
  still serves its work as before, and `POST /v1/proxy/upstreams/<instance>:undrain` puts it back.
- A failure at the retire or after it may leave a member that is **retired**: it refuses new work
  and is not ready until its unit is restarted, and undraining it would put a member that does not
  serve back in the pool. Restart the unit (`restart frontend=<instance>` of `kbf-host-install`)
  or start another roll, which finishes the member. Undrain by hand only once the member is ready
  and runs the target.

### A restart of the updater

Each phase above is written to `roll.json` before its work begins. A roll found `running` after
a restart is resumed with `drive`: the member in hand is drained again (a drain does nothing to a
member that is already out), and the turn goes on from the phase the record holds. A member found
in `installing` asks the host what it has and installs only if it does not have the target
(waiting out an install it reports as in progress); one found in `undraining` is drained again
and put back.

A `roll.json` written by an earlier build, which has none of the phases `draining`, `retiring`,
`restarting` and `undraining`, is read as it was: no field was added, and the format is still 1.
The other way does not hold: an earlier build does not know these phases and refuses to start on
a `roll.json` that holds one, so finish or abort a roll that is in a member's turn before
downgrading the updater.
