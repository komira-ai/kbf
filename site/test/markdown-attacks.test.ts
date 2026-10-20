// T8: hostile Markdown renders to renderer markup only (allowlisted tags and attributes), every href is a checked
// http(s)/mailto URL or an in-page fragment, and raw HTML comes out as escaped text.
import { expect, test } from './jest-compat';
import { readFileSync } from 'fs';
import { join } from 'path';
import { safeHref } from '../tools/mopm/links';
import { expectSafe, md, mdFull, tagsOf } from './md';
import { SITE } from './support';

const corpus = readFileSync(join(SITE, 'test', 'fixtures', 'attacks.md'), 'utf8');
const cases = corpus
  .split(/^%%% /m)
  .slice(1)
  .map((chunk) => {
    const nl = chunk.indexOf('\n');
    return [chunk.slice(0, nl), chunk.slice(nl + 1)] as const;
  });

test('the corpus has its cases', () => {
  expect(cases.length).toBeGreaterThanOrEqual(30);
});

test.each(cases)('%s', (_name, src) => {
  const html = md(src);
  expectSafe(html);
  // Nothing executable survives as markup: no such element, no handler or style attribute, no other href scheme.
  // (The same words may appear as escaped text, which a browser shows and never runs.)
  for (const { tag, attrs } of tagsOf(html)) {
    expect(['script', 'img', 'svg', 'iframe', 'style', 'object', 'embed']).not.toContain(tag);
    for (const [k] of attrs) expect(k).not.toMatch(/^(on|style$)/i);
  }
  expect(html).not.toMatch(/href="(?!https:|http:|mailto:|#readme-)/);
});

test('the whole corpus as one document is safe too', () => {
  expectSafe(md(corpus));
});

test('raw HTML is shown as text', () => {
  const r = mdFull('<script>alert(1)</script>');
  expect(r.html).toBe('<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>');
  expect(r.fallbacks).toBe(2);
});

test('a refused link keeps its text and becomes no anchor', () => {
  expect(md('[click *me*](javascript:alert(1))')).toBe('<p>click <em>me</em></p>');
  expect(md('[x](JaVaScRiPt:alert(1))')).toBe('<p>x</p>');
  expect(md('[x](data:text/html,hi)')).toBe('<p>x</p>');
  expect(md('[x](//evil/x)')).toBe('<p>x</p>');
});

test('a fragment cannot break out of its attribute', () => {
  expect(md('[x](#"><script>)')).toBe('<p><a href="#readme-%22%3E%3Cscript%3E">x</a></p>');
});

const R = { mode: 'readme', idPrefix: 'readme-' } as const;

test.each([
  ['javascript:alert(1)', null],
  ['JaVaScRiPt:alert(1)', null],
  ['java\tscript:alert(1)', null],
  ['java\nscript:alert(1)', null],
  ['\u0000javascript:alert(1)', null],
  [' javascript:alert(1)', null],
  ['java​script:alert(1)', null],
  ['vbscript:x', null],
  ['data:text/html,x', null],
  ['file:///etc/passwd', null],
  ['blob:https://mopm.dev/x', null],
  ['//evil', null],
  ['\\\\evil', null],
  ['/\\evil', null],
  ['relative/path.md', null],
  ['java&#115;cript:alert(1)', null],
  ['https://github.com/komira-ai/mopm', 'https://github.com/komira-ai/mopm'],
  ['HTTPS://GitHub.com/komira-ai/mopm', 'https://github.com/komira-ai/mopm'],
  ['http://mopm.dev', 'http://mopm.dev/'],
  ['mailto:a@mopm.dev', 'mailto:a@mopm.dev'],
  // The decision is made on the URL as a browser parses it: a tab or an invisible character inside the scheme is
  // dropped first, so this is https (and the emitted href is the normalised one the browser would follow).
  ['ht\ttps://mopm.dev/a', 'https://mopm.dev/a'],
  ['ht​tps://mopm.dev/a', 'https://mopm.dev/a'],
  ['https://mopm.dev/a"b<c>', 'https://mopm.dev/a%22b%3Cc%3E'],
] as const)('safeHref(%j) in a readme', (raw, want) => {
  expect(safeHref(raw, R)).toBe(want);
});
