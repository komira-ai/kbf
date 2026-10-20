// The public-repo scan over the built output.
//
// Catches: text that must not ship in a public repository's site: a machine address, a home directory path, a host
// that is not a public one this site links to, a cloud account's resource name, an internal repository name, or a date
// from before the project went public. The site is built from page copy and recorded data (a terminal recording and a
// memory trace), and recordings are where such things leak, so the scan reads every built file, data included.
// The repository lints (crates/kbf-it/tests/repo_lints.rs) scan the sources; this scans what is deployed.
//
// The pages rendered from the repository's Markdown (docs-table.ts) are scanned too, with the rules of prose: they
// cite public references (a standard, a vendor's page), so a URL host must be on DOC_HOSTS (a new reference is added
// there on purpose), a placeholder host (`*.example`) is fine, a path under /root/ and a build-farm path are things a
// guide documents, and an address, a home directory, a private or cloud host, an account resource name, an internal
// repository name or an early date are refused as everywhere.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { listFiles } from '../tools/check-dist';
import { DOCS, docRoute } from '../tools/docs-table';
import { outPath } from '../tools/pages';
import { buildReal, copyTree, plant } from './support';

/** The hosts a page may name in a URL: the project's own, GitHub, the two build tools it serves, the SVG namespace. */
const PUBLIC_HOSTS = new Set(['kbf.dev', 'github.com', 'buck2.build', 'bazel.build', 'www.w3.org']);
/** Placeholders and loopback the configuration reference uses as examples. */
const EXAMPLE_HOSTS = new Set(['host', 'localhost', '127.0.0.1', 'frontend', 'FRONTEND', 'bucket']);

/** Public references the repository's docs link to, and the placeholders their examples use (not hostnames of ours). */
const DOC_HOSTS = new Set([
  'developer.apple.com', 'support.apple.com', 'www.apple.com', 'gdmf.apple.com', 'keith.github.io', 'docs.rs', 'slsa.dev',
  'token.actions.githubusercontent.com', 'docs.github.com', 'ubuntu.com', 'bootc.dev', 'thinkingmachines.ai', 'rustsec.org',
  'raft.github.io', 'ml-explore.github.io', 'developer.jamf.com', 'crates.io', 'coreos.github.io', 'arcbox.dev',
  'www.theodorehq.com', 'www.buildbuddy.io', 'suggestions.simplemdm.com', 'rootlesscontaine.rs', 'research.google.com',
  'micromdm.io', 'mdmcert.download', 'man.openbsd.org', 'kured.dev', 'fwupd.github.io', 'fleetdm.com', 'docs.ansible.com',
  'doc.rust-lang.org', 'developercertificate.org', 'cyclonedx.org', 'cli.github.com', '...', 'user', 'repo', 'kbf',
]);
/** File names that end like a host name (a cgroup file, a Buck2 config file) and the placeholder name a design's examples use. */
const DOC_NOT_HOSTS = new Set(['memory.events.local', 'buckconfig.local', 'farm.internal']);
const isExample = (host: string): boolean => /(^|\.)example(\.|$)/.test(host);

type Kind = 'page' | 'code' | 'doc';

