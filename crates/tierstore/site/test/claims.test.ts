// The statements of the pages about what tierstore does not do, and the strings of the demo, held to the code.
//
// Catches: a page that says "not built" about something the code now does, and a demo line that is no longer what the
// program prints. The site says the per-node cache is a build node (`[local_cache]` with `[cold_store]`),
// that the only counters `/metricz` has are the OCI front's and the cache's, and that the binary does not use `CachedStore`; the demo quotes the bodies of the
// probes. Each of those is read from the crate's source here, so when the code changes the test fails and says which
// page to update. The crate's source is the directory above the site's (crates/tierstore/src, and src/ once the
// crate is a repository of its own).

import assert from 'node:assert/strict';
import { readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { test } from 'node:test';
import { DOCS, read, SITE } from './support';

const SRC = resolve(SITE, '..', 'src');
const isTestFile = (f: string): boolean => /(_tests|_testing|testing)\.rs$/.test(f);

/** The binary's own source: main.rs, the tiers' counters (metrics.rs) and the server module's files that are not tests. */
function binarySource(): Map<string, string> {
  const files = new Map<string, string>([['main.rs', read(SRC, 'main.rs')], ['metrics.rs', read(SRC, 'metrics.rs')]]);
  const walk = (dir: string): void => {
    for (const e of readdirSync(join(SRC, dir), { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      if (e.isDirectory()) walk(`${dir}/${e.name}`);
      else if (e.name.endsWith('.rs') && !isTestFile(e.name)) files.set(`${dir}/${e.name}`, read(SRC, dir, e.name));
    }
  };
  walk('server');
  return files;
}

/** The files that export metrics: the OCI front's, the cache's and the tiers'. */
const EXPORTERS = new Set(['server/oci/metrics.rs', 'server/local_cache/metrics.rs', 'metrics.rs', 'server/build_info.rs', 'server/mod.rs']);

/** What in the source no longer matches the pages. Each problem names the pages to change. */
export function staleClaims(src: ReadonlyMap<string, string>): string[] {
  const out: string[] = [];
  const all = [...src.entries()];
  const config = src.get('server/config.rs') ?? '';
  if (!/build-node shape/.test(config) || !/\[local_cache\]/.test(config)) {
    out.push('config.rs no longer reads a build node ([local_cache] with [cold_store]): update docs/configuration.md, docs/operations.md ("A build node"), content/why.html and content/index.html (the status table)');
  }
  for (const [name, text] of all) {
    if (!EXPORTERS.has(name) && /# TYPE |tierstore_cache_\{/.test(text)) out.push(`${name} exports metrics: the counters of the OCI front, the cache and the tiers (EXPORTERS) were the only ones; update docs/operations.md ("Metrics"), docs/metrics.md and content/why.html ("Where it stops")`);
    if (/\bCachedStore\b/.test(text)) out.push(`${name} uses CachedStore: the binary has a cache now; update content/why.html ("The per-node read-through cache is not in the binary") and content/index.html`);
  }
  const routes = new Set([...(src.get('server/mod.rs') ?? '').matchAll(/\(true, "(\/[a-z]+)"/g)].map((m) => m[1]));
  const want = ['/healthz', '/metricz', '/readyz', '/usagez'];
  if ([...routes].sort().join() !== want.sort().join()) {
    out.push(`the probe routes in server/mod.rs are ${[...routes].sort().join(', ')}, not ${want.join(', ')}: update docs/operations.md ("Probes") and content/demo.html`);
  }
  return out;
}

const page = (name: string): string => read(SITE, 'content', `${name}.html`);

test('the pages say what they say about the cache and metrics, so the code check below is not vacuous', () => {
  assert.match(read(DOCS, 'operations.md'), /the counters of the OCI read front/);
  assert.match(page('why'), /serves the counters of the OCI read front, those of the local cache of a build node and, on main, hit, miss and byte counters of the two tiers/);
  assert.match(page('why'), /The <code>tierstore<\/code> binary does not use it/);
  assert.match(page('why'), /<code>\[local_cache\]<\/code> and <code>\[cold_store\]<\/code> and no <code>\[hot_store\]<\/code>/);
  assert.match(page('index'), /a build node, file only/);
});

test('the code still matches those statements', () => {
  assert.deepEqual(staleClaims(binarySource()), []);
});

test('the check can fail: a second metrics exporter, a CachedStore use and a dropped build-node shape are each reported with the page to change', () => {
  const real = binarySource();
  const with_ = (name: string, f: (s: string) => string): Map<string, string> => new Map([...real].map(([k, v]) => [k, k === name ? f(v) : v]));
  assert.match(staleClaims(with_('server/cli.rs', (s) => s + '\n# TYPE x counter')).join('\n'), /exports metrics.*docs\/operations\.md/);
  assert.match(staleClaims(with_('main.rs', (s) => s + '\nuse tierstore::CachedStore;')).join('\n'), /uses CachedStore.*content\/why\.html/);
  assert.match(staleClaims(with_('server/config.rs', (s) => s.replace('build-node shape', 'ok'))).join('\n'), /no longer reads a build node.*docs\/configuration\.md/);
  assert.match(staleClaims(with_('server/mod.rs', (s) => s.replace('"/usagez"', '"/usagez2"'))).join('\n'), /probe routes/);
});

test('every output line of the demo is a string the program prints', () => {
  const mod = binarySource().get('server/mod.rs') as string;
  for (const body of ['"ok"', '"ready"', '"draining"']) assert.ok(mod.includes(body), `${body} is not in server/mod.rs: update content/demo.html`);
  assert.ok(mod.includes('no usage yet, {secs}s since start'), 'the /usagez body changed: update content/demo.html and docs/operations.md');
  assert.ok(mod.includes('usage published {secs}s ago'), 'the /usagez body changed: update content/demo.html and docs/operations.md');
  const cli = binarySource().get('server/cli.rs') as string;
  for (const line of ['tierstore: testing the {name}', 'tierstore: tested the {name} in {took} ms', 'tierstore {VERSION} listen={}']) {
    assert.ok(cli.includes(line), `"${line}" is not in server/cli.rs: update content/demo.html`);
  }
  const demo = page('demo');
  for (const shown of ['tierstore: testing the hot tier', 'tierstore: tested the cold tier', 'tierstore &lt;version&gt; listen=127.0.0.1:9000', '<span class="o">ok</span>', '<span class="o">ready</span>', '<span class="o">draining</span>']) {
    assert.ok(demo.includes(shown), `the demo no longer shows ${shown}`);
  }
});
