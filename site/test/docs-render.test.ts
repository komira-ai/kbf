// The docs pages as built: a page for every doc of the table, its status tag, its nav, its index entry.
//
// Catches: a doc that renders no page, a page with no status tag or the wrong one (a design shown as working would
// mislead the reader about what the software does), a nav that lacks a page of its section or marks the wrong page as the
// current one, an index that lists a page twice or not at all, and a doc that starts showing unsupported Markdown as
// literal text (the renderer's fallbacks) without anyone looking.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { readTags } from '../tools/check-dist';
import { buildDocPages } from '../tools/docs';
import { DOCS, docRoute, STATUSES } from '../tools/docs-table';
import { outPath, PAGES } from '../tools/pages';
import { REPO, buildReal, copyTree, plant } from './support';

const dist = buildReal('docs-render');
const page = (d: string, route: string): string => readFileSync(join(d, outPath(route)), 'utf8');

/** The text of every element with the class, in order. */
function textsOfClass(html: string, cls: string): string[] {
  return [...html.matchAll(new RegExp(`<span class="([^"]*)">([^<]*)</span>`, 'g'))].filter((m) => m[1].split(' ').includes(cls)).map((m) => m[2]);
}

/** What is wrong with the status tags of a built tree: every doc page names its status, the index names every page's. */
function statusProblems(d: string): string[] {
  const problems: string[] = [];
  for (const e of DOCS) {
    const want = STATUSES[e.status];
    const html = page(d, docRoute(e));
    const tags = textsOfClass(html, 'doc-status');
    if (tags.length !== 1 || tags[0] !== want.label) problems.push(`${e.source}: the page shows ${JSON.stringify(tags)}, the table says ${want.label}`);
    if (!html.includes(`class="tag ${want.tag} doc-status"`)) problems.push(`${e.source}: no .${want.tag} on the tag`);
  }
  const index = page(d, '/docs/');
  const cards = [...index.matchAll(/<li class="doc-card"><h3><a href="([^"]+)">[^<]*<\/a> <span class="tag ([a-z-]+) doc-status">([^<]*)<\/span>/g)];
  const byRoute = new Map(cards.map((m) => [m[1], m[3]]));
  if (byRoute.size !== cards.length) problems.push('the docs index lists a page twice');
  for (const e of DOCS) if (byRoute.get(docRoute(e)) !== STATUSES[e.status].label) problems.push(`index: ${e.source} is ${byRoute.get(docRoute(e))}, the table says ${STATUSES[e.status].label}`);
  for (const p of PAGES.filter((x) => x.indexed)) if (byRoute.get(p.route) === undefined) problems.push(`index: ${p.route} is not listed`);
  return problems;
}

test('every doc of the table is a page with the status tag the table gives it, and the index lists it with the same tag', () => {
  assert.deepEqual(statusProblems(dist), []);
});

test('the status check can fail: a tag removed from a page, a status changed on the index', () => {
  const noTag = copyTree(dist, 'docs-status-a');
  plant(noTag, outPath(docRoute(DOCS[0])), /<span class="tag [a-z-]+ doc-status">[^<]*<\/span>/, '');
  assert.match(statusProblems(noTag).join('\n'), new RegExp(`${DOCS[0].source}: the page shows \\[\\]`));
  const wrong = copyTree(dist, 'docs-status-b');
  const design = DOCS.find((d) => d.status === 'works') as (typeof DOCS)[number];
  plant(wrong, outPath('/docs/'), new RegExp(`(href="${docRoute(design)}">[^<]*</a> <span class="tag )[a-z-]+( doc-status">)[^<]*`), '$1tag-planned$2Design');
  assert.match(statusProblems(wrong).join('\n'), new RegExp(`index: ${design.source} is Design`));
});

test('the four statuses read as the table says: design and planned are never shown as working', () => {
  assert.equal(STATUSES.works.label, 'Works today');
  assert.equal(STATUSES.partial.label, 'Built in part');
  assert.equal(STATUSES.design.label, 'Design');
  assert.equal(STATUSES.planned.label, 'Planned');
  assert.notEqual(STATUSES.design.tag, 'tag-ok');
  assert.notEqual(STATUSES.planned.tag, 'tag-ok');
  assert.notEqual(STATUSES.partial.tag, 'tag-ok');
});

test('each doc page has its title, its source link and exactly one current page in its nav, with every page of its section', () => {
  for (const e of DOCS) {
    const html = page(dist, docRoute(e));
    const nav = /<nav class="docs-nav"[\s\S]*?<\/nav>/.exec(html)?.[0] ?? '';
    const { tags } = readTags(nav);
    const current = tags.filter(([t, a]) => t === 'a' && a.some(([k, v]) => k === 'aria-current' && v === 'page'));
    assert.equal(current.length, 1, `${e.source}: ${current.length} current links`);
    assert.equal(current[0][1].find(([k]) => k === 'href')?.[1], docRoute(e), e.source);
    for (const sibling of DOCS.filter((d) => d.section === e.section)) assert.equal(nav.includes(`href="${docRoute(sibling)}"`), true, `${e.source}: nav lacks ${sibling.slug}`);
    assert.equal(html.includes(`href="https://github.com/komira-ai/kbf/blob/main/${e.source}"`), true, `${e.source}: no source link`);
    assert.match(html, /<article class="prose doc">/);
  }
});

// Constructs the renderer shows as text (raw HTML, setext headings, reference definitions): the documents that hold some
// are listed with their counts, so a new one is noticed. These two were found when the pipeline was built.
const KNOWN_FALLBACKS = new Map([
  ['docs/design/scheduler.md', 5],
  ['docs/design/failure-classes.md', 14],
]);

test('no doc gains unsupported Markdown shown as literal text beyond the known ones', () => {
  const got = new Map(buildDocPages(REPO).filter((p) => p.fallbacks > 0).map((p) => [p.entry.source, p.fallbacks]));
  assert.deepEqual([...got].sort(), [...KNOWN_FALLBACKS].sort());
});
