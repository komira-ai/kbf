# packaging

- `install.sh`: the curl|sh entry of `kbf-installer`
  (docs/design/kbf-installer.md section 9 P11, docs/design/distribution.md section 2).
  It detects the platform (`linux-x86_64`, `linux-arm64`; a Mac gets the `.pkg`),
  downloads `SHA256SUMS` and the `kbf-installer-payload-<commit>-<platform>.tar.gz` it
  lists from a release URL, refuses unless the tarball's sha256 is the listed one and
  every entry is a regular file at a path the package owns, then places the files under
  `/` (or `--root DIR`). It starts and enables nothing and prints the next step,
  `sudo kbf-installer setup`. It is the one shell file the distribution keeps
  (`tools/interpreter_budget.tsv`); every builder is Rust.
- The payload is `cargo xtask installer-tarball`: `usr/bin/kbf-installer`, the unit
  files from `--units` and the trust files from `--trust`, at the paths the Debian
  package uses, with fixed owners and times so the same inputs are the same bytes. It
  holds no kbf release; `setup` installs the first one.
- Tested by `crates/xtask/tests/install_sh.rs` against a local HTTP server, with a stub
  installer (`xtask-fake`) and stub unit and trust files until the real ones land.
