// T10: the parser does work linear in its input on the inputs that make naive Markdown parsers quadratic. The
// measures are deterministic, not a timer: the parser's own step counter (its JS loops), and a meter of the native
// string scans it calls (test/scan-meter.ts), so a rescan written with indexOf or a regex is counted too.
import { expect, test } from './jest-compat';
import { MAX_DEPTH, parse } from '../tools/mopm/markdown/parse';
import { metered } from './scan-meter';

const N = 64 * 1024;
const fill = (unit: string): string => unit.repeat(Math.ceil(N / unit.length)).slice(0, N);

// The bound, in steps per input character, calibrated on these inputs: flat input stays under 5, so 8 leaves room.
// Nested containers are the exception by design: each nesting level sees each of its lines once (a lazy line
// under a 16-deep quote is seen 16 times, about 18 steps per character measured), so those inputs are held to
// MAX_DEPTH + 4.
//
// The native-scan meter overcharges by design (an anchored line regex that fails is charged the whole line), so its
// bounds are looser, again calibrated: flat input measures at most 17 scanned characters per input character (each
// line is tried against a dozen block-start patterns), nested input at most 102 (each level re-tests its lines).
//
// A quadratic step, such as rescanning for a closing backtick run from each opening one, costs thousands of steps or
// scanned characters per input character on 64 KiB, whether it is a JS loop or an indexOf, and fails these bounds.
const FLAT = 8;
const NESTED = MAX_DEPTH + 4;
const SCAN_FLAT = 32;
const SCAN_NESTED = 8 * (MAX_DEPTH + 4);

const inputs: [string, string, boolean?][] = [
  ['open brackets', fill('[')],
  ['open bracket-paren pairs', fill('[](')],
  ['link openers with titles', fill('[a](b "')],
  ['angle destinations', fill('[a](<')],
  ['stars', fill('*')],
  ['alternating stars and letters', fill('*a')],
  ['underscores between punctuation', fill('_!')],
  ['nested emphasis', `${'*a '.repeat(N / 6)}x${' a*'.repeat(N / 6)}`],
  ['backtick runs of growing length', Array.from({ length: 360 }, (_, k) => '`'.repeat(k + 1)).join(' ').slice(0, N)],
  // Openers that never close (every length is unique) with text between them: a parser that looks for each closer
  // by scanning forward walks the rest of the input once per opener.
  ['unclosed backtick runs between text', Array.from({ length: 180 }, (_, k) => '`'.repeat(k + 1) + 'a'.repeat(200)).join('')],
  ['single backticks', fill('`a')],
  ['mixed backtick runs', fill('`` ` ``` `')],
  ['10k nested quotes', '>'.repeat(10_000) + ' x', true],
  ['10k nested quotes, spaced', '> '.repeat(10_000) + 'x', true],
  ['quote lines', fill('>\n')],
  ['deep quote then lazy lines', '>'.repeat(20) + ' a\n' + fill('b\n'), true],
  ['nested list items', Array.from({ length: 2000 }, (_, k) => ' '.repeat(2 * (k % 40)) + '- x').join('\n').slice(0, N), true],
  ['pipes', fill('|')],
  ['table rows of pipes', '|a|b|\n|-|-|\n' + fill('|||\n')],
  ['wide table', '|'.repeat(8000) + '\n' + '-|'.repeat(8000) + '\n' + fill('|x\n')],
  ['raw tag openers', fill('<a')],
  ['autolink openers', fill('<http:')],
  ['backslashes and quotes', fill('\\"')],
  ['parens in destinations', '[a](' + fill('(')],
  ['close parens after bracket pairs', fill('[a](b)')],
  // A link close processes the emphasis inside its brackets: with delimiters outside them, a walk that does not
  // stop at the bracket's bottom visits the whole delimiter stack once per link.
  ['links after stars', fill('*[a](b)')],
  ['empty links after stars', fill('*[]()')],
  ['links before stars', fill('[a](b)*')],
  ['links after underscores', fill('_[a](b)')],
  // openers_bottom must survive the removal of the delimiter it names: here a `*` closer marks a `_` as its stop,
  // that `_` is then matched and removed, and a stop kept as a pointer is never met again, so every later `*`
  // walks the whole `__` stack. Kept as a source position, the stop still holds.
  ['openers below matched pairs', ('__a '.repeat(N / 8) + '_*"'.repeat(N / 6)).slice(0, N)],
  ['star openers below matched pairs', ('a*__'.repeat(N / 8) + '_*"'.repeat(N / 6)).slice(0, N)],
  ['hash lines', fill('#\n')],
  ['fence openers', fill('```\n')],
  ['setext-looking lines', fill('a\n=\n')],
];

test.each(inputs)('%s', (_name, src, nested = false) => {
  const { value, scanned } = metered(() => parse(src));
  expect(value.steps).toBeLessThanOrEqual((nested ? NESTED : FLAT) * src.length);
  expect(scanned).toBeLessThanOrEqual((nested ? SCAN_NESTED : SCAN_FLAT) * src.length);
});

test('the scan meter sees a native rescan', () => {
  // An indexOf from each of 2000 positions to the end of a 4000-character string: about 6 million characters.
  const s = 'a'.repeat(4000);
  const { scanned } = metered(() => {
    for (let k = 0; k < 2000; k++) s.indexOf('b', k);
    return null;
  });
  expect(scanned).toBe(Array.from({ length: 2000 }, (_, k) => 4000 - k).reduce((x, y) => x + y));
  // And the methods are restored afterwards.
  expect(metered(() => null).scanned).toBe(0);
  expect(String.prototype.indexOf.toString()).toContain('[native code]');
  expect(RegExp.prototype.exec.toString()).toContain('[native code]');
});

test('the inputs are full size', () => {
  for (const [name, src] of inputs) expect([name, src.length >= 10_000]).toEqual([name, true]);
});
