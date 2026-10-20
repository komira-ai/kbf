// The inline scanner of the restricted Markdown parser (see parse.ts for the supported subset).
//
// One left-to-right pass builds a doubly linked list of nodes, with CommonMark's delimiter stack for emphasis (using
// openers_bottom, so the look-back is amortised linear), a bracket stack for links, a table of backtick runs indexed
// once by length, and link-destination ends precomputed in two passes. No step rescans the input, so the work is
// linear in the length of the text; `st.steps` counts the loop iterations.

export type Inline =
  | { t: 'text'; v: string }
  /** Markup this renderer does not support (raw HTML and the like), shown as literal text. */
  | { t: 'raw'; v: string }
  | { t: 'code'; v: string }
  | { t: 'em'; c: Inline[] }
  | { t: 'strong'; c: Inline[] }
  | { t: 'link'; href: string; c: Inline[] }
  | { t: 'image'; href: string; alt: string }
  | { t: 'br' };

/** Counters shared by one parse (see parse.ts). */
export interface State {
  steps: number;
  fallbacks: number;
}

/** Emphasis and links nest at most this deep; deeper ones keep only their text. */
export const MAX_INLINE_DEPTH = 32;

// The scanner builds a doubly linked list of nodes so that wrapping a run in emphasis, or in a link, is O(1).
interface LNode {
  t: 'text' | 'raw' | 'code' | 'em' | 'strong' | 'link' | 'image' | 'br';
  v: string;
  href: string;
  prev: LNode | null;
  next: LNode | null;
  first: LNode | null;
  last: LNode | null;
}

interface Delim {
  node: LNode;
  ch: string;
  orig: number;
  /** Where the run starts in the source: delimiters enter the stack left to right, so this rises up the stack. */
  pos: number;
  canOpen: boolean;
  canClose: boolean;
  prev: Delim | null;
  next: Delim | null;
}

interface Bracket {
  node: LNode;
  image: boolean;
  /** The delimiter stack top when the bracket opened: emphasis inside the link text is processed above it. */
  bottom: Delim | null;
  index: number;
}

const ASCII_PUNCT = /[!-/:-@[-`{-~]/;
const UNICODE_WS = /\s/;
const UNICODE_PUNCT = /[\p{P}\p{S}]/u;
const AUTOLINK_URI = /^[A-Za-z][A-Za-z0-9+.-]{1,31}:[^\s<>]*$/;
const AUTOLINK_EMAIL = /^[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*$/;
const RAW_TAG = /^<(?:\/?[A-Za-z][A-Za-z0-9-]*(?:[\s/][^]*)?|![^]*|\?[^]*)>$/;

function mk(t: LNode['t'], v = ''): LNode {
  return { t, v, href: '', prev: null, next: null, first: null, last: null };
}

function isWs(c: string | undefined): boolean {
  return c === undefined || UNICODE_WS.test(c);
}

function isPunct(c: string | undefined): boolean {
  return c !== undefined && UNICODE_PUNCT.test(c);
}

/** Unescapes backslash escapes of ASCII punctuation (link destinations). Entities are left as written. */
function unescape(s: string): string {
  return s.replace(/\\([!-/:-@[-`{-~])/g, '$1');
}

export function parseInline(s: string, st: State): Inline[] {
  return new InlineScanner(s, st).run();
}

class InlineScanner {
  private readonly n: number;
  private head: LNode | null = null;
  private tail: LNode | null = null;
  private delims: Delim | null = null; // top of the delimiter stack
  private brackets: Bracket[] = [];
  private bracketCount = 0;
  /** Non-image brackets opened before this index are inactive (no links inside links). */
  private linkBarrier = 0;
  // Backtick runs by length: ascending positions and a cursor that only moves forward.
  private runs: Map<number, { pos: number[]; at: number }> | null = null;
  // Link destination ends, computed on first use.
  private closeAt: Int32Array | null = null;
  private segEnd: Int32Array | null = null;
  private depth: Int32Array | null = null;
  private nextQuote: Map<string, Int32Array> = new Map();

  constructor(private readonly s: string, private readonly st: State) {
    this.n = s.length;
  }

