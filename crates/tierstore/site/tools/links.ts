// The link checker over a built dist/: every internal href and src names a file that exists, and every #fragment
// names an id on the page it points to. The router serves '/x/' as '/x/index.html' and nothing else is rewritten, so
// a link must be spelled the way the router resolves it ('/x' without a slash is a 404).

import { readFileSync } from 'fs';
import { join, posix } from 'path';
import { decodeAttr, isExternalUrl, listFiles, readTags } from './check-dist';

/** The file under dist a URL path is served from, or null. */
export function fileOf(path: string, have: ReadonlySet<string>): string | null {
  const rel = posix.normalize(path.replace(/^\//, ''));
  const file = rel === '.' || rel === '' || rel.endsWith('/') ? `${rel === '.' ? '' : rel}index.html` : rel;
  return have.has(file) ? file : null;
}

/** The problems found in dist: broken files, broken fragments. Empty when every internal link resolves. */
export function checkLinks(dir: string): string[] {
  const files = listFiles(dir);
  const have = new Set(files);
  const pages = files.filter((f) => f.endsWith('.html'));
  const ids = new Map<string, Set<string>>();
  const tagsOf = new Map<string, ReturnType<typeof readTags>['tags']>();
  for (const p of pages) {
    const { tags } = readTags(readFileSync(join(dir, p), 'utf8'));
    tagsOf.set(p, tags);
    ids.set(p, new Set(tags.flatMap(([, attrs]) => attrs.filter(([k]) => k === 'id').map(([, v]) => v))));
  }
  const problems: string[] = [];
  for (const p of pages) {
    const base = posix.dirname(p);
    for (const [tag, attrs] of tagsOf.get(p) ?? []) {
      for (const [name, raw] of attrs) {
        if (name !== 'href' && name !== 'src') continue;
        const v = decodeAttr(raw);
        if (v === null || isExternalUrl(v)) continue;
        const hash = v.indexOf('#');
        const pathPart = (hash < 0 ? v : v.slice(0, hash)).split('?')[0];
        const frag = hash < 0 ? null : v.slice(hash + 1);
        const target = pathPart === '' ? p : fileOf(pathPart.startsWith('/') ? pathPart : posix.join('/', base, pathPart), have);
        if (target === null) {
          problems.push(`${p}: <${tag} ${name}="${v}"> names no file`);
          continue;
        }
        if (frag !== null && frag !== '' && target.endsWith('.html') && !ids.get(target)?.has(frag)) {
          problems.push(`${p}: <${tag} ${name}="${v}"> names no id #${frag} on ${target}`);
        }
      }
    }
  }
  return problems.sort();
}
