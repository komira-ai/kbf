// A meter of native string scanning: while `metered` runs `f`, the String and RegExp methods that walk a string
// add the number of characters they walk to a counter. The parser's step counter only sees its own JS loops; this
// sees work done inside the engine (an indexOf rescan, a regex run from each position), so T10 can bound it too.
//
// Each method is charged what it reads: indexOf/includes from the start position to the end of the match (or of
// the string), lastIndexOf likewise backwards, startsWith/endsWith the needle, slice/substring the result, and
// trim, and replace/split with a string pattern, the receiver. Once RegExp.prototype.exec is replaced, every regex
// method goes through it (test, and String replace/split/match/search with a regex), so only exec is charged for
// regexes: from where it starts (lastIndex for a global regex, else 0) to the end of its match, or to the end of the
// input when nothing matches; a sticky regex tries one position, so it is charged its match length (at least 1).
// Backtracking inside one exec is not seen.

type Method = (this: unknown, ...args: unknown[]) => unknown;
type Cost = (self: unknown, args: unknown[], out: unknown, before: number) => number;

const pos = (len: number, p: unknown): number => Math.min(len, Math.max(0, Number(p ?? 0) || 0));
const str = (x: unknown): string => String(x);

const STRING_COSTS: Record<string, Cost> = {
  indexOf: (s, [x, p], out) => {
    const start = pos(str(s).length, p);
    return (out as number) >= 0 ? (out as number) + str(x).length - start : str(s).length - start;
  },
  includes: (s, [x, p], out) => (out ? str(x).length : str(s).length - pos(str(s).length, p)),
  lastIndexOf: (s, [, p], out) => {
    const start = p === undefined ? str(s).length : pos(str(s).length, p);
    return (out as number) >= 0 ? start - (out as number) + 1 : start + 1;
  },
  startsWith: (_s, [x]) => str(x).length,
  endsWith: (_s, [x]) => str(x).length,
  slice: (_s, _a, out) => str(out).length,
  substring: (_s, _a, out) => str(out).length,
  trim: (s) => str(s).length,
  trimStart: (s) => str(s).length,
  trimEnd: (s) => str(s).length,
  replace: (s, [pat]) => (typeof pat === 'string' ? str(s).length : 0),
  replaceAll: (s, [pat]) => (typeof pat === 'string' ? str(s).length : 0),
  split: (s, [pat]) => (typeof pat === 'string' ? str(s).length : 0),
};

const execCost: Cost = (re, [input], out, before) => {
  const r = re as RegExp;
  const m = out as RegExpExecArray | null;
  // A sticky regex tries one position only (String split runs one per character this way).
  if (r.sticky) return m ? Math.max(1, m[0].length) : 1;
  const start = r.global ? before : 0;
  return m ? m.index + m[0].length - start : str(input).length - start;
};

export function metered<T>(f: () => T): { value: T; scanned: number } {
  let scanned = 0;
  const saved: [Record<string, Method>, string, Method][] = [];
  const wrap = (proto: Record<string, Method>, name: string, cost: Cost): void => {
    const orig = proto[name];
    saved.push([proto, name, orig]);
    proto[name] = function (this: unknown, ...args: unknown[]) {
      const before = this instanceof RegExp ? this.lastIndex : 0;
      const out = orig.apply(this, args);
      scanned += Math.max(0, cost(this, args, out, before));
      return out;
    };
  };
  try {
    for (const [name, cost] of Object.entries(STRING_COSTS)) {
      wrap(String.prototype as unknown as Record<string, Method>, name, cost);
    }
    wrap(RegExp.prototype as unknown as Record<string, Method>, 'exec', execCost);
    const value = f();
    return { value, scanned };
  } finally {
    for (const [proto, name, orig] of saved.reverse()) proto[name] = orig;
  }
}
