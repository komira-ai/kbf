// The manifest of a built tree: every file's path and sha256, one per line, sorted. test/golden.manifest is its
// committed copy; `npm run golden` rewrites that copy from a fresh build after a change meant to alter the output.

import { createHash } from 'crypto';
import { readFileSync, writeFileSync } from 'fs';
import { join, resolve } from 'path';
import { buildSite } from './build';
import { listFiles } from './check-dist';

export function manifestOf(dir: string): string {
  return listFiles(dir).map((f) => `${createHash('sha256').update(readFileSync(join(dir, f))).digest('hex')}  ${f}\n`).join('');
}

if (require.main === module) {
  const site = resolve(__dirname, '..', '..', '..');
  const out = join(site, 'build', 'golden-dist');
  buildSite({ browserDir: join(site, 'build', 'browser'), staticDir: join(site, 'static'), contentDir: join(site, 'content'), repoRoot: resolve(site, '..'), outDir: out });
  writeFileSync(join(site, 'test', 'golden.manifest'), manifestOf(out));
}
