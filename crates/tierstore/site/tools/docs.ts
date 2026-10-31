// The docs pages: every crates/tierstore/docs/*.md becomes one page at /docs/<name>/, and /docs/ lists them. The
// Markdown is the operator guide itself (a test of the crate holds it to the code), so the site cannot say anything the
// guide does not.
//
// The renderer reads the subset the guide uses, and nothing else: '#' to '###' headings, paragraphs, '-' and '1.'
// lists of one line per item, '>' notes, pipe tables, ``` fences, `code`, **bold** and [text](target) links. It
// escapes all text, makes no markup from anything else, and throws on a construct it does not read, so a page that
// uses more is a build error and not a page that is quietly wrong. No dependency.

import { readdirSync, readFileSync } from 'fs';
import { join } from 'path';
import { escapeText } from './layout';
import type { Page } from './pages';

/** The pages in the order the guide reads; any other page of docs/ (the metrics reference, say) follows, by name. */
export const DOC_ORDER: readonly string[] = ['overview', 'install', 'keys', 'configuration', 'operations', 'troubleshooting', 'limits'];

/** The title of the sections that say what is planned and not in the code. */
export const NOT_YET = 'Not in a release yet';

export interface Doc {
  /** The file's name without '.md'. */
  stem: string;
  /** The text of its '# ' heading. */
  title: string;
  /** Its first paragraph as plain text, for the page's description. */
  summary: string;
  /** The inside of <main>, without the docs navigation. */
  body: string;
}

/** The anchor of a heading, as the test of the crate makes it: letters and digits lowercased, '_' kept, ' ' and '-' as '-'. */
export function slug(heading: string): string {
  let out = '';
  for (const c of heading) {
    if (/[a-z0-9_]/.test(c)) out += c;
    else if (/[A-Z]/.test(c)) out += c.toLowerCase();
    else if (c === ' ' || c === '-') out += '-';
  }
  return out;
}

export const docRoute = (stem: string): string => `/docs/${stem}/`;

/** `text` with its inline markup made HTML: code spans, bold and links. `known` is the set of pages a link may name. */
export function inline(text: string, known: ReadonlySet<string>): string {
  return text
    .split('`')
    .map((part, i) => (i % 2 === 1 ? `<code>${escapeText(part)}</code>` : plain(part, known)))
    .join('');
}

function plain(text: string, known: ReadonlySet<string>): string {
  let out = escapeText(text);
  out = out.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  out = out.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_m, label: string, target: string) => link(label, target, known));
  if (/\*\*|\]\(/.test(out)) throw new Error(`docs: markup this renderer does not read in: ${text}`);
  return out;
}

