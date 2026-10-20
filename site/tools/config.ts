// The configuration reference, generated from the golden kbf.toml of each mode (crates/kbf-cli/tests/golden/*.toml,
// which a kbf-cli test holds equal to the flag definitions). One table per [section], a row per key:
//   the comment lines above a key    the description
//   `key = value`                    the default, `value`
//   `# key = "X"` (commented out)    no default; X is the form of the value
//   modes                            the golden files that hold the key
// So a flag added to kbf-cli reaches this page when its golden is regenerated, and test/config.test.ts fails when the
// page and the goldens disagree.

import { readdirSync, readFileSync } from 'fs';
import { join } from 'path';
import { h, type SElement, serialize, type SNode, StringDoc } from './mopm/doc';
import { anchorHref } from './mopm/links';
import { renderMarkdown } from './mopm/markdown/render';

/** The modes, in the order the table lists them: the golden file is kbf-<mode>.toml. */
export const MODES = ['frontend', 'storage', 'daemon', 'storage-and-daemon', 'all-in-one'] as const;
export type Mode = (typeof MODES)[number];

export const GOLDEN_DIR = 'crates/kbf-cli/tests/golden';

export interface Entry {
  section: string;
  key: string;
  description: string;
  /** The TOML text of the default; null for a commented-out key. */
  value: string | null;
  /** The commented-out key's example value ('"N"', '[]'); null for a key with a default. */
  placeholder: string | null;
}

const SECTION = /^\[([A-Za-z0-9_.-]+)\]$/;
const KEY = /^([a-z][a-z0-9-]*) = (.+)$/;
const COMMENTED_KEY = /^# ([a-z][a-z0-9-]*) = ("[^"]*"|\[\])$/;

/** Reads one golden file: its entries in file order. Text above the first [section] is the file's own header. */
export function parseGolden(text: string): Entry[] {
  const out: Entry[] = [];
  let section: string | null = null;
  let comment: string[] = [];
  for (const line of text.split('\n')) {
    const s = SECTION.exec(line);
    if (s) {
      section = s[1];
      comment = [];
      continue;
    }
    if (section === null) continue;
    const c = COMMENTED_KEY.exec(line);
    const k = c ? null : KEY.exec(line);
    if (c || k) {
      const m = (c ?? k) as RegExpExecArray;
      out.push({ section, key: m[1], description: comment.join(' ').trim(), value: c ? null : m[2], placeholder: c ? m[2] : null });
      comment = [];
    } else if (line.startsWith('#')) {
      comment.push(line.slice(1).trim());
    } else {
      comment = [];
    }
  }
  return out;
}

export interface Row {
  key: string;
  description: string;
  /** Each distinct default with the modes that have it, in mode order; one `null` value for no default. */
  defaults: { value: string | null; placeholder: string | null; modes: Mode[] }[];
  modes: Mode[];
}
export interface Table {
  section: string;
  rows: Row[];
}

/** The golden files of a repository, by mode. Throws when one is missing (a mode with no golden has no page). */
export function readGoldens(repoRoot: string): Map<Mode, Entry[]> {
  const dir = join(repoRoot, GOLDEN_DIR);
  const have = new Set(readdirSync(dir));
  const out = new Map<Mode, Entry[]>();
  for (const m of MODES) {
    const f = `kbf-${m}.toml`;
    if (!have.has(f)) throw new Error(`config: ${GOLDEN_DIR}/${f} is missing`);
    out.set(m, parseGolden(readFileSync(join(dir, f), 'utf8')));
  }
  return out;
}

/** The tables of the page: sections in name order, keys in name order, the modes each key is in. */
export function tablesOf(goldens: ReadonlyMap<Mode, Entry[]>): Table[] {
  const bySection = new Map<string, Map<string, { mode: Mode; e: Entry }[]>>();
  for (const m of MODES) {
    for (const e of goldens.get(m) ?? []) {
      const keys = bySection.get(e.section) ?? new Map<string, { mode: Mode; e: Entry }[]>();
      bySection.set(e.section, keys);
      keys.set(e.key, [...(keys.get(e.key) ?? []), { mode: m, e }]);
    }
  }
  return [...bySection.keys()].sort().map((section) => {
    const keys = bySection.get(section) as Map<string, { mode: Mode; e: Entry }[]>;
    const rows = [...keys.keys()].sort().map((key): Row => {
      const seen = keys.get(key) as { mode: Mode; e: Entry }[];
      // The description is the first mode's (the modes that have a key share its text; the test says where they do not).
      const defaults: Row['defaults'] = [];
      for (const { mode, e } of seen) {
        const hit = defaults.find((d) => d.value === e.value && d.placeholder === e.placeholder);
        if (hit) hit.modes.push(mode);
        else defaults.push({ value: e.value, placeholder: e.placeholder, modes: [mode] });
      }
      return { key, description: seen[0].e.description, defaults, modes: seen.map((s) => s.mode) };
    });
    return { section, rows };
  });
}

