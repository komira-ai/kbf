// The generated configuration reference (/docs/config/) against the golden kbf.toml of each mode.
//
// Catches: the page and the goldens disagreeing: a key added to a golden (a flag added to kbf-cli) and missing from the
// page, a key that is gone from the goldens and still on the page, a key listed under the wrong modes, a default that
// differs from the golden's, a key shown with a default that has none, a description that is not the comment above the
// key. The check reads the goldens with its own small parser (not tools/config.ts's), so a fault in the generator's
// parser shows as a disagreement and not as two copies of one mistake.

import assert from 'node:assert/strict';
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { configMain, GOLDEN_DIR, MODES, parseGolden, readGoldens, tablesOf } from '../tools/config';
import { REPO, SITE, buildReal, scratch } from './support';

const MODE_ORDER: readonly string[] = MODES;

interface Golden {
  /** section -> key -> { modes, default (TOML text) or null, description } */
  keys: Map<string, { modes: string[]; defaults: Set<string | null>; description: string }>;
}

/** An independent reading of the goldens: line by line, what the file says, and nothing the generator decides. */
function readGoldenDir(dir: string): Golden {
  const keys: Golden['keys'] = new Map();
  for (const mode of MODES) {
    let section = '';
    let comment: string[] = [];
    for (const line of readFileSync(join(dir, `kbf-${mode}.toml`), 'utf8').split('\n')) {
      const sec = /^\[(.+)\]$/.exec(line);
      if (sec) {
        section = sec[1];
        comment = [];
        continue;
      }
      if (section === '') continue;
      const commented = /^# ([a-z][a-z0-9-]*) = ("[^"]*"|\[\])$/.exec(line);
      const set = /^([a-z][a-z0-9-]*) = (.*)$/.exec(line);
      if (!commented && !set) {
        if (line.startsWith('#')) comment.push(line.replace(/^#\s?/, ''));
        else comment = [];
        continue;
      }
      const id = `${section}\u0000${(commented ?? set)![1]}`;
      const entry = keys.get(id) ?? { modes: [], defaults: new Set<string | null>(), description: comment.join(' ').replace(/\s+/g, ' ').trim() };
      entry.modes.push(mode);
      entry.defaults.add(commented ? null : (set as RegExpExecArray)[2]);
      keys.set(id, entry);
      comment = [];
    }
  }
  return { keys };
}

const decode = (s: string): string => s.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');
const plain = (html: string): string => decode(html.replace(/<[^>]*>/g, '')).replace(/\s+/g, ' ').trim();

/** What differs between a built config page and the goldens of `dir`. */
function drift(page: string, dir: string): string[] {
  const want = readGoldenDir(dir).keys;
  const problems: string[] = [];
  const seen = new Set<string>();
  for (const m of page.matchAll(/<h2 id="table-([^"]+)">\[([^\]]+)\]<\/h2>[\s\S]*?<tbody>([\s\S]*?)<\/tbody>/g)) {
    const section = m[2];
    for (const r of m[3].matchAll(/<tr id="key-[^"]+">([\s\S]*?)<\/tr>/g)) {
      const cells = [...r[1].matchAll(/<td>([\s\S]*?)<\/td>/g)].map((c) => c[1]);
      const key = /^<code>([^<]+)<\/code>/.exec(cells[0])?.[1] ?? '?';
      const id = `${section}\u0000${key}`;
      seen.add(id);
      const g = want.get(id);
      if (!g) {
        problems.push(`[${section}] ${key}: on the page, in no golden`);
        continue;
      }
      const modesCell = cells[2] === 'all' ? [...MODES] : [...cells[2].matchAll(/<code>([^<]+)<\/code>/g)].map((x) => x[1]);
      if (modesCell.join(',') !== g.modes.join(',')) problems.push(`[${section}] ${key}: modes ${modesCell.join(' ')} on the page, ${g.modes.join(' ')} in the goldens`);
      const shown = [...cells[1].matchAll(/<code>([^<]*)<\/code>|<span class="muted">none<\/span>/g)].map((x) => (x[1] === undefined ? null : decode(x[1])));
      const has = [...g.defaults];
      if (shown.length !== has.length || shown.some((v, i) => v !== has[i])) problems.push(`[${section}] ${key}: default ${JSON.stringify(shown)} on the page, ${JSON.stringify(has)} in the goldens`);
      if (plain(cells[3]) !== g.description.replace(/`/g, '')) problems.push(`[${section}] ${key}: the description differs from the comment above the key`);
    }
  }
  for (const id of want.keys()) if (!seen.has(id)) problems.push(`[${id.replace('\u0000', '] ')}: in the goldens, not on the page`);
  return problems;
}

const page = readFileSync(join(buildReal('config'), 'docs', 'config', 'index.html'), 'utf8');
const goldenDir = join(REPO, GOLDEN_DIR);

test('the page of /docs/config/ agrees with the five goldens: every key, its modes, default and description', () => {
  assert.deepEqual(drift(page, goldenDir), []);
  assert.equal(readGoldenDir(goldenDir).keys.size > 150, true, 'the goldens have the keys the check reads');
});

test('drift is caught: a key added to a golden, a key removed from one, a default changed, a comment changed', () => {
  const copy = join(scratch('config-drift'), 'golden');
  mkdirSync(copy, { recursive: true });
  cpSync(goldenDir, copy, { recursive: true });
  const f = join(copy, 'kbf-daemon.toml');
  const orig = readFileSync(f, 'utf8');
  const edit = (text: string): string[] => {
    writeFileSync(f, text);
    return drift(page, copy);
  };
  // A key added under [daemon] and missing from the page (the case the generator exists for).
  assert.match(edit(orig.replace('[daemon]\n', '[daemon]\n# A new flag.\nnew-flag = 1\n')).join('\n'), /\[daemon\] new-flag: in the goldens, not on the page/);
  // A key the goldens no longer have.
  assert.match(edit(orig.replace(/^daemon-user = .*\n/m, '')).join('\n'), /daemon-user: modes .* on the page/);
  // A default that changed.
  assert.match(edit(orig.replace('reconnect-ms = 1000', 'reconnect-ms = 2000')).join('\n'), /reconnect-ms: default \["1000"\]|reconnect-ms: default \[1000\]/);
  // A comment that changed.
  assert.match(edit(orig.replace('The wait, in milliseconds, after a session ends', 'The wait after a session ends')).join('\n'), /reconnect-ms: the description differs/);
  // And the unedited goldens agree again.
  assert.deepEqual(edit(orig), []);
});

test('the generator reads the three forms of a key line and groups keys by section', () => {
  const entries = parseGolden(
    ['# header', '', '[a]', '# One line.', 'k1 = 5', '# Two', '# lines.', '# k2 = "X"', '# Listed.', 'k3 = []', '', '[b]', 'k4 = "s"', ''].join('\n'),
  );
  assert.deepEqual(entries, [
    { section: 'a', key: 'k1', description: 'One line.', value: '5', placeholder: null },
    { section: 'a', key: 'k2', description: 'Two lines.', value: null, placeholder: '"X"' },
    { section: 'a', key: 'k3', description: 'Listed.', value: '[]', placeholder: null },
    { section: 'b', key: 'k4', description: '', value: '"s"', placeholder: null },
  ]);
});

test('a key is in the modes whose golden has it, and the order of the modes is the table\'s', () => {
  const tables = tablesOf(readGoldens(REPO));
  const daemon = tables.find((t) => t.section === 'daemon');
  assert.equal(daemon !== undefined, true);
  for (const t of tables) for (const r of t.rows) assert.deepEqual(r.modes, MODE_ORDER.filter((m) => r.modes.includes(m as never)));
  const key = (s: string, k: string) => tables.find((t) => t.section === s)?.rows.find((r) => r.key === k);
  assert.deepEqual(key('daemon', 'daemon-user')?.modes, ['daemon', 'storage-and-daemon', 'all-in-one']);
  assert.deepEqual(key('daemon', 'async-threads')?.modes, [...MODES]);
  assert.equal(key('daemon', 'async-threads')?.defaults[0].value, null, 'a commented key has no default');
  assert.equal(key('daemon', 'daemon-user')?.defaults[0].value, '"_kbf"');
});

test('the hand-written page is gone: /docs/config/ is generated, and says so', () => {
  assert.equal(existsSync(join(SITE, 'content', 'config.html')), false);
  assert.match(configMain(readGoldens(REPO)), /This page is generated from the templates in <code>crates\/kbf-cli\/tests\/golden<\/code>/);
  assert.equal(readdirSync(goldenDir).filter((f) => f.endsWith('.toml')).length, MODES.length);
});
