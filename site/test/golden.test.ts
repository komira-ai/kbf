// Golden output: the built tree is exactly the committed manifest (every file's path and sha256).
//
// Catches: any change to the output nobody meant. A template byte, a content word, a stylesheet, a compiler upgrade
// that changes the emitted JavaScript, a file that appears or disappears, an asset id that moves. The manifest is
// reviewed in the pull request that changes it (`npm run golden` rewrites it). Also catches a build that is not
// deterministic (two builds of the same inputs differ), which would make a deploy upload what did not change.

import assert from 'node:assert/strict';
import { readFileSync, unlinkSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { manifestOf } from '../tools/manifest';
import { buildReal, copyTree, plant, SITE } from './support';

const golden = readFileSync(join(SITE, 'test', 'golden.manifest'), 'utf8');

test('the built tree matches the committed manifest', () => {
  const got = manifestOf(buildReal('golden-a'));
  assert.equal(got, golden, 'the output changed: if that is meant, run `npm run golden` and commit test/golden.manifest');
});

test('two builds of the same inputs are byte for byte the same', () => {
  assert.equal(manifestOf(buildReal('golden-b')), manifestOf(buildReal('golden-c')));
});

test('the manifest sees a one byte change in a page and a missing file (the check can fail)', () => {
  const dist = buildReal('golden-d');
  const clean = manifestOf(dist);
  const changed = copyTree(dist, 'golden-e');
  plant(changed, 'index.html', '<h1', '<h2');
  assert.notEqual(manifestOf(changed), clean);
  const missing = copyTree(dist, 'golden-f');
  unlinkSync(join(missing, '404.html'));
  assert.notEqual(manifestOf(missing), clean);
});