  private push(node: LNode): LNode {
    node.prev = this.tail;
    node.next = null;
    if (this.tail) this.tail.next = node;
    else this.head = node;
    this.tail = node;
    return node;
  }

  private text(v: string): void {
    if (v !== '') this.push(mk('text', v));
  }

  run(): Inline[] {
    const s = this.s;
    let i = 0;
    let textStart = 0;
    const flush = (end: number): void => {
      this.text(s.slice(textStart, end));
    };
    while (i < this.n) {
      this.st.steps++;
      const c = s[i];
      if (c === '\\') {
        const d = s[i + 1];
        if (d !== undefined && ASCII_PUNCT.test(d)) {
          flush(i);
          this.text(d);
          i += 2;
          textStart = i;
        } else if (d === '\n') {
          flush(i);
          this.push(mk('br'));
          i += 2;
          textStart = i;
        } else i++;
      } else if (c === '`') {
        flush(i);
        i = this.codeSpan(i);
        textStart = i;
      } else if (c === '*' || c === '_') {
        flush(i);
        i = this.delimRun(i);
        textStart = i;
      } else if (c === '[' || (c === '!' && s[i + 1] === '[')) {
        flush(i);
        const image = c === '!';
        const node = this.push(mk('text', image ? '![' : '['));
        this.brackets.push({ node, image, bottom: this.delims, index: this.bracketCount++ });
        i += image ? 2 : 1;
        textStart = i;
      } else if (c === ']') {
        flush(i);
        i = this.closeBracket(i);
        textStart = i;
      } else if (c === '<') {
        const end = this.angle(i, () => flush(i));
        if (end > i) textStart = i = end;
        else i++;
      } else if (c === '\n') {
        let k = i;
        while (k > textStart && s[k - 1] === ' ') {
          this.st.steps++;
          k--;
        }
        if (i - k >= 2) {
          flush(k);
          this.push(mk('br'));
          i++;
          textStart = i;
        } else i++;
      } else i++;
    }
    flush(this.n);
    this.processEmphasis(null);
    return this.toInlines(this.head);
  }

  // ---- code spans

  private indexRuns(): Map<number, { pos: number[]; at: number }> {
    const runs = new Map<number, { pos: number[]; at: number }>();
    const s = this.s;
    let i = 0;
    while (i < this.n) {
      this.st.steps++;
      if (s[i] !== '`') {
        i++;
        continue;
      }
      const start = i;
      while (i < this.n && s[i] === '`') {
        this.st.steps++;
        i++;
      }
      const len = i - start;
      let r = runs.get(len);
      if (!r) runs.set(len, (r = { pos: [], at: 0 }));
      r.pos.push(start);
    }
    return runs;
  }

  /** At a backtick run: a code span up to the next run of the same length, else the run as text. */
  private codeSpan(i: number): number {
    const s = this.s;
    let j = i;
    while (j < this.n && s[j] === '`') {
      this.st.steps++;
      j++;
    }
    const len = j - i;
    if (!this.runs) this.runs = this.indexRuns();
    const r = this.runs.get(len);
    let close = -1;
    if (r) {
      while (r.at < r.pos.length && r.pos[r.at] < j) {
        this.st.steps++;
        r.at++;
      }
      if (r.at < r.pos.length) close = r.pos[r.at];
    }
    if (close < 0) {
      this.text(s.slice(i, j));
      return j;
    }
    let v = s.slice(j, close).replace(/\n/g, ' ');
    if (v.length >= 2 && v[0] === ' ' && v[v.length - 1] === ' ' && v.trim() !== '') v = v.slice(1, -1);
    this.push(mk('code', v));
    return close + len;
  }

  // ---- emphasis

