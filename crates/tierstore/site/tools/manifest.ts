// The manifest of a built tree: every file's path and sha256, one per line, sorted, except the pages rendered from the guide (docs/<stem>/index.html;
// docs.test.ts holds them). test/golden.manifest is its committed copy; `npm run golden` rewrites it from a fresh build after a change meant to alter the output.

import { createHash } from 'crypto';
import { readdirSync, readFileSync, writeFileSync } from 'fs';
import { join, resolve } from 'path';
import { buildSite } from './build';
import { listFiles } from './check-dist';

/** The stems of the guide: one per crates/tierstore/docs/<stem>.md. */
export function guideStems(docsDir: string): string[] {
  return readdirSync(docsDir).filter((f) => f.endsWith('.md')).map((f) => f.replace(/\.md$/, '')).sort();
}

/** The pages rendered from the guide, docs/<stem>/index.html: not in the manifest, docs.test.ts holds them. docs/index.html is
 *  hand-made (its heading, lede and title are in tools/docs.ts), so it is in the manifest, and so is anything else under docs/. */
export function isGuidePage(file: string, stems: readonly string[]): boolean {
  return stems.some((s) => file === `docs/${s}/index.html`);
}

export function manifestOf(dir: string, stems: readonly string[]): string {
  return listFiles(dir).filter((f) => !isGuidePage(f, stems)).map((f) => `${createHash('sha256').update(readFileSync(join(dir, f))).digest('hex')}  ${f}\n`).join('');
}

if (require.main === module) {
  const site = resolve(__dirname, '..', '..', '..');
  const out = join(site, 'build', 'golden-dist');
  buildSite({ browserDir: join(site, 'build', 'browser'), staticDir: join(site, 'static'), contentDir: join(site, 'content'), docsDir: join(site, '..', 'docs'), outDir: out });
  writeFileSync(join(site, 'test', 'golden.manifest'), manifestOf(out, guideStems(join(site, '..', 'docs'))));
}
