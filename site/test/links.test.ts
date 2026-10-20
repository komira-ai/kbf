// The link checker over the built HTML.
//
// Catches: a page that links to a page, asset or #fragment that is not in the build: a renamed route, a deleted
// heading or id, a typo, a link spelled '/x' where the router serves only '/x/', an asset id that went stale. A
// broken link on a deployed site is a 404 for a visitor and nothing in the build would say so.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { checkLinks, checkRepoLinks, fileOf } from '../tools/links';
import { buildReal, copyTree, plant, REPO } from './support';

const dist = buildReal('links');

// Links in the repository's own documents that name a heading which is not there (a heading was renamed, the link was
// not). The docs are not rewritten by this site, so each is listed with what it should be: a new one fails the test,
// and so does one that is fixed in the document (delete its line here).
const KNOWN_DANGLING: readonly string[] = [
  'docs/design/worker-protocol/index.html: <a href="/docs/api/#doc-post-v1denied-and-delete-v1denied"> names no id #doc-post-v1denied-and-delete-v1denied on docs/api/index.html',
];

test('every internal link, asset and #fragment of the built site resolves (but the known broken links of the docs)', () => {
  assert.deepEqual(checkLinks(dist).filter((p) => !KNOWN_DANGLING.includes(p)), []);
});

test('the known broken links of the docs are still broken (the list does not go stale)', () => {
  const found = checkLinks(dist);
  assert.deepEqual(KNOWN_DANGLING.filter((p) => !found.includes(p)), []);
});

test('every link of a doc to a file of the repository names a file that is there', () => {
  assert.deepEqual(checkRepoLinks(dist, REPO), []);
});

test('a link to a repository file that is not there is reported', () => {
  const t = copyTree(dist, 'links-repo');
  plant(t, 'docs/architecture/detail/index.html', /href="https:\/\/github\.com\/komira-ai\/kbf\/blob\/main\/(crates\/[^"#]*)"/, 'href="https://github.com/komira-ai/kbf/blob/main/$1.gone"');
  assert.match(checkRepoLinks(t, REPO).join('\n'), /docs\/architecture\/detail\/index\.html: <a href="[^"]*\.gone"> names crates\/[^ ]*\.gone, which is not in the repository/);
});

test('a link between two docs to a heading that is gone is reported, and one to a heading that is there is not', () => {
  const t = copyTree(dist, 'links-doc-fragment');
  const route = 'docs/design/daemon/index.html';
  // docs/design/capabilities.md links to this heading of docs/design/daemon.md.
  plant(t, route, 'id="doc-the-container-driver"', 'id="doc-the-container-driver-renamed"');
  const problems = checkLinks(t).filter((p) => !KNOWN_DANGLING.includes(p));
  assert.match(problems.join('\n'), /docs\/design\/capabilities\/index\.html: <a href="\/docs\/design\/daemon\/#doc-the-container-driver"> names no id #doc-the-container-driver on docs\/design\/daemon\/index\.html/);
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
  assert.match(problems.join('\n'), /index\.html: <a href="\/quickstarts\/"> names no file/);
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