  private delimRun(i: number): number {
    const s = this.s;
    const ch = s[i];
    let j = i;
    while (j < this.n && s[j] === ch) {
      this.st.steps++;
      j++;
    }
    const before = i > 0 ? s[i - 1] : undefined;
    const after = s[j];
    const left = !isWs(after) && (!isPunct(after) || isWs(before) || isPunct(before));
    const right = !isWs(before) && (!isPunct(before) || isWs(after) || isPunct(after));
    const canOpen = ch === '*' ? left : left && (!right || isPunct(before));
    const canClose = ch === '*' ? right : right && (!left || isPunct(after));
    const node = this.push(mk('text', s.slice(i, j)));
    if (canOpen || canClose) {
      const d: Delim = { node, ch, orig: j - i, pos: i, canOpen, canClose, prev: this.delims, next: null };
      if (this.delims) this.delims.next = d;
      this.delims = d;
    }
    return j;
  }

  private removeDelim(d: Delim): void {
    if (d.prev) d.prev.next = d.next;
    if (d.next) d.next.prev = d.prev;
    else this.delims = d.prev;
  }

  private unlink(node: LNode): void {
    if (node.prev) node.prev.next = node.next;
    else this.head = node.next;
    if (node.next) node.next.prev = node.prev;
    else this.tail = node.prev;
  }

  /** CommonMark's process-emphasis over the delimiters above `bottom`, with openers_bottom to stay linear. */
  private processEmphasis(bottom: Delim | null): void {
    let current: Delim | null = this.delims;
    // Walk down to the first delimiter above bottom, stopping at bottom itself (when nothing is above it), so a link
    // close never walks the delimiters below its bracket.
    while (current && current !== bottom && current.prev !== bottom) {
      this.st.steps++;
      current = current.prev;
    }
    if (current === bottom) current = null;
    // openers_bottom holds source positions, not delimiters: the delimiter that marks a stop can itself be matched
    // and removed later, and a stop kept as a pointer to it would never be met again.
    const bottomPos = bottom ? bottom.pos : -1;
    const openersBottom: number[] = new Array(12).fill(bottomPos);
    while (current) {
      this.st.steps++;
      if (!current.canClose) {
        current = current.next;
        continue;
      }
      const key = (current.ch === '*' ? 0 : 6) + (current.canOpen ? 3 : 0) + (current.orig % 3);
      let opener: Delim | null = current.prev;
      let found = false;
      while (opener && opener !== bottom && opener.pos > openersBottom[key]) {
        this.st.steps++;
        if (opener.ch === current.ch && opener.canOpen) {
          const odd =
            (opener.canClose || current.canOpen) &&
            (opener.orig + current.orig) % 3 === 0 &&
            !(opener.orig % 3 === 0 && current.orig % 3 === 0);
          if (!odd) {
            found = true;
            break;
          }
        }
        opener = opener.prev;
      }
      if (found && opener) {
        const use = current.node.v.length >= 2 && opener.node.v.length >= 2 ? 2 : 1;
        opener.node.v = opener.node.v.slice(use);
        current.node.v = current.node.v.slice(use);
        const em = mk(use === 2 ? 'strong' : 'em');
        if (opener.node.next !== current.node) {
          em.first = opener.node.next;
          em.last = current.node.prev;
          if (em.first) em.first.prev = null;
          if (em.last) em.last.next = null;
        }
        opener.node.next = em;
        em.prev = opener.node;
        em.next = current.node;
        current.node.prev = em;
        // Delimiters between the opener and the closer are now plain text inside the emphasis.
        opener.next = current;
        current.prev = opener;
        if (opener.node.v === '') {
          this.unlink(opener.node);
          this.removeDelim(opener);
        }
        if (current.node.v === '') {
          this.unlink(current.node);
          const next: Delim | null = current.next;
          this.removeDelim(current);
          current = next;
        }
      } else {
        openersBottom[key] = current.prev ? current.prev.pos : bottomPos;
        const next: Delim | null = current.next;
        if (!current.canOpen) this.removeDelim(current);
        current = next;
      }
    }
    // Every delimiter above bottom is now plain text.
    while (this.delims && this.delims !== bottom) {
      this.st.steps++;
      this.removeDelim(this.delims);
    }
  }

  // ---- links and images

