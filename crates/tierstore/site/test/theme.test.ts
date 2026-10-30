// The shared theme.
//
// Catches: an edited copy of theme.css. The theme is one file shared by kbf.dev, mopm.dev, ucdk.dev and tierstore.dev; each site
// copies it byte for byte and never changes its copy, so a change is made once, upstream, with a new digest. The
// digest below is the one the upstream spec (THEME.md) states; a hand edit, a reformat, a line-ending change or a
// stale copy changes it, and the test goes red. The built site must also ship that very file, unchanged, and load
// it before the site's own stylesheet (so the site's rules can override the theme and never the other way round).

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { readTags } from '../tools/check-dist';
import { outPath, PAGES } from '../tools/pages';
import { assetDir, buildReal, copyTree, plant, SITE } from './support';

/** theme.css sha256, as THEME.md states it. */
const THEME_SHA256 = 'ea04dacad9e29d2b7e5cf0afe34c9631b05881f0cb82f9e23d1688926f5d1993';
const sha256 = (b: Buffer): string => createHash('sha256').update(b).digest('hex');

test('static/theme.css is the pinned theme, byte for byte', () => {
  assert.equal(sha256(readFileSync(join(SITE, 'static', 'theme.css'))), THEME_SHA256);
});

test('a one byte edit of the theme changes the digest (the pin can fail)', () => {
  const bytes = Buffer.from(readFileSync(join(SITE, 'static', 'theme.css')));
  bytes[0] = bytes[0] === 0x2f ? 0x2e : 0x2f;
  assert.notEqual(sha256(bytes), THEME_SHA256);
});

test('the built site ships the pinned theme, and every page loads it before site.css', () => {
  const dist = buildReal('theme');
  assert.equal(sha256(readFileSync(join(dist, assetDir(dist), 'theme.css'))), THEME_SHA256);
  for (const page of PAGES.map((p) => outPath(p.route))) {
    const sheets = readTags(readFileSync(join(dist, page), 'utf8'))
      .tags.filter(([t, a]) => t === 'link' && a.some(([k, v]) => k === 'rel' && v === 'stylesheet'))
      .map(([, a]) => (a.find(([k]) => k === 'href') as [string, string])[1].replace(/^.*\//, ''));
    assert.deepEqual(sheets, ['theme.css', 'site.css'], page);
  }
  const edited = copyTree(dist, 'theme-edited');
  plant(edited, `${assetDir(edited)}/theme.css`, /^/, ' ');
  assert.notEqual(sha256(readFileSync(join(edited, assetDir(edited), 'theme.css'))), THEME_SHA256);
});
