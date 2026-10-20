// Checks a built dist/ before anything deploys it. The site is served under a Content-Security-Policy that allows
// 'self' only (infra/site: default-src 'none'; script-src, style-src and img-src 'self'; no connect-src), so each
// rule below is one the policy would otherwise enforce in a visitor's browser, found here instead:
//  - the pages the router and the error responses name exist;
//  - every '<' in a page starts a tag the scanner reads as the browser does (no comment, no '<' inside a tag, no
//    unclosed tag), so nothing below can be skipped;
//  - every <script> is empty and has a src under /assets/; no style attribute or element, no event-handler attribute,
//    no javascript:, data: or vbscript: URL in any *href, src, action, formaction or data, read after entity decoding
//    as a browser reads it;
//  - no subresource (link, script, img, ...) is loaded from another origin; only an <a> may leave the site;
//  - no stylesheet imports another or loads a url() from outside the site;
//  - no script fetches (no connect-src), evaluates a string, or writes markup from a string;
//  - only .html, .js and .css files are published, JavaScript and CSS within their budgets.
//
// The scanner and the attribute decoding are mopm.dev's (komira-ai/mopm, site/tools/check-dist.ts), the same rules.

import { readdirSync, readFileSync, statSync } from 'fs';
import { join, posix, relative, sep } from 'path';
import { DOCS, docRoute } from './docs-table';
import { outPath, PAGES } from './pages';

/** Relative paths ('/'-separated) of the files under dir, sorted. */
export function listFiles(dir: string): string[] {
  const out: string[] = [];
  const walk = (d: string): void => {
    for (const f of readdirSync(d).sort()) {
      const p = join(d, f);
      if (statSync(p).isDirectory()) walk(p);
      else out.push(relative(dir, p).split(sep).join('/'));
    }
  };
  walk(dir);
  return out.sort();
}

