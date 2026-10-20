// T9 (from mopm.dev's site/): on arbitrary Markdown-shaped input the renderer never throws, emits only renderer markup
// with safe link targets, and escapes every '<' that is text (a '<' in the output always starts an allowlisted tag).
// mopm's test draws its input with fast-check; this site has no such dependency, so the input is drawn from a seeded
// generator (the same pieces, joined at random): 2000 documents, the same on every run.

import { expect, test } from './jest-compat';
import { expectSafe, md, MD_TAGS, tagsOf } from './md';

// Pieces that exercise every construct and every escape path, joined at random.
const PIECES = [
  '*', '**', '_', '__', '`', '``', '[', ']', '(', ')', '![', '<', '>', '&', '"', "'", '\\', '|', '#', '-', '+',
  '1.', '\n', '\n\n', '  \n', '    ', '\t', '> ', '- ', '```', '~~~', '---', '|---|', ':', '/', '#x',
  'javascript:', 'JaVaScRiPt:', 'data:', 'vbscript:', 'https://mopm.dev/', 'mailto:a@mopm.dev', '//evil', 'a@mopm.dev',
  '<https://mopm.dev/>', '<a@mopm.dev>', '[<https://mopm.dev/>](https://mopm.dev/x)',
  '<script>', '</script>', '<img onerror=x>', 'onerror=', 'style=', '&#115;', '%0a', '\u0000', '​', ' ',
];

/** mulberry32: a small seeded generator, so a failing document is the same on every run. */
function prng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function documentOf(rand: () => number): string {
  const n = Math.floor(rand() * 60);
  const parts: string[] = [];
  for (let i = 0; i < n; i++) {
    const r = rand();
    if (r < 0.7) parts.push(PIECES[Math.floor(rand() * PIECES.length)]);
    else if (r < 0.85) parts.push(Array.from({ length: Math.floor(rand() * 7) }, () => String.fromCharCode(32 + Math.floor(rand() * 95))).join(''));
    else parts.push(String.fromCodePoint(Math.floor(rand() * 0x2fff), Math.floor(rand() * 0x2fff) + 0x1f300).slice(0, 3));
  }
  return parts.join('');
}

test('never throws; output is allowlisted markup with safe links; text "<" is always escaped', () => {
  const rand = prng(20261014);
  for (let run = 0; run < 2000; run++) {
    const src = documentOf(rand);
    const html = md(src);
    // tagsOf throws on a '<' that does not start a well-formed tag, so this also proves text '<' was escaped.
    for (const t of tagsOf(html)) expect(MD_TAGS).toContain(t.tag);
    expectSafe(html);
  }
});
