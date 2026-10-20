# site: the kbf.dev website

Plain TypeScript compiled by `tsc`, and a build program (`tools/`) that prerenders the pages. No framework, no bundler,
no runtime dependencies: `package.json` lists two development packages (`typescript`, `@types/node`), pinned exactly.
Node is the exact version in `.node-version`. The stack is mopm.dev's (komira-ai/mopm, `site/`), with `node:test` in
place of jest.

```sh
npm ci --ignore-scripts --no-audit --no-fund
npm run typecheck
npm test            # compiles, then runs test/*.test.ts
npm run build       # writes dist/
npm run golden      # after a change meant to alter the output: rewrites test/golden.manifest
```

| Path | What |
|---|---|
| `content/*.html` | the page copy: the inside of each page's `<main>` (`/docs/config/` has none: it is generated) |
| `content/data/*.json` | recorded data the demo and the story page replay; the build writes each as an ES module |
| `static/theme.css` | the shared theme (kbf.dev, mopm.dev, ucdk.dev), copied byte for byte; `test/theme.test.ts` pins its sha256 |
| `static/site.css` | this site's own rules, loaded after the theme |
| `static/docs.css` | the rules of the docs layout (nav, on-this-page list, code blocks, index cards), loaded after `site.css` by the docs pages only |
| `src/` | browser modules: copy buttons, the demo player, the story page |
| `tools/` | `pages.ts` (routes, titles, scripts), `layout.ts` (the page shell), `build.ts`, `check-dist.ts`, `links.ts`, `csp.ts`, `docs-table.ts` and `docs.ts` (the docs), `config.ts` (the configuration reference), `mopm/` (the Markdown renderer) |
| `test/` | golden output, links, Content-Security-Policy, theme digest, public-repo scan, supply-chain policy, the docs table and pages, the configuration page, the renderer copy and its attack corpus |

## Docs

`/docs/` is the repository's Markdown, rendered at build time (`tools/docs.ts`). `tools/docs-table.ts` is the table: each file, the
route it is served at, its section (guides, design, project) and its status, shown as a tag beside the title and on the
index: `works` (works today), `partial` (built in part), `design`, `planned`. The status is the table's reading of the
document's own text; a document that moves from design to built is a one-line change there. A file under `docs/` that is
neither in the table nor in its `EXCLUDE` list fails `test/docs-table.test.ts`, so a new document does not go unpublished.

A relative link between docs becomes the route of the page it names (a fragment keeps GitHub's heading slug); a link to any
other file of the repository becomes `https://github.com/komira-ai/kbf/blob/main/<path>`; a link that leaves the repository
is shown as text. `test/links.test.ts` fails on a link or `#fragment` that names nothing, and on a link to a repository file
that is not there. The links of the documents themselves that are broken today are listed in that test, not rewritten.

`/docs/config/` is generated from `crates/kbf-cli/tests/golden/*.toml` (`tools/config.ts`): one table per `[section]`, a row per
key, the comment above a key as its description, `key = value` as its default, a commented `# key = "X"` as no default, and
the modes whose golden has the key. `test/config.test.ts` fails when the page and the goldens disagree.

The pages `install`, `architecture`, `design` and `updates` are written by hand (`content/`).

### The Markdown renderer

`tools/mopm/` is a copy of mopm.dev's restricted Markdown renderer (komira-ai/mopm, `site/src/`: `doc.ts`, `links.ts`,
`markdown/`): linear time on any input, no raw HTML, every URL checked, only the tags and attributes of an allowlist, so it
satisfies the Content-Security-Policy below. `tools/mopm/SOURCE.json` records the upstream commit and the sha256 of each copied
file; `test/mopm-copy.test.ts` holds the copy to them. `links.ts` has three patches (the repository URL, the table of
published docs, the prefix on a fragment into another doc), each listed in `SOURCE.json` so that undoing them gives the upstream
digest. To take a newer renderer: copy the files from the new commit, redo the patches, update `SOURCE.json`. mopm's own tests
for it are ported to `node:test` (`test/markdown*.test.ts`, `doc.test.ts`, `mopm-links.test.ts`, on a small jest-shaped layer,
`test/jest-compat.ts`; the fuzz test draws its input from a seeded generator, as this site has no `fast-check`).

## Output

`dist/` holds the pages (`index.html`, `<route>/index.html`, `404.html`) and `assets/<id>/`, where `<id>` is a hash of the
assets' contents: the same inputs give the same bytes, with no clock or commit in them. Assets never change under a URL.

## Rules the build enforces (`tools/check-dist.ts`)

The site is served under `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'` (see `infra/site`).
So: no inline script or style, no `style=` attribute, no `on*=` handler, no `javascript:` or `data:` URL, nothing loaded
from another origin, no script that fetches, evaluates a string or writes markup from a string. Only `.html`, `.js` and
`.css` files are published, within size budgets. A page that breaks one fails the build.

## Theme

`static/theme.css` is never edited here. A change to the theme is made once, upstream, and the new file and digest are
taken by every site that shares it.
