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

## The old and the new task of one slot

A frontend stops by draining: it waits for its open work, however long that takes, and only
then exits. With `order: start-first` Swarm starts the slot's new task first and tells the old
one to stop once the new one is healthy; with `order: stop-first` it starts the new task once
the old one has stopped or one minute after it told the old one to stop, whichever comes first
(the grace period only puts off the kill). So a drain longer than a minute overlaps the new task
under either order, and under `start-first` the overlap is the whole point: the new task must
start, and pass its health check, while the old one is running.

Both tasks name the slot's `file://` history directory, and that works: the directory is opened
shared. A shared open takes no `LOCK`; each process keeps its own file under `instances/` locked
for as long as it runs, and its temporary files, in `tmp-shared/`, carry its name. Every key a history
writer writes carries its own writer id (its start time in microseconds), so the old and the new
task never write one key, and nothing in history deletes, evicts or lists the directory. The new
task starts, serves and passes its health check beside the old one, and each of them goes on
writing under its own id until the old one has drained and exited. A new task's open removes
the leftovers of a task that is gone (its `tmp-shared/` files and its `instances/` file) and leaves a
running task's alone. The cold store's directory is a different matter: it is evicted, scanned
and cleaned, and one process owns it (`LOCK`).

A release that still opens the directory exclusively (the one before this) holds `LOCK` for as
long as it runs. The new task opens the directory beside it all the same, since a shared open
does not look at `LOCK`. The reverse costs nothing: an old-release process that starts beside a
new one (a rollback, or a task restarted by Swarm) empties `tmp/` as it opens, and a new task
keeps its temporary files in `tmp-shared/`, which the old release never lists or empties, so
no write of the new task is cut and the old process's open does not fail.

What bounds the overlap is the service's health check: a new task that has not passed it by the
end of its `start_period` and retries is replaced, and that counts as a failed update.

With an `s3://` `--history-objects` (`conditional-put=true`, the key pair in
`--history-credentials-file`) the old and the new task write the
one store at once in the same way, each under its own writer id
([frontends-behind-a-load-balancer.md](frontends-behind-a-load-balancer.md#what-the-frontends-share)),
and the new task serves as soon as it has started. When a frontend stops, after its drain, its
history writer closes every build it recorded and writes until nothing is left or its stores stop
taking writes, for at most five seconds, so a rolling update leaves the old task's builds closed.
History is not all an update with `order: start-first` needs: the old and the new frontend must
also share the executor's jobs, which is the subject of
[docs/design/n-frontends.md](../design/n-frontends.md) (section 9 gives the order of the steps).

## A release that removes flags

A frontend refuses a `--config` file that still sets a flag its release removed: the start exits
2 and names every such key of the file at once, each as a line `removed key <table>.<key> in <file>: ...`,
and `--check` says the same without starting. Under `order: start-first` the old task keeps
serving and the update fails on the new task, so nothing is lost but the update; still, the
file has to change before the release can run.

The deploy does not check the service's file against the new image before it updates the
service: docker-swarm-updater has no step that runs the new image first. So, before a release
whose notes list flags under "Config migration" (the flags it removed that the release before it
still took, read from the two builds), delete those keys from the service's file, and check the
file with the new image, mounted where the service mounts it:

```text
docker run --rm --volume <the service's kbf.toml>:/etc/kbf/kbf.toml:ro <the release's image> --config /etc/kbf/kbf.toml --check
```

(the image's entry point is `kbf --mode=frontend`; arguments after the image replace its default
`--config /etc/kbf/kbf.toml`, so the command names the file again). It exits 0 when the release takes the file, and 2 naming every key it
refuses. A host whose frontend container is switched by `kbf-host-install --frontend-container`
needs none of this: the switch runs that check itself, with the container's own mounts, and
refuses (`config_invalid`) before it drains or changes anything
([host-install.md](host-install.md#a-frontend-container)).

## The state schema

A frontend with a `postgres://` `--state-store` brings the database to the schema its build needs
itself, before it serves: after the store is open and before the listeners accept, it runs the step
`kbf boot-check --mode=frontend` runs (expand migrations only, under a lock, waiting at most 30
seconds for it), over its own `--state-store` connection. A database already at that schema is left
alone, and one line of the log says what the step did (`migrated the schema from 3 to 4`). Nothing
else has to run before a release that raises the schema: no job, no manual `boot-check`. The
database role the frontend connects as must be allowed to create the tables; if it is not, the
schema is busy past the wait, or a pending migration is a `contract` step, the start exits 2 naming
the cause and `kbf boot-check --mode=frontend`, and Swarm's rollback applies. In a start-first
update the new task migrates while the old one serves; that is safe because a step only adds, and
the previous release runs on a schema one step ahead
([the N and N-1 design](../design/kbf-cicd.md)).

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
