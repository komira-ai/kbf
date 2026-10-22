# The gamma trigger: what a person sets up once

> RETIRED: `gamma.yml` is deleted and no release workflow run starts a gamma deploy; gamma moves to kbf-updater (package-update design, section 13). Nothing below is needed for a release any more; the page stays until the xtask `gamma` verbs are deleted.

`.github/workflows/gamma.yml` follows a cut release to the one gamma host, the frontend of our own build farm. The
design is `docs/design/kbf-cicd.md`, section 5.4; the command it runs on the host is `kbf-installer deploy`
(`docs/design/gamma-deploy.md`). The workflow is inert until the settings below exist: while any of the repository
variables `GAMMA_HOST`, `GAMMA_USER` and `TS_TAG` is unset, its `deploy` job is skipped, which is a green run, and so is the
`check` job that builds komira's `main` against the farm after the deploy (`gamma-deploy.md`, "The check after a deploy").
Deleting one of them stops gamma again.

Nothing here is applied by code. `cargo xtask gamma held-steps` prints every step for the names it is given, checks the
names first, and has no `--apply`:

```sh
cargo xtask gamma held-steps --repo <owner/name> --host <tailnet name> --user <deploy account> [--tag tag:<name>]
```

What it prints, in order:

1. **GitHub.** The `gamma` environment, restricted to deployments from `main`; the three repository variables; and the
   secrets `TS_OAUTH_CLIENT_ID` and `TS_OAUTH_SECRET` of that environment, which `gh` asks for on the terminal (no value is
   on a command line). The OAuth client is the only credential that reaches the host. Two more settings serve the check:
   the repository variable `GAMMA_CHECK_TARGETS` (the komira targets to build, separated by spaces) and the secret
   `GAMMA_FARM_BUCKCONFIG` of the environment (the buckconfig that points buck2 at the farm; a secret and not a variable
   because it names the farm and GitHub prints a variable in the step's log).
2. **Tailscale.** An OAuth client with the scope "Auth Keys: Write" only, restricted to the tag, and the policy block: the
   tag owners, a grant to the host's SSH port and an SSH rule that lets the tag log in to the host as the deploy account.
   An SSH rule's destination is a tag, not a device name, so the host carries its own tag (`tag:kbf-gamma-host`).
3. **The frontend host, as root.** The deploy account (no password: it only receives the ssh command), the host's tag
   (`tailscale set --advertise-tags`), the sudoers line from
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
platform, state and seconds of each node, the post checks and the offer's numbers). **The host name and the account name
are public.** GitHub prints a repository variable in the log of any step that has it in its environment, and the deploy
step has two, so the step's header shows them and masking cannot hide what is already printed (the deploy verb masks them
in what it prints after, and drops what `tailscale ssh` writes to its standard error, which can name the machine and its
paths). Give `GAMMA_HOST` the short tailnet name, never an address, and use an account whose name you are willing to
show. Keeping them out of the log needs secrets scoped to that one step and a quiet `cargo` (it prints its command line);
that is a choice for the repository owner and is not built.

## Order

The workflow runs from the default branch's copy of the file, so it does nothing until `gamma.yml` is on `main`, and it
follows runs of the `release` workflow, which cuts a release on a push to `main`. Set the host up and run the printed steps,
then set the three variables last. The first deploy is a person's to watch: what `gamma-deploy.md` lists under "What is not
built" decides whether it can end `0`.

A release run in which any job failed, a `site` or `bump` job after a published release included, ends as a failed run,
and `gamma.yml` follows only a run that succeeded: gamma stays silent until the failed jobs are re-run and the run turns
green. The jobs of `gamma.yml` can be re-run; its uploads
overwrite what the first attempt uploaded.

A deploy can hold the `gamma` concurrency group (the `deploy` job's own, not the workflow's) for up to four hours; GitHub
keeps one pending deploy per group, so a newer release replaces an older pending one. A run for a commit that is no longer the highest
release tag is refused before anything is sent to the host; a run whose offer is more than 72 hours old is refused by the
host (`offer_stale`) before it changes the farm.
