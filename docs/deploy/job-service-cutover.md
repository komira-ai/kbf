# Moving a frontend's executor onto the job service: the check, the switch and the back-out

The job service is the jobscheduler's `jobscheduler.v1.JobService`, served by its lease server
(`js-lease-server`) on the port of that server's `--job-listen`. kbf is a client of it: a
frontend whose executor is `js-agent` submits each action as a job and reads the job's result,
instead of running it on a node's daemon. This page covers what has to be running before a
frontend is pointed at the service, how kbf's client token gets to the frontend's host, the
read-only check that the service answers that token, the switch itself (`kbf-deploy job-service
plan` and `apply`, with a canary job), and the way back (`kbf-deploy job-service back-out`).

**This is not kbf's own `--job-listen`.** A kbf frontend also has a flag of that name: the
frontend's job-facing read listener, from which a task fetches an action's inputs with a read
grant ([kbf-modes.md](kbf-modes.md), "The job-facing read listener"). That listener is part of
kbf and is not an executor mode. The job service on this page is another process, on another
host or port, reached with the jobscheduler lease server's `--job-listen`.

## The mode

The executor-to-job-service mode is `executor = "js-agent"` in the `[executor]` table of the
frontend's `kbf.toml` (the `--executor` flag; [kbf-modes.md](kbf-modes.md) describes every key).
The switch sets exactly these keys and leaves every other line of the file as it was:

| key (flag) | set to |
|---|---|
| `executor` | `"js-agent"` |
| `executor-job-url` | the job service, `https://host:port` (or `http://` to loopback) |
| `executor-job-token-file` | the path of kbf's client token on the frontend's host |
| `executor-job-ca-file` | the CA of the service's certificate, for an `https` URL; removed for an `http` one |
| `executor-hosts` | how many virtual workers the frontend registers, `js:<frontend id>-<n>` |
| `executor-host-cpus` | the cores of each virtual worker |
| `executor-host-memory-gib` | the memory of each virtual worker, in GiB |
| `executor-host-slots` | the actions each virtual worker runs at once (default 8) |

The rest of what `js-agent` needs must be in the file already, and the switch does not write
it: `executor-frontend-id`, `executor-base-image`, `executor-kbf-image`, the secret delivery
(`executor-secret-delivery = "files"`, or `executor-insecure-argv-secrets = true` for the
default `argv`), the packs (`--pack-store-endpoint`), the task runs and the job-facing read
listener. `plan` names the `[executor]` keys that are missing, and `apply` checks the whole
new file with `kbf --check` before it puts it in place.

**A flag on the frontend's command line overrides its key.** The switch changes `kbf.toml`
only. If the unit or a drop-in of it (`systemctl cat <unit>`) passes an `--executor*` flag,
take that flag out first, or the frontend goes on reading it.

## What must be running

The check, and later the cutover, need the job service's whole stack running and reachable from
the frontend's host. kbf does not install or start any of it; the jobscheduler's documents say how
each part is set up:

