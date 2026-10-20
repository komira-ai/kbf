# The gamma trigger: what a person sets up once

`.github/workflows/gamma.yml` follows a cut release to the one gamma host, the frontend of our own build farm. The
design is `docs/design/kbf-cicd.md`, section 5.4; the command it runs on the host is `kbf-installer deploy`
(`docs/design/gamma-deploy.md`). The workflow is inert until the settings below exist: while any of the repository
variables `GAMMA_HOST`, `GAMMA_USER` and `TS_TAG` is unset, its `deploy` job is skipped, which is a green run. Deleting one
of them stops gamma again.

Nothing here is applied by code. `cargo xtask gamma held-steps` prints every step for the names it is given, checks the
names first, and has no `--apply`:

```sh
cargo xtask gamma held-steps --repo <owner/name> --host <tailnet name> --user <deploy account> [--tag tag:<name>]
```

What it prints, in order:

1. **GitHub.** The `gamma` environment, restricted to deployments from `main`; the three repository variables; and the
   secrets `TS_OAUTH_CLIENT_ID` and `TS_OAUTH_SECRET` of that environment, which `gh` asks for on the terminal (no value is
   on a command line). The OAuth client is the only credential in the workflow.
2. **Tailscale.** An OAuth client with the scope "Auth Keys: Write" only, restricted to the tag, and the policy block: the
   tag owner, a grant to the host's SSH port and an SSH rule that lets the tag log in to the host as the deploy account.
3. **The frontend host, as root.** The deploy account (no password: it only receives the ssh command), the sudoers line from
   `cargo xtask gamma sudoers-line --user <account>` in `/etc/sudoers.d` (edit it with `visudo`), and `kbf-installer setup`
   with `--deploy-stage gamma`, which writes `/etc/kbf-installer/deploy.json`, so the workflow names no API address, token or
   stage.
4. **The Actions policy** allows `tailscale/github-action`.

The sudoers line lets the account run one command as root: `/usr/bin/kbf-installer deploy --apply --json --commit <40
hex digits> --offer - --bundle -`. The offer and the release's files arrive in a tar on standard input; the account can
name no path of the host, and the installer refuses its host flags (`--api`, `--token-file`, `--stage` and the others) when
`sudo` ran it.

## What is public

This repository's logs and artifacts are public. The installer's summary names nodes, so the workflow shows only the
allow-list `cargo xtask gamma deploy` writes (the commit, the result, the exit status and reason code, the step names, the
platform, state and seconds of each node, the post checks and the offer's numbers). The host and the user are masked in the
log, but GitHub prints a repository variable in the log of any step that has it in its environment, and the deploy step has
two: give `GAMMA_HOST` the short tailnet name, never an address, and an account whose name you are willing to show.

## Order

The workflow runs from the default branch's copy of the file, so it does nothing until `gamma.yml` is on `main`, and it
follows runs of the `release` workflow, which cuts a release on a push to `main`. Set the host up and run the printed steps,
then set the three variables last. The first deploy is a person's to watch: what `gamma-deploy.md` lists under "What is not
built" decides whether it can end `0`.

A deploy can hold the `gamma` concurrency group for up to four hours. A run for a commit that is no longer the highest
release tag is refused before anything reaches the host, and so is a run whose offer is more than 72 hours old.
