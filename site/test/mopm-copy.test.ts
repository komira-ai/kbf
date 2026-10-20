// The copy of mopm.dev's Markdown renderer (tools/mopm/): which upstream commit it is, and that it is that and nothing else.
//
// Catches: an edit of the copied renderer that nobody recorded. tools/mopm/SOURCE.json names the upstream commit and, for
// each copied file, the sha256 of the upstream file. A file copied as is must have exactly that digest. links.ts is the
// one file with patches (the repository URL, the table of published docs, the prefix on a fragment into another doc);
// the manifest lists each patch as the upstream text and the text that replaces it, and the test holds both ways: the
// local file has the recorded digest, and undoing the patches gives the upstream digest. So the renderer, its
// linear-time guarantees and its attack-corpus tests (markdown-*.test.ts) are the ones mopm.dev has, and an update from
// upstream is a new commit, new digests and a reviewed diff.

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { SITE } from './support';

interface Patch {
  upstream: string;
  local: string;
}
interface Manifest {
  repo: string;
  commit: string;
  root: string;
  files: Record<string, { upstream: string; local?: string; patches?: Patch[] }>;
}

const DIR = join(SITE, 'tools', 'mopm');
const manifest = JSON.parse(readFileSync(join(DIR, 'SOURCE.json'), 'utf8')) as Manifest;
const sha256 = (s: string): string => createHash('sha256').update(s).digest('hex');
const read = (f: string): string => readFileSync(join(DIR, f), 'utf8');

/** The .ts files under dir, relative and '/'-separated, sorted. */
function tsFiles(dir: string, prefix = ''): string[] {
  return readdirSync(dir, { withFileTypes: true })
    .flatMap((e) => (e.isDirectory() ? tsFiles(join(dir, e.name), `${prefix}${e.name}/`) : e.name.endsWith('.ts') ? [`${prefix}${e.name}`] : []))
    .sort();
}

test('the manifest names an upstream commit and lists every copied file', () => {
  assert.equal(manifest.repo, 'komira-ai/mopm');
  assert.match(manifest.commit, /^[0-9a-f]{40}$/);
  assert.deepEqual(Object.keys(manifest.files).sort(), tsFiles(DIR));
  for (const [f, e] of Object.entries(manifest.files)) assert.match(e.upstream, /^[0-9a-f]{64}$/, f);
});

test('each copied file is the upstream file at the recorded commit, digest for digest', () => {
  for (const [f, e] of Object.entries(manifest.files)) {
    if (e.patches === undefined) {
      assert.equal(sha256(read(f)), e.upstream, `${f} differs from ${manifest.repo}@${manifest.commit.slice(0, 12)} ${manifest.root}/${f}: a copy is changed upstream, then copied again`);
    } else {
      // The patched file: the recorded local digest, and with the patches undone the upstream one.
      assert.equal(sha256(read(f)), e.local, `${f}: the local digest changed; record a patch in SOURCE.json`);
      let text = read(f);
      for (const p of [...e.patches].reverse()) {
        assert.equal(text.split(p.local).length, 2, `${f}: the patch text must occur exactly once: ${JSON.stringify(p.local.slice(0, 60))}`);
        text = text.replace(p.local, () => p.upstream);
      }
      assert.equal(sha256(text), e.upstream, `${f} with its patches undone is not the upstream file`);
    }
  }
});

test('the check can fail: a one byte edit of a copied file, or of a patch, changes the digest it is held to', () => {
  for (const [f, e] of Object.entries(manifest.files)) {
    const edited = `${read(f)} `;
    assert.notEqual(sha256(edited), e.patches === undefined ? e.upstream : e.local, f);
  }
  const links = manifest.files['links.ts'];
  assert.equal(links.patches !== undefined && links.patches.length > 0, true);
});

test('the copy keeps its guarantees: the parser and renderer have no dependency outside the copy and the docs table', () => {
  for (const f of Object.keys(manifest.files)) {
    for (const m of read(f).matchAll(/from '(\.[^']*)'/g)) {
      const target = join(f, '..', m[1]).replace(/\.js$/, '');
      assert.equal(
        ['doc', 'links', 'markdown/inline', 'markdown/parse', 'markdown/render', '../docs-table'].includes(target.split('\\').join('/')),
        true,
        `${f} imports ${m[1]}`,
      );
    }
  }
});
