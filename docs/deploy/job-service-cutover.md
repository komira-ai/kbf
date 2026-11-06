# Moving a frontend's executor onto the job service: what must be in place, and the check

The job service is the jobscheduler's `jobscheduler.v1.JobService`, served by its lease server
(`js-lease-server`) on the port of that server's `--job-listen`. The design makes kbf a client of
it: a frontend's executor will submit each action as a job and read the job's result, instead of
leasing hosts. This page covers what has to be running before a frontend is pointed at the
service, how kbf's client token gets to the frontend's host, the read-only check that the service
answers that token, and the way back.

**This is not kbf's own `--job-listen`.** A kbf frontend also has a flag of that name: the
frontend's job-facing read listener, from which a task fetches an action's inputs with a read
grant ([kbf-modes.md](kbf-modes.md), "The job-facing read listener"). That listener is part of
kbf and is not an executor mode. The job service on this page is another process, on another
host or port, reached with the jobscheduler lease server's `--job-listen`.

## Not covered yet

This page and `kbf-deploy job-service check` are the first part of the cutover. Three parts are
not written yet, because the executor that submits jobs is not in a release yet:

- **The executor's flags**: rendering the flags that point a frontend's executor at the job
  service (its address, the token file, the shape of the virtual workers).
- **`apply`**: switching a frontend onto the service through the update path.
- **The canary**: one action run through the job service, checked to complete, after the switch.

Until they are, nothing on this page changes a frontend: the check only reads.

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
   waits in the queue until one is there.

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
the job service, such as its lease port (`--listen`), and not its `--job-listen`.

**Refused** (exit 2, nothing is dialled): a `--url` that is not `http(s)://host[:port]`, or is
`http` off the loopback; a token file that cannot be read or breaks the rules above; an `https`
URL without `--ca-file`, an `http` one with it, or a CA file that cannot be read or holds no
certificate. The message names the flag.

**The token never prints.** The output names the token file by its path, in the pass and in every
failure, and never holds the token itself.

## Backing out

Until the executor that submits jobs is released, there is nothing to back out: the check changes
nothing, and a frontend runs as it did. To take kbf off the service altogether, remove its
`[[client]]` entry and restart the lease server, and delete the token file on the frontend's host.

After a frontend has been switched onto the service, the way back is to set its executor back to the
mode it ran before, through the same update path: its `--executor` and the flags of that mode as
they were, or `executor = "kbf-daemon"`, the default, with `--executor-locate` removed in the same
edit ([kbf-modes.md](kbf-modes.md) gives the rule). **That flag flip works only while the release
still carries the mode it flips back to.** A frontend that ran on the embedded engine (the
jobscheduler core built into kbf, `--scheduler-core js`) or on the lease-based `js-agent` executor
can be flipped back to it only with a release that has it; once a release without them is
installed, the way back to them is the previous kbf release, not a flag. The steps for the switch
and for its back-out are written with the switch itself (see "Not covered yet").

## Related pages

- [kbf-modes.md](kbf-modes.md): the modes, the executors (`--executor`) and the frontend's own
  job-facing read listener.
- [state-store-cutover.md](state-store-cutover.md): the other cutover of a frontend, its state onto
  Postgres.
- [kbf-updater.md](kbf-updater.md): the update path a switch goes through.