1. **The lease server's database**: set up as the jobscheduler's
   [state database runbook](https://github.com/komira-ai/jobscheduler/blob/main/docs/runbooks/state-database.md)
   says, with its least-rights role. As built, a lease server that runs jobs keeps them in its job
   store, a file beside its state file, and the runbook says what is kept where.
2. **`js-lease-server` with the job listener on**: `--job-listen <addr>` (`server.job_listen`),
   which needs a pool that registers agents, and `--tls-cert` with `--tls-key` for any address off
   the loopback
   ([configuration](https://github.com/komira-ai/jobscheduler/blob/main/docs/design/configuration.md),
   "The job service's port").
3. **At least one farm agent** (`js-agent`) registered with that pool, so that a job can be
   placed. The check does not need one (an empty list passes), but every job a frontend submits
   waits in the queue until one is there. Each agent also needs the egress below, or no job of
   kbf's is placed on it.

## The farm agents' egress

The executor submits every job with the network level `--executor-network` names, by default
`allow-list`: a task PUTs its packs to the tier store's proxy and reads its inputs from the
frontend's job-facing read listener, and nothing else. A farm agent runs such a job only if its own
ceiling is `allow-list` (`--egress allow-list`, on `--runtime crun`), and the job reaches only the
endpoints its `--egress-allow` lines name. An agent with the default ceiling, `none`, is never
given kbf's jobs. kbf asks for `open` only when the frontend is given `--executor-network open`
with `--executor-allow-open-network`. An agent that cannot check the limits of its job network
refuses the start (`egress-unchecked`), and the farm runs the action again.

Those lines are made from the frontend's own `kbf.toml`, so that they name the places the frontend
gives its tasks:

```text
kbf-deploy job-service agent-egress --config /etc/kbf/kbf.toml
```

It reads the file and prints the lines; it changes nothing. For a frontend whose `[task-runs]`
says `task-put-endpoint = "http://192.0.2.10:9000"` and
`task-read-url = "https://192.0.2.20:8444"`:

```text
# The egress of kbf's tasks, from /etc/kbf/kbf.toml. Put these lines in /etc/js-agent/flags on
# every agent of the farm (js-agent --runtime crun): a job of kbf's reaches these and
# nothing else.
--egress=allow-list
# task-put-endpoint = http://192.0.2.10:9000
--egress-allow=192.0.2.10:9000
# task-read-url = https://192.0.2.20:8444
--egress-allow=192.0.2.20:8444
```

With `executor-locate = true` and a `job-store-url`, a task also reads straight from that store,
and a third `--egress-allow` names it (without the url a locating task reads from the listener
alone, and no third line is printed). An endpoint named by two keys is printed once. Put the lines in
the flags file of every farm agent (the jobscheduler's
[farm node runbook](https://github.com/komira-ai/jobscheduler/blob/main/docs/runbooks/farm-node-enroll.md)
gives the file's rules). The agent reads the file when it starts, so restart it as that runbook
says. When the frontend's
`task-put-endpoint`, `task-read-url` or `job-store-url` changes, render the lines again and replace
them on every agent.

**Refused** (exit 2, nothing printed; the message names the file and the key):

- `task-put-endpoint` or `task-read-url` is not in the file: the task would have nowhere to
  write its packs or read its inputs;
- a url whose host is not an IPv4 address: a job's network has no DNS, and the agent allows
  IPv4 endpoints only. Give the frontend IP addresses (with certificates that name them);
- an IPv6 address, a port that is not 1 to 65535, or a scheme other than `http` or `https`;
- a loopback, unspecified, link-local (the cloud metadata range), multicast or broadcast address,
  which no job reaches through the host;
- a file that cannot be read, is not TOML, has a key of the wrong type, or has a key in two
  tables.

## kbf's client token

The job service lets a call through only with the bearer token of one of its clients. kbf is one
client of its own, with the role `client`: it reads, lists and cancels only the jobs it submitted,
which is all the executor does. Give it an entry of its own on the service rather than the
`default` token, which is an operator's
([the gateway's clients and roles](https://github.com/komira-ai/jobscheduler/blob/main/docs/http-gateway.md),
"Clients, roles and token reload"):

1. **Make the token** on the job service's host: one token of at least 32 bytes, printable ASCII
   without spaces, in a file owned by the lease server's user with mode 0600, for example
   `/run/credentials/js/kbf-token`.
2. **Add the client** to the lease server's configuration and restart it (adding a client takes a
   restart; a restart ends no job):

   ```toml
   [[client]]
   name       = "kbf"
   token_file = "/run/credentials/js/kbf-token"
   ```

   With flags, the entry is `--client-token kbf=/run/credentials/js/kbf-token`. No `role` line:
   the default role, `client`, is the one kbf needs.
3. **Deliver the token to the frontend's host** over a channel that is already authenticated and
   encrypted (the configuration management that delivers the frontend's other secrets, or `scp`
   between administrators' accounts). Write it to a file owned by the user the frontend runs as,
   with mode 0600 or 0400, for example `/etc/kbf/job-service.token`. The file holds the token and
   at most one trailing newline; kbf's client refuses a file that is not a regular file, has any
   other mode, is over 4 KiB, or holds anything that is not one token. The token is never a
   command-line argument or an environment variable: argv and the environment can be read by other
   users of the host.
4. **Rotating it** later: write the new token into the service's file, and the service refuses the
   old one from its next tick; then write it into the frontend's file. Run the check again after
   both.

## The check

```text
kbf-deploy job-service check --url <URL> --token-file <FILE> [--ca-file <FILE>]
```

| Flag | Meaning |
|---|---|
| `--url <URL>` | the job service: `https://host:port`, the address of the lease server's `--job-listen`. `http://` is accepted for a loopback address only, so the token never crosses a network in clear |
| `--token-file <FILE>` | the file with kbf's client token, as delivered above |
| `--ca-file <FILE>` | the CA certificate (PEM) the service's certificate must chain to. Needed for `https`, refused for `http`. It is the only trust anchor: the system's roots are not used. The certificate must name the host of `--url` |

Run it on the frontend's host, as the user the frontend runs as, so that the address, the
certificate and the token file's permissions are checked as the executor will meet them:

```text
sudo -u kbf kbf-deploy job-service check --url https://jobs.example:8443 \
    --token-file /etc/kbf/job-service.token --ca-file /etc/kbf/job-service-ca.pem
```

It makes one `ListJobs` call, with kbf's client and its rules: the origin, the token file and the
CA are checked by the same code the executor will use. It changes nothing on the service.

**Pass** (exit 0): the service answered. An empty list passes: a service with no job of kbf's yet
is working.

```text
job service: https://jobs.example:8443
token file: /etc/kbf/job-service.token
pass: the job service answered ListJobs (no job of this client yet)
```

After the first jobs the last line says `(jobs of this client listed)`.

**Unreachable** (exit 1): the connection, or its TLS session, could not be made. The message names
the address and the connect failure:

```text
FAIL: unreachable: cannot connect to the job service at https://jobs.example:8443: connecting: Connection refused (os error 111)
```

Check that the lease server runs with `--job-listen` on that address, that a firewall lets the
frontend's host reach the port, and, for a TLS failure (`the TLS handshake: invalid peer
certificate: ...`), that `--ca-file` is the CA of the lease server's `--tls-cert` and that the
certificate names the host of `--url`.

**Unauthorized** (exit 1): the service refused the token. `UNAUTHENTICATED` is a token that is no
client's (a typo, a file not delivered, a client not added, or a server not restarted after it was
added); `PERMISSION_DENIED` is a token the service knows that may not make the call. The service
gives `UNAUTHENTICATED` no reason, so the code is shown alone; a status that has a reason shows it
after the code (`PermissionDenied: <reason>`):

```text
FAIL: unauthorized: the job service at https://jobs.example:8443 refused the token in /etc/kbf/job-service.token (Unauthenticated)
```

**Another answer** (exit 1): the service answered some other error, which the message quotes. An
`UNIMPLEMENTED` answer usually means the address is a port of the lease server that does not serve
the job service, such as its agent and status port (`--listen`; the server has no lease routes
since jobscheduler's release 0.1.218), and not its `--job-listen`.

**Refused** (exit 2, nothing is dialled): a `--url` that is not `http(s)://host[:port]`, or is
`http` off the loopback; a token file that cannot be read or breaks the rules above; an `https`
URL without `--ca-file`, an `http` one with it, or a CA file that cannot be read or holds no
certificate. The message names the flag.

**The token never prints.** The output names the token file by its path, in the pass and in every
failure, and never holds the token itself.

## The plan

```text
kbf-deploy job-service plan --config FILE --url URL --token-file FILE [--ca-file FILE]
                            --hosts N --host-cpus N --host-memory-gib N [--host-slots N]
```

| Flag | Meaning |
|---|---|
| `--config <FILE>` | the frontend's `kbf.toml` |
| `--url`, `--token-file`, `--ca-file` | the job service, as for the check; the token file is named by its path, and `plan` never opens it |
| `--hosts`, `--host-cpus`, `--host-memory-gib` | the virtual workers' number and shape (each at least 1) |
| `--host-slots` | the actions each virtual worker runs at once, 1 to 256, default 8 |

It reads the file and prints, for each key the switch sets, what the file holds now and what the
switch sets; the executor flags of the job-service mode; what the switch keeps from the file;
and the back-out, the flags of the mode the frontend runs now. It changes nothing.

```text
kbf-deploy job-service plan (changes nothing)
config: [executor] in /etc/kbf/kbf.toml
  executor now: kbf-daemon
  executor: absent -> "js-agent"
  executor-job-url: absent -> "https://192.0.2.30:8443"
  executor-job-token-file: absent -> "/etc/kbf/job-service.token"
  executor-job-ca-file: absent -> "/etc/kbf/job-service-ca.pem"
  executor-hosts: absent -> 4
  executor-host-cpus: absent -> 16
  executor-host-memory-gib: absent -> 64
  executor-host-slots: absent -> 8
the executor flags of the job-service mode:
  --executor=js-agent
  --executor-job-url=https://192.0.2.30:8443
  --executor-job-token-file=/etc/kbf/job-service.token
  --executor-job-ca-file=/etc/kbf/job-service-ca.pem
  --executor-hosts=4
  --executor-host-cpus=16
  --executor-host-memory-gib=64
  --executor-host-slots=8
kept from the file (the executor needs them):
  executor-frontend-id = "fe-a"
  executor-base-image = "registry.example/kbf-base@sha256:bbbb...bbbb"
  executor-kbf-image = "registry.example/kbf@sha256:aaaa...aaaa"
  executor-secret-delivery = "files"
back-out: the flags of the mode it runs now:
  --executor=kbf-daemon (the default: the file has no key)
  no --executor-job-url
  no --executor-job-token-file
  no --executor-job-ca-file
  no --executor-hosts
  no --executor-host-cpus
  no --executor-host-memory-gib
  no --executor-host-slots
to switch: kbf-deploy job-service apply with these flags and the restart's (a dry run without --apply)
```

`no --<flag>` is a key the file does not have. Keep the output: its back-out is the way back
if the backup that `apply` keeps is ever lost.

Exit 0 the plan is complete. Exit 1 the file lacks something the executor needs: the plan is
printed with a `MISSING:` line for each, and `FAIL: not ready: ...` names them on stderr. Exit 2
refused, with nothing printed: a URL that is not `http(s)://host[:port]` or is `http` off the
loopback, an `https` URL without `--ca-file` or an `http` one with it, a path that is not
absolute or holds a space, quote, backslash, control character, `%` or `$`, or a file that
cannot be read, is not TOML, or has an `executor` that is not a table.

## The switch

`apply` is a held step: only an operator runs it, at a time they chose, and without `--apply`
it is a dry run that changes nothing. There is no `--force`, and no flag skips a step.

```text
kbf-deploy job-service apply <the flags of plan> <the restart flags>
                             [--canary-deadline-secs SECS] [--apply]
```

The restart flags (`back-out` takes the same):

| Flag | Meaning |
|---|---|
| `--instance <NAME>` | the frontend's unit as `kbf-host-install` names it: `-` for `kbf-frontend.service`, `NAME` for `kbf-frontend@NAME.service` |
| `--api <ADDR>` | the frontend's operator API (its `--api-listen`), on loopback; default `127.0.0.1:8982` |
| `--api-token-file <FILE>` | the operator API's token file, for the drain |
| `--ssh-host <ADDRESS>`, `--ssh-port <PORT>` | the frontend's host as the updater's inventory lists it, and its ssh port when it is not 22 |
| `--ssh-user <USER>`, `--ssh-key-file <FILE>` | the user and key the updater logs in with: the key the forced command of `kbf-host-install` is set up for |
| `--known-hosts <FILE>` | a known-hosts file with the host's key: the only key that is trusted |
| `--kbf <FILE>`, `--ssh <FILE>` | the `kbf` and `ssh` binaries; default `/usr/bin/kbf` and `/usr/bin/ssh` |
| `--poll-secs <SECS>` | the time between two asks of a wait; default 5 |
| `--drain-deadline-secs <SECS>` | how long to wait for the drained frontend to exit. Not given (the default): however long its open executions run, and none is cut |
| `--ready-deadline-secs <SECS>` | how long to wait for the restarted frontend to be ready, and for its first worker; default 300 |
| `--canary-deadline-secs <SECS>` | (`apply` only) how long the canary job may take to end; default 600 |

Run it on the frontend's host, as root or as the user that can read the token files and the
updater's key. With `--apply` it makes these steps in order, printing each first (`+ <step>`),
and stops at the first that fails:

1. **The check**: one `ListJobs` with kbf's client token, exactly as `job-service check`. A
   service that cannot be reached or refuses the token stops the switch here, with the message
   of [The check](#the-check), and nothing has changed.
2. **The new file**: the switched text is written beside the file as
   `kbf.toml.job-service-new`, and the original, byte for byte, as `kbf.toml.pre-job-service`,
   both with the file's mode and owner. `kbf --mode=frontend --config kbf.toml.job-service-new
   --check` must pass on it (its startup checks: a key the executor needs that is missing or
   wrong fails here, with kbf's own message). If it fails, both files are removed, and the
   frontend and its file are as they were.
3. **The switch**: the staged file is renamed over `kbf.toml`. From here on, a failure says
   that the switch is in place and prints the back-out.
4. **The drain and the restart**, the way a roll takes a frontend: `kbf cluster drain
   --frontend` (`POST /v1/frontend/drain`: the frontend goes out of `/readyz`, refuses new
   Execute calls and exits when its open executions are done), a wait until its operator API
   refuses the connection (it has exited), then `restart frontend=<instance>` to the host's
   `kbf-host-install` over ssh, with the updater's key and the updater's own argument vector
   (`ssh -F /dev/null -i KEY -o BatchMode=yes ... -T -- USER@ADDRESS 'restart frontend=NAME'`;
   [host-install.md](host-install.md#what-a-restart-does)). The answer must be `restarted`; a
   `refused` (`busy` while an install runs) or `failed` answer is quoted. A frontend whose
   operator API refuses the connection before the drain is not running: it is not drained
   (`the frontend is not running: nothing to drain`), and the restart starts it. Then a wait
   until `GET /readyz` answers 200.
5. **The first worker**: a wait until `GET /v1/nodes/js:<frontend id>-0` answers 200, which
   it does once the executor of the job service runs and has registered its virtual workers.
6. **The canary** (below).

```text
kbf-deploy job-service apply (apply)
config: [executor] in /etc/kbf/kbf.toml
  ... the plan, as above ...
+ check: ListJobs on https://192.0.2.30:8443 with the token in /etc/kbf/job-service.token
job service: https://192.0.2.30:8443
token file: /etc/kbf/job-service.token
pass: the job service answered ListJobs (no job of this client yet)
+ stage /etc/kbf/kbf.toml.job-service-new and keep the original as /etc/kbf/kbf.toml.pre-job-service
+ /usr/bin/kbf --mode=frontend --config /etc/kbf/kbf.toml.job-service-new --check
+ rename the staged file over the config
+ /usr/bin/kbf cluster --api 127.0.0.1:8982 drain --frontend --token-file /etc/kbf/api-token (unless http://127.0.0.1:8982/healthz refuses the connection)
+ wait until http://127.0.0.1:8982/healthz refuses the connection (the frontend has exited), however long
+ /usr/bin/ssh -F /dev/null -i /var/lib/kbf-updater/id_ed25519 ... -T -- kbf-updater@192.0.2.20 "restart frontend=-" (env PATH, HOME, LANG)
+ wait for http://127.0.0.1:8982/readyz to answer 200 (the frontend is ready), at most 60 asks
+ wait for http://127.0.0.1:8982/v1/nodes/js:fe-a-0 to answer 200 (the executor of the job service registered its first worker), at most 60 asks
+ canary: submit job kbf-canary-1794048581 (registry.example/kbf@sha256:aaaa...aaaa --version, no network, label kbf-canary=fe-a) and ask after it at most 120 times
pass: the canary job kbf-canary-1794048581 succeeded on host farm-7
done: the frontend runs executor = "js-agent"
back-out: kbf-deploy job-service back-out --config /etc/kbf/kbf.toml with the same restart flags (restores /etc/kbf/kbf.toml.pre-job-service and restarts the same way); it returns to:
  --executor=kbf-daemon (the default: the file has no key)
  ...
```

The dry run prints the same plan and each step as `DRY-RUN: <step>`, then `dry run: nothing
changed. Re-run with --apply.` and the back-out. It contacts nothing: not the job service, not
the operator API, not ssh.

Exit 0 done. Exit 1 a step failed (`FAIL: <step's message>` on stderr; after step 3, stdout
also says the switch is in place and prints the back-out). Exit 2 refused before any step:
the refusals of `plan`; a file that lacks a key the executor needs (run `plan`); a file that
already runs `executor = "js-agent"`; a `kbf.toml.pre-job-service` or
`kbf.toml.job-service-new` that is there already (a switch that was not backed out); a file
the switch cannot rewrite line by line (a key written as a dotted or quoted name, a value over
several lines: make the change by hand); an `--instance`, `--ssh-user` or `--ssh-host` that
`kbf-host-install` or ssh would not take; an `--api` off the loopback.

A restart of the frontend fails the builds in flight on it, so the drain waits for them
first; pick a quiet time all the same, as for any restart of a frontend.

## The canary

After the switch, `apply` submits one job to the job service with kbf's client token and asks
after it once per `--poll-secs` until it ends:

- **What it runs**: the frontend's own kbf image (`executor-kbf-image`, without `docker://`),
  whose entrypoint is the task tool, with the one argument `--version`. It needs no grant, no
  pack and no network: it shows that the service takes kbf's token and places a job of the
  executor's kind (`os=linux`, `executor-arch`, containers), and that a farm agent pulls the kbf
  image by its digest and runs it to exit 0.
- **Its shape**: 1 core, 1 GiB of memory and 1 GiB of disk, no network
  (`NETWORK_LEVEL_NONE`), a wall limit of 300 seconds. Its id is `kbf-canary-<seconds since
  the epoch>`, so every run makes a new job.
- **Its label**: `kbf-canary=<frontend id>`, and no `kbf-frontend` label. The executor reads
  `kbf-frontend` to find the jobs of other frontends and stops when it finds one, so a canary
  carrying it would stop the frontend it checks.

A job that `SUCCEEDED` passes (`pass: the canary job <id> succeeded on host <host>`). One that
ended any other way (`failed`, `failed to start`, with the agent's words when it gave some,
`cancelled`, `lost`), or that the service does not know, fails, naming how. One that has not
ended by `--canary-deadline-secs` is cancelled, and the canary fails naming its last state
(`queued` means no farm agent took it: see [What must be running](#what-must-be-running)). A
failed canary leaves the switch in place and prints the back-out: the frontend runs on the job
service, but whether its jobs can run is in doubt until the cause is found.

## Backing out

```text
kbf-deploy job-service back-out --config FILE <the restart flags> [--apply]
```

`back-out` puts back the file `apply` kept and restarts the frontend the same way. With
`--apply`: `kbf --check` on `kbf.toml.pre-job-service`; the backup put back over `kbf.toml`
(its bytes, mode and owner are staged beside the file and renamed over it, so the file is the
original again, byte for byte); the drain (skipped for a frontend that is not running), the wait
for the exit, `restart frontend=<instance>` through `kbf-host-install` and the wait for
`/readyz`; and only then the removal of the backup. A back-out that stops on the way says
`the back-out stopped; ... is kept: run back-out again once the cause is fixed`, and run again
it starts over from the check. Without `--apply` it prints the flags it returns to and its
steps, and changes nothing.

```text
kbf-deploy job-service back-out (dry run)
config: /etc/kbf/kbf.toml.pre-job-service back over /etc/kbf/kbf.toml
  executor now: js-agent, then: kbf-daemon
the flags it returns to:
  --executor=kbf-daemon (the default: the file has no key)
  no --executor-job-url
  ...
DRY-RUN: /usr/bin/kbf --mode=frontend --config /etc/kbf/kbf.toml.pre-job-service --check
DRY-RUN: put /etc/kbf/kbf.toml.pre-job-service back over /etc/kbf/kbf.toml
DRY-RUN: /usr/bin/kbf cluster --api 127.0.0.1:8982 drain --frontend --token-file /etc/kbf/api-token (unless http://127.0.0.1:8982/healthz refuses the connection)
...
DRY-RUN: remove /etc/kbf/kbf.toml.pre-job-service
dry run: nothing changed. Re-run with --apply.
```

It is refused (exit 2) when there is no `kbf.toml.pre-job-service`. The whole file is put back,
not only the executor's keys: a change made to it by hand after the switch is lost, and has to
be made again. Stopping the frontend for the restart cancels any job of its still on the
service; the drain before it has waited for the builds open on it.

Without the backup, the back-out is the flags `plan` printed before the switch: set those keys
back as they were (`no --<flag>`: remove the key), or `executor = "kbf-daemon"`, the default,
with `executor-locate` and `executor-secret-delivery = "files"` removed in the same edit, since
`kbf-daemon` refuses them ([kbf-modes.md](kbf-modes.md) gives the rule); then restart through
the same path. **That flip works only while the release still carries the mode it flips back
to.** A frontend that ran on the embedded engine (the jobscheduler core that was built into
kbf, `--scheduler-core js`, which this release refuses at start) or on the lease-based
`js-agent` executor can be flipped back to it only with a release that has it; once a release without it is installed, the way back to it is the
previous kbf release, not a flag.

To take kbf off the service altogether, back the frontend out, then remove kbf's `[[client]]`
entry, restart the lease server, and delete the token file on the frontend's host.

## Related pages

- [kbf-modes.md](kbf-modes.md): the modes, the executors (`--executor`) and the frontend's own
  job-facing read listener.
- [state-store-cutover.md](state-store-cutover.md): the other cutover of a frontend, its state onto
  Postgres.
- [kbf-updater.md](kbf-updater.md): the roll, whose drain and `kbf-host-install` restart the
  switch and its back-out use.
- [host-install.md](host-install.md): `kbf-host-install`'s forced command, its `restart` request
  and its answers.
