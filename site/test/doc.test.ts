// The Doc port's runtime guards. The types already keep a view from passing a bad tag, attribute or URL; these checks
// are what still holds when a cast or an `any` gets past the types (PR2's views pass API strings into attributes).
// Each case below would go green if its guard in h() or serialize() were removed.
import { describe, expect, test } from './jest-compat';
import { type Attrs, escapeAttr, h, type SafeHref, type SElement, type SNode, serialize, StringDoc, type Tag } from '../tools/mopm/doc';

const doc = new StringDoc();
const html = (n: SNode) => serialize(n);
const bad = (a: Record<string, unknown>) => a as unknown as Attrs;

describe('h() tag allowlist', () => {
  test.each(['script', 'img', 'iframe', 'style', 'svg', 'object', 'A', 'link'])('<%s> throws', (tag) => {
    expect(() => h(doc, tag as Tag)).toThrow(/tag .* is not allowed/);
  });

  test('an allowed tag builds', () => {
    expect(html(h(doc, 'p', null, 'x'))).toBe('<p>x</p>');
  });
});

describe('h() attribute allowlist', () => {
  test.each(['onclick', 'OnLoad', 'onerror', 'ONMOUSEOVER', 'style', 'Style', 'STYLE'])('%s is never allowed', (k) => {
    expect(() => h(doc, 'p', bad({ [k]: 'x' }))).toThrow(/is never allowed/);
  });

  test.each(['src', 'srcdoc', 'formaction', 'xlink:href', 'data-x', 'Href', 'ID'])('%s is not in the allowlist', (k) => {
    expect(() => h(doc, 'p', bad({ [k]: 'x' }))).toThrow(/is not allowed/);
  });

  test.each([1, true, {}, ['x'], null])('a non-string value (%j) throws', (v) => {
    expect(() => h(doc, 'p', bad({ title: v }))).toThrow(/must be a string/);
  });

  test('an undefined value is skipped, an allowed one is set', () => {
    expect(html(h(doc, 'p', { title: undefined, class: 'c' }))).toBe('<p class="c"></p>');
  });
});

describe('h() URL attributes take a checked URL only', () => {
  // Every one of these reaches h() only through a cast; links.ts never produces them.
  test.each([
    'javascript:alert(1)', 'JAVASCRIPT:x', 'data:text/html,x', 'vbscript:x', '//evil.example', '/\\evil', 'relative',
    '', ' https://mopm.dev', '/\t/evil.example', '/\n/evil.example', '/\r/evil', '/\u0000/evil', '/\u007f/evil',
    'https://mopm.dev/\t', 'https://mopm.dev/a b', '#a\u200bb', '/x\u2028', '/x\u00ad', '/x\u0085',
  ])('href %j throws', (v) => {
    expect(() => h(doc, 'a', { href: v as SafeHref })).toThrow(/not a checked URL/);
    expect(() => h(doc, 'form', { action: v as SafeHref })).toThrow(/not a checked URL/);
  });

  test.each(['https://mopm.dev/x', 'http://mopm.dev/', 'mailto:a@mopm.dev', '#readme-x', '/packages/ab', '/'])(
    'href %j is accepted',
    (v) => {
      expect(html(h(doc, 'a', { href: v as SafeHref }))).toBe(`<a href="${v}"></a>`);
    },
  );
});

describe('serialization escapes', () => {
  test('attribute values escape quotes and markup', () => {
    expect(html(h(doc, 'p', { title: 'a"b<c>&' }))).toBe('<p title="a&quot;b&lt;c&gt;&amp;"></p>');
    expect(escapeAttr(`"'<>&`)).toBe(`&quot;'&lt;&gt;&amp;`);
  });

  test('text escapes markup', () => {
    expect(html(h(doc, 'p', null, '<script>&amp;</script>'))).toBe('<p>&lt;script&gt;&amp;amp;&lt;/script&gt;</p>');
  });

  test('a void element refuses children and serializes without a closing tag', () => {
    expect(() => h(doc, 'br', null, 'x')).toThrow(/has no children/);
    expect(() => h(doc, 'input', null, h(doc, 'span'))).toThrow(/has no children/);
    expect(html(h(doc, 'input', { type: 'search', name: 'q' }))).toBe('<input type="search" name="q">');
  });

  test('a text node has no children or attributes', () => {
    const t = doc.text('x');
    expect(() => doc.append(t, doc.text('y'))).toThrow();
    expect(() => doc.setAttr(t, 'id', 'y')).toThrow();
  });

  test('setting an attribute twice keeps one, the last', () => {
    const n = doc.el('p') as SElement;
    doc.setAttr(n, 'id', 'a');
    doc.setAttr(n, 'id', 'b');
    expect(serialize(n)).toBe('<p id="b"></p>');
  });
});
