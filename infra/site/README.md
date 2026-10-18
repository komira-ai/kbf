# infra/site: AWS infrastructure for the kbf website and release downloads

CDK (TypeScript, us-east-1) for a static site and its release downloads: a private S3 bucket behind CloudFront (origin
access control), a Route 53 zone, a certificate, and an IAM role that the publish workflow assumes through GitHub OIDC.
It is self-contained: nothing else in this repository depends on Node.

The bucket holds the site build at its root and each release under `releases/<version>/`, with `releases/latest.json`.
The publish workflow uploads with `aws s3`; it does not run CDK and has no rights beyond the bucket and one
distribution.

**Every step is held**: an operator runs it with their own AWS credentials (an administrator of the AWS account you
deploy to) and their own `gh` login. Each step is a dry run unless `--apply` is given, never forces, and refuses to run
unless the AWS profile's account equals the `--account` you pass. Nothing here writes an account id, an organization
name or a repository id to a file: they are flags and CDK context.

The held steps are one small CLI, `held/main.ts`, run as `npm run held -- <command> [flags]` from this directory (the
repository allows no shell scripts). It calls `aws`, `gh` and `cdk` with argument arrays, and its tests drive it with a
fake so they can see which commands a dry run does and does not run.

## Parameters

| parameter | where | meaning |
|---|---|---|
| `--account` / `-c account` | `dns`, `site`, `vars` | 12-digit id of the AWS account; also checked against the profile's account |
| `--profile` | `dns`, `site`, `vars` | AWS CLI profile to use |
| `--repo` / `-c githubRepo` | `github`, `site`, `vars` | `<owner>/<name>` of the repository that hosts the publish workflow |
| `-c githubRepoId`, `-c githubOwnerId` | `cdk` (set by `site`) | immutable numeric ids; `site` reads them with read-only `gh api repos/<repo>` |
| `--domain` / `-c domain` | `dns`, `site` | site name; default `kbf.dev` |
| `--oidc-provider-arn` / `-c oidcProviderArn` | `site` | the account's existing GitHub OIDC provider; default `arn:aws:iam::<account>:oidc-provider/token.actions.githubusercontent.com` |

Names are fixed: bucket `kbf-prod-site-<account>`, headers policy `kbf-prod-site-headers`, function
`kbf-prod-site-router`, role `kbf-prod-publish`, stacks `kbf-dns-zone` and `kbf-prod-site`.

## Stacks

| stack | creates | on delete |
|---|---|---|
| `kbf-dns-zone` | public hosted zone for the domain | zone RETAINed (a new zone has new name servers) |
| `kbf-prod-site` | the bucket, OAC, distribution (alias = the domain), response headers policy, router function, ACM certificate (DNS-validated in the zone), A and AAAA alias records at the apex, the publish role | bucket RETAINed |

Both use the legacy synthesizer: no `cdk bootstrap` and no assets.

## Order

Prerequisite: the AWS account already has the GitHub OIDC identity provider (`token.actions.githubusercontent.com`,
audience `sts.amazonaws.com`). `site` imports it by ARN and refuses if it is absent; it never creates one (an account
can hold only one provider for that URL).

```sh
cd infra/site && npm ci --ignore-scripts
```

