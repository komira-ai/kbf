// Shared by the tests: the site root and a build of the real site into a scratch directory under build/test/.
// (Compiled to build/tools/test/; the site root is three levels up from there.)

import { cpSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { join, resolve } from 'path';
import { buildSite } from '../tools/build';

export const SITE = resolve(__dirname, '..', '..', '..');
export const REPO = resolve(SITE, '..');

/** A fresh empty directory for one test. */
export function scratch(name: string): string {
  const dir = join(SITE, 'build', 'test', name);
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  return dir;
}

/** Builds the real site (check-dist off: each test runs the check it is about) and returns the dist directory. */
export function buildReal(name: string): string {
  const out = join(scratch(name), 'dist');
  buildSite({
    browserDir: join(SITE, 'build', 'browser'),
    staticDir: join(SITE, 'static'),
    contentDir: join(SITE, 'content'),
    outDir: out,
    check: false,
  });
  return out;
}

/** A copy of a built tree to plant a defect in. */
export function copyTree(from: string, name: string): string {
  const to = join(scratch(name), 'dist');
  cpSync(from, to, { recursive: true });
  return to;
}

/** Replaces the first occurrence of `find` in a file of the tree; throws when it is not there (a mutant must land). */
export function plant(dir: string, file: string, find: string | RegExp, replace: string): void {
  const p = join(dir, file);
  const text = readFileSync(p, 'utf8');
  const next = text.replace(find, replace);
  if (next === text) throw new Error(`plant: ${String(find)} not found in ${file}`);
  writeFileSync(p, next);
}

export const read = (...parts: string[]): string => readFileSync(join(...parts), 'utf8');

/** 'assets/<id>' of a built tree. */
export function assetDir(dist: string): string {
  const ids = readdirSync(join(dist, 'assets'));
  if (ids.length !== 1) throw new Error(`expected one asset directory, found ${ids.join(', ')}`);
  return `assets/${ids[0]}`;
}
