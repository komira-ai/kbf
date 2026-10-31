# Install

tierstore ships as a Linux tarball attached to every kbf release, with its SBOM and a provenance bundle. There is no package and no container image, and nothing updates the binary afterwards: to move to a new release, place the new binary the same way and restart the unit.

## Take and verify the tarball

A release lists `tierstore-<commit>-linux-x86_64.tar.gz` and `tierstore-<commit>-linux-arm64.tar.gz`, each with a `.sigstore.json` bundle, and all of them in `SHA256SUMS`. Take the tarball of the host's architecture by its digest and verify it as every kbf tarball is verified: [docs/artifacts.md](https://github.com/komira-ai/kbf/blob/main/docs/artifacts.md), "Provenance bundles: verifying a tarball offline".

## Place the binary

The tarball holds one member, the binary under its own name.

```sh
tar -xzf tierstore-<commit>-linux-x86_64.tar.gz tierstore
install -d -m 0755 /usr/libexec/kbf
install -m 0755 -o root -g root tierstore /usr/libexec/kbf/tierstore
```

## Create the account and the directories

The example unit runs as a `tierstore` account with no login and no home. The sysusers entry and the unit are in `packaging/tierstore` of the repository.

```sh
install -m 0644 tierstore.conf /etc/sysusers.d/tierstore.conf
systemd-sysusers /etc/sysusers.d/tierstore.conf
install -d -m 0750 -o root -g tierstore /etc/tierstore
install -m 0644 tierstore.service /etc/systemd/system/tierstore.service
systemctl daemon-reload
```

`/etc/tierstore` holds the key files and, if you use one, the configuration file. If a tier is a directory (`dir:<path>`), create it, give it to the `tierstore` account, and add its path to `ReadWritePaths=` in the unit: the unit runs with `ProtectSystem=strict`, so the process cannot write anywhere else. Put the hot tier's directory on the SSD and the cold tier's on the spinning disks.

```sh
install -d -m 0700 -o tierstore -g tierstore /srv/tierstore/hot
install -d -m 0700 -o tierstore -g tierstore /srv/tierstore/cold
```

## Write the key files

Write each key file owned by `tierstore` with mode 0600 or 0400. [Keys and roles](keys.md) says which files you need and what each may do. A start with a key file that its group or others can access is refused.

## Edit the unit

The values in the example unit are examples: `192.0.2.10` is a documentation address, the endpoints are `s3-hot.example` and `s3-cold.example`. Edit the listen address, the proxy id, the bucket names, the tiers and `--hot-bytes`. Keep the proxy id across restarts and give each process over the same tiers one of its own. [Configuration](configuration.md) lists every flag.

The unit has no `[Install]` section, so installing it starts nothing and `systemctl enable` has nothing to link; it is also skipped while `/etc/tierstore/access.key` does not exist. Start it by hand once the files are in place:

```sh
systemctl start tierstore
```

To start it at boot, add the section in a drop-in (`systemctl edit tierstore`, with `[Install]` and `WantedBy=multi-user.target`) and run `systemctl enable tierstore`.

## Restarts and the unit's exit rules

The unit restarts a failed start after 30 seconds. It does not restart exit status 2 (a command line or file the process refused, which a restart cannot mend) or 3 (a tier answered the start-up self-test wrongly). After 5 failed starts in an hour systemd stops trying; `systemctl reset-failed tierstore` and a new start try again. [Troubleshooting](troubleshooting.md) lists the exit statuses.

`TimeoutStopSec=` in the unit is above the sum of `--drain-lb-wait`, `--drain-deadline` and the reconciler's lease wait, so systemd does not kill a drain in progress. If you raise `--drain-deadline`, raise it too.