  private closeBracket(i: number): number {
    const opener = this.brackets[this.brackets.length - 1];
    if (!opener) {
      this.text(']');
      return i + 1;
    }
    this.brackets.pop();
    const active = opener.image || opener.index >= this.linkBarrier;
    const tail = active && this.s[i + 1] === '(' ? this.linkTail(i + 2) : null;
    if (!tail) {
      this.text(']');
      return i + 1;
    }
    this.processEmphasis(opener.bottom);
    const node = mk(opener.image ? 'image' : 'link');
    node.href = unescape(tail.dest);
    node.first = opener.node.next;
    node.last = node.first ? this.tail : null;
    if (node.first) node.first.prev = null;
    // Replace the opener's '[' with the link node, which takes everything after it as its text.
    node.prev = opener.node.prev;
    if (node.prev) node.prev.next = node;
    else this.head = node;
    this.tail = node;
    if (opener.image) node.v = this.plain(node.first);
    else this.linkBarrier = this.bracketCount;
    return tail.end;
  }

  /** Parses `dest [title])` from position j (just after '('). */
  private linkTail(j: number): { dest: string; end: number } | null {
    const s = this.s;
    j = this.skipSpace(j);
    let dest = '';
    if (s[j] === '<') {
      let k = j + 1;
      while (k < this.n && s[k] !== '>' && s[k] !== '<' && s[k] !== '\n') {
        this.st.steps++;
        k += s[k] === '\\' ? 2 : 1;
      }
      if (s[k] !== '>') return null;
      dest = s.slice(j + 1, k);
      j = k + 1;
    } else if (s[j] !== ')') {
      this.destIndex();
      const close = this.closeAt![j];
      const seg = this.segEnd![j];
      if (close >= 0 && close < seg) {
        return { dest: s.slice(j, close), end: close + 1 };
      }
      if (seg === j || this.depth![seg] !== this.depth![j]) return null;
      dest = s.slice(j, seg);
      j = seg;
    }
    const k = this.skipSpace(j);
    if (s[k] === ')') return { dest, end: k + 1 };
    if (k === j) return null; // a title must be separated from the destination
    const q = s[k];
    if (q !== '"' && q !== "'" && q !== '(') return null;
    const closeQ = this.nextUnescaped(q === '(' ? ')' : q)[k + 1];
    if (closeQ < 0) return null;
    const e = this.skipSpace(closeQ + 1);
    return s[e] === ')' ? { dest, end: e + 1 } : null;
  }

  private skipSpace(j: number): number {
    let newlines = 0;
    while (j < this.n && (this.s[j] === ' ' || this.s[j] === '\t' || this.s[j] === '\n')) {
      this.st.steps++;
      if (this.s[j] === '\n' && ++newlines > 1) break;
      j++;
    }
    return j;
  }

  /**
   * For every position j: closeAt[j], the first unescaped ')' at j's parenthesis depth before the next whitespace or
   * control character (-1 if none), and segEnd[j], that whitespace position. A destination may hold balanced
   * parentheses, so this is what ends a destination starting at j. Two passes, computed once per inline string.
   */
  private destIndex(): void {
    if (this.closeAt) return;
    const s = this.s;
    const n = this.n;
    const depth = new Int32Array(n + 1);
    const escaped = new Uint8Array(n + 1);
    const boundary = (c: string): boolean => c <= ' ' || c === '\u007f';
    let d = 0;
    for (let k = 0; k < n; k++) {
      this.st.steps++;
      const c = s[k];
      // A boundary records the depth the segment before it ended at, then the next segment starts again at 0.
      depth[k] = d;
      if (boundary(c)) {
        d = 0;
        continue;
      }
      if (c === '\\' && k + 1 < n && ASCII_PUNCT.test(s[k + 1])) {
        escaped[k + 1] = 1;
        depth[k + 1] = d;
        k++;
        continue;
      }
      if (c === '(') d++;
      else if (c === ')') d--;
    }
    depth[n] = d;
    const closeAt = new Int32Array(n + 1).fill(-1);
    const segEnd = new Int32Array(n + 1);
    let nextClose = new Map<number, number>();
    let end = n;
    for (let k = n - 1; k >= 0; k--) {
      this.st.steps++;
      const c = s[k];
      if (boundary(c)) {
        nextClose = new Map();
        end = k;
        segEnd[k] = k;
        continue;
      }
      segEnd[k] = end;
      if (c === ')' && !escaped[k]) nextClose.set(depth[k], k);
      closeAt[k] = nextClose.get(depth[k]) ?? -1;
    }
    segEnd[n] = n;
    this.closeAt = closeAt;
    this.segEnd = segEnd;
    this.depth = depth;
  }

