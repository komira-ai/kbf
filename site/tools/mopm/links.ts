// URL checking: the only producer of SafeHref.
//
// A link from markdown (a package readme, or a repo doc rendered at build time) becomes a SafeHref or nothing; the
// renderer shows a refused link as its text. The decision is made on the string as a browser's URL parser sees it:
// control characters, whitespace and invisible format characters are removed first, the URL is resolved with the
// WHATWG parser, and only the normalised result is emitted, so what was checked is exactly what the browser follows.

import { DOCS } from '../docs-table.js';
import { type SafeHref, URL_UNSAFE_CHAR } from './doc.js';

export const REPO_URL = 'https://github.com/komira-ai/kbf';
const REPO_BLOB = `${REPO_URL}/blob/main/`;
const REPO_BLOB_PATH = new URL(REPO_BLOB).pathname;

/** Repo docs the site publishes, by source path: a relative link to one of them stays on the site. */
export const DOC_PAGES: readonly { slug: string; source: string }[] = DOCS.map((d) => ({ slug: d.slug, source: d.source }));

const ALLOWED_SCHEMES: ReadonlySet<string> = new Set(['https:', 'http:', 'mailto:']);
// C0 controls, space, DEL, C1 controls, and invisible format characters (zero-width, bidi controls, BOM): the set
// h() refuses in a URL attribute.
const STRIP = new RegExp(URL_UNSAFE_CHAR.source, 'g');
const HAS_SCHEME = /^[a-z][a-z0-9+.-]*:/i;

export type LinkContext =
  /** A package readme: absolute links and in-page fragments only; a relative link becomes text. */
  | { mode: 'readme'; idPrefix: string }
  /**
   * A repo doc at `source` (a repo path): relative links resolve in the repo, as GitHub resolves them, except that a
   * published doc named by file name or repo path stays on the site (sitePathByName).
   */
  | { mode: 'docs'; idPrefix: string; source: string };

function fragment(frag: string, idPrefix: string): SafeHref | null {
  if (frag === '') return null;
  let decoded = frag;
  try {
    decoded = decodeURIComponent(frag);
  } catch {
    // keep the raw text; it is encoded below either way
  }
  return `#${idPrefix}${encodeURIComponent(decoded)}` as SafeHref;
}

function sitePathFor(repoPath: string): string | null {
  for (const d of DOC_PAGES) if (d.source === repoPath) return `/docs/${d.slug}/`;
  return null;
}

/**
 * A link that names a published doc by its file name ('mopm-toml.md') or by its repo path ('spec/mopm-toml.md') means
 * that doc, from either published doc, even where GitHub would resolve it next to the linking doc (and show a 404).
 * `path` is the link with any query and fragment removed.
 */
function sitePathByName(path: string): string | null {
  const p = decodeURIComponentSafe(path.replace(/^(?:\.\/)+/, ''));
  for (const d of DOC_PAGES) {
    if (p === d.source || p === d.source.slice(d.source.lastIndexOf('/') + 1)) return `/docs/${d.slug}/`;
  }
  return null;
}

function repoLink(s: string, source: string, idPrefix: string): SafeHref | null {
  // A path starting with '/' is relative to the repo root, as on GitHub; anything else to the doc's directory.
  const dir = source.includes('/') ? source.slice(0, source.lastIndexOf('/') + 1) : '';
  const base = s.startsWith('/') ? REPO_BLOB : REPO_BLOB + dir;
  let u: URL;
  try {
    u = new URL(s.startsWith('/') ? s.slice(1) : s, base);
  } catch {
    return null;
  }
  // '//host/x', '..' past the repo root and a scheme the regex missed all land outside the blob prefix.
  if (!u.href.startsWith(REPO_BLOB)) return null;
  const site =
    sitePathFor(decodeURIComponentSafe(u.pathname.slice(REPO_BLOB_PATH.length))) ?? sitePathByName(s.split(/[?#]/)[0]);
  // A fragment on a site page names a heading id of that page, which carries the prefix the renderer gives its ids.
  return (site ? site + (fragment(u.hash.slice(1), idPrefix) ?? '') : u.href) as SafeHref;
}

function decodeURIComponentSafe(s: string): string {
  try {
    return decodeURIComponent(s);
  } catch {
    return s;
  }
}

/** The checked href for a markdown link destination, or null when it must be shown as text. */
export function safeHref(raw: string, ctx: LinkContext): SafeHref | null {
  const s = raw.replace(STRIP, '');
  if (s === '') return null;
  if (s.startsWith('#')) return fragment(s.slice(1), ctx.idPrefix);
  if (HAS_SCHEME.test(s)) {
    let u: URL;
    try {
      u = new URL(s);
    } catch {
      return null;
    }
    return ALLOWED_SCHEMES.has(u.protocol) ? (u.href as SafeHref) : null;
  }
  if (s.startsWith('//') || s.startsWith('\\') || s.startsWith('/\\')) return null;
  return ctx.mode === 'docs' ? repoLink(s, ctx.source, ctx.idPrefix) : null;
}

/** A same-site path built by routes.ts (always '/...', never protocol-relative). Throws on anything else. */
export function internalHref(path: string): SafeHref {
  // A tab or newline would be dropped by the browser's URL parser ('/\t/x' is '//x'), so any such character refuses.
  if (path.replace(STRIP, '') !== path || !path.startsWith('/') || path.startsWith('//') || path.startsWith('/\\')) {
    throw new Error(`internalHref: '${path}' is not a same-site path`);
  }
  return path as SafeHref;
}

/** A web link from API metadata (a repository or homepage): http or https only, normalised; anything else is null. */
export function webHref(raw: string): SafeHref | null {
  const href = safeHref(raw, { mode: 'readme', idPrefix: '' });
  return href !== null && /^https?:\/\//.test(href) ? href : null;
}

/**
 * A file's download link: only an https URL on the conda channel of the site the page is served on (`channel` is
 * condaBase(location.hostname)), with no credentials in it. Anything else, including a URL on another stage's
 * channel, is null and the page shows the file name as text.
 */
export function channelFileHref(raw: string, channel: string | null): SafeHref | null {
  if (channel === null || raw.replace(STRIP, '') !== raw) return null;
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    return null;
  }
  const c = new URL(channel);
  if (u.protocol !== 'https:' || u.host !== c.host || u.username !== '' || u.password !== '') return null;
  return u.href as SafeHref;
}

/** An in-page link to an element id the site itself sets (the skip link's '#main'). Throws on anything else. */
export function anchorHref(id: string): SafeHref {
  if (!/^[A-Za-z][A-Za-z0-9_-]*$/.test(id)) throw new Error(`anchorHref: '${id}' is not a plain id`);
  return `#${id}` as SafeHref;
}
