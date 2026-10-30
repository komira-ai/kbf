// Builds dist/: the prerendered pages and the hashed assets, then checks the result (check-dist.ts).
//
//   dist/index.html, 404.html, <route>/index.html   the pages (pages.ts), around the copy in content/ (layout.ts)
//   dist/assets/<id>/                               the browser modules (build/browser, from tsc), the recorded data
//                                                   as modules (data/<name>.js, from content/data/<name>.json) and
//                                                   the stylesheets of static/
//
// <id> is the first 16 hex digits of a sha256 over the assets' sorted (path, bytes): the same inputs give the same id
// and the same bytes everywhere, with no clock or commit in them, so every stage deploys one artifact and an asset URL
// never serves two contents.

import { createHash } from 'crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'fs';
import { dirname, join, resolve } from 'path';
import { checkDist, importsOf, listFiles } from './check-dist';
import { renderPage, STYLESHEETS } from './layout';
import { outPath, PAGES, rewriteLinks } from './pages';

export interface BuildOptions {
  /** tsc's browser output (build/browser): the modules the pages load and what they import. */
  browserDir: string;
  /** site/static: the stylesheets. */
  staticDir: string;
  /** site/content: the page copy (<name>.html) and the recorded data (data/<name>.json). */
  contentDir: string;
  /** Emptied and rewritten. */
  outDir: string;
  /** Run check-dist on the result (default). The tests turn it off to run each check on its own. */
  check?: boolean;
}

/** The asset id: sha256 over each (path, bytes) pair in path order, length-prefixed so no two sets collide. */
export function assetId(files: ReadonlyMap<string, Buffer>): string {
  const hash = createHash('sha256');
  for (const path of [...files.keys()].sort()) {
    const bytes = files.get(path) as Buffer;
    hash.update(`${Buffer.byteLength(path)}:${path}:${bytes.length}:`);
    hash.update(bytes);
  }
  return hash.digest('hex').slice(0, 16);
}

/** A recorded JSON file as an ES module: its text, verbatim, as the default export. */
export function dataModule(json: string): Buffer {
  JSON.parse(json);
  return Buffer.from(`export default ${json.trim()};\n`);
}

/** The modules reachable from the entries, entries included, sorted. */
export function moduleGraph(js: ReadonlyMap<string, Buffer>, entries: readonly string[]): string[] {
  const seen = new Set<string>(entries);
  const queue = [...entries];
  while (queue.length > 0) {
    const m = queue.shift() as string;
    const src = js.get(m);
    if (src === undefined) throw new Error(`build: ${m} is imported but was not compiled`);
    if (m.startsWith('data/')) continue;
    for (const dep of importsOf(m, src.toString('utf8'))) {
      if (!seen.has(dep)) {
        seen.add(dep);
        queue.push(dep);
      }
    }
  }
  return [...seen].sort();
}

function write(path: string, data: string | Buffer): void {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, data);
}

export function buildSite(opts: BuildOptions): { id: string; files: string[] } {
  const compiled = new Map<string, Buffer>();
  for (const f of listFiles(opts.browserDir)) {
    if (f.endsWith('.js')) compiled.set(f, readFileSync(join(opts.browserDir, f)));
  }
  const dataDir = join(opts.contentDir, 'data');
  if (existsSync(dataDir)) {
    for (const f of readdirSync(dataDir).sort()) {
      if (f.endsWith('.json')) compiled.set(`data/${f.replace(/\.json$/, '.js')}`, dataModule(readFileSync(join(dataDir, f), 'utf8')));
    }
  }
  // Only the scripts the pages name and what they import ship (types.js-like modules of types only are never loaded).
  const entries = [...new Set(PAGES.flatMap((p) => p.scripts))].sort();
  for (const e of entries) if (!compiled.has(e)) throw new Error(`build: ${e} is missing; run tsc -p tsconfig.browser.json first`);
  const assets = new Map<string, Buffer>();
  for (const m of moduleGraph(compiled, entries)) assets.set(m, compiled.get(m) as Buffer);
  for (const css of STYLESHEETS) assets.set(css, readFileSync(join(opts.staticDir, css)));

  const id = assetId(assets);
  const refs = { base: `/assets/${id}/` };

  rmSync(opts.outDir, { recursive: true, force: true });
  for (const [path, bytes] of assets) write(join(opts.outDir, 'assets', id, path), bytes);
  for (const page of PAGES) {
    const main = rewriteLinks(readFileSync(join(opts.contentDir, `${page.source}.html`), 'utf8'), PAGES);
    write(join(opts.outDir, outPath(page.route)), renderPage(page, main, refs));
  }

  if (opts.check !== false) checkDist(opts.outDir);
  return { id, files: listFiles(opts.outDir) };
}

if (require.main === module) {
  // Compiled to site/build/tools/tools/build.js.
  const site = resolve(__dirname, '..', '..', '..');
  const { id, files } = buildSite({
    browserDir: join(site, 'build', 'browser'),
    staticDir: join(site, 'static'),
    contentDir: join(site, 'content'),
    outDir: join(site, 'dist'),
  });
  process.stdout.write(`dist: ${files.length} files, assets/${id}/\n`);
}
