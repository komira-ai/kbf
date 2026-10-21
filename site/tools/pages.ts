// The pages of the site: where each is served, which file under content/ holds its <main>, what its head says and
// which scripts it loads. The content files are the page copy as written for the site (content/*.html: the inside of
// <main>); the document around them is layout.ts.

export type NavKey = 'install' | 'quickstart' | 'config' | 'updates' | 'architecture';

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
  /** The home and the story run in bands that each carry their own .wrap; every other page sits in one. */
  bands: boolean;
  noindex?: boolean;
}

export const SITE_NAME = 'kbf';
export const REPO_URL = 'https://github.com/komira-ai/kbf';

/** The navigation, in order: the label and the page it names. */
export const NAV: readonly { key: NavKey; label: string; route: string }[] = [
  { key: 'install', label: 'Install', route: '/install/' },
  { key: 'quickstart', label: 'Quick start', route: '/quickstart/' },
  { key: 'config', label: 'Configuration', route: '/docs/config/' },
  { key: 'updates', label: 'Updates', route: '/docs/updates/' },
  { key: 'architecture', label: 'Architecture', route: '/docs/architecture/' },
];

export const PAGES: readonly Page[] = [
  {
    route: '/',
    source: 'index',
    title: 'Remote execution build farm',
    description: 'kbf is an open-source remote execution build farm for Buck2 and Bazel.',
    scripts: ['copy.js', 'demo.js'],
    bands: true,
  },
  { route: '/install/', source: 'install', title: 'Install', description: 'Install of kbf.', nav: 'install', scripts: ['copy.js'], bands: false },
  {
    route: '/quickstart/',
    source: 'quickstart',
    title: 'Quick start',
    description: 'From a host to a first remote build with kbf.',
    nav: 'quickstart',
    scripts: ['copy.js'],
    bands: false,
  },
  {
    route: '/story/',
    source: 'story',
    title: 'The story',
    description: 'Why we built kbf: what broke as we grew from one laptop to a farm.',
    scripts: ['copy.js', 'story.js'],
    bands: true,
  },
  {
    route: '/docs/config/',
    source: 'config',
    title: 'Configuration reference',
    description: 'Every kbf.toml setting, generated from the flag definitions.',
    nav: 'config',
    scripts: ['copy.js'],
    bands: false,
  },
  {
    route: '/docs/architecture/',
    source: 'architecture',
    title: 'Architecture',
    description: 'Architecture of kbf.',
    nav: 'architecture',
    scripts: ['copy.js'],
    bands: false,
  },
  { route: '/docs/design/', source: 'design', title: 'Design', description: 'How kbf is built.', scripts: ['copy.js'], bands: false },
  { route: '/docs/updates/', source: 'updates', title: 'Updates', description: 'Updates of kbf.', nav: 'updates', scripts: ['copy.js'], bands: false },
  {
    route: '/404.html',
    source: '404',
    title: 'Page not found',
    description: 'There is no such page on kbf.dev.',
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
