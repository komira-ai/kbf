// The white-paper slot.
//
// Catches: a link, a name or a placeholder for a white paper reaching a built page before the paper is final, and a
// footer that stops showing the link once WHITE_PAPER_URL is set. While the paper is a draft the site says nothing
// about it: no visible placeholder, no hidden element, no navigation entry.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { listFiles } from '../tools/check-dist';
import { footer } from '../tools/layout';
import { NAV, WHITE_PAPER_URL } from '../tools/pages';
import { buildReal } from './support';

const dist = buildReal('whitepaper');

test('while WHITE_PAPER_URL is null, no built file mentions a white paper', () => {
  assert.equal(WHITE_PAPER_URL, null, 'the paper is final: also add a page that links to it, and update this test');
  const hits = listFiles(dist).filter((f) => /white[\s_-]*paper/i.test(readFileSync(join(dist, f), 'utf8')));
  assert.deepEqual(hits, []);
  assert.equal(NAV.some((n) => /paper/i.test(n.label + n.route)), false);
});

test('the footer has the link when there is a URL and not when there is none (the check can fail)', () => {
  assert.doesNotMatch(footer(null), /paper/i);
  assert.match(footer('https://example.org/paper.html'), /<a href="https:\/\/example\.org\/paper\.html">White paper<\/a>/);
});