const doc = new StringDoc();

/** A description (Markdown inline code and emphasis, as the comments are written) as the cell's markup. */
function describe(text: string): SNode[] {
  const cell = h(doc, 'div') as SElement;
  renderMarkdown(doc, cell, text, { idPrefix: 'cfg-', headingShift: 0, links: { mode: 'readme' } });
  return cell.children;
}

function modesCell(modes: readonly Mode[]): SNode[] {
  if (modes.length === MODES.length) return [doc.text('all')];
  return modes.flatMap((m, i) => [i > 0 ? doc.text(' ') : null, h(doc, 'code', null, m)]).filter((x): x is SNode => x !== null);
}

function defaultCell(row: Row): SNode[] {
  const out: SNode[] = [];
  row.defaults.forEach((d, i) => {
    if (i > 0) out.push(h(doc, 'br'));
    const label = d.value === null ? [h(doc, 'span', { class: 'muted' }, 'none')] : [h(doc, 'code', null, d.value)];
    out.push(...label);
    if (row.defaults.length > 1) out.push(doc.text(' in '), ...modesCell(d.modes));
  });
  return out;
}

/** The inside of <main> of /docs/config/. */
export function configMain(goldens: ReadonlyMap<Mode, Entry[]>): string {
  const tables = tablesOf(goldens);
  const parts: SNode[] = [
    h(doc, 'h1', null, 'Configuration reference'),
    h(
      doc,
      'p',
      null,
      'Every setting of ',
      h(doc, 'code', null, '/etc/kbf/kbf.toml'),
      ', by table. A key is the long name of a flag (',
      h(doc, 'code', null, '--storage-listen'),
      ' is ',
      h(doc, 'code', null, 'storage-listen'),
      '), and a flag on the command line overrides the key. A secret is never a value: the key holds the path of the file that has it. The file must not be writable by group or other. A key with no default is left out unless it is set; its example form is under the key.',
    ),
    h(
      doc,
      'p',
      null,
      'This page is generated from the templates in ',
      h(doc, 'code', null, GOLDEN_DIR),
      ', one for each mode (',
      ...MODES.flatMap((m, i) => [i > 0 ? ', ' : '', h(doc, 'code', null, m)]),
      '), which a test holds equal to the flag definitions in ',
      h(doc, 'code', null, 'kbf-cli'),
      '. A test of this site fails when the page and the templates disagree. The Modes column lists the modes whose template has the key.',
    ),
    h(doc, 'nav', { class: 'toc', 'aria-label': 'Tables' }, 'Tables: ', ...tables.flatMap((t, i) => [i > 0 ? ' ' : '', h(doc, 'a', { href: anchorHref(tableId(t.section)) }, t.section)])),
  ];
  for (const t of tables) {
    const rows = t.rows.map((r) =>
      h(
        doc,
        'tr',
        { id: rowId(t.section, r.key) },
        h(doc, 'td', null, h(doc, 'code', null, r.key), ...exampleOf(r)),
        h(doc, 'td', null, ...defaultCell(r)),
        h(doc, 'td', null, ...modesCell(r.modes)),
        h(doc, 'td', null, ...describe(r.description)),
      ),
    );
    parts.push(
      h(doc, 'h2', { id: tableId(t.section) }, `[${t.section}]`),
      h(
        doc,
        'div',
        { class: 'table-wrap', role: 'region', tabindex: '0', 'aria-label': `Table ${t.section}` },
        h(doc, 'table', null, h(doc, 'thead', null, h(doc, 'tr', null, ...['Key', 'Default', 'Modes', 'Description'].map((c) => h(doc, 'th', { scope: 'col' }, c)))), h(doc, 'tbody', null, ...rows)),
      ),
    );
  }
  return serialize(parts);
}

/** The example form of a key with no default, in a muted line under the key; none for a key with a default. */
function exampleOf(r: Row): SNode[] {
  const ph = r.defaults.find((d) => d.placeholder !== null)?.placeholder;
  return ph ? [h(doc, 'br'), h(doc, 'span', { class: 'muted small' }, `= ${ph}`)] : [];
}

export const tableId = (section: string): string => `table-${section}`;
export const rowId = (section: string, key: string): string => `key-${section}-${key}`;
