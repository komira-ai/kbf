# site: the tierstore.dev website

The same stack as kbf.dev (`site/` at the root of this repository, itself mopm.dev's with `node:test` in place of jest):
plain TypeScript compiled by `tsc`, a build program (`tools/`) that prerenders the pages, no framework, no bundler, no
runtime dependencies. `package.json` lists two development packages, pinned exactly; Node is the exact version in
`.node-version`.

```sh
npm ci --ignore-scripts --no-audit --no-fund
npm run typecheck
npm test            # compiles, then runs test/*.test.ts
npm run build       # writes dist/
npm run golden      # after a change meant to alter a hand-made page or asset: rewrites test/golden.manifest
```

## Why it is here

tierstore's own repository is being planned. The site lives with the crate (`crates/tierstore/site`) so that it moves
with it: in the new repository this directory becomes `site/`. The path is the only thing that changes. The tests find
the repository root by looking for `.github/workflows` above them, and compute the site's path from it
(`test/support.ts`); `.github/workflows/tierstore-site.yml` names the directory in three places (the `paths:` filters,
`working-directory` and `node-version-file`), `.github/workflows/tierstore-site-publish.yml` in those, in the artifact
path and in the two files its deploy job checks out, and `test/supply-chain.test.ts` checks that the
workflows and the site agree. The test `csp.test.ts` reads the Content-Security-Policy from `infra/site/lib/site-stack.ts`, so the new
repository needs that file too, or the policy copied beside the site.

| Path | What |
|---|---|
| `../docs/*.md` | the operator guide, rendered by `tools/docs.ts` into `/docs/` and `/docs/<name>/` (a page for each file, so a new file is a new page; `Not in a release yet` sections are marked). The crate's test `src/server/docs_tests.rs` holds the guide to the code, so the pages cannot say what the code does not. A minimal renderer for the subset the guide uses, no dependency; it refuses what it does not read |
| `content/*.html` | the page copy: the inside of each page's `<main>`. Each claim comes from the crate's documentation, `packaging/tierstore/README.md` or the code; a page says where |
| `static/theme.css` | the shared theme (kbf.dev, mopm.dev, ucdk.dev, tierstore.dev), copied byte for byte; `test/theme.test.ts` pins its sha256 |
| `static/site.css` | this site's own rules, loaded after the theme |
| `src/copy.ts` | copy buttons on command blocks |
| `tools/` | `pages.ts` (routes, titles, the white-paper URL), `layout.ts` (the page shell, with the `> tierstore` mark), `build.ts`, `check-dist.ts`, `links.ts`, `csp.ts`, `publish.ts` (uploads a built `dist/`; see Deploying) |
| `test/` | golden output (`golden.manifest` guards the hand-made pages and assets, the docs index included; the pages rendered from the guide (`docs/<name>/index.html`) are left out of it and held by `docs.test.ts` and the crate's `docs_tests.rs`, so a guide edit needs no new manifest), the docs pages (`docs.test.ts`), links, Content-Security-Policy, theme digest, the white-paper slot, the "not built" statements and demo strings held to the crate source (`claims.test.ts`), public-repo scan, supply-chain policy, the publish program against stand-ins for the token service and the `aws` CLI (`publish.test.ts`) |

## The demo

There is no recording: a recorded session of tierstore does not exist in the repository, and the page may not carry
output that nobody produced. `content/demo.html` is a transcript whose output lines are the strings the program prints,
read from its source, and each block names the file. Values that differ on every run are written as `<N>` and
`<version>`.

## The white paper

`WHITE_PAPER_URL` in `tools/pages.ts` is `null`: until the paper is final the site says nothing about it, and
`test/whitepaper.test.ts` fails if any built file mentions one. Setting the URL adds a footer link on every page.

## Deploying

A push to `main` that changes the site is published by `.github/workflows/tierstore-site-publish.yml`, and so is a manual run of
that workflow on `main`. It is the same flow as mopm.dev's: a GitHub environment holds the settings, the job proves its identity
to AWS with an OpenID Connect token (no stored key), and a role that can only write this site's bucket and invalidate its
distribution does the upload. A pull request never reaches it.

| Job | Holds | Does |
|---|---|---|
| `build` | `contents: read`; no environment, no token | the steps of `tierstore-site.yml` (install with scripts off, type-check, the tests, the build), then hands `dist/` alone on as an artifact |
| `deploy` | `contents: read` and `id-token: write`; environment `prod-site-tierstore`; runs only for a push or a manual run on `main` and `needs` the build | checks out the commit's `tools/publish.ts` and `.node-version` (sparse, no credentials kept), downloads the pages, and runs the program from source with Node's type stripping: no install, no build |

`tools/publish.ts` is what holds the credentials, so it never comes from the build job, which runs dependency code. It imports
`node:` built-ins only and uses only syntax Node can strip (`erasableSyntaxOnly` in `tsconfig.json`); the lint in
`crates/kbf-integration-test/tests/site_deploy_workflow` holds both.

The publish program asks the runner for the job's token, exchanges it with `aws sts assume-role-with-web-identity` for credentials
that last an hour, uploads the assets (immutable) then the pages (short cache) then `version.txt`, sweeps what the build no
longer has, and invalidates the distribution. It refuses a `dist/` with fewer than 8 pages or 10 files (`MIN_HTML_PAGES` and `MIN_FILES`; a test keeps them under the real site and above half of it), so
an empty build can never make the sweep empty the bucket, and a file type other than `.html`, `.js` and `.css` (the sync
gives only those a content type and a cache rule; `check-dist` already refuses others). `releases/` is never touched. Because the repository and its Actions logs are
public, the role, the bucket (its name ends in the account id) and the distribution are masked before the first command.

A run that is skipped, replaced or red does not publish later by itself: GitHub keeps only the newest pending run of the
workflow's group, so a pending run can be replaced by a later push or dispatch, and a run that failed stays failed. After any
skipped, replaced or failed run, run the workflow again by hand from `main` (Actions, tierstore-site-publish, Run workflow). A
merge that changes nothing under the site's paths does not start it at all.

The settings are three variables of the environment `prod-site-tierstore`, set once by whoever sets the site up
(`TIERSTORE_SITE_BUCKET`, `TIERSTORE_SITE_DISTRIBUTION_ID`, `TIERSTORE_SITE_PUBLISH_ROLE_ARN`); there is no secret. An unset
variable is an empty flag, which the program refuses. The environment, the role and its trust, the bucket, the distribution and
the certificate are created once by an operator with their own AWS credentials (`infra/site`); the workflow only publishes.

`crates/kbf-integration-test/tests/site_deploy_workflow` holds the workflow's safety properties: main only, one job with the
token, the environment, the role from a variable and no stored key, the build and the tests before the upload, a deploy that is
never cancelled. The workflow lint additionally allows `id-token: write` for this job and this environment alone.

The output and its rules are those of `site/README.md`: `dist/` holds the pages and `assets/<id>/`, served under
`default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'`.

## Theme

`static/theme.css` is never edited here. A change to the theme is made once, upstream, and the new file and digest are
taken by every site that shares it.
