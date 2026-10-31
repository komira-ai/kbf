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
`working-directory` and `node-version-file`), and `test/supply-chain.test.ts` checks that the workflow and the site
agree. The test `csp.test.ts` reads the Content-Security-Policy from `infra/site/lib/site-stack.ts`, so the new
repository needs that file too, or the policy copied beside the site.

| Path | What |
|---|---|
| `../docs/*.md` | the operator guide, rendered by `tools/docs.ts` into `/docs/` and `/docs/<name>/` (a page for each file, so a new file is a new page; `Not in a release yet` sections are marked). The crate's test `src/server/docs_tests.rs` holds the guide to the code, so the pages cannot say what the code does not. A minimal renderer for the subset the guide uses, no dependency; it refuses what it does not read |
| `content/*.html` | the page copy: the inside of each page's `<main>`. Each claim comes from the crate's documentation, `packaging/tierstore/README.md` or the code; a page says where |
| `static/theme.css` | the shared theme (kbf.dev, mopm.dev, ucdk.dev, tierstore.dev), copied byte for byte; `test/theme.test.ts` pins its sha256 |
| `static/site.css` | this site's own rules, loaded after the theme |
| `src/copy.ts` | copy buttons on command blocks |
| `tools/` | `pages.ts` (routes, titles, the white-paper URL), `layout.ts` (the page shell, with the `> tierstore` mark), `build.ts`, `check-dist.ts`, `links.ts`, `csp.ts` |
| `test/` | golden output (`golden.manifest` guards the hand-made pages and assets; the docs pages are left out of it and held by `docs.test.ts` and the crate's `docs_tests.rs`, so a guide edit needs no new manifest), the docs pages (`docs.test.ts`), links, Content-Security-Policy, theme digest, the white-paper slot, the "not built" statements and demo strings held to the crate source (`claims.test.ts`), public-repo scan, supply-chain policy |

## The demo

There is no recording: a recorded session of tierstore does not exist in the repository, and the page may not carry
output that nobody produced. `content/demo.html` is a transcript whose output lines are the strings the program prints,
read from its source, and each block names the file. Values that differ on every run are written as `<N>` and
`<version>`.

## The white paper

`WHITE_PAPER_URL` in `tools/pages.ts` is `null`: until the paper is final the site says nothing about it, and
`test/whitepaper.test.ts` fails if any built file mentions one. Setting the URL adds a footer link on every page.

## Deploying

This directory deploys nothing, and neither does CI. The site is published by a held step run by a person with their
own AWS credentials (`tools/held/tierstore` in the internal repository), as kbf.dev's is (`infra/site`). The output and
its rules are those of `site/README.md`: `dist/` holds the pages and `assets/<id>/`, served under
`default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'`.

## Theme

`static/theme.css` is never edited here. A change to the theme is made once, upstream, and the new file and digest are
taken by every site that shares it.
