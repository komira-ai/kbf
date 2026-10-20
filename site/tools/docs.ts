// The docs pages: each file of the docs table (docs-table.ts) rendered at build time by the restricted Markdown renderer
// (tools/mopm/, a copy of mopm.dev's) through StringDoc, and /docs/, the list of them.
//
// A docs page sits in a layout of a left nav (the pages of the page's section, and a link to each other section on
// /docs/), the page, and an on-this-page list of its h2 and h3 headings. Its title is the document's first '# '
// heading, and beside it is the status tag the table gives it (works today, built in part, design, planned).
//
// Heading ids carry a 'doc-' prefix so a heading can never take an id the page itself uses ('main'). Relative links
// resolve as tools/mopm/links.ts says: a published doc stays on the site, anything else goes to the file on GitHub.

import { readFileSync } from 'fs';
import { join } from 'path';
import { type DocEntry, DOCS, docRoute, type Section, SECTIONS, STATUSES, type Status } from './docs-table';
import { h, type SElement, serialize, type SNode, StringDoc } from './mopm/doc';
import { anchorHref, internalHref, REPO_URL, webHref } from './mopm/links';
import { renderMarkdown } from './mopm/markdown/render';

export const DOC_ID_PREFIX = 'doc-';
const doc = new StringDoc();

/** The id of a section's heading on /docs/ (the links of the nav to it). */
export const sectionId = (s: Section): string => `section-${s}`;

export interface DocPage {
  entry: DocEntry;
  route: string;
  title: string;
  description: string;
  /** The inside of <main> (serialized). */
  main: string;
  /** Markdown the renderer showed as literal text (test/docs-render.test.ts keeps it low and lists what remains). */
  fallbacks: number;
}

/** The text of the first '# ' heading, without inline markup characters; the fallback when there is none. */
export function docTitle(src: string, fallback: string): string {
  const m = /^# +(.+?)\s*#*\s*$/m.exec(src);
  return m ? m[1].replace(/`|\*\*/g, '').trim() : fallback;
}

/** The first sentence after the title, as plain text, for the page description and the docs index. */
export function docSummary(src: string): string {
  const lines = src.split('\n');
  let i = lines.findIndex((l) => /^# /.test(l));
  i = i < 0 ? 0 : i + 1;
  while (i < lines.length && lines[i].trim() === '') i++;
  const para: string[] = [];
  for (; i < lines.length && lines[i].trim() !== '' && !/^(?:#|```|\||[-*>] |\d+\. )/.test(lines[i]); i++) para.push(lines[i].trim());
  const plain = para
    .join(' ')
    .replace(/`|\*\*/g, '')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1');
  // A sentence ends at '.', '!' or '?' before a space ('kbf.toml' does not end one).
  const end = /[.!?](?=\s|$)/.exec(plain);
  const first = end ? plain.slice(0, end.index + 1) : plain;
  return first.length > 240 ? `${first.slice(0, 237).replace(/\s+\S*$/, '')}...` : first;
}

/** The status tag, as markup of the theme's tag classes. */
export function statusTag(status: Status): SNode {
  const s = STATUSES[status];
  return h(doc, 'span', { class: `tag ${s.tag} doc-status` }, s.label);
}

interface Titled {
  entry: DocEntry;
  title: string;
}

/** The nav beside every docs page: the index, the pages of the current section, then the other sections. */
export function docsNav(all: readonly Titled[], current: DocEntry | null, currentSection: Section | null): SNode {
  const item = (href: string, title: string, here: boolean, status?: Status): SNode =>
    h(
      doc,
      'li',
      null,
      h(doc, 'a', here ? { href: internalHref(href), 'aria-current': 'page' } : { href: internalHref(href) }, title),
      status ? h(doc, 'span', { class: 'visually-hidden' }, ` (${STATUSES[status].label})`) : null,
    );
  const groups: SNode[] = [];
  for (const s of SECTIONS) {
    if (s.id === currentSection) {
      groups.push(
        h(doc, 'p', { class: 'side-title' }, s.title),
        h(
          doc,
          'ul',
          null,
          ...all.filter((t) => t.entry.section === s.id).map((t) => item(docRoute(t.entry), t.title, current !== null && t.entry.slug === current.slug, t.entry.status)),
        ),
      );
    } else {
      groups.push(h(doc, 'ul', { class: 'other-section' }, h(doc, 'li', null, h(doc, 'a', { href: internalHref(`/docs/#${sectionId(s.id)}`) }, s.title))));
    }
  }
  return h(
    doc,
    'nav',
    { class: 'docs-nav', 'aria-label': 'Docs pages' },
    h(doc, 'p', { class: 'side-title' }, 'Docs'),
    h(doc, 'ul', null, item('/docs/', 'Overview', current === null)),
    ...groups,
  );
}

/** The on-this-page list: the page's h2 and h3 headings, linked to their anchors; null when there are none. */
export function tocNav(headings: readonly { level: number; id: string; text: string }[]): SNode | null {
  const linkable = headings.filter((x) => /^[A-Za-z][A-Za-z0-9_-]*$/.test(x.id));
  if (linkable.length === 0) return null;
  return h(
    doc,
    'nav',
    { class: 'page-toc', 'aria-label': 'On this page' },
    h(doc, 'p', { class: 'side-title' }, 'On this page'),
    h(doc, 'ul', null, ...linkable.map((x) => h(doc, 'li', { class: `toc-${x.level}` }, h(doc, 'a', { href: anchorHref(x.id) }, x.text)))),
  );
}

