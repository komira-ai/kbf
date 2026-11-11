# Deploying each release's frontend to Docker Swarm

A farm that runs its frontends as a Docker Swarm service, updated by
[docker-swarm-updater](https://github.com/komira-ai/docker-swarm-updater), can have every kbf
release deploy itself there. The release workflow (`.github/workflows/release.yml`) does it in two
jobs, after the release is published and after the gamma stage:

| job | what it does |
|---|---|
| `deploy-digest` | `cargo xtask release image-digest`: downloads `SHA256SUMS`, the frontend image `kbf-frontend-image-<commit>-linux-x86_64.oci.tar` and its bundle `<image>.sigstore.json` from the published release, refuses an image that is not the file `SHA256SUMS` lists or is no image layout with one manifest, and outputs the manifest digest the image's `index.json` names and the release's download addresses of the image and the bundle. It holds `contents: read` alone. |
| `deploy` | calls docker-swarm-updater's reusable workflow `deploy.yml`, pinned by commit, with the service `kbf-frontend`, the released commit as the version, that digest and those addresses. It holds `id-token: write` alone: the called workflow asks GitHub for a new OIDC token for every request. |

The called workflow sends the deploy, then asks for its status until it ends. docker-swarm-updater
checks the token's claims, verifies the bundle against this repository's
`.github/workflows/artifacts.yml` on `refs/heads/main`, refuses a release older than the one
running, pushes the image by digest into its registry and updates the Swarm service to
`<registry>/<repository>@sha256:<digest>`; Swarm rolls back an update whose new task does not
become ready. The job passes when the deploy ends `converged`. It fails with the deployer's error
when the deploy ends rolled back, failed or paused, and when 240 minutes pass first (the deploy
then goes on; a frontend's drain waits for its open work, however long that takes).

Nothing deploys while either repository variable `CP_DEPLOY_ENDPOINT` or `CP_DEPLOY_TAILSCALE_TAGS`
is unset or empty: both jobs are skipped and the release ends as it does without them. `deploy`
needs `gamma`, so a release whose gamma stage failed is not deployed (re-run the failed jobs once
the farm takes it). A failed `deploy` makes the release run red; the release stays published.

## Turn it on

There is no required reviewer: a release that passed the pipeline deploys without an operator
step. The protection is the environment's branch rule, the exact claims the deploy front and the
updater check, the attestation check and Swarm's rollback.

1. In this repository, create the environment `controlplane`, with deployment branches limited to
   `main`, **no required reviewer and no wait timer**. The called job runs in it; the token's
   `environment` claim exists only for a job of this environment.
2. Configure docker-swarm-updater's service `kbf-frontend` with the signer
   `komira-ai/kbf/.github/workflows/artifacts.yml` on `refs/heads/main`, and one caller with
   exactly these claims (the deploy front checks the same ones): `repository` `komira-ai/kbf`,
   this repository's numeric `repository_id`, `job_workflow_ref`
   `komira-ai/docker-swarm-updater/.github/workflows/deploy.yml@<the commit release.yml pins>`,
   `ref` `refs/heads/main`, `environment` `controlplane`, `runner_environment` `github-hosted`.
3. When the deploy front is reachable only on a Tailscale network, use a workload-identity client
   that trusts this repository's `controlplane` environment (the token's subject is
   `repo:<owner>/<repo>:environment:controlplane`) and can issue a tag the tailnet's policy lets
   reach the deploy front. The client that CI jobs join with as `tag:ci` can serve, once its trust
   also covers `controlplane`. The deploy joins with the tags of `CP_DEPLOY_TAILSCALE_TAGS`, which
   must be tags that client can issue, or the join fails.
4. Set the repository variables:

| variable | value |
|---|---|
| `CP_DEPLOY_ENDPOINT` | the deploy front, `http[s]://<host>[:<port>]`; setting it, with `CP_DEPLOY_TAILSCALE_TAGS`, turns the deploy on |
| `CP_DEPLOY_AUDIENCE` | the audience the deploy front expects of every token |
| `CP_DEPLOY_TAILSCALE_CLIENT_ID` | the workload-identity client of step 3; empty: the job joins no network (`CP_DEPLOY_TAILSCALE_TAGS` must still be set, or nothing deploys) |
| `CP_DEPLOY_TAILSCALE_AUDIENCE` | that client's audience |
| `CP_DEPLOY_TAILSCALE_TAGS` | the tags the job joins the tailnet with, comma-separated, each one the client of step 3 can issue; required: while it is empty nothing deploys |

To turn the deploy off, clear `CP_DEPLOY_ENDPOINT` (or `CP_DEPLOY_TAILSCALE_TAGS`).

## Moving the pin

`release.yml` calls `deploy.yml` at a full commit with its version comment; the workflow lint
(`crates/kbf-integration-test/tests/workflows/called.rs`) and the release workflow lint
(`crates/kbf-integration-test/tests/release_workflow/deploy.rs`) refuse any other call, input,
token scope or condition. The token's `job_workflow_ref` names that commit, so moving the pin
takes three changes: the deploy front and the updater accept the new commit beside the old, this
repository moves its pin (the call and both lints), and the old commit is removed from the deploy
front and the updater.

## What the token can do if something is wrong

The `deploy` job's token is minted for the deploy front's audience. It is accepted only with the
exact claims above: this repository, on `main`, in `controlplane`, through the pinned workflow, on
a GitHub-hosted runner. Even then it deploys only an image whose bundle this repository's
`artifacts.yml` signed on `main` for a newer release than the one running. The release workflow
lint refuses any other permission, input, secret or called workflow on that job.
