# The gamma trigger (retired)

There is no gamma trigger. `.github/workflows/gamma.yml` is deleted, and so are the commands it ran:
`kbf-installer deploy` (and the `update` verb), `kbf-installer setup --deploy-*` and the
`/etc/kbf-installer/deploy.json` it wrote, and `cargo xtask gamma`. Nothing in a release run connects to a host to
install a release.
The gamma stage today is the `gamma` job of `release.yml`: it no longer sets the farm's desired version. The
farm's Linux nodes take a release from the apt repository through hostkeeper, which `hostkeeper-desired.yml` tells
the version ([farm-hostkeeper.md](farm-hostkeeper.md)); `gamma` prints a notice saying so and, when
`KBF_SETS_DESIRED` is `true`, runs a remote-only build on the farm ([the design](../design/gamma-cicd.md)).

A host outside the farm takes a release from its operating-system package and from `kbf-updater`; see
`docs/deploy/kbf-installer-setup.md` for installing a host. The page `docs/design/gamma-deploy.md` describes the
retired deploy and is kept as history.

The repository variables and secrets the trigger used (`GAMMA_HOST`, `GAMMA_USER`, `TS_TAG`, `GAMMA_CHECK_TARGETS`,
`GAMMA_FARM_BUCKCONFIG`, `TS_OAUTH_CLIENT_ID`, `TS_OAUTH_SECRET`), the `gamma` environment, the Tailscale policy and the
sudoers line on the frontend host are no longer read by anything and can be removed.
