# kbf-all-in-one

The farm server and a worker daemon in one process, for a one-node Linux farm. The
daemon's blobs move through calls into the server's cache instead of over the network;
everything else is what `kbf-server` and `kbf-daemon` do on one host today. The design
is in komira-build-farm#657.

Linux only, the container driver only. Elsewhere the binary exits 2 and says to run
`kbf-server` and `kbf-daemon`.

## What the binary does today

1. **The helpers first.** Before any flag is read, a run of this binary as one of the
   daemon's helpers (each run's keeper, the container driver's `__chown-dirs`) does that
   and exits, as `kbf-daemon` does. So a helper never reads the command line, opens the
   server's directories or starts a runtime.
2. **One command line.** Every flag of `kbf-daemon` and every flag of `kbf-server`, in one
   command. The three thread overrides that both binaries have (`--async-threads`,
   `--blocking-threads`, `--cpu-threads`) size the daemon half, with `--reserve-cpus`; the
   server half sizes itself as `kbf-server` does when none is given. Refused: a
   `--driver` other than `container`, and `--cas`.
3. **The server half.** It starts as `kbf-server` starts (`kbf_server::start`), on an
   async runtime of its own, and prints the same start line on stdout.
4. **The daemon half.** It starts as `kbf-daemon` starts (`kbf_node::start_with`), on an
   async runtime of its own, with `LocalDataPlane` as its data plane in place of the
   transfer engine. Its control plane is a remote daemon's: `--server` names the server
   half's worker listener, over mutual TLS with the node's certificate, as any node's.
5. **Stopping.** SIGTERM or SIGINT stops the server half as it stops `kbf-server`; the
   daemon half stops once the server half has, abandoning the leases still running, and
   the process exits 0. A server half that stops serving on its own stops the daemon too,
   and the process exits 1. A start the server half refuses exits 2, as `kbf-server`
   does; one the daemon half refuses exits as `kbf-daemon` does (1, or 2 for an
   `--image` the node cannot run).

## The in-process data plane

`LocalDataPlane` implements `kbf_transfer::Cas` over the server's `LocalStore`
(`ServerHandle::local`). Every call runs on the server half's runtime, through the
server's own code:

- **Outputs, stdout and stderr** (`put_records`): the driver's walk has hashed each file
  once. The plane asks the cache which digests it lacks, packs those records into the
  segments and uploads the transfer engine plans (`kbf_transfer::plan`), and gives the
  `Segments` handlers each range of their bytes, read from where the walk left them:
  `Begin`, `PutRange`, `Commit`, as for a remote daemon, with no TLS, encoding or socket.
  Nothing is spooled and nothing is hashed on the way. With a write tier, `Commit` trusts
  the digests and the flusher hashes every record before its PUT, dropping one that does
  not match; without one, `PutRange` hashes each record as its bytes arrive.
- **A whole blob** (`put`, and `put_chunks` through it): hashed once, by the server,
  under the admission a ByteStream `Write` takes.
- **Reads** (`get`, and `read_to` through it) and **FindMissing** (`missing`): the cache's
  own, which checks every blob it reads against its digest.

No message a remote daemon sends changes: `kbf-daemon` still uses the transfer engine.

## How it is tested

- `tests/conformance.rs`: every check of `kbf-cas-conformance` against `LocalDataPlane`,
  over a server started as `kbf-server` starts, with a write tier and without. The
  transfer engine runs the same checks in `kbf-cas-conformance`'s own tests.
- `tests/binary.rs`: the binary end to end. One action runs on the container driver over
  the driver's own stand-in `podman` (`kbf-fake-podman`, built from
  `kbf-driver-container`'s source, which runs the action as a plain process) and a cgroup
  tree of plain directories; its output file, output directory, stdout and stderr are
  read back over REAPI; SIGTERM ends the process with 0. The binary is given no `--cas`,
  so a daemon half wired to the transfer engine would not start. Also the starts each
  half refuses, with their exit statuses, and the binary run with keeper arguments,
  which answers as the keeper before it reads a flag and starts no server.
- `src/plane_tests.rs`: a digest that does not parse, a server half whose runtime is
  gone, a file changed after the walk hashed it.
- `src/cli_tests.rs`: each flag reaches its half, once; the refusals.

## Not done yet

- **Production use.** The server half's scheduler state is in memory, so a restart of
  the process loses every lease and operation, and with the default `--meta-log=memory`
  the cache's index too (`--blob-dir` with `--meta-log=raft` keeps the cache). Until the
  scheduler's state is durable, all-in-one is a development shape.
- **The in-process plane's admission.** A remote daemon's blob calls are admitted by its
  certificate and the deny list; this plane's calls are not admitted.
- **The walk's buffers.** A file the walk hashed is read again from the file to be
  stored (never hashed again); handing the walk's own buffer to the plane is a driver
  change not made here.
- **`--reserve-cpus` across both halves.** The daemon half takes the flag as
  `kbf-daemon` does; the server half uses every CPU, as `kbf-server` does.
- **Crash and panic behaviour** (the next change): the whole-process crash mode of the
  simulation, a test that a panic in a driver task leaves the process serving, and the
  rest of the helper-dispatch tests (each helper mode runs and exits with its code, with
  no listener bound and no server directory opened).
- **Deploying it** (the change after): the `all-in-one` shape in `kbf-deploy`, its unit
  and `check-units` rules, the ownership step that moves the state directories and keys
  between the split pair and this binary, and its place among the release artifacts.
