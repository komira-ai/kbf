# Kit-host fixtures

A Mac installed by the Mac kit (`deploy/kbf-mac/kbf_av2_mac.sh` in the internal deploy repo, its
`main` branch) as the kit leaves it. They are the facts a `migrate` that adopts such a host must
start from. Every file here is made-up test data: no real certificate, key, host name or commit.
The frontend name is `frontend.example.invalid`; the two commits are 40 `a` and 40 `b`.

Citations below are `path:line` in that repo at its `main`; `kit` is `deploy/kbf-mac/kbf_av2_mac.sh`,
`readme` is `deploy/kbf-mac/README.md`, `selftest` is `deploy/kbf-mac/selftest.sh`.

| Fixture | What it is | Source |
|---|---|---|
| `tree.manifest` | the file tree, owners and modes | kit:14-18, 164-176, 682-691, 704-705, 728, 741, 743, 749-750; readme:12-22 |
| `launchd_new.plist` | the job for a release that takes `--mode=daemon` | kit:237-242, 296-347; selftest:555-562 |
| `launchd_old.plist` | the job for a release that takes the `daemon` word form | kit:219-233, 296-347 |
| `kbf.toml` | the new form's config, as `daemon_toml` renders it (no `label` key) | kit:251-279; selftest:527-554 |
| `av2_mac.conf` | the kit's own settings | kit:137-158; `kbf_av2_mac.config.example` |
| `../../src/migrate/kit_host.rs` | loads them under a scratch root, and checks them | (this repo) |

The tree: `/usr/local/kbf/releases/<sha>/kbf` for two releases, `current` and `previous` as relative
links `releases/<sha>`, certificates under `etc/`, the data directory under `work/daemon/data`,
`removed/`, the plist in `/Library/LaunchDaemons` and `kbf.toml` in `/usr/local/etc/kbf`. There is
no `/var/db/kbf-deploy`, no `state.json`, no `helper.json` and no lock file.

## Facts migrate must honor

1. **The links are the truth.** A Mac has no `/run`, so the kit keeps no lock file and no
   `state.json` (kit:18; readme:24). What is current is what `current` names, and what is previous is
   what `previous` names; the kit reads them with `readlink` and strips `releases/` (kit:213-216), and
   rollback swaps them (kit:475-494). A kit host therefore has no old state to disagree with its links
   and no swap on trial.
2. **The release binary is `kbf`, not `kbf-daemon`.** Each release directory holds one file, `kbf`,
   root:wheel 0755 (kit:15, 704-705; readme:15). The probe that asks a release its version on macOS
   must run `releases/<sha>/kbf`; `Roots::binary` in `migrate/roots.rs` names `kbf-daemon` for macOS today.
3. **Restart is `launchctl kickstart -k system/com.komira.kbf-daemon`** when the job is loaded and
   its plist did not change (kit:443-445, 460-463). When the plist changed the kit uses `bootout`,
   `enable`, `bootstrap system <plist>` instead (kit:439-442). The label is `com.komira.kbf-daemon`
   (kit:138-139, 173).
4. **The daemon runs as `_kbf`** (`UserName` and `GroupName` of the plist; kit:139, 312-315). The
   releases are root's, so that user cannot replace its binary (kit:15). `etc/v2` is root:_kbf 0750 and
   the key is _kbf 0400 (kit:683, 691). The data directory and `work` are _kbf 0700 (kit:685).
5. **Which form a host is in is read from the plist**: a plist containing `--mode=daemon` is the new
   form, any other the old form (kit:482). The program arguments of the new form are the binary,
   `--mode=daemon`, `--config` and the file, and nothing else (kit:237-242); every setting is a key of
   `/usr/local/etc/kbf/kbf.toml` (root:wheel 0644, in a 0755 directory; kit:140-142, 727-728). The
   old form passes the settings as flags (kit:219-233). A release of the other form would start with
   arguments it does not take, so the kit refuses a rollback across forms (kit:480-485).
6. **The plist the kit replaced** is kept as `removed/com.komira.kbf-daemon.plist.before-av2` when an
   install found a different job there (kit:739-742). The fixture tree has the directory and not that
   file: whether it exists depends on the host's history.

## The rollback target

Rolling `migrate` back leaves the host as it was before the migration:

- `/Library/LaunchDaemons/com.komira.kbf-daemon.plist` has the text it had, byte for byte
  (`launchd_new.plist` or `launchd_old.plist`, whichever the host ran);
- `/usr/local/kbf/current` names the same release as before, and `previous` the same one;
- `/usr/local/kbf` (releases, links, `etc`, `work`, `removed`) is exactly as before: `migrate` does
  not change it, so a rollback has nothing there to undo;
- `/usr/local/etc/kbf/kbf.toml` is the same bytes;
- the job is loaded again and running as `_kbf` (the kit reloads with `bootout`, `bootstrap`, or
  restarts with `kickstart -k`, per fact 3).

## What the tests here check

`migrate/kit_host.rs` lays the manifest out under a scratch root and checks that it matches the kit's
facts (modes, links, no old state), that the plist's program arguments are exactly the two forms,
that `kbf.toml` parses with the keys `daemon_toml` renders and names the files that exist, and that
no fixture names a real host. They change no behaviour of `migrate`.
