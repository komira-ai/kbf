// Test helper: Markdown to HTML through StringDoc, the way build-time pages are rendered.
import { expect } from './jest-compat';
import { type SElement, serialize, StringDoc } from '../tools/mopm/doc';
import { type RenderOptions, renderMarkdown } from '../tools/mopm/markdown/render';

export const README: RenderOptions = { idPrefix: 'readme-', headingShift: 0, links: { mode: 'readme' } };

export function md(src: string, opts: RenderOptions = README): string {
  return mdFull(src, opts).html;
}

export function mdFull(src: string, opts: RenderOptions = README): { html: string; fallbacks: number } {
  const doc = new StringDoc();
  const root = doc.el('div') as SElement;
  const fallbacks = renderMarkdown(doc, root, src, opts);
  return { html: serialize(root.children), fallbacks };
}

/** The markup the renderer may emit, and nothing else. */
export const MD_TAGS = new Set([
  'a', 'blockquote', 'br', 'code', 'div', 'em', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'hr', 'li', 'ol', 'p', 'pre', 'strong',
  'table', 'tbody', 'td', 'th', 'thead', 'tr', 'ul',
]);
export const MD_ATTRS = new Set(['href', 'rel', 'id', 'class', 'start', 'scope', 'role', 'tabindex', 'aria-label']);

/**
 * Every opening tag in serialized output, with its attributes. Throws if a '<' does not start a well-formed opening
 * or closing tag, so a text '<' that escaped escaping is caught.
 */
export function tagsOf(html: string): { tag: string; attrs: [string, string][] }[] {
  const out: { tag: string; attrs: [string, string][] }[] = [];
  let i = 0;
  while ((i = html.indexOf('<', i)) >= 0) {
    const m = /^<(\/?)([a-z0-9]+)((?: [a-z-]+="[^"<>]*")*)>/.exec(html.slice(i));
    if (!m || (m[1] === '/' && m[3] !== '')) {
      throw new Error(`a '<' that is not a tag at ${i}: ${JSON.stringify(html.slice(i, i + 40))}`);
    }
    if (m[1] === '/') {
      if (!MD_TAGS.has(m[2])) throw new Error(`closing tag </${m[2]}> outside the allowlist`);
    } else {
      const attrs = [...m[3].matchAll(/ ([a-z-]+)="([^"]*)"/g)].map((a) => [a[1], a[2]] as [string, string]);
      out.push({ tag: m[2], attrs });
    }
    i += m[0].length;
  }
  return out;
}

/** Decodes the escapes serialize() produces in attribute values. */
export function unescapeAttr(v: string): string {
  return v.replace(/&quot;/g, '"').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');
}

/** Asserts the output holds only renderer markup and only safe link targets, and that no link holds a link. */
export function expectSafe(html: string): void {
  expectNoNestedLinks(html);
  expectLinkText(html);
  for (const { tag, attrs } of tagsOf(html)) {
    expect(MD_TAGS).toContain(tag);
    for (const [k, v] of attrs) {
      expect(MD_ATTRS).toContain(k);
      if (k === 'href') expectSafeHref(unescapeAttr(v));
    }
  }
}

export function expectSafeHref(href: string): void {
  if (href.startsWith('#')) {
    expect(href).toMatch(/^#readme-/);
    return;
  }
  if (href.startsWith('/')) {
    expect(href).toMatch(/^\/docs\/[a-z0-9-]+\/(#.*)?$/);
    return;
  }
  const u = new URL(href);
  expect(['https:', 'http:', 'mailto:']).toContain(u.protocol);
  expect(u.href).toBe(href);
}

/** A link inside a link is invalid HTML (a browser re-parses it into a different tree) and nested interactive content. */
export function expectNoNestedLinks(html: string): void {
  let depth = 0;
  for (const m of html.matchAll(/<(\/?)a[ >]/g)) {
    depth += m[1] === '/' ? -1 : 1;
    if (depth > 1) throw new Error(`a link inside a link at ${m.index}: ${JSON.stringify(html.slice(0, 200))}`);
  }
}

/** Every link has text (an accessible name): an anchor never closes right after it opens. */
export function expectLinkText(html: string): void {
  expect(html).not.toMatch(/<a [^>]*><\/a>/);
}
