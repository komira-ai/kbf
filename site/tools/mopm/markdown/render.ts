// Renders the parsed Markdown through a Doc (so the same code serves a readme in the browser and a repo doc at build
// time), with every element built by h() and every link checked by safeHref.
//
// Heading ids are GitHub-style slugs with a prefix (a readme uses 'readme-', so its ids cannot clobber the page's own)
// and are deduplicated as GitHub does ('a', 'a-1', ...). A refused link shows its text; a link with no text shows its
// target, so every link has an accessible name. An image is never loaded: it becomes a link reading "image: <alt>",
// or, inside a link (a readme badge), just its alt text; an autolink inside a link is its text. So a link never holds a
// link. Raw HTML was kept as text by the parser and stays text here.
//
// Wide blocks scroll on their own: a table sits in a focusable, named scroll region (class 'scroll'; styling the
// table itself to scroll would strip its table semantics) and a code block is focusable, so a keyboard reaches the
// overflow. A docs page (opts.docs) also gets heading anchors, code blocks the page script adds a Copy button to, and
// its h2 and h3 headings reported for an on-this-page list; a readme gets none of them.

import { type Child, type Doc, h, type SafeHref, type Tag } from '../doc.js';
import { anchorHref, type LinkContext, safeHref } from '../links.js';
import { type Align, type Block, type Inline, parse } from './parse.js';

export interface RenderOptions {
  /** Prefix for heading ids and in-page fragment links, e.g. 'readme-'. */
  idPrefix: string;
  /** Added to every heading level (capped at 6): a readme under the page's h1 shifts by 1. */
  headingShift: number;
  /** How links resolve: a readme keeps absolute links only; a repo doc resolves relative links in the repo. */
  links: { mode: 'readme' } | { mode: 'docs'; source: string };
  /**
   * A docs page's extras (repo docs at build time; a readme leaves them out): each heading's text becomes a link to
   * its own id (no link when the heading holds a link, as anchors never nest, or when its id is not a plain id), each
   * code block sits in a named .code-box .copy-box so the page script adds its Copy button (copy.ts), and `onHeading`
   * hears every h2 and h3 for an on-this-page list.
   */
  docs?: { onHeading?: (level: number, id: string, text: string) => void };
}

const PLAIN_ID = /^[A-Za-z][A-Za-z0-9_-]*$/;

function hasLink(inl: readonly Inline[]): boolean {
  return inl.some((x) => x.t === 'link' || x.t === 'image' || ((x.t === 'em' || x.t === 'strong') && hasLink(x.c)));
}

const EXTERNAL_REL = 'nofollow noopener noreferrer ugc';
const LANG = /^[A-Za-z0-9_+-]{1,32}$/;
const HEADINGS: readonly Tag[] = ['h1', 'h2', 'h3', 'h4', 'h5', 'h6'];

/** GitHub's heading slug: lowercase, punctuation other than '-' and '_' removed, spaces to '-'. */
export function slugify(text: string): string {
  return text
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{M}\p{N}\p{Pc} -]/gu, '')
    .replace(/ /g, '-');
}

/** Hands out unique slugs: a repeat of 'x' becomes 'x-1', 'x-2', ..., skipping any already taken. */
export class Slugger {
  private readonly seen = new Map<string, number>();
  slug(text: string): string {
    const base = slugify(text) || 'section';
    let result = base;
    while (this.seen.has(result)) {
      const n = (this.seen.get(base) ?? 0) + 1;
      this.seen.set(base, n);
      result = `${base}-${n}`;
    }
    this.seen.set(result, 0);
    return result;
  }
}

function plainText(inl: readonly Inline[]): string {
  let out = '';
  for (const x of inl) {
    if (x.t === 'text' || x.t === 'code' || x.t === 'raw') out += x.v;
    else if (x.t === 'em' || x.t === 'strong' || x.t === 'link') out += plainText(x.c);
    else if (x.t === 'image') out += x.alt;
  }
  return out;
}

function alignClass(a: Align): string | undefined {
  return a ? `align-${a}` : undefined;
}

function isExternal(href: SafeHref): boolean {
  return /^(?:https?|mailto):/.test(href);
}

class Renderer<N> {
  private readonly slugger = new Slugger();
  private readonly ctx: LinkContext;

  constructor(private readonly doc: Doc<N>, private readonly opts: RenderOptions) {
    this.ctx =
      opts.links.mode === 'docs'
        ? { mode: 'docs', idPrefix: opts.idPrefix, source: opts.links.source }
        : { mode: 'readme', idPrefix: opts.idPrefix };
  }

  blocks(parent: N, blocks: readonly Block[], tight = false): void {
    for (const b of blocks) {
      if (tight && b.t === 'para') {
        this.inlines(parent, b.c);
        continue;
      }
      this.doc.append(parent, this.block(b));
    }
  }

