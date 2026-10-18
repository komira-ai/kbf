# kbf gamma trigger: setup

The kbf gamma stage deploys an operator's own farm from a pushed, attested commit:
`.github/workflows/gamma.yml` streams the commit's files over
Tailscale SSH to the frontend host and runs `kbf-installer deploy` there, which sets the desired
release through the operator API (loopback and the operator token only), waits for every node,
updates the frontend and checks the farm. Design: `docs/design/kbf-cicd.md` section 5.4.

`deploy/gamma/setup.sh` is the one-time setup. Every site value (repository, frontend host, deploy account, tag) is an argument. It is a held script:
dry run by default, nothing forced or deleted, no secret value in the file.

```sh
deploy/gamma/setup.sh --repo <owner/name> --host <frontend-tailscale-name> --user <deploy-account>   # dry run
deploy/gamma/setup.sh --repo <...> --host <...> --user <...> --api 127.0.0.1:<port> --token-file <path> --apply
```

`--apply` (gh only): creates the `gamma` environment, sets the repository variables
`GAMMA_HOST`, `GAMMA_USER`, `TS_TAG` (all on the repository you name), and prompts for the Tailscale OAuth client id and secret
(no echo, piped to `gh secret set --env gamma`). Until all three variables are set the workflow's
job is skipped (green), so nothing happens if you stop half way.

Printed, never done by the script (a person does them):

1. Tailscale: an OAuth client with only "Auth Keys: Write", restricted to the tag; the policy
   block (tag owner, grant, SSH rule `action: accept` for the deploy account); the script prints
   the JSON with your names in it.
2. The frontend host: the deploy account; the sudoers drop-in, which pins the whole argument
   list (`<user> ALL=(root) NOPASSWD: /usr/bin/kbf-installer deploy --apply --json --commit`
   followed by forty `[0-9a-f]` classes, then `--bundle -`; the script prints the exact line,
   installed with `visudo -c` first). It has no wildcard, so the deploy account cannot add
   `--api`, `--token-file`, `--config` or `--lock`; the deploy also refuses those when run
   through sudo. `/etc/kbf-installer/deploy.json` holds the loopback API address and the
   operator token file, so the workflow names neither.
3. Repository settings: the Actions policy must allow `tailscale/github-action`.

Order: merge `kbf-installer deploy`, then `gamma.yml`, then the feature branch to
main; install the `kbf-installer` binary on the frontend host, run this script, do the printed
steps, then push to `main` or `feature/arch-v2`. The first deploy ends exit 5 (needs operator)
until `kbf-cas-smoke` exists: the smoke is reported `skipped` and a skipped check never reads as a
pass. Read the `gamma-summary` artifact of the run.

Rerun is safe: the environment PUT and the variable and secret sets overwrite in place.
