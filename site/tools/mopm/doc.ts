// The Doc port: the only way the site builds markup.
//
// Pages are built from element and text nodes through a Doc, never from HTML strings. Two backends implement it:
// browserDoc (DOM nodes, at runtime) and StringDoc (a node tree with an escaping serializer, for build-time pages and
// for tests, which therefore need no DOM). The markdown renderer and every view write through h(), which accepts only
// the tags and attributes listed below, refuses any event handler or style attribute, and takes URLs (href, action)
// only as a SafeHref, which links.ts alone produces.

export const TAGS = [
  'a', 'article', 'aside', 'blockquote', 'br', 'button', 'caption', 'code', 'dd', 'details', 'div', 'dl', 'dt', 'em',
  'footer', 'form', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'header', 'hr', 'input', 'kbd', 'label', 'li', 'main', 'meta',
  'nav', 'ol', 'option', 'p', 'pre', 'section', 'select', 'small', 'span', 'strong', 'table', 'tbody', 'td', 'th',
  'summary', 'thead', 'time', 'tr', 'ul',
] as const;
export type Tag = (typeof TAGS)[number];

export const ATTRS = [
  'action', 'aria-busy', 'aria-current', 'aria-describedby', 'aria-hidden', 'aria-label', 'aria-labelledby',
  'aria-live', 'autocomplete', 'class', 'colspan', 'content', 'data-page', 'datetime', 'disabled', 'enterkeyhint',
  'for', 'hidden', 'href', 'id', 'lang', 'maxlength', 'method', 'name', 'placeholder', 'rel', 'role', 'scope',
  'selected', 'spellcheck', 'start', 'tabindex', 'title', 'type', 'value',
] as const;
export type AttrName = (typeof ATTRS)[number];

/** Attributes whose value is a URL: they take a SafeHref only. */
const URL_ATTRS: ReadonlySet<string> = new Set(['href', 'action']);

declare const safeHrefBrand: unique symbol;
/** A URL that links.ts has checked. Nothing else should produce one (`as SafeHref` appears only in links.ts). */
export type SafeHref = string & { readonly [safeHrefBrand]: true };

export type Attrs = { href?: SafeHref; action?: SafeHref } & {
  [K in Exclude<AttrName, 'href' | 'action'>]?: string;
};

export interface Doc<N> {
  el(tag: Tag): N;
  text(s: string): N;
  append(parent: N, child: N): void;
  setAttr(node: N, name: AttrName, value: string): void;
}

const TAG_SET: ReadonlySet<string> = new Set(TAGS);
const ATTR_SET: ReadonlySet<string> = new Set(ATTRS);
const VOID: ReadonlySet<string> = new Set(['br', 'hr', 'input', 'meta']);
// A last check on URL attributes at runtime, in case a cast slipped past the type: the schemes and forms links.ts
// can produce (https, http, mailto, a fragment, or a same-site path that is not protocol-relative) ...
const URL_SHAPE = /^(?:https:|http:|mailto:|#|\/(?![/\\]))/;
/**
 * ... and none of the characters a browser's URL parser drops or a reader cannot see: C0 controls, space, DEL, C1
 * controls, and invisible format characters (zero-width, bidi controls, BOM). A browser removes a tab or newline
 * anywhere, so '/\t/evil' would be followed as '//evil'. links.ts strips this same set before it checks a URL, so a
 * SafeHref it produced never holds one.
 */
export const URL_UNSAFE_CHAR = /[\u0000-\u0020\u007f-\u009f\u00ad\u200b-\u200f\u2028-\u202e\u2060-\u2064\ufeff]/;

export type Child<N> = N | string | null | undefined | false;

/** Builds an element through the allowlist. Throws on a tag or attribute outside it. */
export function h<N>(doc: Doc<N>, tag: Tag, attrs?: Attrs | null, ...children: Child<N>[]): N {
  if (!TAG_SET.has(tag)) throw new Error(`h(): tag '${String(tag)}' is not allowed`);
  const node = doc.el(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs) as [string, unknown][]) {
      if (v === undefined) continue;
      if (/^on/i.test(k) || k.toLowerCase() === 'style') throw new Error(`h(): attribute '${k}' is never allowed`);
      if (!ATTR_SET.has(k)) throw new Error(`h(): attribute '${k}' is not allowed`);
      if (typeof v !== 'string') throw new Error(`h(): attribute '${k}' must be a string`);
      if (URL_ATTRS.has(k) && (URL_UNSAFE_CHAR.test(v) || !URL_SHAPE.test(v))) throw new Error(`h(): '${k}' is not a checked URL`);
      doc.setAttr(node, k as AttrName, v);
    }
  }
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    doc.append(node, typeof c === 'string' ? doc.text(c) : c);
  }
  return node;
}

// ---- StringDoc ----

export interface SElement {
  tag: Tag;
  attrs: [AttrName, string][];
  children: SNode[];
}
export interface SText {
  text: string;
}
export type SNode = SElement | SText;

export class StringDoc implements Doc<SNode> {
  el(tag: Tag): SNode {
    return { tag, attrs: [], children: [] };
  }
  text(s: string): SNode {
    return { text: s };
  }
  append(parent: SNode, child: SNode): void {
    if (!('tag' in parent)) throw new Error('StringDoc: a text node has no children');
    if (VOID.has(parent.tag)) throw new Error(`StringDoc: <${parent.tag}> has no children`);
    parent.children.push(child);
  }
  setAttr(node: SNode, name: AttrName, value: string): void {
    if (!('tag' in node)) throw new Error('StringDoc: a text node has no attributes');
    const i = node.attrs.findIndex(([k]) => k === name);
    if (i >= 0) node.attrs[i] = [name, value];
    else node.attrs.push([name, value]);
  }
}

export function escapeText(s: string): string {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export function escapeAttr(s: string): string {
  return escapeText(s).replace(/"/g, '&quot;');
}

/** Serializes StringDoc nodes to HTML; every text and attribute value is escaped. */
export function serialize(nodes: SNode | readonly SNode[]): string {
  const out: string[] = [];
  const walk = (n: SNode): void => {
    if (!('tag' in n)) {
      out.push(escapeText(n.text));
      return;
    }
    out.push('<', n.tag);
    for (const [k, v] of n.attrs) out.push(' ', k, '="', escapeAttr(v), '"');
    out.push('>');
    if (VOID.has(n.tag)) return;
    for (const c of n.children) walk(c);
    out.push('</', n.tag, '>');
  };
  for (const n of Array.isArray(nodes) ? nodes : [nodes as SNode]) walk(n);
  return out.join('');
}

// ---- browserDoc ----

/** The DOM backend. Element and text nodes only: nothing here parses markup. */
export const browserDoc: Doc<Node> = {
  el: (tag) => document.createElement(tag),
  text: (s) => document.createTextNode(s),
  append: (parent, child) => {
    parent.appendChild(child);
  },
  setAttr: (node, name, value) => {
    (node as Element).setAttribute(name, value);
  },
};