function scanText(text: string, kind: Kind = 'page'): string[] {
  const doc = kind === 'doc';
  const found: string[] = [];
  for (const m of text.matchAll(/\b(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\b/g)) {
    const parts = [m[1], m[2], m[3], m[4]].map(Number);
    if (parts.every((p) => p <= 255)) {
      const a = m[0];
      if (a !== '127.0.0.1' && a !== '0.0.0.0' && !/^(?:192\.0\.2|198\.51\.100|203\.0\.113)\./.test(a)) found.push(`an address, ${a}`);
    }
  }
  for (const m of text.matchAll(doc ? /(?<![.\/-])\/(?:home|Users)\/[A-Za-z0-9._-]+/g : /(?<![.\/-])\/(?:home|Users|root)\/[A-Za-z0-9._-]+/g)) found.push(`a home path, ${m[0]}`);
  for (const m of doc ? [] : text.matchAll(/\bbuck-out\/\S*/g)) found.push(`a build-farm path, ${m[0]}`);
  for (const m of text.matchAll(/[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+\.[A-Za-z]{2,}/g)) if (!(doc && /@example\.(com|org|net)$/.test(m[0]))) found.push(`an email address or user@host, ${m[0]}`);
  for (const m of text.matchAll(/\b[a-z][a-z0-9+.-]*:\/\/([A-Za-z0-9.-]+)/g)) {
    const host = m[1];
    const known = doc ? DOC_HOSTS.has(host) || isExample(host) : false;
    if (!PUBLIC_HOSTS.has(host) && !EXAMPLE_HOSTS.has(host) && !known) found.push(`a host, ${host}`);
  }
  // Program text is skipped here: `c.local` is a property of an object, not a host (recorded data is not program text).
  for (const m of kind === 'code' ? [] : text.matchAll(/\b[\w.-]+\.(?:internal|local|lan|corp|amazonaws\.com|cloudfront\.net)\b/g)) {
    if (!(doc && (DOC_NOT_HOSTS.has(m[0]) || isExample(m[0])))) found.push(`a private or cloud host name, ${m[0]}`);
  }
  for (const m of text.matchAll(/arn:aws[\w-]*:\S+/g)) found.push(`a cloud resource name, ${m[0]}`);
  for (const m of text.matchAll(new RegExp('komira-' + 'internal', 'g'))) found.push(`an internal repository name, ${m[0]}`);
  for (const m of text.matchAll(/\b(20(?:[01]\d|2[0-5])-\d\d-\d\d|2026-0[1-8]-\d\d)\b/g)) found.push(`a date before the project was public, ${m[1]}`);
  return found;
}

const dist = buildReal('hygiene');
// The docs and the install page, which quotes them (its commands and paths are the guides').
const DOC_FILES = new Set([...DOCS.map((d) => outPath(docRoute(d))), 'install/index.html']);
const kindOf = (f: string): Kind => (/^assets\/[0-9a-f]+\/[a-z]+\.js$/.test(f) ? 'code' : DOC_FILES.has(f) ? 'doc' : 'page');
const scanTree = (dir: string): string[] => listFiles(dir).flatMap((f) => scanText(readFileSync(join(dir, f), 'utf8'), kindOf(f)).map((p) => `${f}: ${p}`));

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
  ];
  for (const [text, why] of cases) assert.match(scanText(text).join('\n'), why, text);
  assert.deepEqual(scanText('https://github.com/komira-ai/kbf and http://127.0.0.1:8990 and 2026-10-01'), []);
});

test('a leak in the recorded data or in the page copy is found in the built tree', () => {
  const t = copyTree(dist, 'hygiene-planted');
  plant(t, 'index.html', '<h1', `<p>${['10', '9', '8', '7'].join('.')}</p><h1`);
  assert.match(scanTree(t).join('\n'), /index\.html: an address/);
});

test('the scan of rendered docs refuses the same leaks, a host not on the list of references, and allows what a guide documents', () => {
  const ip = ['10', '1', '2', '3'].join('.');
  const cases: [string, RegExp][] = [
    [`ssh ${ip}`, /an address/],
    ['cd /ho' + 'me/alice/src', /a home path/],
    ['see https://build-7.farm.net/x', /a host, build-7\.farm\.net/],
    ['at ip-1-2-3-4.compute.internal', /a private or cloud host name/],
    ['arn:aws:iam::123456789012:role/x', /a cloud resource name/],
    ['komira-' + 'internal#12', /an internal repository name/],
    ['on ' + '2026-0' + '3-14 we', /a date before/],
    ['mail alice@corp-mail.test', /an email address/],
  ];
  for (const [text, why] of cases) assert.match(scanText(text, 'doc').join('\n'), why, text);
  assert.deepEqual(
    scanText('token at /root/kbf.token, output in buck-out/v2/tmp, see https://slsa.dev/x, https://farm.example/y, https://x.example.internal/z, you@example.com', 'doc'),
    [],
  );
  // The same text on a hand-written page is refused: the relaxed rules are for the docs only.
  assert.notDeepEqual(scanText('see https://slsa.dev/x and /root/kbf.token', 'page'), []);
});

test('a leak in a rendered doc is found in the built tree', () => {
  const t = copyTree(dist, 'hygiene-doc');
  plant(t, 'docs/api/index.html', '<h1', `<p>${['10', '9', '8', '7'].join('.')}</p><h1`);
  assert.match(scanTree(t).join('\n'), /docs\/api\/index\.html: an address/);
});
