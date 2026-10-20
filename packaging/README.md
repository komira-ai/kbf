# packaging

- `install.sh`: the curl|sh entry of `kbf-installer`
  (docs/design/kbf-installer.md section 9 P11, docs/design/distribution.md section 2).
  It detects the platform (`linux-x86_64`, `linux-arm64`; a Mac gets the `.pkg`),
  downloads `SHA256SUMS` and the `kbf-installer-payload-<commit>-<platform>.tar.gz` it
  lists from a release URL, refuses unless the tarball's sha256 is the listed one and
  every entry is a regular file at a path the package owns, then places the files under
  `/` (or `--root DIR`). It starts and enables nothing and prints the next step,
  `sudo kbf-installer setup`. After placing it makes the users and directories from the
  two placed files (`systemd-sysusers`, `systemd-tmpfiles --create`, those files only); a
  host without those tools is refused before anything is placed. It is the one shell file the distribution keeps
  (`tools/interpreter_budget.tsv`); every builder is Rust.
- The payload is `cargo xtask installer-tarball`: the Debian package's file list, read
  from the same Rust list (`xtask::pkg::files`, which the deb and rpm are built from), so
  paths and modes cannot drift: `usr/bin/kbf-installer` and the pinned `gh` 0755, the
  `kbf-*.service` and `kbf-*.timer` units and `kbf.slice`, exactly the three trust files, and
  `usr/lib/sysusers.d/kbf-installer.conf` and `usr/lib/tmpfiles.d/kbf-installer.conf`
  (the files under `packaging/linux/`), with fixed owners and times so the same inputs
  are the same bytes. It holds no kbf release; `setup` installs the first one.
- Tested by `crates/xtask/tests/install_sh.rs` against a local HTTP server, with a stub
  installer (`xtask-fake`) and stub unit, trust, sysusers.d and tmpfiles.d files, and fake systemd tools.