  /** next[k]: the first unescaped `ch` at or after k, or -1. Computed once per character, right to left. */
  private nextUnescaped(ch: string): Int32Array {
    let a = this.nextQuote.get(ch);
    if (a) return a;
    const s = this.s;
    a = new Int32Array(this.n + 1).fill(-1);
    let next = -1;
    for (let k = this.n - 1; k >= 0; k--) {
      this.st.steps++;
      if (s[k] === ch) {
        let b = 0;
        while (k - 1 - b >= 0 && s[k - 1 - b] === '\\') {
          this.st.steps++;
          b++;
        }
        if (b % 2 === 0) next = k;
      }
      a[k] = next;
    }
    this.nextQuote.set(ch, a);
    return a;
  }

  /** At '<': an autolink or a raw HTML tag; returns the position after it, or i when it is neither. */
  private angle(i: number, flush: () => void): number {
    const s = this.s;
    let k = i + 1;
    while (k < this.n && s[k] !== '>' && s[k] !== '<') {
      this.st.steps++;
      k++;
    }
    if (s[k] !== '>') return i;
    const inner = s.slice(i + 1, k);
    if (AUTOLINK_URI.test(inner) || AUTOLINK_EMAIL.test(inner)) {
      flush();
      const node = mk('link');
      node.href = AUTOLINK_URI.test(inner) ? inner : `mailto:${inner}`;
      const t = mk('text', inner);
      node.first = node.last = t;
      this.push(node);
      return k + 1;
    }
    const whole = s.slice(i, k + 1);
    if (RAW_TAG.test(whole)) {
      flush();
      this.st.fallbacks++;
      this.push(mk('raw', whole));
      return k + 1;
    }
    return i;
  }

  // ---- output

  /** The text of a node list, without markup. Iterative: emphasis can nest as deep as the input is long. */
  private plain(n: LNode | null): string {
    let out = '';
    const resume: (LNode | null)[] = [n];
    while (resume.length > 0) {
      let x = resume.pop() ?? null;
      while (x) {
        this.st.steps++;
        if (x.t === 'em' || x.t === 'strong' || x.t === 'link') {
          resume.push(x.next);
          x = x.first;
          continue;
        }
        if (x.t !== 'br') out += x.v;
        x = x.next;
      }
    }
    return out;
  }

  private toInlines(n: LNode | null, depth = 0): Inline[] {
    const out: Inline[] = [];
    for (let x = n; x; x = x.next) {
      // Past MAX_INLINE_DEPTH, emphasis and links keep only their text, so rendering never recurses deeply.
      if (depth >= MAX_INLINE_DEPTH && (x.t === 'em' || x.t === 'strong' || x.t === 'link')) {
        const v = this.plain(x.first);
        const last = out[out.length - 1];
        if (last && last.t === 'text') last.v += v;
        else if (v !== '') out.push({ t: 'text', v });
        continue;
      }
      this.st.steps++;
      switch (x.t) {
        case 'text': {
          if (x.v === '') break;
          const last = out[out.length - 1];
          if (last && last.t === 'text') last.v += x.v;
          else out.push({ t: 'text', v: x.v });
          break;
        }
        case 'raw':
        case 'code':
          out.push({ t: x.t, v: x.v });
          break;
        case 'br':
          out.push({ t: 'br' });
          break;
        case 'em':
        case 'strong':
          out.push({ t: x.t, c: this.toInlines(x.first, depth + 1) });
          break;
        case 'link':
          out.push({ t: 'link', href: x.href, c: this.toInlines(x.first, depth + 1) });
          break;
        case 'image':
          out.push({ t: 'image', href: x.href, alt: x.v });
          break;
      }
    }
    return out;
  }
}
