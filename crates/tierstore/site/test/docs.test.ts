// The docs pages are the operator guide (crates/tierstore/docs/*.md), rendered by tools/docs.ts.
//
// Catches: a page of the guide that does not reach the site (or one that is there without a file behind it), a link to
// another page of the guide that is dead, markup the renderer does not read that would be shown wrongly, text that is
// not escaped, and a change to the guide that does not change the page. The statements of the guide are held to the
// code by a test of the crate (src/server/docs_tests.rs); this holds the site to the guide.

import assert from 'node:assert/strict';
import { cpSync, existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import { test } from 'node:test';
import { buildSite } from '../tools/build';
import { loadDocs, renderMarkdown, slug } from '../tools/docs';
import { buildReal, DOCS, REPO, scratch, SITE } from './support';

const stems = readdirSync(DOCS).filter((f) => f.endsWith('.md')).map((f) => f.replace(/\.md$/, '')).sort();
const dist = buildReal('docs');
const built = (route: string): string => readFileSync(join(dist, route.replace(/^\//, ''), 'index.html'), 'utf8');

/** A build of the real site with `docs` as the guide. */
function buildWith(name: string, docs: string): string {
  const out = join(scratch(name), 'dist');
  buildSite({ browserDir: join(SITE, 'build', 'browser'), staticDir: join(SITE, 'static'), contentDir: join(SITE, 'content'), docsDir: docs, outDir: out, check: true });
  return out;
}

/** A copy of the guide to change. */
function copyDocs(name: string): string {
  const dir = join(scratch(name), 'docs');
  cpSync(DOCS, dir, { recursive: true });
  return dir;
}

test('every file of docs/ is a page of the site, under /docs/<name>/, and /docs/ lists them all', () => {
  assert.ok(stems.length >= 7, `the guide has ${stems.length} pages`);
  const index = built('/docs/');
  for (const stem of stems) {
    assert.ok(existsSync(join(dist, 'docs', stem, 'index.html')), `docs/${stem}.md has no page`);
    assert.ok(index.includes(`href="/docs/${stem}/"`), `/docs/ does not list ${stem}`);
  }
});

test('the guide is read in its order, and a page the site has not heard of follows by name', () => {
  const order = loadDocs(DOCS).map((d) => d.stem);
  assert.deepEqual(order.slice(0, 3), ['overview', 'install', 'keys']);
  const dir = copyDocs('docs-order');
  writeFileSync(join(dir, 'zzz.md'), '# A page\n\nText.\n');
  const after = loadDocs(dir).map((d) => d.stem);
  assert.equal(after[after.length - 1], 'zzz');
});

test('a page added to docs/ is built, and a change to the guide changes its page', () => {
  const dir = copyDocs('docs-add');
  writeFileSync(join(dir, 'metrics.md'), '# Metrics\n\nThe counters.\n\n## Names\n\nA name.\n');
  const out = buildWith('docs-add-out', dir);
  assert.match(readFileSync(join(out, 'docs', 'metrics', 'index.html'), 'utf8'), /<h2 id="names">Names<\/h2>/);
  assert.match(readFileSync(join(out, 'docs', 'index.html'), 'utf8'), /href="\/docs\/metrics\/"/);
  const text = readFileSync(join(dir, 'limits.md'), 'utf8');
  writeFileSync(join(dir, 'limits.md'), text.replace('No TLS on the S3 listener.', 'A planted sentence.'));
  assert.match(readFileSync(join(buildWith('docs-change', dir), 'docs', 'limits', 'index.html'), 'utf8'), /A planted sentence\./);
});

test('a link to a page of the guide is a link when the page is there and its label when it is not (the metrics page, until it lands)', () => {
  const md = 'See [the metrics](metrics.md#names) and [keys](keys.md#rotation).\n';
  const without = renderMarkdown(`# T\n\n${md}`, new Set(['keys']));
  assert.match(without.body, /See the metrics and <a href="\/docs\/keys\/#rotation">keys<\/a>\./);
  const withIt = renderMarkdown(`# T\n\n${md}`, new Set(['keys', 'metrics']));
  assert.match(withIt.body, /<a href="\/docs\/metrics\/#names">the metrics<\/a>/);
});

test('text is escaped, in prose, code, tables and fences; a quote in a link target cannot leave its attribute', () => {
  const md = ['# T <x>', '', 'a <commit> & "q" `<b>` **<i>**', '', '| a | b |', '|---|---|', '| `<c>` | <d> |', '', '```text', '<pre> & "x"', '```', ''].join('\n');
  const { body } = renderMarkdown(md, new Set());
  assert.match(body, /<h1>T &lt;x&gt;<\/h1>/);
  assert.match(body, /a &lt;commit&gt; &amp; &quot;q&quot; <code>&lt;b&gt;<\/code> <strong>&lt;i&gt;<\/strong>/);
  assert.match(body, /<td><code>&lt;c&gt;<\/code><\/td><td>&lt;d&gt;<\/td>/);
  assert.match(body, /<pre>&lt;pre&gt; &amp; &quot;x&quot;<\/pre>/);
  const quoted = renderMarkdown('# T\n\n[x](https://a.example/"onclick=1)\n', new Set()).body;
  assert.match(quoted, /<a href="https:\/\/a\.example\/&quot;onclick=1">x<\/a>/);
  assert.equal(/<(?!\/?(?:h1|p|code|strong|div|table|thead|tbody|tr|th|td|pre)\b)/.test(body), false, body);
});

test('markup the renderer does not read is an error, not a page that is quietly wrong', () => {
  assert.throws(() => renderMarkdown('# T\n\n  - nested\n', new Set()), /indented/);
  assert.throws(() => renderMarkdown('# T\n\n[x](http://insecure.example/)\n', new Set()), /only https links/);
  assert.throws(() => renderMarkdown('# T\n\n[x](nope.txt)\n', new Set()), /only https links/);
  assert.throws(() => renderMarkdown('# T\n\n```sh\nunclosed\n', new Set()), /not closed/);
  assert.throws(() => renderMarkdown('# T\n\n| a | b |\n| c | d |\n', new Set()), /separator/);
  assert.throws(() => renderMarkdown('# T\n\n| a | b |\n|---|---|\n| c |\n', new Set()), /cells/);
  assert.throws(() => renderMarkdown('Text\n', new Set()), /no # heading/);
  assert.throws(() => renderMarkdown('# T\n\n# U\n', new Set()), /second # heading/);
  assert.throws(() => renderMarkdown('# T\n\nan **bold\n', new Set()), /does not read/);
});

test('headings get the anchors the test of the crate computes, repeats are numbered, and a "Not in a release yet" section is marked', () => {
  assert.equal(slug('The start-up self-test'), 'the-start-up-self-test');
  assert.equal(slug('`[hot_store]` and `[cold_store]`'), 'hot_store-and-cold_store');
  const { body } = renderMarkdown('# T\n\n## Not in a release yet\n\nx\n\n## Not in a release yet\n\ny\n', new Set());
  assert.match(body, /<h2 id="not-in-a-release-yet">Not in a release yet <span class="tag tag-planned">planned, not built<\/span><\/h2>/);
  assert.match(body, /<h2 id="not-in-a-release-yet-2">/);
  assert.match(built('/docs/configuration/'), /<h2 id="hot_store-and-cold_store">/);
  assert.match(built('/docs/operations/'), /<h2 id="the-start-up-self-test">/);
});

test('the first paragraph is the lede, and the page names the guide in its navigation with the current page marked', () => {
  const page = built('/docs/keys/');
  assert.match(page, /<h1>Keys and roles<\/h1>\n<p class="lede">/);
  assert.match(page, /<a href="\/docs\/keys\/" aria-current="page">Keys and roles<\/a>/);
  assert.match(page, /<a href="\/docs\/" aria-current="page">Docs<\/a>/);
});

test('the workflow runs the site tests when the guide or the crate source changes, on a pull request and on a push', () => {
  const wf = readFileSync(join(REPO, '.github', 'workflows', 'tierstore-site.yml'), 'utf8');
  const docs = relative(REPO, DOCS).split(sep).join('/');
  // test/claims.test.ts reads the crate's src/ (the directory beside docs/): a code change that makes a page wrong must fail on its own pull request.
  const src = `${docs.replace(/\/docs$/, '')}/src`;
  for (const dir of [docs, src]) {
    const glob = `      - ${dir}/**\n`;
    assert.equal(wf.split(glob).length - 1, 2, `the workflow's paths: filters (pull_request and push) each name ${glob.trim()}`);
  }
});
