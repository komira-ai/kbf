# Storage nodes: the server-only and converged profiles

How to run `kbf-server` on a storage host: the systemd units, the priorities between
the processes that share the host, and, on a host that also runs builds, how much
memory builds may use. Where servers run and why is in
[deployment-topology.md](../design/deployment-topology.md); a host that runs builds
only is [linux-build-host.md](linux-build-host.md).

## Profiles

| Profile | Runs | Units on this page |
|---|---|---|
| server-only | a node of the object store (MinIO here) and `kbf-server` | [server-only units](#server-only-units) |
| converged | the same, and a `kbf-daemon` with the container driver ([converged topology](../design/deployment-topology.md#converged-topology)) | the server-only units, with the slice and daemon of the [converged units](#converged-units) |
| build-only | a `kbf-daemon` only | [linux-build-host.md](linux-build-host.md) |

## Priorities

When the host runs short, the object store goes last, then `kbf-server`, then the
daemon; builds go first. The object store holds every blob and the server holds the
farm's state, so losing either stops every build, while a build that is killed runs
again. Each unit says so in four settings:

| Unit | `OOMScoreAdjust=` | `MemoryMin=` | `CPUWeight=` | `MemorySwapMax=` |
|---|---|---|---|---|
| MinIO | -950 | 32G | 400 | 0 |
| `kbf-server` | -925 | 16G | 300 | 0 |
| `kbf-daemon` | -900 | 256M | 200 | not set |
| build containers | 0 (the driver's `--oom-score-adj=0`) | none | each lease's `cpu.weight`, inside the daemon's | not set |

- **`OOMScoreAdjust=`**: under a host-wide OOM the kernel kills the process with the
  highest score, so the lowest adjustment goes to what must survive longest.
- **`MemoryMin=`**: memory the kernel does not reclaim from the unit while it uses no
  more. The kernel caps it at what the unit's slice protects, so all three units sit
  in `kbf.slice`, whose `MemoryMin=` is the sum of theirs (see
  [linux-build-host.md](linux-build-host.md#memory-on-the-host) for why it is equal,
  not more). The figures are this page's example; size MinIO's and the server's to
  what they use at their peak on your host.
- **`CPUWeight=`**: how CPU is shared between the units of `kbf.slice` when it is
  contended. Nothing is capped: CPU a unit leaves idle goes to the others, builds
  included.
- **`MemorySwapMax=0`**: the object store's and the server's pages stay in RAM. A
  converged host has a large swapfile for builds; a server answering from swap would
  slow every build at once.

## Memory on a converged host

On a converged host, builds get what is left after the object store, the server and
the OS:

```text
--actions-memory-max-gib = RAM - (MinIO's reservation + kbf-server's reservation + 4 to 8 GiB for the OS)
```

The 4 to 8 GiB is the same headroom as on a build host
([linux-build-host.md](linux-build-host.md#headroom---actions-memory-max-gib)): the
kernel, systemd, sshd, the daemon itself and Podman's processes. A reservation is what
the process uses at its peak, and the same figure as its `MemoryMin=` above. The daemon
writes the result to `actions/memory.max` and reports the lower of it and the host's
memory as `mem_gib`, so the scheduler books no more than builds may use.

### Worked example: a 256 GiB host

| Part | GiB |
|---|---|
| RAM | 256 |
| minus MinIO's reservation | 32 |
| minus `kbf-server`'s reservation | 16 |
| minus the OS, systemd, sshd, the daemon and Podman | 8 |
| `--actions-memory-max-gib` | 200 |

## Server-only units

`/etc/systemd/system/kbf.slice`:

```ini
[Unit]
Description=kbf build farm

[Slice]
# The sum of its units' MemoryMin=: MinIO 32G + kbf-server 16G.
MemoryMin=48G
```

MinIO's own unit stays as its installation wrote it; a drop-in puts it in the slice
with its priorities.

`/etc/systemd/system/minio.service.d/kbf.conf`:

```ini
[Service]
Slice=kbf.slice
MemoryMin=32G
MemorySwapMax=0
OOMScoreAdjust=-950
CPUWeight=400
```

`/etc/systemd/system/kbf-server.service`:

```ini
[Unit]
Description=kbf build farm server
Wants=network-online.target
After=network-online.target minio.service

[Service]
# Not the daemon's user: builds must not read the operator API token.
User=kbf-server
Slice=kbf.slice
MemoryMin=16G
MemorySwapMax=0
OOMScoreAdjust=-925
CPUWeight=300
# AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY for the bucket, mode 0600: a secret
# never goes on the command line.
EnvironmentFile=/etc/kbf/s3.env
ExecStart=/usr/local/bin/kbf-server \
    --store=s3 \
    --s3-endpoint=http://127.0.0.1:9000 \
    --s3-bucket=kbf \
    --s3-conditional-put \
    --listen=<storage host address>:8980 \
    --reapi-tls-cert=/etc/kbf/reapi.pem \
    --reapi-tls-key=/etc/kbf/reapi.key \
    --worker-listen=<storage host address>:8981 \
    --worker-tls-cert=/etc/kbf/worker.pem \
    --worker-tls-key=/etc/kbf/worker.key \
    --worker-client-ca=/etc/kbf/ca.pem \
    --worker-deny-list=/etc/kbf/deny-list \
    --api-listen=127.0.0.1:8982 \
    --api-token-file=/etc/kbf/api-token
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

What each group of flags does:

- **The store.** Blob bytes go to the bucket through the host's own MinIO node. The
  index is still in memory, so a restart forgets every blob, and each start writes
  under a fresh key prefix.
- **The REAPI listener** serves TLS with a certificate from the farm's internal CA, for
  the front's proxy on another host and for clients that trust that CA. A server whose
  proxy runs on the same host may bind `--listen` to loopback and leave out the two TLS
  flags. Without `--reapi-auth-policy` every call is accepted ([reapi-auth.md](../reapi-auth.md)).
  The proxy health-checks the server with `grpc.health.v1` on this listener
  ([api.md](../api.md#grpchealthv1-on-the-reapi-listener)).
- **The worker listener** serves daemons over mutual TLS: their certificates must
  chain to `--worker-client-ca`, and the deny list is read again at every `Hello` and
  `Heartbeat` ([worker-protocol.md](../design/worker-protocol.md)).
- **The operator API** (`/v1`, `/healthz`, `/readyz`) answers reads to whoever reaches
  it, so it is bound to loopback; writes need the token in `--api-token-file`, owned
  by `kbf-server` with mode 0600 or 0400 ([api.md](../api.md)).

Then:

```sh
sudo systemctl daemon-reload
sudo systemctl restart minio.service
sudo systemctl enable --now kbf-server.service
```

## Converged units

A converged host runs the server-only units above with this slice in place of theirs,
and a daemon set up as on a build host
([linux-build-host.md](linux-build-host.md): rootless Podman, the `kbf` user, swap),
in the same slice, with this unit.

`/etc/systemd/system/kbf.slice`:

```ini
[Unit]
Description=kbf build farm

[Slice]
# The sum of its units' MemoryMin=: MinIO 32G + kbf-server 16G + kbf-daemon 256M.
MemoryMin=49408M
```

`/etc/systemd/system/kbf-daemon.service`:

```ini
[Unit]
Description=kbf build farm node daemon
Wants=network-online.target
After=network-online.target kbf-server.service

[Service]
User=kbf
Slice=kbf.slice
Delegate=yes
# Equal to --supervisor-memory-min-mib.
MemoryMin=256M
OOMScoreAdjust=-900
CPUWeight=200
Environment=XDG_RUNTIME_DIR=/run/user/<kbf uid>
ExecStart=/usr/local/bin/kbf-daemon \
    --driver=container \
    --server=https://<storage host name>:8981 \
    --cas=https://<storage host name>:8981 \
    --ca-cert=/etc/kbf/ca.pem \
    --cert=/etc/kbf/node.pem \
    --key=/etc/kbf/node.key \
    --node-id=<node id> \
    --scratch=/var/lib/kbf/leases \
    --actions-memory-max-gib=200 \
    --supervisor-memory-min-mib=256
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

`--actions-memory-max-gib` is the [worked example](#worked-example-a-256-gib-host)'s
result; on another host, compute it from that host's RAM. The daemon's other settings
and what it logs at start are as on a build host
([linux-build-host.md](linux-build-host.md#the-units)).

## One host without MinIO: `--blob-dir`

For a development farm on one host, `kbf-server` can keep blob bytes in a directory on
the local disk instead of a bucket, and the index of what it holds (the CAS and the
action cache) in the Raft log, so a restart of the server keeps both: a build after the
restart gets the cache hits it got before.

```sh
kbf-server \
    --blob-dir=/var/lib/kbf-server/blobs \
    --meta-log=raft \
    --raft-dir=/var/lib/kbf-server/raft \
    --listen=127.0.0.1:8980 \
    --worker-listen=127.0.0.1:8981
```

- `--blob-dir` takes the place of `--store`: with `--store` or any `--s3-*` flag
  (`--s3-prefix` included) the server refuses to start. It needs `--meta-log=raft`;
  with the index in memory a restart would forget every blob while their files
  stayed, with nothing to delete them.
- The directory is created if missing and locked while the server runs: a second
  server on it exits 2. Objects are keyed under the fixed prefix `kbf/<log id>/`, one
  file each in `objects/` (layout in
  [storage.md](../design/storage.md#the-object-store-interface)).
- A write is synced, file and directory, before it is answered, so an answered upload
  survives a crash or a power cut. A crash part-way through a write leaves a file in
  `tmp/`, removed at the next start.
- The start-up pass: once the server serves (it never waits for the pass to bind or
  to report ready), a background task checks the directory against the index. It
  walks the 256 shards of `objects/` reading each file's header (its key, length and
  modification time, never its bytes) at about 1000 files a second, or as fast as it
  can while the disk is under the free-space floor, and then:
  - keeps each file of an object the index names (staged on the write tier
    included) or of this process's own writer epoch;
  - deletes each file of an earlier epoch under this log's prefix that the index does
    not name: an orphan, such as an upload that failed before its commit;
  - keeps and counts a file under another prefix, and leaves a file without a header;
  - then forgets each indexed object, not staged, that has no file: its blobs leave
    the index, with every action-cache entry that needs one of them, so
    `FindMissingBlobs` reports them and the client uploads them again. Before it
    forgets the first, it raises the Raft log to machine version 3, which a server
    binary older than this one refuses.
  - The guard: it forgets nothing unless the walk found the file of at least one
    indexed object. A directory with none is not the one the index was written over
    (a disk that is not mounted: the server creates the directory empty), and the
    pass logs an error instead of emptying the index.

  It logs one line at the end: files read, kept, orphans deleted, foreign, without a
  header, objects, blobs and entries forgotten, objects held back by the guard, the
  bytes held and how long it took. A pass that fails is logged and runs again at the
  next start.
- Until the pass reaches it, or for a file lost while the server runs, a blob whose
  file was lost, damaged or left empty is still reported present by
  `FindMissingBlobs` until a read of it fails: that read answers `UNAVAILABLE` and
  marks its object unreachable, so the blob is reported missing and the client
  uploads it again, which heals it. The pass reads headers only, so a damaged object
  is found by a read. Before an action-cache hit is served, the server checks that
  the file of every object the hit needs is there and starts with its header; if one
  is not, the hit is a miss (the action runs again) and its blobs are reported
  missing.
- No eviction: only the start-up pass deletes objects (orphans), so the directory
  grows with every blob the cache keeps. A write that would leave the file system
  with less than `--blob-dir-min-free-bytes` free (default 1 GiB) is refused, and its
  upload answered `UNAVAILABLE`. Size the disk for the cache you want to keep.

## Planned: the Raft log on local disk

`kbf-server` keeps its metadata in memory by default
([deployment-topology.md](../design/deployment-topology.md#built-and-planned)).
`--meta-log=raft --raft-dir=/var/lib/kbf-server/raft` is accepted (with `--store=s3`, or `--blob-dir` [above](#one-host-without-minio---blob-dir))
but is not in the unit above yet: until the scheduler's state is in the log too, a
restart over it keeps the daemons' epoch but not the server's leases, so the old runs
go on as orphans whose results are refused. This flag is planned and not accepted yet;
the unit gains all three once both are built:

```text planned-flags kbf-server
--runtime-dir=/run/kbf
```

- `--meta-log=raft` and `--raft-dir`: the server's state applied from a single-voter
  Raft log kept in the directory. Mount that directory on a partition of its own, not
  shared with build scratch, swap or the object store's data, so a full build disk
  cannot stop the log
  ([converged topology](../design/deployment-topology.md#converged-topology)).
- `--runtime-dir`: where the log's store records that a write to disk failed, so it
  does not reopen the log until the host reboots. `/run` is a tmpfs, cleared at reboot
  as the page cache is.

## Rendering the converged units

`kbf-deploy linux render-units` writes the converged host's four files from templates
compiled into the binary (`crates/kbf-deploy/templates/linux/`) and a settings file
(`crates/kbf-deploy/templates/linux/config.example` documents each key; an unknown or
repeated key is refused). `kbf-deploy linux check-units` then checks them against the
[priorities](#priorities) and the memory split, one `FAIL <rule>: <reason>` line per
broken rule:

```text
kbf-deploy linux render-units --config node.conf --out units --kbf-uid 1500
kbf-deploy linux check-units --config node.conf --dir units --installable
```

`--out` is laid out as `/etc/systemd/system`: `kbf.slice`, `<MINIO_UNIT>.d/kbf.conf`,
`kbf-server.service`, `kbf-daemon.service`. Rendering writes nothing else, so it needs
no `--apply`. A template placeholder (`@KEY@`) with no value is refused rather than
written, and `check-units` fails a file that still holds one; without `--kbf-uid` the
daemon's unit keeps `<kbf uid>`, which `--installable` fails. Exit codes: 0 rendered or
every rule holds, 1 a rule is broken, 2 refused (a bad setting or an unreadable file),
with a `REFUSED:` line on stderr.

## Installing on the host

Three verbs change the host. Each runs as root, reads the host first, prints its plan
as `DRY-RUN:` lines and changes nothing; `--apply` carries the plan out, stopping at
the first step that fails. A re-run after an apply plans nothing. Exit codes: 0 done
(or nothing to do), 1 a step or the check after an apply failed, 2 refused
(`REFUSED:` on stderr, with the fix). They take the same `--config` file as
`render-units`.

```text
sudo kbf-deploy linux swap --config node.conf [--kubelet-allows-swap] [--remove] [--apply]
sudo kbf-deploy linux image-pull --config node.conf [--image REF]... [--apply]
sudo kbf-deploy linux install-units --config node.conf [--bin-dir DIR] [--restart-minio] [--apply]
sudo kbf-deploy linux install-units --config node.conf --uninstall [--apply]
```

- `swap` makes `KBF_SWAP_SIZE` at `KBF_SWAP_FILE` (`fallocate`, `chmod 600`, `mkswap`,
  on ext4 or xfs with 16 GiB to spare), turns it on and adds its `/etc/fstab` line,
  each only if the host lacks it. A file of another size is refused, never resized. It
  refuses while a kubelet (`kubelet`, `k3s`, `k3s-agent`, `rke2-server`, `rke2-agent`)
  is active, since kubelet's default `failSwapOn` would take the node out of its
  cluster at its next restart; `--kubelet-allows-swap` says its config sets
  `failSwapOn: false`. `--remove` turns it off, drops the line and removes the file.
- `image-pull` pulls each of `KBF_IMAGES` (or `--image`) missing from the kbf user's
  rootless Podman store, as that user: its uid, its gid and all its groups (as
  `sudo -u` gives them), `HOME` and `XDG_RUNTIME_DIR=/run/user/<uid>`. Each image in
  the store must have its pinned digest and be `--platform` (`linux/amd64`); after an
  apply each pulled image is checked the same way, and one still missing or not the
  pin exits 1 with a `FAIL` line (the pulls ran, so it is not a refusal). Even a dry
  run needs root, since it reads the store as the kbf user.
- `install-units` checks the host (a cgroup v2 memory controller, `MINIO_UNIT` is a
  systemd unit, both users, the kbf user's subuid and subgid ranges, each a line
  keyed by its name or its uid, and linger, each key file in `KBF_ETC` its service's
  alone, the certificates, the deny list and `s3.env`'s key pair, and the three ports
  free unless kbf-server runs), renders the units with the kbf user's uid and refuses
  them unless `check-units --installable` passes, and refuses a binary whose `--help`
  lacks a flag its unit passes. It then installs, each only if it differs: the
  binaries from `--bin-dir` (keeping the replaced one as `<name>.previous`), the four
  files under `/etc/systemd/system`, and the Raft and scratch directories (0700, their
  service's user). The binaries, the `.previous` copies and the four files are root's
  (uid and gid 0): one with the right bytes and mode that another user owns is
  replaced. After a change it runs `daemon-reload` and enables and restarts
  kbf-server and kbf-daemon; otherwise it only enables or starts one that is not.
  After any start or restart it waits up to 60 s for kbf-server's `/readyz`; a run
  that starts nothing does not wait. MinIO restarts only with `--restart-minio`: its
  new priorities take effect at its next start. `--uninstall` stops and disables both
  services when either is enabled or running, and moves the four files to
  `/var/lib/kbf-linux/removed/<unix seconds>/`; the binaries, the Raft log, the
  scratch, the bucket and the swapfile stay.

## What the tests check

- `crates/kbf-it/tests/deploy_units.rs` runs `kbf-server`'s flag parser over the
  `ExecStart=` of every server unit in `docs/deploy/`, checks that the planned flag
  above is still refused, that the worked example adds up and matches the converged
  daemon's `--actions-memory-max-gib`, and that the units keep the
  [priority](#priorities) order.
- `kbf-node`'s tests (`src/deploy_doc_tests.rs`) run `kbf-daemon`'s flag parser over
  every daemon unit in `docs/deploy/`.
- `kbf-deploy`'s tests (`src/linux/tests.rs`) plant one defect per rule in the
  templates (a swap limit dropped, an OOM score at the containers' 0, a later
  assignment that breaks the order, a slice minimum that is not the sum, a key on the
  command line, a bare `--image`) and require that rule's `FAIL` line; they
  also refuse each bad setting and a placeholder with no value. The host verbs'
  tests (`src/linux/install_tests.rs`, `images_tests.rs`, `swap_tests.rs`) apply each
  plan into a scratch directory with the tools played by a fake, check what landed,
  and require a re-run to plan nothing; each missing prerequisite is a row that must
  be refused with its own refusal. `src/user_tests.rs` runs `id` as another user
  through `sudo` (CI has passwordless sudo) and requires that user's uid, gid and
  supplementary groups in the child. The rendered units'
  `ExecStart=` lines go through `kbf-server`'s parser (`kbf-it`) and `kbf-daemon`'s
  (`kbf-node`) too.
