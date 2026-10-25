# tierstore: an example unit

`tierstore` (`crates/tierstore`) is an S3 server over a hot and a cold tier of S3-compatible
stores: it writes new objects to the hot tier, copies them to the cold tier in the background, and
frees the hot tier when it fills. It is a release asset for Linux only, as a tarball with its
SBOM and its provenance bundle (`docs/artifacts.md`); there is no package and no container image.
These files go beside the binary.

| file | what |
|---|---|
| `tierstore.service` | the unit: the binary as `tierstore`, an account that is not root, a two-tier S3 setup, a stop that waits for the drain, and the hardening of the other example units |
| `tierstore.conf` | the sysusers.d entry of the `tierstore` account (no login, no home) |

The values in the unit are examples (`192.0.2.10`, `hot-s3.farm.example`, the bucket names, the
sizes): edit them to the setup's. `crates/tierstore/tests/packaging.rs` reads these files and
checks that the command line parses as the binary parses it, that no secret is on it, that the
stop timeout covers the drain, and that nothing starts the unit by itself.

## Placing the binary

The release carries `tierstore-<commit>-linux-x86_64.tar.gz` and
`tierstore-<commit>-linux-arm64.tar.gz`, each with `<tarball>.sigstore.json` and listed in
`SHA256SUMS`. Take the tarball of the host's architecture by its digest, and verify it as every
tarball is verified (`docs/artifacts.md`, "Provenance bundles: verifying a tarball offline").
The tarball holds one member, the binary under its own name:

```sh
tar -xzf tierstore-<commit>-linux-x86_64.tar.gz tierstore
install -d -m 0755 /usr/libexec/kbf
install -m 0755 -o root -g root tierstore /usr/libexec/kbf/tierstore
```

Nothing updates it afterwards. The host updater (`kbf-updater`, `kbf-host-install`) does not
manage `tierstore` yet: whoever runs the setup places the next binary the same way and restarts
the unit.

## Placing the unit

```sh
install -m 0644 tierstore.conf /etc/sysusers.d/tierstore.conf
systemd-sysusers /etc/sysusers.d/tierstore.conf
install -d -m 0750 -o root -g tierstore /etc/tierstore
install -m 0644 tierstore.service /etc/systemd/system/tierstore.service
systemctl daemon-reload
```

Then write the three key files, each owned by `tierstore` with mode 0600 (the process refuses a
key file that others can read). Each holds the access key id on its first line and the secret
access key on its second:

- `/etc/tierstore/access.key`: the key clients sign their requests to `tierstore` with
  (`--access-key-file`);
- `/etc/tierstore/hot.credentials` and `/etc/tierstore/cold.credentials`: the keys `tierstore`
  signs its requests to the two stores with (`--hot-credentials-file`,
  `--cold-credentials-file`). The two tiers may be two stores, and have a key each.

A secret is never on the command line or in the environment: the process reads no environment
variable, and the unit sets none.

## Filling in the flags

The unit's `ExecStart=` is a complete command line. The flags to edit:

| flag | what |
|---|---|
| `--listen` | the address the S3 API, `/healthz` and `/readyz` are served on |
| `--proxy-id` | names this process's journal and its lease in the hot tier; keep it across restarts, and give each process over the same tiers one of its own |
| `--bucket` | the name of the one bucket clients see |
| `--hot`, `--cold` | the tiers: `s3:<bucket>` for a bucket of an S3-compatible store (also `dir:<path>` and `memory`; a `dir:` path must also be writable under `ProtectSystem=strict`, so add it to `ReadWritePaths=`) |
| `--hot-endpoint`, `--cold-endpoint` | where the tier's store is, `http://host[:port]` (TLS is not built yet); `--hot-region` and `--cold-region` set the signing region of a tier, `us-east-1` if not given |
| `--hot-bytes` | the bytes the hot tier is meant to hold; the water marks are shares of it (`--high-water`, `--low-water`, `--critical`) |
| `--drain-lb-wait`, `--drain-deadline` | the drain on SIGTERM: how long `/readyz` says 503 while the server goes on serving, and how long it then waits for requests in flight |

`tierstore --help` lists them all, with their defaults.

## Starting it

The unit does not start at install and does not start at boot: it has no `[Install]` section,
so `systemctl enable` has nothing to link, and it is skipped while `/etc/tierstore/access.key`
does not exist. Start it by hand when the files are in place:

```sh
systemctl start tierstore
```

To start it at boot, add the section in a drop-in (`systemctl edit tierstore` takes
`[Install]` and `WantedBy=multi-user.target`) and run `systemctl enable tierstore`; that is
the operator's choice, and the shipped file does not make it.

## Stopping it

systemd sends SIGTERM. The process answers `/readyz` 503, serves for `--drain-lb-wait` so that a
load balancer sees it, refuses new requests, waits for those in flight for at most
`--drain-deadline`, gives up the reconciler's lease and exits 0. `TimeoutStopSec=` is set above
the sum of the two flags and the lease's wait, so that systemd does not send SIGKILL to a drain
in progress; if `--drain-deadline` is raised, raise it too. A start that fails is tried again
after five seconds; exit status 2 (a command line the argument parser refused: an unknown flag, a
missing or malformed value) is not, since a restart cannot mend it. Flag combinations and key
files refused after parsing exit 1, and are tried again every five seconds.
