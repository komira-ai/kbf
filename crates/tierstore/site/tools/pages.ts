// The pages of the site: where each is served, which file under content/ holds its <main>, what its head says and
// which scripts it loads. The content files are the page copy as written for the site (content/*.html: the inside of
// <main>); the document around them is layout.ts.

export type NavKey = 'install' | 'demo' | 'why' | 'config' | 'operations';

export interface Page {
  /** The URL path: '/' or '/name/' or '/404.html'. */
  route: string;
  /** content/<source>.html, the inside of <main>. */
  source: string;
  /** The <title> without the site suffix. */
  title: string;
  description: string;
  /** The navigation entry this page is. */
  nav?: NavKey;
  /** Browser modules (names under assets/<id>/) this page loads, in order. */
  scripts: readonly string[];
  /** The home runs in bands that each carry their own .wrap; every other page sits in one. */
  bands: boolean;
  noindex?: boolean;
}

export const SITE_NAME = 'tierstore';
export const DOMAIN = 'tierstore.dev';
/** Where the source is today: tierstore ships from the kbf repository for now. */
export const REPO_URL = 'https://github.com/komira-ai/kbf';
export const CRATE_URL = `${REPO_URL}/tree/main/crates/tierstore`;
export const PACKAGING_URL = `${REPO_URL}/blob/main/packaging/tierstore/README.md`;
export const RELEASES_URL = `${REPO_URL}/releases`;

/**
 * The white paper. null while there is no final version: nothing on the site links to it, names it or reserves room
 * for it (test/hygiene.test.ts checks that). Set to the paper's URL, and the footer gains the link.
 */
export const WHITE_PAPER_URL: string | null = null;

/** The navigation, in order: the label and the page it names. */
export const NAV: readonly { key: NavKey; label: string; route: string }[] = [
  { key: 'install', label: 'Install', route: '/install/' },
  { key: 'demo', label: 'Demo', route: '/demo/' },
  { key: 'why', label: 'Why', route: '/why/' },
  { key: 'config', label: 'Configuration', route: '/docs/config/' },
  { key: 'operations', label: 'Operations', route: '/docs/operations/' },
];

export const PAGES: readonly Page[] = [
  {
    route: '/',
    source: 'index',
    title: 'A hot tier over a cold store, behind one S3 endpoint',
    description: 'tierstore is an S3 server over a hot and a cold tier of S3-compatible stores. Open source, Apache 2.0.',
    scripts: ['copy.js'],
    bands: true,
  },
  { route: '/install/', source: 'install', title: 'Install', description: 'Get tierstore from a kbf release and run it.', nav: 'install', scripts: ['copy.js'], bands: false },
  { route: '/demo/', source: 'demo', title: 'Demo', description: 'A start, the probes and a drain of tierstore, as the code prints them.', nav: 'demo', scripts: ['copy.js'], bands: false },
  { route: '/why/', source: 'why', title: 'Why tierstore', description: 'The problem tierstore solves and how the tiers work.', nav: 'why', scripts: ['copy.js'], bands: false },
  {
    route: '/docs/config/',
    source: 'config',
    title: 'Configuration',
    description: 'The tierstore TOML file, the flags and the key files.',
    nav: 'config',
    scripts: ['copy.js'],
    bands: false,
  },
  {
    route: '/docs/operations/',
    source: 'operations',
    title: 'Operations',
    description: 'Starting, stopping, health probes, usage and the start-up self-test of tierstore.',
    nav: 'operations',
    scripts: ['copy.js'],
    bands: false,
  },
  {
    route: '/404.html',
    source: '404',
    title: 'Page not found',
    description: 'There is no such page on tierstore.dev.',
    scripts: ['copy.js'],
    bands: false,
    noindex: true,
  },
];

/** The file a route is written to: '/' is index.html, '/a/b/' is a/b/index.html, '/404.html' is itself. */
export function outPath(route: string): string {
  const rel = route.replace(/^\//, '');
  return rel === '' || rel.endsWith('/') ? `${rel}index.html` : rel;
}

/**
 * The pages of the page copy name each other as '<source>.html' (the copy was written for a flat directory). Writes
 * those as the routes they are served at; an internal '.html' link that names no page is an error.
 */
export function rewriteLinks(html: string, pages: readonly Page[]): string {
  const bySource = new Map(pages.map((p) => [p.source, p.route]));
  return html.replace(/(\bhref=")([a-z0-9-]+)\.html(#[^"]*)?"/g, (_m, pre: string, name: string, frag: string | undefined) => {
    const route = bySource.get(name);
    if (route === undefined) throw new Error(`pages: a link to ${name}.html, which is no page`);
    return `${pre}${route}${frag ?? ''}"`;
  });
}
