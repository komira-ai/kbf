# infra/site: AWS infrastructure for the kbf websites and release downloads

CDK (TypeScript, us-east-1) for a static site and its release downloads: a private S3 bucket behind CloudFront (origin
access control), a Route 53 zone, a certificate, and an IAM role that the publish workflow assumes through GitHub OIDC.
It is self-contained: the Rust workspace does not depend on Node; Node is used only by `infra/site` and `site/` (the website's
build), both with lockfiles and `--ignore-scripts`.

The bucket holds the site build at its root and each release under `releases/<version>/`, with `releases/latest.json`.
The publish workflow uploads with `aws s3`; it does not run CDK and has no rights beyond the bucket and one
distribution.

**Every step is held**: an operator runs it with their own AWS credentials (an administrator of the AWS account you
deploy to) and their own `gh` login. Each step is a dry run unless `--apply` is given, never forces, and refuses to run
unless the AWS profile's account equals the `--account` you pass. Nothing here writes an account id, an organization
name or a repository id to a file: they are flags and CDK context.

Dry runs: `dns`, `site` and `upload` make AWS read calls even without `--apply` (the caller identity, and `cdk diff` and its
lookups); `github`'s dry run makes no AWS call and no change (it reads the repository from GitHub). `site` and `github`
refuse a `--repo` whose GitHub `full_name` is not exactly the name given (case-insensitive), and print the repository
name and the two numeric ids the role will trust before any write or `cdk` call.

The held steps are one small CLI, `held/main.ts`, run as `npm run held -- <command> [flags]` from this directory (the
repository allows no shell scripts). It calls `aws`, `gh` and `cdk` with argument arrays, and its tests drive it with a
fake so they can see which commands a dry run does and does not run.

## Several sites in one account

One account and one GitHub repository can host several sites. Every name is derived from a **site id**, chosen by the domain
(`--domain`) or given with `--site`:

| domain | site id |
|---|---|
| `kbf.dev` (the default) | `kbf` |
| `tierstore.dev` | `tierstore` |

A domain that is not in that table (`SITES` in `lib/config.ts`) must pass `--site <id>`: it is never silently given another
site's names. `--site` may not contradict a known domain, nor reuse another listed site's id (two unlisted domains with the
same id cannot be detected), and an id on an unlisted domain is refused if any name derived from it (stack, bucket, role,
GitHub environment, workflow, variable) equals a name of a listed site: `--site site` would take kbf.dev's `prod-site`. An id is lower-case letters and digits,
starting with a letter, at most 16.