function link(label: string, target: string, known: ReadonlySet<string>): string {
  // The text was escaped before the links were read: undo that for the target, which is escaped again as an attribute.
  const t = target.replace(/&quot;/g, '"').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');
  if (/^https:\/\//.test(t)) return `<a href="${escapeText(t)}">${label}</a>`;
  const m = /^([a-z0-9_-]+)\.md(#[a-z0-9_-]+)?$/.exec(t);
  if (m === null) throw new Error(`docs: a link to ${t}: only https links and links to another page of docs/ are read`);
  // A page that is not there yet (the metrics reference, until it lands) is its label and not a dead link.
  if (!known.has(m[1])) return label;
  return `<a href="${docRoute(m[1])}${m[2] ?? ''}">${label}</a>`;
}

function cells(row: string): string[] {
  const trimmed = row.trim().replace(/^\|/, '').replace(/\|$/, '');
  return trimmed.split('|').map((c) => c.trim());
}

/** The inside of a page for `md`: the HTML, and the title and summary. `known` is the set of stems of the docs. */
export function renderMarkdown(md: string, known: ReadonlySet<string>, name = 'a page'): Omit<Doc, 'stem'> {
  const lines = md.replace(/\n$/, '').split('\n');
  const out: string[] = [];
  const ids = new Map<string, number>();
  let title = '';
  let summary = '';
  let i = 0;
  const fail = (why: string): never => {
    throw new Error(`docs: ${name}, line ${i + 1}: ${why}`);
  };
  const para: string[] = [];
  const flush = (): void => {
    if (para.length === 0) return;
    const text = para.join(' ');
    const first = title !== '' && summary === '';
    if (first) summary = text.replace(/`/g, '').replace(/\[([^\]]+)\]\([^)]*\)/g, '$1').replace(/\*\*/g, '');
    out.push(`<p${first && out.length === 1 ? ' class="lede"' : ''}>${inline(text, known)}</p>`);
    para.length = 0;
  };
  for (; i < lines.length; i++) {
    const line = lines[i];
    if (line.trim() === '') {
      flush();
      continue;
    }
    const fence = /^```(.*)$/.exec(line);
    if (fence !== null) {
      flush();
      const body: string[] = [];
      for (i++; i < lines.length && !lines[i].startsWith('```'); i++) body.push(lines[i]);
      if (i >= lines.length) fail('a code fence that is not closed');
      out.push(`<div class="code"><pre>${escapeText(body.join('\n'))}</pre></div>`);
      continue;
    }
    const heading = /^(#{1,3}) (.+)$/.exec(line);
    if (heading !== null) {
      flush();
      const level = heading[1].length;
      const text = heading[2].trim();
      if (level === 1) {
        if (title !== '') fail('a second # heading');
        title = text.replace(/`/g, '');
        out.push(`<h1>${inline(text, known)}</h1>`);
        continue;
      }
      let id = slug(text);
      const seen = (ids.get(id) ?? 0) + 1;
      ids.set(id, seen);
      if (seen > 1) id = `${id}-${seen}`;
      const tag = text === NOT_YET ? ' <span class="tag tag-planned">planned, not built</span>' : '';
      out.push(`<h${level} id="${id}">${inline(text, known)}${tag}</h${level}>`);
      continue;
    }
    if (line.startsWith('> ')) {
      flush();
      const quote: string[] = [];
      for (; i < lines.length && lines[i].startsWith('> '); i++) quote.push(lines[i].slice(2));
      i--;
      out.push(`<div class="notice notice-warn"><p>${inline(quote.join(' '), known)}</p></div>`);
      continue;
    }
    const item = /^(- |\d+\. )/.exec(line);
    if (item !== null) {
      flush();
      const ordered = item[1] !== '- ';
      const items: string[] = [];
      for (; i < lines.length && /^(- |\d+\. )/.test(lines[i]); i++) items.push(lines[i].replace(/^(- |\d+\. )/, ''));
      i--;
      const tag = ordered ? 'ol' : 'ul';
      out.push(`<${tag}>`, ...items.map((t) => `<li>${inline(t, known)}</li>`), `</${tag}>`);
      continue;
    }
    if (line.startsWith('|')) {
      flush();
      const rows: string[] = [];
      for (; i < lines.length && lines[i].startsWith('|'); i++) rows.push(lines[i]);
      i--;
      if (rows.length < 3 || !/^\|[\s:|-]+\|$/.test(rows[1])) fail('a table needs a header row and a separator row');
      const head = cells(rows[0]);
      const html = ['<div class="table-wrap"><table>', `<thead><tr>${head.map((c) => `<th>${inline(c, known)}</th>`).join('')}</tr></thead>`, '<tbody>'];
      for (const row of rows.slice(2)) {
        const c = cells(row);
        if (c.length !== head.length) fail(`a table row of ${c.length} cells under a header of ${head.length}`);
        html.push(`<tr>${c.map((x) => `<td>${inline(x, known)}</td>`).join('')}</tr>`);
      }
      html.push('</tbody></table></div>');
      out.push(...html);
      continue;
    }
    if (/^\s/.test(line)) fail('an indented line (nested lists and indented code are not read)');
    para.push(line);
  }
  flush();
  if (title === '') fail('no # heading');
  return { title, summary, body: out.join('\n') };
}

/** Every docs/*.md of `dir`, in the order of DOC_ORDER and then by name. */
export function loadDocs(dir: string): Doc[] {
  const stems = readdirSync(dir)
    .filter((f) => f.endsWith('.md'))
    .map((f) => f.replace(/\.md$/, ''));
  const rank = (s: string): number => (DOC_ORDER.includes(s) ? DOC_ORDER.indexOf(s) : DOC_ORDER.length);
  stems.sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
  const known = new Set(stems);
  return stems.map((stem) => ({ stem, ...renderMarkdown(readFileSync(join(dir, `${stem}.md`), 'utf8'), known, `docs/${stem}.md`) }));
}

/** The navigation of the guide: each page, the current one marked. */
function docsNav(docs: readonly Doc[], current: string | null): string {
  const items = docs.map((d) => `<li><a href="${docRoute(d.stem)}"${d.stem === current ? ' aria-current="page"' : ''}>${escapeText(d.title)}</a></li>`);
  return `<nav class="docs-nav" aria-label="Guide"><ul>\n${items.join('\n')}\n</ul></nav>`;
}

/** The pages of the docs: /docs/ and one for each file. Each carries its prerendered <main> in `html`. */
export function docsPages(docs: readonly Doc[]): Page[] {
  const index = [
    '<h1>Operator guide</h1>',
    '<p class="lede">How to install, configure and run tierstore. It is written from the source on main and checked against it by a test of the crate: every flag, key and message it names exists. What is planned and not in the code is under a heading that says so.</p>',
    '<ul class="docs-list">',
    ...docs.map((d) => `<li><a href="${docRoute(d.stem)}">${escapeText(d.title)}</a><p>${escapeText(d.summary)}</p></li>`),
    '</ul>',
  ].join('\n');
  const pages: Page[] = [
    {
      route: '/docs/',
      source: 'docs',
      title: 'Operator guide',
      description: 'The tierstore operator guide: install, keys and roles, configuration, operations, troubleshooting and limits.',
      nav: 'docs',
      scripts: ['copy.js'],
      bands: false,
      html: index,
    },
  ];
  for (const d of docs) {
    const description = d.summary.length > 200 ? `${d.summary.slice(0, 197)}...` : d.summary;
    pages.push({ route: docRoute(d.stem), source: `docs-${d.stem}`, title: d.title, description, nav: 'docs', scripts: ['copy.js'], bands: false, html: `${docsNav(docs, d.stem)}\n${d.body}` });
  }
  return pages;
}
