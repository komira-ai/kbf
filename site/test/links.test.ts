// The link checker over the built HTML.
//
// Catches: a page that links to a page, asset or #fragment that is not in the build: a renamed route, a deleted
// heading or id, a typo, a link spelled '/x' where the router serves only '/x/', an asset id that went stale. A
// broken link on a deployed site is a 404 for a visitor and nothing in the build would say so.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { checkLinks, fileOf } from '../tools/links';
import { buildReal, copyTree, plant } from './support';

const dist = buildReal('links');

test('every internal link, asset and #fragment of the built site resolves', () => {
  assert.deepEqual(checkLinks(dist), []);
});

test('the checker has something to check: the home page links to the pages it names', () => {
  const have = new Set(['index.html', 'quickstart/index.html']);
  assert.equal(fileOf('/', have), 'index.html');
  assert.equal(fileOf('/quickstart/', have), 'quickstart/index.html');
  assert.equal(fileOf('/quickstart', have), null, 'the router does not serve /x for /x/index.html');
  assert.equal(fileOf('/nope/', have), null);
});

test('a link to a page that is not built is reported', () => {
  const t = copyTree(dist, 'links-page');
  plant(t, 'index.html', 'href="/quickstart/"', 'href="/quickstarts/"');
  const problems = checkLinks(t);
  assert.equal(problems.length > 0, true);
  assert.match(problems[0], /index\.html: <a href="\/quickstarts\/"> names no file/);
});

test('a link without the trailing slash is reported', () => {
  const t = copyTree(dist, 'links-slash');
  plant(t, 'index.html', 'href="/quickstart/"', 'href="/quickstart"');
  assert.match(checkLinks(t).join('\n'), /names no file/);
});

test('a #fragment whose heading was deleted is reported', () => {
  const t = copyTree(dist, 'links-fragment');
  plant(t, 'index.html', 'id="demo-title"', 'id="demo-heading"');
  assert.match(checkLinks(t).join('\n'), /names no id #demo-title on index\.html/);
});

test('a stylesheet or script at an asset id that is not built is reported', () => {
  const t = copyTree(dist, 'links-asset');
  plant(t, 'install/index.html', /\/assets\/[0-9a-f]+\/theme\.css/, '/assets/0000000000000000/theme.css');
  assert.match(checkLinks(t).join('\n'), /install\/index\.html: <link href="\/assets\/0+\/theme\.css"> names no file/);
});
