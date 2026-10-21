// The supply-chain policy of the site's own dependencies (and the Node version it is built with).
//
// Catches: a runtime dependency (the site ships none: its pages are static files and its script has no imports from
// packages), a range (^ or ~) or a tag where an exact version belongs, a lockfile entry that runs an install script,
// is fetched from anywhere but the npm registry, or carries no integrity hash, a Node version that is not the one
// pinned in .node-version, and a workflow that builds with another version or without --ignore-scripts. A drift in any
// of them changes what code runs in the build before anyone reviews it.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { read, REPO, SITE } from './support';

interface Lock {
  lockfileVersion: number;
  packages: Record<string, { version?: string; resolved?: string; integrity?: string; hasInstallScript?: boolean; link?: boolean }>;
}

function lockProblems(lock: Lock): string[] {
  const out: string[] = [];
  for (const [path, p] of Object.entries(lock.packages)) {
    if (path === '') continue;
    if (p.hasInstallScript) out.push(`${path}: runs an install script`);
    if (p.link === true || !p.resolved?.startsWith('https://registry.npmjs.org/')) out.push(`${path}: not resolved from registry.npmjs.org (${p.resolved ?? 'no resolved'})`);
    if (!p.integrity?.startsWith('sha512-')) out.push(`${path}: no sha512 integrity`);
  }
  return out;
}

function manifestProblems(pkg: { dependencies?: Record<string, string>; devDependencies?: Record<string, string> }): string[] {
  const out: string[] = [];
  if (pkg.dependencies && Object.keys(pkg.dependencies).length > 0) out.push('has runtime dependencies');
  for (const [name, v] of Object.entries(pkg.devDependencies ?? {})) if (!/^\d+\.\d+\.\d+$/.test(v)) out.push(`${name}: "${v}" is not an exact version`);
  return out;
}

const pkg = JSON.parse(read(SITE, 'package.json'));
const lock: Lock = JSON.parse(read(SITE, 'package-lock.json'));

test('package.json has no runtime dependencies and exact devDependencies', () => {
  assert.deepEqual(manifestProblems(pkg), []);
  assert.deepEqual(Object.keys(pkg.devDependencies).sort(), ['@types/node', 'typescript']);
});

test('engines pin the same Node as .node-version, and .npmrc makes a mismatch an error', () => {
  assert.equal(pkg.engines?.node, read(SITE, '.node-version').trim());
  assert.match(read(SITE, '.npmrc'), /^engine-strict=true$/m);
});

test('every lockfile entry is from the npm registry, hashed, and runs no install script', () => {
  assert.equal(lock.lockfileVersion, 3);
  assert.deepEqual(lockProblems(lock), []);
  assert.equal(lock.packages[''] !== undefined, true);
});

test('the policy checks can fail', () => {
  assert.deepEqual(manifestProblems({ dependencies: { x: '1.0.0' } }), ['has runtime dependencies']);
  assert.match(manifestProblems({ devDependencies: { a: '^1.2.3', b: '1.2.3' } }).join(), /a: "\^1.2.3" is not an exact version/);
  assert.match(lockProblems({ lockfileVersion: 3, packages: { 'node_modules/a': { resolved: 'https://example.org/a.tgz', integrity: 'sha512-x' } } }).join(), /not resolved from registry/);
  assert.match(lockProblems({ lockfileVersion: 3, packages: { 'node_modules/a': { resolved: 'https://registry.npmjs.org/a', integrity: 'sha1-x' } } }).join(), /no sha512 integrity/);
  assert.match(lockProblems({ lockfileVersion: 3, packages: { 'node_modules/a': { resolved: 'https://registry.npmjs.org/a', integrity: 'sha512-x', hasInstallScript: true } } }).join(), /runs an install script/);
});

test('Node is pinned to one exact version, and the workflow builds with that file, scripts off', () => {
  assert.match(read(SITE, '.node-version'), /^\d+\.\d+\.\d+\n$/);
  assert.equal(read(SITE, '.node-version'), '24.21.0\n');
  const wf = read(REPO, '.github', 'workflows', 'site.yml');
  assert.match(wf, /node-version-file: site\/\.node-version\n/);
  assert.equal(/^\s*node-version:/m.test(wf), false, 'a literal node-version would shadow .node-version');
  assert.match(wf, /npm ci --ignore-scripts --no-audit --no-fund/);
});