0. **GitHub**: `npm run held -- github --repo <owner>/<name>` (dry run), then add `--apply` (needs admin on the repo).
   - Creates environment `prod` with `deployment_branch_policy` `{protected_branches:false, custom_branch_policies:true}`
     and one policy `{name:"main", type:"branch"}`. The `sub` below holds the called workflow's ref but NOT the caller's,
     so this branch policy is what stops a feature-branch caller of `site-publish.yml@refs/heads/main` from assuming the
     role.
   - Sets the OIDC sub template to `repo`, `context`, `job_workflow_ref` with the immutable subject (the default
     template's `sub` has no workflow file).
   - Prints, and does not run, a reminder to check the token (see "The OIDC `sub`").
1. **Zone**: `npm run held -- dns --account <id> --profile <profile>` (runs the tests, then `cdk diff`), then `--apply`.
   Creates the zone and prints its four name servers.
2. **Registrar (by hand)**: at your registrar, set the domain's nameservers to those four hosts (the stack output
   `NameServers` of `kbf-dns-zone`, also the zone's NS record in the Route 53 console), one per line, no trailing dots.
   Check `dig +short NS <domain>` lists the same four before step 3: the certificate cannot validate until the
   delegation is live (the deploy waits for it).
3. **Site**: `npm run held -- site --account <id> --profile <profile> --repo <owner>/<name>` (diff), then `--apply`.
   Refuses without the OIDC provider or the zone.
4. **Variables**: `npm run held -- vars --account <id> --profile <profile> --repo <owner>/<name>`. It first verifies the
   GitHub side read-only (environment `prod` exists with custom branch policies, its branch policies are exactly `main`,
   the sub template is as above) and refuses to print anything if any differs. Then it reads the stack outputs and
   prints three `gh variable set ... --env prod` commands (`KBF_SITE_BUCKET`, `KBF_SITE_DISTRIBUTION_ID`,
   `KBF_SITE_PUBLISH_ROLE_ARN`). You run them. Nothing is committed.

### Undo

- Step 4:
  ```sh
  gh variable delete KBF_SITE_BUCKET --env prod --repo <owner>/<name>
  gh variable delete KBF_SITE_DISTRIBUTION_ID --env prod --repo <owner>/<name>
  gh variable delete KBF_SITE_PUBLISH_ROLE_ARN --env prod --repo <owner>/<name>
  ```
- Step 3: `npx cdk destroy kbf-prod-site` with the same `-c` context and profile, or delete the stack in CloudFormation.
  It removes the distribution, certificate, records, function, headers policy and role. The bucket is RETAINed with every
  release in it. **A retained bucket blocks re-running `site --apply` after a destroy**: the name
  `kbf-prod-site-<account>` exists, so CloudFormation fails creating it. Recover by importing the bucket into a new stack
  (CloudFormation resource import), or, if it is empty and you do not need it, delete the empty bucket by hand (all
  versions and delete markers included). Take down the registrar change first if the site must stop resolving.
- Step 2: put the registrar's previous nameservers back.
- Step 1: delete the stack; the zone is RETAINed, delete it in Route 53 by hand once nothing delegates to it.
- Step 0: delete the environment; restore the sub template with `use_default=true`.

## The publish workflow

The role trusts a workflow named exactly `.github/workflows/site-publish.yml`, run from `refs/heads/main`, in GitHub
environment `prod`. It is a reusable workflow; this directory does not contain it. It must:

- set `permissions: id-token: write` on a job that declares `environment: prod`, inside that file;
- be called with `uses: <owner>/<name>/.github/workflows/site-publish.yml@refs/heads/main` (a branch ref, not a SHA);
- read the three variables from step 4, assume the role (1 h session), `aws s3 sync` the site build with
  `--delete --exclude 'releases/*'`, upload release files with `aws s3api put-object --if-none-match '*'`, and create
  an invalidation of `/*`.

## The OIDC `sub`

Role `kbf-prod-publish`, max session 1 h, trust `sts:AssumeRoleWithWebIdentity` from the imported provider with
`StringEquals`:

- `token.actions.githubusercontent.com:aud` = `sts.amazonaws.com`
- `token.actions.githubusercontent.com:sub` =
  `repo:<owner>@<ownerId>/<name>@<repoId>:environment:prod:job_workflow_ref:<owner>/<name>/.github/workflows/site-publish.yml@refs/heads/main`

`<ownerId>` and `<repoId>` are the immutable numeric ids of `gh api repos/<owner>/<name> --jq '.owner.id,.id'`, in the
immutable-subject form. `job_workflow_ref` is the ref of the workflow file that contains the job: for a reusable
workflow it is the CALLED file, not the caller. This follows GitHub's documentation of the claim; it is NOT verified
against a live token. Verify before relying on it: run a manual workflow from `site-publish.yml@refs/heads/main` in
environment `prod` that prints the token's `sub` and `job_workflow_ref` (never the token itself) and compare. If the
printed `sub` differs, change `PUBLISH_WORKFLOW`, `WORKFLOW_REF` or `GITHUB_ENVIRONMENT` in `lib/config.ts`, re-run
`npm test`, and redeploy step 3.

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
invalidation (no `GetInvalidation`). Multipart uploads work: CreateMultipartUpload, UploadPart and
CompleteMultipartUpload are covered by `s3:PutObject` (with the write-once deny, the conditional header goes on the final
complete/put). Only `AbortMultipartUpload` and `ListMultipartUploadParts` are not granted; the 7-day lifecycle rule
cleans up incomplete uploads.

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
- `test/held.test.ts`: flag parsing (`--force` refused), the account guard stops each AWS step after one call, dry runs
  run no write and no `cdk deploy`, `--apply` runs the tests before `cdk`, `site` passes the ids read from `gh api` as
  context and never creates a provider, and `vars` prints nothing unless the environment, its branch policy and the
  sub template are as expected.