| name | kbf.dev (`kbf`) | tierstore.dev (`tierstore`) |
|---|---|---|
| zone stack | `kbf-dns-zone` | `tierstore-dns-zone` |
| site stack | `kbf-prod-site` | `tierstore-prod-site` |
| bucket | `kbf-prod-site-<account>` | `tierstore-prod-site-<account>` |
| headers policy | `kbf-prod-site-headers` | `tierstore-prod-site-headers` |
| router function | `kbf-prod-site-router` | `tierstore-prod-site-router` |
| publish role | `kbf-prod-publish` | `tierstore-prod-publish` |
| approval environment | `prod` | `prod-tierstore` |
| site environment (the role trusts it) | `prod-site` | `prod-site-tierstore` |
| publish workflow the role trusts | `.github/workflows/site-publish.yml` | `.github/workflows/tierstore-site-publish.yml` (komira-ai/tierstore; the role's trust in `lib/config.ts` still names this repository until it is moved, below) |
| GitHub variables | `KBF_SITE_BUCKET`, `KBF_SITE_DISTRIBUTION_ID`, `KBF_SITE_PUBLISH_ROLE_ARN` | `TIERSTORE_SITE_BUCKET`, `TIERSTORE_SITE_DISTRIBUTION_ID`, `TIERSTORE_SITE_PUBLISH_ROLE_ARN` |

**kbf.dev's names are the ones it was first deployed with and must never change**: CloudFormation replaces a resource whose
logical id or physical name changes. Logical ids are the same in every site's stack (they are scoped by the stack), so only the
physical names above differ. `test/kbf-names.test.ts` pins every kbf.dev name and compares the synthesized zone and site
templates (domain and no-domain mode) byte for byte with `test/golden/*.json`, which were made from the code before names
were parameterized; if that test fails, a kbf.dev resource would be replaced. `test/site-names.test.ts` requires the two
sites to share no name. Regenerate a golden only for a change you intend to deploy to kbf.dev.

Everything else is shared by all sites on purpose: the Content-Security-Policy and the other response headers, the router,
cache and error behavior, the role's permissions, the OIDC provider (one per account, imported by ARN) and the
`prod`-style policies (approval: `main` branch and `v*` tag; site: `v*` tag only). A site that needs a different policy
changes `lib/site-stack.ts` for that site deliberately. `site/test/csp.test.ts` reads the CSP from `lib/site-stack.ts`.

`.github/workflows/tierstore-site-publish.yml` (now in komira-ai/tierstore, no longer in this repository) is the workflow tierstore's publish role is to trust. The role's trust is not moved yet: `lib/config.ts` still builds it for this repository's workflow path, so a publish from komira-ai/tierstore is refused at `AssumeRoleWithWebIdentity` until a maintainer redeploys the stack with the trust set to komira-ai/tierstore (its repository id and workflow path), the one-time setup of that repository's step 6. It publishes on a push to `main`
that changes the site (and on a manual run of `main`), so its job runs in the environment `prod-site-tierstore` from the `main`
branch: that environment's deployment rule is the branch `main`, where kbf.dev's `prod-site` admits `v*` tags only. The held
`github` step creates the tag rule for every site, so for tierstore.dev the rule is changed by hand once, and a later `github`
dry run reports the difference. The held `upload` step is the manual fallback.

Each site has its own GitHub environments and variables, so two sites in one repository share no reviewer rule or variable.
Every step is per site: pass `--domain` (or `--site`) to `github`, `dns`, `site`, `vars` and `upload`. `oidc-provider`
is per account, not per site. For a second site: `github --domain tierstore.dev`, `dns --domain tierstore.dev`, the registrar,
`site --domain tierstore.dev`, `vars --domain tierstore.dev`, `upload --domain tierstore.dev`. A step run for one site never
reads or changes another's stacks, environments or variables.

## One approval per release: two environments

The table names are kbf.dev's; another site has its own pair (see above).

A release asks the reviewer once. Two GitHub environments split the work:

| environment | reviewer | deployment policies | referenced by |
|---|---|---|---|
| `prod` | the one required reviewer | branch `main`, tag `v*` | the `publish` job (`contents: write`, no `id-token`) |
| `prod-site` | none | tag `v*` only | the site job (`id-token: write`, no repo write), which `needs` `publish` |

GitHub pauses a job for review when it references an environment with required reviewers
([reviewing deployments](https://docs.github.com/en/actions/managing-workflow-runs-and-deployments/managing-deployments/reviewing-deployments);
[environments](https://docs.github.com/en/actions/managing-workflow-runs-and-deployments/managing-deployments/managing-environments-for-deployment):
"Only one of the required reviewers needs to approve the job"). Reviewers are optional there, and deployment branch/tag
policies are a separate rule a job must satisfy; an environment without reviewers does not pause. A job's OIDC token carries
`environment:<name>` in its `sub` whenever the job declares `environment:`
([OIDC reference](https://docs.github.com/en/actions/security-for-github-actions/security-hardening-your-deployments/about-security-hardening-with-openid-connect)).
Sequential jobs in the same environment are separate pending deployments, which is why a second job in `prod` would have
been a second click. Two jobs cannot share one click without running in parallel (losing the order) or holding contents:write
and id-token:write together. So: `publish` (in `prod`) is approved; the site job `needs` it, runs in `prod-site` without a
prompt, and the role trusts `prod-site`, so the approved `publish` job (which has `contents: write`) cannot assume the role.
Ordering comes from `needs`; the site job's own guard (in `site-publish.yml`) also downloads the GitHub release, which exists
only after `publish` ran. What the documentation does not state, and only a real run shows: that a `prod-site` job with no
reviewer starts immediately, is admitted by the tag policy, and carries `environment:prod-site` in its `sub` (probe, below).
The cost of no reviewer on `prod-site`: a workflow on a `v*` tag that calls `site-publish.yml` directly would not be
paused; the guard job (commit on `main`, attestations) and the repository's ruleset on who may push `v*` tags are what
stand there.

## Parameters

| parameter | where | meaning |
|---|---|---|
| `--account` / `-c account` | `oidc-provider`, `dns`, `site`, `vars` | 12-digit id of the AWS account; also checked against the profile's account |
| `--profile` | `oidc-provider`, `dns`, `site`, `vars` | AWS CLI profile to use |
| `--reviewer` | `github` | GitHub login of the person who approves a `prod` deployment (the only reviewer; `prod-site` has none) (a value you pass, never stored in a file) |
| `--repo` / `-c githubRepo` | `github`, `site`, `vars` | `<owner>/<name>` of the repository that hosts the publish workflow |
| `-c githubRepoId`, `-c githubOwnerId` | `cdk` (set by `site`) | immutable numeric ids; `site` reads them with read-only `gh api repos/<repo>` |
| `--domain` / `-c domain` | `github`, `dns`, `site`, `vars`, `upload` | site name; default `kbf.dev`; selects the site id (see "Several sites") |
| `--site` / `-c site` | `github`, `dns`, `site`, `vars`, `upload` | site id; only for a domain that is not a known site; passed to `cdk` only when given |
| `--no-domain` / `-c noDomain=true` | `site` | no certificate, alias or zone; serve on the `*.cloudfront.net` name (see "Before DNS") |
| `--oidc-provider-arn` / `-c oidcProviderArn` | `site` | the account's existing GitHub OIDC provider; default `arn:aws:iam::<account>:oidc-provider/token.actions.githubusercontent.com` |

Names are derived from the site id (see "Several sites"). For kbf.dev: bucket `kbf-prod-site-<account>`, headers policy
`kbf-prod-site-headers`, function `kbf-prod-site-router`, role `kbf-prod-publish`, stacks `kbf-dns-zone` and `kbf-prod-site`.
The rest of this file uses kbf.dev's names; substitute the site's for another.

## Stacks

| stack | creates | on delete |
|---|---|---|
| `kbf-dns-zone` | public hosted zone for the domain | zone RETAINed (a new zone has new name servers) |
| `kbf-prod-site` | the bucket, OAC, distribution (alias = the domain), response headers policy, router function, ACM certificate (DNS-validated in the zone), A and AAAA alias records at the apex, the publish role | bucket RETAINed |

Both use the legacy synthesizer: no `cdk bootstrap` and no assets.

## Order

Prerequisite: the AWS account has the GitHub OIDC identity provider (`token.actions.githubusercontent.com`, audience
`sts.amazonaws.com`). `site` imports it by ARN and refuses if it is absent; it never creates one (an account can hold
only one provider for that URL). Step `oidc-provider` creates it, and only when it is absent.

```sh
cd infra/site && npm ci --ignore-scripts
```

0. **GitHub**: `npm run held -- github --repo <owner>/<name> --reviewer <login>` (dry run), then add `--apply` (needs admin
   on the repo). Read-only calls first (the repo's ids, the reviewer's user id, the environment's current settings); each
   write is skipped when GitHub already shows it, so a second run changes nothing but re-puts the sub template.
   - Creates or updates environment `prod-site` like `prod` (below) but with NO reviewer (`reviewers: []`), `can_admins_bypass: false`
     and the single policy `{name:"v*", type:"tag"}`; `vars` refuses if it has a reviewer (that would be a second click) or
     any other policy. `github` prints the role-trust `sub` the role must have (it names `prod-site`).
   - Creates or updates environment `prod` with `deployment_branch_policy` `{protected_branches:false,
     custom_branch_policies:true}`, the required reviewer (the user named by `--reviewer`; a team is not supported),
     `prevent_self_review: false` and `can_admins_bypass: false`. Self-review stays allowed on purpose: the person who
     pushes the release tag is the approver, and with it on a release would wait for a second person forever. The
     environment is one PUT whose body goes on stdin (`--input -`), because that call replaces the whole protection set.
   - Adds exactly two deployment policies: `{name:"main", type:"branch"}` and `{name:"v*", type:"tag"}`. The tag policy
     is needed because the release workflow's `publish` job runs on `refs/tags/v*`, which a branch policy never matches;
     the `main` policy is what stops a feature-branch caller of `site-publish.yml@refs/heads/main` from assuming the
     role (the `sub` below holds the called workflow's ref but NOT the caller's). A policy that is already there is not
     posted again; an extra one is reported and left for you to delete (`vars` refuses while it exists).
   - Sets the OIDC sub template to `repo`, `context`, `job_workflow_ref` with the immutable subject (the default
     template's `sub` has no workflow file).
   - Prints, and does not run, a reminder to check the token (see "The OIDC `sub`").
   - The tag `v*` policy only admits a tag run; who may push a tag is a repository ruleset, which is a separate held
     setting (see the CICD design notes).
0b. **OIDC provider**: `npm run held -- oidc-provider --account <id> --profile <profile>` (dry run: lists the account's
   providers and prints either "already present" or the exact `aws iam create-open-id-connect-provider` it would run),
   then `--apply`. It creates url `https://token.actions.githubusercontent.com` with client id `sts.amazonaws.com` only when
   no provider for that URL exists, and never changes an existing one. No thumbprint is passed: IAM validates GitHub's
   certificate chain itself. Run it before step 3.
1. **Zone**: `npm run held -- dns --account <id> --profile <profile>` (runs the tests, then `cdk diff`), then `--apply`.
   Creates the zone and prints its four name servers.
2. **Registrar (by hand)**: at your registrar, set the domain's nameservers to those four hosts (the stack output
   `NameServers` of `kbf-dns-zone`, also the zone's NS record in the Route 53 console), one per line, no trailing dots.
   Check `dig +short NS <domain>` lists the same four before step 3: the certificate cannot validate until the
   delegation is live (the deploy waits for it).
3. **Site**: `npm run held -- site --account <id> --profile <profile> --repo <owner>/<name>` (diff), then `--apply`.
   Refuses without the OIDC provider or the zone.
3b. **Extra deployment policies (by hand)**: before running `vars`, delete any extra deployment policy on environment `prod` or `prod-site`
   yourself (Settings > Environments > prod); `github` warns about extras and never removes them, and `vars` refuses while one exists.
4. **Variables**: `npm run held -- vars --account <id> --profile <profile> --repo <owner>/<name>`. It first verifies the
   GitHub side read-only (environment `prod` exists with custom branch policies, its policies are exactly the branch `main`
   and the tag `v*`, it has a required reviewer; environment `prod-site` exists with exactly the tag `v*` and NO reviewer, self-review is allowed, administrators cannot bypass, the sub template
   is as above) and refuses to print anything if any differs. Then it reads the stack outputs and
   prints three `gh variable set ... --env prod-site` commands (`KBF_SITE_BUCKET`, `KBF_SITE_DISTRIBUTION_ID`,
   `KBF_SITE_PUBLISH_ROLE_ARN`). You run them. Nothing is committed.

### Held for the maintainers when this change lands

If the stack `kbf-prod-site` was already deployed (trusting environment `prod`), its role trust must change to `prod-site`:
re-run step 3 (`site`, diff, then `--apply`; the diff shows the one changed `sub` string), then step 0 (`github`) for
`prod-site`, then move the three variables (step 4 prints them for `prod-site`; delete the `prod` copies). Not yet
deployed: nothing to change; just run the steps in order.

### The first release needs no verdict

`release verify` reads no verdict (the tag's message is the Beta record, which nothing gates on), so the first tag needs
no hand-written one and no waiver: it passes the checks every tag passes. Gamma is a farm build check
(the retired `gamma.yml`; gamma moves to kbf-updater); it does not gate the release.

### Undo

- Step 4:
  ```sh
  gh variable delete KBF_SITE_BUCKET --env prod-site --repo <owner>/<name>
  gh variable delete KBF_SITE_DISTRIBUTION_ID --env prod-site --repo <owner>/<name>
  gh variable delete KBF_SITE_PUBLISH_ROLE_ARN --env prod-site --repo <owner>/<name>
  ```
- Step 3: `npx cdk destroy kbf-prod-site` with the same `-c` context and profile, or delete the stack in CloudFormation.
  It removes the distribution, certificate, records, function, headers policy and role. The bucket is RETAINed with every
  release in it. **A retained bucket blocks re-running `site --apply` after a destroy**: the name
  `kbf-prod-site-<account>` exists, so CloudFormation fails creating it. Recover by importing the bucket into a new stack
  (CloudFormation resource import), or, if it is empty and you do not need it, delete the empty bucket by hand (all
  versions and delete markers included). Take down the registrar change first if the site must stop resolving.
- Step 2: put the registrar's previous nameservers back.
- Step 1: delete the stack; the zone is RETAINed, delete it in Route 53 by hand once nothing delegates to it.
- Step 0b: delete the provider in IAM only if nothing else in the account uses it.
- Step 0: delete the environments `prod` and `prod-site`; restore the sub template with `use_default=true`.

## Before DNS: no-domain mode

While the domain has no delegation yet, deploy the same stack without the domain: no ACM certificate, no alias, no
Route 53 lookup or records. CloudFront serves the site on its own `*.cloudfront.net` name with the default certificate.
`held site --no-domain` (context `-c noDomain=true`) does not look up or need the zone, still refuses without the OIDC
provider or on an account mismatch, and prints the distribution's domain name (stack output `DistributionDomainName`).
`dns` is not needed.

```sh
npm run held -- github --repo <owner>/<name>                      # dry run, then add --apply
npm run held -- site --no-domain --account <id> --profile <profile> --repo <owner>/<name>   # dry run, then add --apply
npm run held -- vars --account <id> --profile <profile> --repo <owner>/<name>              # prints the gh variable commands and the site URL
npm run held -- upload --account <id> --profile <profile> --commit <40-hex sha>             # dry run, then add --apply
```

`upload` is TEMPORARY (marked `// temporary: removed by site-publish.yml` in `held/steps.ts`): it puts the website on the
bucket until the publish workflow does. It runs with your own profile, never the CI role, and needs the stack from
`site` (the bucket and distribution come from its outputs; it works for the no-domain stack). It
1. refuses unless the sha is a commit of this repository and the `node` on PATH is exactly the one in `site/.node-version`
   (both checked before anything is built);
2. makes a temporary `git worktree add --detach` at the sha, runs `npm ci --ignore-scripts --no-audit --no-fund` and
   `npm run build` in its `site/`, runs check-dist (CSP, links, hygiene) on the `dist/` and refuses if it fails, then
   removes the worktree;
3. syncs `dist/` with `aws s3 sync --delete` in passes per class, each excluding `releases/*` last (it never touches
   `releases/`): `assets/*.js` and `assets/*.css` with `public, max-age=31536000, immutable`, pages (`*.html`,
   `text/html; charset=utf-8`) with `public, max-age=60, s-maxage=300`, then a sweep that deletes what the build no
   longer has (the sweep skips `releases/*` and `version.txt`);
4. uploads `version.txt` (the sha, `text/plain`, `no-cache`), then invalidates `/*` last.

The dry run builds and checks locally, prints the file list, and runs each `aws s3` call with `--dryrun` (S3 read
calls, which list the uploads and deletes); it writes nothing and does not invalidate.

Moving to the domain later: run `dns` (step 1), set the nameservers at the registrar (step 2), then run `site` again
WITHOUT `--no-domain` (dry run first). The bucket and the distribution keep their logical ids (a test compares them
between the two modes), so nothing is replaced. In place, the deploy adds the certificate (validated in the zone), the
A and AAAA alias records, and the distribution's alias and certificate; the bucket, OAC, headers policy, router function
and publish role do not change. The `*.cloudfront.net` name keeps working. The variables do not change.

## The publish workflow

The role trusts a workflow named exactly `.github/workflows/site-publish.yml`, run from `refs/heads/main`, in GitHub
environment `prod-site` (not `prod`, see "One approval per release"). It is a reusable workflow; this directory does not contain it. It must:

- set `permissions: id-token: write` on a job that declares `environment: prod-site`, inside that file;
- be called with `uses: <owner>/<name>/.github/workflows/site-publish.yml@refs/heads/main` (a branch ref, not a SHA);
- read the three variables from step 4, assume the role (1 h session), `aws s3 sync` the site build with
  `--delete --exclude 'releases/*'`, upload each release file as ONE `aws s3api put-object --if-none-match '*'` (see
  Permissions: no multipart), and create an invalidation of `/*`.

## The OIDC `sub`

Role `kbf-prod-publish`, max session 1 h, trust `sts:AssumeRoleWithWebIdentity` from the imported provider with
`StringEquals`:

- `token.actions.githubusercontent.com:aud` = `sts.amazonaws.com`
- `token.actions.githubusercontent.com:sub` =
  `repo:<owner>@<ownerId>/<name>@<repoId>:environment:prod-site:job_workflow_ref:<owner>/<name>/.github/workflows/site-publish.yml@refs/heads/main`

`<ownerId>` and `<repoId>` are the immutable numeric ids of `gh api repos/<owner>/<name> --jq '.owner.id,.id'`, in the
immutable-subject form. `job_workflow_ref` is the ref of the workflow file that contains the job: for a reusable
workflow it is the CALLED file, not the caller. This follows GitHub's documentation of the claim; it is NOT verified
against a live token. Verify before relying on it: run a manual workflow from `site-publish.yml@refs/heads/main` in
environment `prod-site` that prints the token's `sub` and `job_workflow_ref` (never the token itself) and compare. If the
printed `sub` differs, change `PUBLISH_WORKFLOW`, `WORKFLOW_REF` or `GITHUB_ENVIRONMENT` in `lib/config.ts`, re-run
`npm test`, and redeploy step 3.

**After a repository rename:** the `sub` embeds `<name>` twice (the subject and `job_workflow_ref`), and tokens carry the
name current when they are minted, so a stack deployed under the old name stops trusting the workflow the day the
repository is renamed (the ids do not change). Re-run step 3 with `--repo komira-ai/kbf` (the diff shows the two
changed strings) before the next publish; step 4's check of the variables uses the same `--repo`.

## Permissions

Nothing beyond this: no managed policies, no IAM, no CDK bootstrap roles.

- `s3:ListBucket` on the bucket; `s3:PutObject` and `s3:DeleteObject` on `bucket/*`.
- **Release files are write-once.** An explicit Deny of `s3:PutObject` on `bucket/releases/*/*` unless the S3
  conditional-write key `s3:if-none-match` equals `*`. `releases/latest.json` (directly under `releases/`) is not
  matched by `releases/*/*`, so it can be overwritten. Upload release files with
  `aws s3api put-object --if-none-match '*' ...`, so re-publishing an existing version fails instead of overwriting it.
- An explicit Deny of `s3:DeleteObject` and `s3:DeleteObjectVersion` on `bucket/releases/*`: CI never deletes a release.
- `cloudfront:CreateInvalidation` on the one distribution.

`aws s3 sync` of the site must exclude `releases/` (the deny would fail the sync otherwise). The role cannot wait on an
invalidation (no `GetInvalidation`).

**Release files cannot be uploaded in multiple parts.** CreateMultipartUpload and UploadPart are `s3:PutObject` calls
without an `if-none-match` header, so the write-once deny refuses them under `releases/<version>/`; the deny is not
loosened for them. Upload each release file with a single `aws s3api put-object --if-none-match '*' --body <file> ...`
(`aws s3 cp` switches to multipart above its threshold, so do not use it for releases). One `PutObject` is limited to
5 GB per object, which is far above any package here. Outside `releases/` (site pages and assets) multipart works;
`AbortMultipartUpload` and `ListMultipartUploadParts` are not granted, and the 7-day lifecycle rule cleans up
incomplete uploads.

## Serving

- One behavior: HTTPS redirect, GET/HEAD, `CACHING_OPTIMIZED`, TLS 1.2_2021, HTTP/2+3, root object `index.html`, 403 and
  404 answer `/404.html` with status 404 (10 s).
- The router function maps `/x/` to `/x/index.html` and never touches `/releases/*`.
- Headers on every response (override): CSP `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self';
  base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'` (no `connect-src`: the site fetches
  nothing; no inline script or style, so pages must use external files, including no `style=""` attributes), HSTS 730
  days with subdomains, nosniff, frame DENY, referrer no-referrer, COOP and CORP same-origin, Permissions-Policy.
- Content types and cache-control are set by the uploader (CloudFront serves what S3 stores). Upload with explicit
  `--content-type`: `.deb` `application/vnd.debian.binary-package`, `.rpm` `application/x-rpm`, `.pkg`
  `application/octet-stream`, `install.sh` `text/x-shellscript` (or `text/plain`), `.json` `application/json`, `.tar.gz`
  `application/gzip`. Suggested cache-control: `releases/<version>/*` `public, max-age=31536000, immutable`;
  `releases/latest.json` `public, max-age=60, s-maxage=60`; assets immutable; pages `public, max-age=60, s-maxage=300`.
- Bucket versioning is on. One lifecycle rule expires noncurrent versions after 365 days (bucket-wide, releases
  included) and aborts incomplete multipart uploads after 7. Current objects, releases included, are kept forever.

## Tests

`npm ci --ignore-scripts && npm test` here (a CI job runs it when `infra/site/**` changes; an operator should run it
before any `--apply`, and the held CLI runs it for `dns` and `site`).

- `test/site-stack.test.ts` (`aws-cdk-lib/assertions`): all four public-access blocks, SSE-S3, versioning, owner-enforced,
  RETAIN, noncurrent expiry of exactly 365 days, the non-TLS deny over the bucket and its objects, the CloudFront allow
  limited by `AWS:SourceArn`, OAC and no OAI, HTTPS redirect and the other distribution settings, every header value, no
  `connect-src`/`unsafe-inline`, A and AAAA apex records, DNS-validated certificate, the exact trust policy and `sub`, no
  second OIDC provider, exactly four allowed actions with bucket- and distribution-scoped resources, the releases delete
  deny, the write-once deny and its `latest.json` carve-out, the router run over its vectors, the region and
  provider-ARN guards, the zone stack, and the context parameters (the ids come only from context; bad values refused).
  The tests use the documentation account `123456789012` and fake ids.
- `test/kbf-names.test.ts`: every kbf.dev name pinned to its deployed value, and the zone and site templates byte-equal to
  `test/golden/`. `test/site-names.test.ts`: site-id resolution (a known domain, an explicit id, refusals), kbf and
  tierstore names share no string, their synthesized templates share no physical name and tierstore's contain no `kbf`,
  and the held CLI selects the right stacks, environments and variables per `--domain`. No test depends on timing.
- `test/held.test.ts`: flag parsing (`--force` refused), the account guard stops each AWS step after one call, dry runs
  run no write and no `cdk deploy`, `github` writes, for `prod`, the reviewer, `prevent_self_review=false`, `can_admins_bypass=false` and the
  two policies, and for `prod-site` an empty reviewer list and the single `v*` tag policy, each exactly once and nothing when present, `oidc-provider` creates only when absent, `--apply` runs the tests before `cdk`, `site` passes the ids read from `gh api` as
  context and never creates a provider, and `vars` prints nothing unless both environments, their reviewer settings (one reviewer on `prod`, none on `prod-site`), their policy sets
  (`prod`: exactly `main`/branch and `v*`/tag; `prod-site`: exactly `v*`/tag) and the sub template are as expected.
