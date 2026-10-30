// The statements of the pages about what tierstore does not do, and the strings of the demo, held to the code.
//
// Catches: a page that says "not built" about something the code now does, and a demo line that is no longer what the
// program prints. The site says the per-node cache is not built, that `[local_cache]` and `cache = true` are refused,
// that there is no metrics endpoint, and that the binary does not use `CachedStore`; the demo quotes the bodies of the
// probes. Each of those is read from the crate's source here, so when the code changes the test fails and says which
// page to update. The crate's source is the directory above the site's (crates/tierstore/src, and src/ once the
// crate is a repository of its own).

import assert from 'node:assert/strict';
import { readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { test } from 'node:test';
import { read, SITE } from './support';

const SRC = resolve(SITE, '..', 'src');
const isTestFile = (f: string): boolean => /(_tests|_testing|testing)\.rs$/.test(f);

/** The binary's own source: main.rs and the server module's files that are not tests. */
function binarySource(): Map<string, string> {
  const files = new Map<string, string>([['main.rs', read(SRC, 'main.rs')]]);
  for (const f of readdirSync(join(SRC, 'server')).sort()) {
    if (f.endsWith('.rs') && !isTestFile(f)) files.set(`server/${f}`, read(SRC, 'server', f));
  }
  return files;
}

/** What in the source no longer matches the pages. Each problem names the pages to change. */
export function staleClaims(src: ReadonlyMap<string, string>): string[] {
  const out: string[] = [];
  const all = [...src.entries()];
  const config = src.get('server/config.rs') ?? '';
  if (!/a cache is not supported yet/.test(config) || !/\[local_cache\]/.test(config)) {
    out.push('config.rs no longer refuses [local_cache] and `cache = true` as not supported: update content/config.html, content/why.html and content/index.html (the status table)');
  }
  for (const [name, text] of all) {
    if (/"\/metrics?z?"|prometheus/i.test(text)) out.push(`${name} has a metrics route or exporter: update content/operations.html ("Metrics"), content/why.html ("Where it stops") and content/index.html`);
    if (/\bCachedStore\b/.test(text)) out.push(`${name} uses CachedStore: the binary has a cache now; update content/why.html ("The per-node read-through cache is not in the binary") and content/index.html`);
  }
  const routes = new Set([...(src.get('server/mod.rs') ?? '').matchAll(/\(true, "(\/[a-z]+)"/g)].map((m) => m[1]));
  const want = ['/healthz', '/readyz', '/usagez'];
  if ([...routes].sort().join() !== want.sort().join()) {
    out.push(`the probe routes in server/mod.rs are ${[...routes].sort().join(', ')}, not ${want.join(', ')}: update content/operations.html ("Probes") and content/demo.html`);
  }
  return out;
}

const page = (name: string): string => read(SITE, 'content', `${name}.html`);

test('the pages say what they say about the cache and metrics, so the code check below is not vacuous', () => {
  assert.match(page('operations'), /There is no metrics endpoint/);
  assert.match(page('why'), /The <code>tierstore<\/code> binary does not use it/);
  assert.match(page('why'), /refuses <code>cache = true<\/code>/);
  assert.match(page('index'), /not built: the file refuses it/);
});

test('the code still matches those statements', () => {
  assert.deepEqual(staleClaims(binarySource()), []);
});

test('the check can fail: a metrics route, a CachedStore use and a lifted refusal are each reported with the page to change', () => {
  const real = binarySource();
  const with_ = (name: string, f: (s: string) => string): Map<string, string> => new Map([...real].map(([k, v]) => [k, k === name ? f(v) : v]));
  assert.match(staleClaims(with_('server/mod.rs', (s) => s + '\nconst M: &str = "/metrics";')).join('\n'), /metrics route.*content\/operations\.html/);
  assert.match(staleClaims(with_('main.rs', (s) => s + '\nuse tierstore::CachedStore;')).join('\n'), /uses CachedStore.*content\/why\.html/);
  assert.match(staleClaims(with_('server/config.rs', (s) => s.replace('a cache is not supported yet', 'ok'))).join('\n'), /no longer refuses \[local_cache\].*content\/config\.html/);
  assert.match(staleClaims(with_('server/mod.rs', (s) => s.replace('"/usagez"', '"/usagez2"'))).join('\n'), /probe routes/);
});

test('every output line of the demo is a string the program prints', () => {
  const mod = binarySource().get('server/mod.rs') as string;
  for (const body of ['"ok"', '"ready"', '"draining"']) assert.ok(mod.includes(body), `${body} is not in server/mod.rs: update content/demo.html`);
  assert.ok(mod.includes('no usage yet, {secs}s since start'), 'the /usagez body changed: update content/demo.html and content/operations.html');
  assert.ok(mod.includes('usage published {secs}s ago'), 'the /usagez body changed: update content/demo.html and content/operations.html');
  const cli = binarySource().get('server/cli.rs') as string;
  for (const line of ['tierstore: testing the {name}', 'tierstore: tested the {name} in {took} ms', 'tierstore {version} listen={addr}']) {
    assert.ok(cli.includes(line), `"${line}" is not in server/cli.rs: update content/demo.html`);
  }
  const demo = page('demo');
  for (const shown of ['tierstore: testing the hot tier', 'tierstore: tested the cold tier', 'tierstore &lt;version&gt; listen=127.0.0.1:9000', '<span class="o">ok</span>', '<span class="o">ready</span>', '<span class="o">draining</span>']) {
    assert.ok(demo.includes(shown), `the demo no longer shows ${shown}`);
  }
});