/** The nav, then a column holding the breadcrumb, the on-this-page list (when there is one) and the page. */
function docsLayout(nav: SNode, crumb: SNode | null, body: readonly SNode[], toc: SNode | null): SNode {
  return h(
    doc,
    'div',
    { class: 'docs-layout' },
    nav,
    h(doc, 'div', { class: toc ? 'docs-col' : 'docs-col no-toc' }, crumb, toc, h(doc, 'div', { class: 'docs-main' }, ...body)),
  );
}

export function buildDocPage(repoRoot: string, entry: DocEntry, nav: readonly Titled[]): DocPage {
  const src = readFileSync(join(repoRoot, entry.source), 'utf8');
  const title = docTitle(src, entry.slug);
  const article = h(doc, 'article', { class: 'prose doc' }) as SElement;
  const headings: { level: number; id: string; text: string }[] = [];
  const fallbacks = renderMarkdown(doc, article, src, {
    idPrefix: DOC_ID_PREFIX,
    headingShift: 0,
    links: { mode: 'docs', source: entry.source },
    docs: { onHeading: (level, id, text) => headings.push({ level, id, text }) },
  });
  const sourceLink = h(
    doc,
    'p',
    { class: 'doc-source muted small' },
    'Source: ',
    h(doc, 'a', { href: webHref(`${REPO_URL}/blob/main/${entry.source}`) ?? undefined }, h(doc, 'code', null, entry.source), ' on GitHub'),
  );
  const section = SECTIONS.find((s) => s.id === entry.section) as (typeof SECTIONS)[number];
  const crumb = h(
    doc,
    'nav',
    { class: 'crumb', 'aria-label': 'Breadcrumb' },
    h(doc, 'a', { href: internalHref('/docs/') }, 'Docs'),
    h(doc, 'span', { class: 'sep', 'aria-hidden': 'true' }, '/'),
    h(doc, 'a', { href: internalHref(`/docs/#${sectionId(entry.section)}`) }, section.title),
    h(doc, 'span', { class: 'sep', 'aria-hidden': 'true' }, '/'),
    h(doc, 'span', { 'aria-current': 'page' }, title),
    statusTag(entry.status),
  );
  const main = serialize(docsLayout(docsNav(nav, entry, entry.section), crumb, [article, sourceLink], tocNav(headings)));
  return { entry, route: docRoute(entry), title, description: docSummary(src) || title, main, fallbacks };
}

export function buildDocPages(repoRoot: string, entries: readonly DocEntry[] = DOCS): DocPage[] {
  const nav = entries.map((entry) => ({ entry, title: docTitle(readFileSync(join(repoRoot, entry.source), 'utf8'), entry.slug) }));
  return entries.map((entry) => buildDocPage(repoRoot, entry, nav));
}

/** A hand-written page the /docs/ index lists beside the rendered ones. */
export interface IndexedPage {
  route: string;
  title: string;
  summary: string;
  status: Status;
}

/** The inside of <main> of /docs/: the start-here pages, then one list per section. */
export function docsIndexMain(pages: readonly DocPage[], handwritten: readonly IndexedPage[]): string {
  const card = (route: string, title: string, summary: string, status: Status, source: string | null): SNode =>
    h(
      doc,
      'li',
      { class: 'doc-card' },
      h(doc, 'h3', null, h(doc, 'a', { href: internalHref(route) }, title), ' ', statusTag(status)),
      summary ? h(doc, 'p', null, summary) : null,
      source ? h(doc, 'p', { class: 'muted small' }, h(doc, 'code', null, source)) : null,
    );
  const nav = docsNav(pages.map((p) => ({ entry: p.entry, title: p.title })), null, null);
  const startHere = h(
    doc,
    'section',
    null,
    h(doc, 'h2', { id: 'start-here' }, 'Start here'),
    h(doc, 'ul', { class: 'doc-list' }, ...handwritten.map((p) => card(p.route, p.title, p.summary, p.status, null))),
  );
  const sections = SECTIONS.map((s) =>
    h(
      doc,
      'section',
      null,
      h(doc, 'h2', { id: sectionId(s.id) }, s.title),
      h(doc, 'p', { class: 'muted' }, s.blurb),
      h(
        doc,
        'ul',
        { class: 'doc-list' },
        ...pages.filter((p) => p.entry.section === s.id).map((p) => card(p.route, p.title, p.description, p.entry.status, p.entry.source)),
      ),
    ),
  );
  const legend = h(
    doc,
    'p',
    { class: 'muted small' },
    'Each page carries a tag for how much of it is built: ',
    ...(Object.keys(STATUSES) as Status[]).flatMap((s, i) => [i > 0 ? ', ' : '', statusTag(s)]),
    '. The tag is the docs table\'s reading of the document\'s own text; where a page and the code differ, the code is right.',
  );
  return serialize(
    docsLayout(nav, null, [h(doc, 'h1', null, 'Documentation'), h(doc, 'p', { class: 'lede' }, 'Guides, reference and design documents of kbf, rendered from the Markdown in the repository.'), legend, startHere, ...sections], null),
  );
}