  private block(b: Block): N {
    const d = this.doc;
    switch (b.t) {
      case 'heading': {
        const level = Math.min(6, Math.max(1, b.level + this.opts.headingShift));
        const id = this.opts.idPrefix + this.slugger.slug(plainText(b.c));
        const node = h(d, HEADINGS[level - 1], { id });
        const docs = this.opts.docs;
        if (docs && PLAIN_ID.test(id) && !hasLink(b.c)) {
          const a = h(d, 'a', { href: anchorHref(id), class: 'anchor' });
          this.inlines(a, b.c, true);
          d.append(node, a);
        } else {
          this.inlines(node, b.c);
        }
        if (docs?.onHeading && (level === 2 || level === 3)) docs.onHeading(level, id, plainText(b.c).trim());
        return node;
      }
      case 'para': {
        const node = h(d, 'p');
        this.inlines(node, b.c);
        return node;
      }
      case 'code': {
        const lang = LANG.test(b.lang) ? b.lang : null;
        const code = h(d, 'code', lang ? { class: `language-${lang}` } : null, b.v);
        if (!this.opts.docs) return h(d, 'pre', { tabindex: '0' }, code);
        const label = lang ? `${lang} example` : 'code example';
        // The bar above the code names its language for the eye; the <pre>'s own name says it to a screen reader.
        return h(
          d,
          'div',
          { class: 'code-box copy-box' },
          h(d, 'span', { class: 'code-lang', 'aria-hidden': 'true' }, lang ?? 'code'),
          h(d, 'pre', { tabindex: '0', role: 'region', 'aria-label': label }, code),
        );
      }
      case 'quote': {
        const node = h(d, 'blockquote');
        this.blocks(node, b.c);
        return node;
      }
      case 'list': {
        const attrs = b.ordered && b.start !== 1 ? { start: String(b.start) } : null;
        const node = h(d, b.ordered ? 'ol' : 'ul', attrs);
        for (const item of b.items) {
          const li = h(d, 'li');
          this.blocks(li, item, b.tight);
          d.append(node, li);
        }
        return node;
      }
      case 'hr':
        return h(d, 'hr');
      case 'table': {
        const headRow = h(d, 'tr');
        b.head.forEach((cell, k) => {
          const th = h(d, 'th', { scope: 'col', class: alignClass(b.align[k]) });
          this.inlines(th, cell);
          d.append(headRow, th);
        });
        const table = h(d, 'table', null, h(d, 'thead', null, headRow));
        if (b.rows.length > 0) {
          const body = h(d, 'tbody');
          for (const row of b.rows) {
            const tr = h(d, 'tr');
            row.forEach((cell, k) => {
              const td = h(d, 'td', { class: alignClass(b.align[k]) });
              this.inlines(td, cell);
              d.append(tr, td);
            });
            d.append(body, tr);
          }
          d.append(table, body);
        }
        const label = b.head.map((cell) => plainText(cell).trim()).filter((t) => t !== '').join(', ');
        const region = { class: 'scroll', role: 'region', tabindex: '0', 'aria-label': label ? `Table: ${label}` : 'Table' };
        return h(d, 'div', region, table);
      }
    }
  }

  /** `inLink`: inside a link's text, where an image is its alt text and a link its text (an anchor never nests). */
  private inlines(parent: N, inl: readonly Inline[], inLink = false): void {
    const d = this.doc;
    for (const x of inl) {
      let child: Child<N> = null;
      switch (x.t) {
        case 'text':
        case 'raw':
          child = d.text(x.v);
          break;
        case 'code':
          child = h(d, 'code', null, x.v);
          break;
        case 'br':
          child = h(d, 'br');
          break;
        case 'em':
        case 'strong':
          child = h(d, x.t);
          this.inlines(child, x.c, inLink);
          break;
        case 'link': {
          if (inLink) {
            this.inlines(parent, x.c, true); // an autolink in a link's text is just its text
            break;
          }
          const href = safeHref(x.href, this.ctx);
          if (!href) {
            this.inlines(parent, x.c, inLink); // a refused link keeps its text
            break;
          }
          child = h(d, 'a', isExternal(href) ? { href, rel: EXTERNAL_REL } : { href });
          if (plainText(x.c).trim() === '') d.append(child, d.text(href)); // no text: the target names the link
          else this.inlines(child, x.c, true);
          break;
        }
        case 'image': {
          if (inLink) {
            if (x.alt) child = d.text(x.alt);
            break;
          }
          const label = x.alt ? `image: ${x.alt}` : 'image';
          const href = safeHref(x.href, this.ctx);
          child = href ? h(d, 'a', isExternal(href) ? { href, rel: EXTERNAL_REL } : { href }, label) : d.text(label);
          break;
        }
      }
      if (child) d.append(parent, child);
    }
  }
}

/** Renders parsed blocks into `parent`. */
export function renderBlocks<N>(doc: Doc<N>, parent: N, blocks: readonly Block[], opts: RenderOptions): void {
  new Renderer(doc, opts).blocks(parent, blocks);
}

/** Parses and renders Markdown into `parent`; returns the parse's fallback count (unsupported constructs shown as text). */
export function renderMarkdown<N>(doc: Doc<N>, parent: N, src: string, opts: RenderOptions): number {
  const parsed = parse(src);
  renderBlocks(doc, parent, parsed.blocks, opts);
  return parsed.fallbacks;
}
