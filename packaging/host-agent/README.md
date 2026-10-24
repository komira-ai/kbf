# The host agent: units and jobs

The files an operator places to run a host's update agent (`docs/design/updates.md`). Nothing
here is in a package yet: the binaries are release assets (`docs/deploy/host-install.md`,
"Getting the binary"), and these files go beside them.

| file | what |
|---|---|
| `linux/kbf-host-local.service` | `kbf-host-install --serve`, as root: the local socket (`/run/kbf-host-install/local.sock`, mode 0600, owned by `kbf-updater`) that stages a release, switches to it on trial, and goes back by itself at the deadline |
| `linux/kbf-updater-agent.service` | `kbf-updater agent`, as `kbf-updater`: the heartbeat to the coordinator and the host's A/B through that socket |
| `linux/kbf-updater.conf` | the sysusers.d entry of the `kbf-updater` account (no login, no home) |
| `macos/com.komira.kbf-host-install-local.plist` | the same socket on a Mac, as root, in `/var/db/kbf-host-install` |
| `macos/com.komira.kbf-updater-agent.plist` | the agent on a Mac, as `_kbf-updater` |

The values in the files are examples (`coordinator.farm.example`, the paths of the certificate,
the key and the CA): edit them to the farm's. `crates/kbf-updater/tests/agent_packaging.rs`
reads these files and checks that every command line parses as the programs parse it, that
the two ends agree on the socket and on the account that may connect to it, and that the agent
is not root.

## What the agent needs

- **Files an operator places**: the updater's certificate and key, and the farm CA (enrollment
  is a later step). The certificate says which node this is and what the host runs.
- **A configuration for the helper's channels**: on Linux `/etc/kbf/host-install-releases.toml`,
  on a Mac the existing `host-install.toml`. See `docs/deploy/host-install.md`, "The local socket".
- **No credential for the frontend's operator API, as shipped.** The drain of the node and the
  node's health are calls to it, and it has no scoped credential yet (`docs/api.md`: its roles are
  planned), so they need the full operator token, which the design keeps off hosts. The units
  and jobs here give the agent none: it heartbeats and refuses every update order before it
  fetches anything. An operator who accepts the token on a host adds `--operator-api` and
  `--token-file` to the agent's command line (the API is on the frontend's loopback), knowing
  that the agent's account can then read the token. A scoped credential for `:drain`,
  `:uncordon` and node status is what lifts the boundary.
- **A frontend host is not served**: its certificate is refused. The roll updates frontends.
