// The docs table (tools/docs-table.ts): which repository files become pages.
//
// Catches: a listed file that is not in the repository (a renamed or deleted doc leaves a page that cannot be built, or
// worse a link to nothing), a document under docs/ that no page publishes and nobody excluded on purpose (a new design
// document that never reaches the site), a slug used twice or one that takes a route a hand-written page has, and a
// page without one of the four statuses. The files are read from the repository, not from a list in this test.

import assert from 'node:assert/strict';
import { existsSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { DOCS, docRoute, EXCLUDE, SECTIONS, STATUSES } from '../tools/docs-table';
import { PAGES } from '../tools/pages';
import { REPO } from './support';

/** Every .md file under dir (a repo path), '/'-separated and sorted. */
function markdownUnder(rel: string): string[] {
  return readdirSync(join(REPO, rel), { withFileTypes: true })
    .flatMap((e) => (e.isDirectory() ? markdownUnder(`${rel}/${e.name}`) : e.name.endsWith('.md') ? [`${rel}/${e.name}`] : []))
    .sort();
}

test('every source of the table is a file of the repository', () => {
  const missing = DOCS.filter((d) => !existsSync(join(REPO, d.source))).map((d) => d.source);
  assert.deepEqual(missing, []);
});

test('every document under docs/ is published or excluded on purpose, and every exclusion exists', () => {
  const listed = new Set([...DOCS.map((d) => d.source), ...EXCLUDE]);
  assert.deepEqual(
    markdownUnder('docs').filter((f) => !listed.has(f)),
    [],
    'a document under docs/ is not in the docs table: add a row for it to site/tools/docs-table.ts ' +
      '(or list it in EXCLUDE), then run `npm run golden` in site/ and commit test/golden.manifest (CONTRIBUTING.md, "Documentation")',
  );
  assert.deepEqual(EXCLUDE.filter((f) => !existsSync(join(REPO, f))), []);
  assert.deepEqual(EXCLUDE.filter((f) => DOCS.some((d) => d.source === f)), [], 'a file is either published or excluded');
});

test('sources and slugs are unique, slugs are route-safe, and no slug takes the route of another page', () => {
  assert.equal(new Set(DOCS.map((d) => d.source)).size, DOCS.length);
  assert.equal(new Set(DOCS.map((d) => d.slug)).size, DOCS.length);
  for (const d of DOCS) assert.match(d.slug, /^[a-z0-9]+(?:-[a-z0-9]+)*(?:\/[a-z0-9]+(?:-[a-z0-9]+)*)*$/, d.slug);
  const taken = new Set(PAGES.map((p) => p.route));
  assert.deepEqual(DOCS.filter((d) => taken.has(docRoute(d))).map((d) => d.slug), []);
});

test('every page has a known section and a known status', () => {
  const sections = new Set(SECTIONS.map((s) => s.id));
  for (const d of DOCS) {
    assert.equal(sections.has(d.section), true, d.source);
    assert.equal(d.status in STATUSES, true, d.source);
  }
  for (const s of SECTIONS) assert.equal(DOCS.some((d) => d.section === s.id), true, `section ${s.id} has no page`);
});
