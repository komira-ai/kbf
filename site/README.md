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
| `content/*.html` | the page copy: the inside of each page's `<main>` |
| `content/data/*.json` | recorded data the demo and the story page replay; the build writes each as an ES module |
| `static/theme.css` | the shared theme (kbf.dev, mopm.dev, ucdk.dev), copied byte for byte; `test/theme.test.ts` pins its sha256 |
| `static/site.css` | this site's own rules, loaded after the theme |
| `src/` | browser modules: copy buttons, the demo player, the story page |
| `tools/` | `pages.ts` (routes, titles, scripts), `layout.ts` (the page shell), `build.ts`, `check-dist.ts`, `links.ts`, `csp.ts` |
| `test/` | golden output, links, Content-Security-Policy, theme digest, public-repo scan, supply-chain policy |

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
