// The manifest of a built tree: every file's path and sha256, one per line, sorted, except the pages under docs/ (rendered from the guide;
// docs.test.ts holds them). test/golden.manifest is its committed copy; `npm run golden` rewrites it from a fresh build after a change meant to alter the output.

import { createHash } from 'crypto';
import { readFileSync, writeFileSync } from 'fs';
import { join, resolve } from 'path';
import { buildSite } from './build';
import { listFiles } from './check-dist';

/** The pages rendered from the guide are not in the manifest: docs.test.ts holds them, so a guide edit changes no golden. */
export function isGuidePage(file: string): boolean {
  return file.startsWith('docs/');
}

export function manifestOf(dir: string): string {
  return listFiles(dir).filter((f) => !isGuidePage(f)).map((f) => `${createHash('sha256').update(readFileSync(join(dir, f))).digest('hex')}  ${f}\n`).join('');
}

if (require.main === module) {
  const site = resolve(__dirname, '..', '..', '..');
  const out = join(site, 'build', 'golden-dist');
  buildSite({ browserDir: join(site, 'build', 'browser'), staticDir: join(site, 'static'), contentDir: join(site, 'content'), docsDir: join(site, '..', 'docs'), outDir: out });
  writeFileSync(join(site, 'test', 'golden.manifest'), manifestOf(out));
}
