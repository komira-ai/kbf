# The gamma trigger (retired)

There is no gamma trigger. `.github/workflows/gamma.yml` is deleted, and so are the commands it ran:
`kbf-installer deploy` (and the `update` verb), `kbf-installer setup --deploy-*` and the
`/etc/kbf-installer/deploy.json` it wrote, and `cargo xtask gamma`. Nothing in a release run deploys to a host.

A host takes a release from its operating-system package and from `kbf-updater`; see
`docs/deploy/kbf-installer-setup.md` for installing a host. The page `docs/design/gamma-deploy.md` describes the
retired deploy and is kept as history.

The repository variables and secrets the trigger used (`GAMMA_HOST`, `GAMMA_USER`, `TS_TAG`, `GAMMA_CHECK_TARGETS`,
`GAMMA_FARM_BUCKCONFIG`, `TS_OAUTH_CLIENT_ID`, `TS_OAUTH_SECRET`), the `gamma` environment, the Tailscale policy and the
sudoers line on the frontend host are no longer read by anything and can be removed.
