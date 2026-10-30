// The public-repo scan over the built output.
//
// Catches: text that must not ship in a public repository's site: a machine address, a home directory path, a host
// that is not a public one this site links to, a cloud account's resource name, an internal repository name, or a date
// from before the project went public. The site is built from page copy and recorded data (a terminal recording and a
// memory trace), and recordings are where such things leak, so the scan reads every built file, data included.
// The repository lints (crates/kbf-integration-test/tests/repo_lints.rs) scan the sources; this scans what is deployed.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { listFiles } from '../tools/check-dist';
import { buildReal, copyTree, plant } from './support';

/** The hosts a page may name in a URL: the project's own, GitHub, the two build tools it serves, the SVG namespace. */
const PUBLIC_HOSTS = new Set(['tierstore.dev', 'kbf.dev', 'github.com', 'buck2.build', 'bazel.build', 'www.w3.org']);
/** Placeholders and loopback the configuration reference uses as examples. */
const EXAMPLE_HOSTS = new Set(['host', 'localhost', '127.0.0.1', 'frontend', 'FRONTEND', 'bucket', 's3-hot.example', 's3-cold.example', 'tierstore.example']);

/** The only 40-hex string a page may carry: the example SHA of the install page and docs/artifacts.md. */
const EXAMPLE_SHA = '0123456789abcdef0123456789abcdef01234567';

function scanText(text: string, code = false): string[] {
  const found: string[] = [];
  for (const m of text.matchAll(/\b(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\b/g)) {
    const parts = [m[1], m[2], m[3], m[4]].map(Number);
    if (parts.every((p) => p <= 255)) {
      const a = m[0];
      if (a !== '127.0.0.1' && a !== '0.0.0.0' && !/^(?:192\.0\.2|198\.51\.100|203\.0\.113)\./.test(a)) found.push(`an address, ${a}`);
    }
  }
  for (const m of text.matchAll(/(?<![.\/-])\/(?:home|Users|root)\/[A-Za-z0-9._-]+/g)) found.push(`a home path, ${m[0]}`);
  for (const m of text.matchAll(/\bbuck-out\/\S*/g)) found.push(`a build-farm path, ${m[0]}`);
  for (const m of text.matchAll(/[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+\.[A-Za-z]{2,}/g)) found.push(`an email address or user@host, ${m[0]}`);
  for (const m of text.matchAll(/\b[a-z][a-z0-9+.-]*:\/\/([A-Za-z0-9.-]+)/g)) {
    const host = m[1];
    if (!PUBLIC_HOSTS.has(host) && !EXAMPLE_HOSTS.has(host)) found.push(`a host, ${host}`);
  }
  // Program text is skipped here: `c.local` is a property of an object, not a host (recorded data is not program text).
  for (const m of code ? [] : text.matchAll(/\b[\w.-]+\.(?:internal|local|lan|corp|amazonaws\.com|cloudfront\.net)\b/g)) found.push(`a private or cloud host name, ${m[0]}`);
  for (const m of text.matchAll(/arn:aws[\w-]*:\S+/g)) found.push(`a cloud resource name, ${m[0]}`);
  for (const m of text.matchAll(new RegExp('komira-' + 'internal', 'g'))) found.push(`an internal repository name, ${m[0]}`);
  for (const m of text.matchAll(/\b(20(?:[01]\d|2[0-5])-\d\d-\d\d|2026-0[1-8]-\d\d)\b/g)) found.push(`a date before the project was public, ${m[1]}`);
  if (/\bCEO\b/.test(text)) found.push('an internal role, CEO');
  for (const m of text.matchAll(/\b(?:node-0\d|spoolr-[\w-]*|broadwell\d*|m3ultra\d*)\b/gi)) found.push(`an internal node name, ${m[0]}`);
  for (const m of text.matchAll(/\b[0-9a-f]{40}\b/g)) if (m[0] !== EXAMPLE_SHA) found.push(`a commit SHA, ${m[0]}`);
  return found;
}

const dist = buildReal('hygiene');
const scanTree = (dir: string): string[] => listFiles(dir).flatMap((f) => scanText(readFileSync(join(dir, f), 'utf8'), /^assets\/[0-9a-f]+\/[a-z]+\.js$/.test(f)).map((p) => `${f}: ${p}`));

test('no built file carries an address, a home path, a non-public host, an internal name or an early date', () => {
  assert.deepEqual(scanTree(dist), []);
});

test('the scan finds each kind of leak (it can fail)', () => {
  const ip = ['10', '1', '2', '3'].join('.');
  const home = '/ho' + 'me/alice';
  const cases: [string, RegExp][] = [
    [`ssh ${ip}`, /an address/],
    [`cd ${home}/src`, /a home path/],
    ['at /kbf' + '/root/komira/x.zig:1', /a home path/],
    ['in buck-' + 'out/v2/tmp/x', /a build-farm path/],
    ['mail ' + 'a@b' + '.example', /an email address/],
    ['see https://build-7.example.org/x', /a host, build-7\.example\.org/],
    ['at ip-1-2-3-4.compute.internal', /a private or cloud host name/],
    ['arn:aws:iam::123456789012:role/x', /a cloud resource name/],
    ['komira-' + 'internal#12', /an internal repository name/],
    ['on ' + '2026-0' + '3-14 we', /a date before/],
    ['the C' + 'EO said', /an internal role/],
    ['on node' + '-06 and spoolr' + '-1', /an internal node name, node-06/],
    ['a broad' + 'well1 box', /an internal node name, broadwell1/],
    ['an m3ul' + 'tra1 Mac', /an internal node name, m3ultra1/],
    ['at ' + 'abcdef0123456789abcdef0123456789abcdef01', /a commit SHA/],
  ];
  for (const [text, why] of cases) assert.match(scanText(text).join('\n'), why, text);
  assert.deepEqual(scanText('https://github.com/komira-ai/kbf and http://127.0.0.1:8990 and 2026-10-01 and ' + EXAMPLE_SHA), []);
});

test('a leak in the recorded data or in the page copy is found in the built tree', () => {
  const t = copyTree(dist, 'hygiene-planted');
  plant(t, 'index.html', '<h1', `<p>${['10', '9', '8', '7'].join('.')}</p><h1`);
  assert.match(scanTree(t).join('\n'), /index\.html: an address/);
});