const IMPORT = /(?:\bfrom\s*|\bimport\s*\(?\s*)['"](\.{1,2}\/[^'"]+)['"]/g;

/** The relative imports of a module's source, resolved against its own path. */
export function importsOf(path: string, source: string): string[] {
  return [...source.matchAll(IMPORT)].map((m) => posix.normalize(posix.join(posix.dirname(path), m[1])));
}

/** One module. The recorded demo (cast) is the largest, about 45 KB. */
export const JS_MAX_BYTES = 64 * 1024;
/** The modules one page loads (its scripts and what they import), together. */
export const PAGE_JS_MAX_BYTES = 128 * 1024;
/** The theme (shared, never edited) and the site's own stylesheet, and the two a page links together. */
export const THEME_CSS_MAX_BYTES = 24 * 1024;
export const SITE_CSS_MAX_BYTES = 16 * 1024;
export const PAGE_CSS_MAX_BYTES = 40 * 1024;

export const REQUIRED: readonly string[] = [...PAGES.map((p) => p.route), ...DOCS.map(docRoute)].map(outPath);

export class DistError extends Error {
  constructor(problems: string[]) {
    super(`dist check failed:\n  ${problems.join('\n  ')}`);
    this.name = 'DistError';
  }
}

export type Tag = [string, [string, string][]];

const WS = new Set(['\t', '\n', '\f', '\r', ' ']);
const isAlpha = (c: string | undefined): boolean => c !== undefined && /^[a-zA-Z]$/.test(c);

/**
 * Reads one start or end tag beginning at html[i] (a '<'), as the HTML tokenizer does: a '/' separates attributes; a
 * name runs to whitespace, '/', '>' or '=' (and may start with '='); a quoted value runs to its quote, an unquoted
 * one to whitespace or '>'. Returns the tag and the index after its '>', or null when the page ends first.
 */
function readTag(html: string, i: number): { tag: Tag; end: number } | null {
  let j = i + 1;
  if (html[j] === '/') j++;
  let name = '';
  while (j < html.length && !WS.has(html[j]) && html[j] !== '/' && html[j] !== '>') name += html[j++];
  const attrs: [string, string][] = [];
  for (;;) {
    while (j < html.length && (WS.has(html[j]) || html[j] === '/')) j++;
    if (j >= html.length) return null;
    if (html[j] === '>') return { tag: [name.toLowerCase(), attrs], end: j + 1 };
    let attr = html[j++];
    while (j < html.length && !WS.has(html[j]) && html[j] !== '/' && html[j] !== '>' && html[j] !== '=') attr += html[j++];
    while (j < html.length && WS.has(html[j])) j++;
    let value = '';
    if (html[j] === '=') {
      j++;
      while (j < html.length && WS.has(html[j])) j++;
      const q = html[j];
      if (q === '"' || q === "'") {
        const close = html.indexOf(q, j + 1);
        if (close < 0) return null;
        value = html.slice(j + 1, close);
        j = close + 1;
      } else {
        while (j < html.length && !WS.has(html[j]) && html[j] !== '>') value += html[j++];
      }
    }
    attrs.push([attr.toLowerCase(), value]);
  }
}

/**
 * Every start tag in a page with its attributes (lowercased names, raw values, duplicates kept), and what the scanner
 * could not read. It fails closed: any '<' that does not start a start tag, an end tag or the leading doctype (a
 * comment, a '<' inside a tag), or a tag the page does not close, is a problem, so no markup the browser reads can
 * hide from the checks.
 */
export function readTags(html: string): { tags: Tag[]; problems: string[] } {
  const tags: Tag[] = [];
  const problems: string[] = [];
  const doctype = /^<!doctype html>/i.exec(html);
  let i = doctype ? doctype[0].length : 0;
  for (;;) {
    const lt = html.indexOf('<', i);
    if (lt < 0) break;
    const isTag = isAlpha(html[lt + 1]) || (html[lt + 1] === '/' && isAlpha(html[lt + 2]));
    if (!isTag) {
      problems.push(`a '<' that does not start a tag, at offset ${lt}`);
      i = lt + 1;
      continue;
    }
    const r = readTag(html, lt);
    if (r === null) {
      problems.push(`a tag the page does not close, at offset ${lt}`);
      break;
    }
    if (html.slice(lt + 1, r.end).includes('<')) problems.push(`a '<' inside a tag, at offset ${lt}`);
    if (html[lt + 1] !== '/') tags.push(r.tag);
    i = r.end;
  }
  return { tags, problems };
}

const NAMED: Readonly<Record<string, string>> = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", colon: ':', tab: '\t', newline: '\n' };
// Every *href (href, xlink:href) and the other attributes that take a URL a browser may navigate to or load.
const URL_ATTRS = new Set(['src', 'action', 'formaction', 'data']);
const isUrlAttr = (name: string): boolean => name.endsWith('href') || URL_ATTRS.has(name);

/** An attribute value as the browser reads it: character references decoded in one pass, null when one is unknown. */
export function decodeAttr(raw: string): string | null {
  let unknown = false;
  const out = raw.replace(/&(#[xX][0-9a-fA-F]+|#[0-9]+|[a-zA-Z][a-zA-Z0-9]*);?/g, (_m, ref: string) => {
    if (ref[0] === '#') {
      const cp = ref[1] === 'x' || ref[1] === 'X' ? parseInt(ref.slice(2), 16) : parseInt(ref.slice(1), 10);
      return cp > 0 && cp <= 0x10ffff ? String.fromCodePoint(cp) : '�';
    }
    const c = NAMED[ref.toLowerCase()];
    if (c === undefined) unknown = true;
    return c ?? '';
  });
  return unknown ? null : out;
}

/** The URL as the browser's URL parser sees it: leading C0 controls and spaces dropped, tabs and newlines removed. */
function urlOf(decoded: string): string {
  return decoded.replace(/[\t\n\r]/g, '').replace(/^[\x00-\x20]+/, '');
}

/** Whether a URL attribute value runs script or embeds a document. */
export function isScriptUrl(raw: string): boolean {
  const v = decodeAttr(raw);
  if (v === null) return true;
  return /^(?:javascript|data|vbscript):/i.test(urlOf(v));
}

/** Whether a URL attribute value names another origin (a scheme, or '//'). */
export function isExternalUrl(raw: string): boolean {
  const v = decodeAttr(raw);
  if (v === null) return true;
  return /^(?:[a-z][a-z0-9+.-]*:|\/\/|\\\\)/i.test(urlOf(v));
}

/** What no script may use: a fetch (the policy has no connect-src), a string run as code, or markup from a string. */
const FORBIDDEN_JS = /\b(?:fetch|XMLHttpRequest|WebSocket|EventSource|sendBeacon|eval|Function|innerHTML|outerHTML|insertAdjacentHTML|document\.write|importScripts)\b/;

function checkJs(f: string, text: string, have: ReadonlySet<string>, problems: string[]): void {
  const n = Buffer.byteLength(text);
  if (n > JS_MAX_BYTES) problems.push(`${f} is ${n} bytes; the limit for one module is ${JS_MAX_BYTES}`);
  if (!f.startsWith('assets/')) problems.push(`${f}: a script outside assets/`);
  if (/^assets\/[0-9a-f]+\/data\//.test(f)) {
    // Recorded data (build output text among it) is one JSON value and nothing else.
    const m = /^export default ([\s\S]*);\n$/.exec(text);
    try {
      if (m === null) throw new Error('no single export default');
      JSON.parse(m[1]);
    } catch {
      problems.push(`${f}: a data module must be \`export default <JSON>;\` and nothing else`);
    }
    return;
  }
  for (const dep of importsOf(f, text)) if (!have.has(dep)) problems.push(`${f}: imports ${dep}, which is not in dist`);
  const bad = FORBIDDEN_JS.exec(text);
  if (bad) problems.push(`${f}: uses ${bad[0]}, which the policy or the no-markup-from-strings rule refuses`);
  if (/\bimport\s*\(/.test(text)) problems.push(`${f}: a dynamic import (its target cannot be checked)`);
}

function checkCss(f: string, text: string, problems: string[]): void {
  const n = Buffer.byteLength(text);
  const name = posix.basename(f);
  const max = name === 'theme.css' ? THEME_CSS_MAX_BYTES : SITE_CSS_MAX_BYTES;
  if (n > max) problems.push(`${f} is ${n} bytes; the limit for ${name} is ${max}`);
  if (!f.startsWith('assets/')) problems.push(`${f}: a stylesheet outside assets/`);
  if (/@import\b/i.test(text)) problems.push(`${f}: an @import`);
  for (const m of text.matchAll(/url\(\s*(['"]?)([^'")]*)\1\s*\)/gi)) {
    if (isExternalUrl(m[2])) problems.push(`${f}: url(${m[2]}) leaves the site (img-src and font-src allow 'self' only)`);
  }
}

/** Throws a DistError listing every problem found. */
export function checkDist(dir: string): void {
  const files = listFiles(dir);
  const have = new Set(files);
  const problems: string[] = [];

  for (const r of REQUIRED) if (!have.has(r)) problems.push(`missing ${r}`);

  const size = new Map<string, number>();
  for (const f of files) size.set(f, statSync(join(dir, f)).size);

  for (const f of files) {
    const ext = posix.extname(f);
    if (!['.html', '.js', '.css'].includes(ext)) {
      problems.push(`${f}: only .html, .js and .css files are published`);
      continue;
    }
    const text = readFileSync(join(dir, f), 'utf8');
    if (ext === '.js') checkJs(f, text, have, problems);
    if (ext === '.css') checkCss(f, text, problems);
    if (ext !== '.html') continue;
    // A script's content runs whatever its attributes say; every one must be empty (an unclosed one runs to the end).
    for (const m of text.matchAll(/<script\b[^>]*>([\s\S]*?)(?:<\/script\s*>|$)/gi)) {
      if (m[1] !== '') problems.push(`${f}: an inline <script>`);
    }
    if (/<style\b/i.test(text)) problems.push(`${f}: a <style> element`);
    const { tags, problems: unread } = readTags(text);
    for (const p of unread) problems.push(`${f}: ${p}`);
    let pageCss = 0;
    const loaded: string[] = [];
    for (const [tag, attrs] of tags) {
      const href = attrs.find(([k]) => k === 'href')?.[1] ?? '';
      if (tag === 'link' && attrs.some(([k, v]) => k === 'rel' && v === 'stylesheet')) pageCss += size.get(href.replace(/^\//, '')) ?? 0;
      if (tag === 'script') {
        const src = attrs.filter(([name]) => name === 'src');
        if (src.length !== 1 || !src[0][1].startsWith('/assets/')) problems.push(`${f}: a <script> without one src under /assets/`);
        else loaded.push(src[0][1].slice(1));
      }
      for (const [name, value] of attrs) {
        if (name === 'style') problems.push(`${f}: a style attribute on <${tag}>`);
        if (name.startsWith('on')) problems.push(`${f}: an event-handler attribute ${name} on <${tag}>`);
        if (isUrlAttr(name) && isScriptUrl(value)) problems.push(`${f}: a script URL (or an unknown character reference) in ${name} on <${tag}>`);
        // Only an <a> may leave the site; anything the page loads or submits to stays on it.
        if (tag !== 'a' && isUrlAttr(name) && isExternalUrl(value)) problems.push(`${f}: <${tag} ${name}> names another origin`);
        const asset = (name === 'href' || name === 'src') && /^\/(assets\/[^#?]+)/.exec(value);
        if (asset && !have.has(asset[1])) problems.push(`${f}: references /${asset[1]}, which is not in dist`);
      }
    }
    if (pageCss > PAGE_CSS_MAX_BYTES) problems.push(`${f} links ${pageCss} bytes of CSS; the limit for one page is ${PAGE_CSS_MAX_BYTES}`);
    const mods = new Set<string>();
    const queue = loaded.filter((m) => have.has(m));
    while (queue.length > 0) {
      const m = queue.pop() as string;
      if (mods.has(m)) continue;
      mods.add(m);
      if (m.includes('/data/')) continue;
      for (const dep of importsOf(m, readFileSync(join(dir, m), 'utf8'))) if (have.has(dep)) queue.push(dep);
    }
    const js = [...mods].reduce((a, m) => a + (size.get(m) ?? 0), 0);
    if (js > PAGE_JS_MAX_BYTES) problems.push(`${f} loads ${js} bytes of JavaScript; the limit for one page is ${PAGE_JS_MAX_BYTES}`);
  }
  if (problems.length > 0) throw new DistError(problems);
}
