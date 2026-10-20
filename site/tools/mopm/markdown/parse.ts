// A restricted Markdown parser (a CommonMark and GFM subset) for package readmes and the repo docs.
//
// Readmes are untrusted (up to 64 KiB), so the parser is built to run in linear time on any input: the block parser
// is line-based with a container depth cap, and the inline scanner makes one pass with a delimiter stack, a
// pre-indexed table of backtick runs and precomputed link-destination ends, so nothing rescans the input. Regular
// expressions are anchored and unambiguous. `steps` counts the iterations of the parser's own loops; a test holds it
// to a constant multiple of the input length.
//
// Supported: ATX headings, paragraphs, hard breaks, fenced and indented code, block quotes, bullet and ordered lists,
// thematic breaks, GFM tables, code spans, emphasis and strong emphasis, inline links, autolinks, images (as links).
// Everything else stays literal text: raw HTML (a `raw` node, counted in `fallbacks`), setext heading underlines and
// reference definitions (counted too), reference-style links, and entities (never decoded).

import { type Inline, type State, parseInline } from './inline.js';

export type { Inline } from './inline.js';
export { MAX_INLINE_DEPTH } from './inline.js';

export type Align = 'left' | 'center' | 'right' | null;

export type Block =
  | { t: 'heading'; level: number; c: Inline[] }
  | { t: 'para'; c: Inline[] }
  | { t: 'code'; lang: string; v: string }
  | { t: 'quote'; c: Block[] }
  | { t: 'list'; ordered: boolean; start: number; tight: boolean; items: Block[][] }
  | { t: 'hr' }
  | { t: 'table'; align: Align[]; head: Inline[][]; rows: Inline[][][] };

export interface Parsed {
  blocks: Block[];
  /** Loop iterations the parse took (see the header). */
  steps: number;
  /** Constructs shown as literal text because they are not supported (each is one `raw` node). */
  fallbacks: number;
}

/** Block quotes and list items nest at most this deep; deeper content is shown as literal text. */
export const MAX_DEPTH = 16;


export function parse(src: string): Parsed {
  const st: State = { steps: 0, fallbacks: 0 };
  const lines = src.replace(/\u0000/g, '\ufffd').split(/\r\n|\r|\n/);
  const { blocks } = parseBlocks(lines, 0, st);
  return { blocks, steps: st.steps, fallbacks: st.fallbacks };
}

// ---------------------------------------------------------------- blocks

const FENCE_OPEN = /^( {0,3})(`{3,}|~{3,})(.*)$/;
const FENCE_CLOSE = /^ {0,3}(`{3,}|~{3,})[ \t]*$/;
const ATX = /^ {0,3}(#{1,6})(?=[ \t]|$)/;
const QUOTE = /^ {0,3}>/;
const LIST_MARKER = /^( {0,3})([-+*]|[0-9]{1,9}[.)])(?=[ \t]|$)/;
const INTERRUPTING_ITEM = /^ {0,3}(?:[-+*]|1[.)])[ \t]+[^ \t]/;
const SETEXT_UNDERLINE = /^ {0,3}(?:=+|-+)[ \t]*$/;
const REF_DEFINITION = /^ {0,3}\[[^\]]+\]:/;
const BLANK = /^[ \t]*$/;

/** Expands tabs in the leading whitespace to spaces (tab stops of 4). */
function expandLead(line: string): string {
  let i = 0;
  while (i < line.length && (line[i] === ' ' || line[i] === '\t')) i++;
  if (line.slice(0, i).indexOf('\t') < 0) return line;
  let out = '';
  for (let k = 0; k < i; k++) out += line[k] === '\t' ? ' '.repeat(4 - (out.length % 4)) : ' ';
  return out + line.slice(i);
}

function indentOf(line: string): number {
  let i = 0;
  while (i < line.length && line[i] === ' ') i++;
  return i;
}

function isThematicBreak(line: string, st: State): boolean {
  if (indentOf(line) > 3) return false;
  let ch = '';
  let count = 0;
  for (let i = 0; i < line.length; i++) {
    st.steps++;
    const c = line[i];
    if (c === ' ' || c === '\t') continue;
    if (ch === '') {
      if (c !== '*' && c !== '-' && c !== '_') return false;
      ch = c;
    } else if (c !== ch) return false;
    count++;
  }
  return count >= 3;
}

/** Splits a table row on unescaped '|' (leading and trailing pipes dropped); '\|' becomes '|'. */
function splitCells(line: string, st: State): string[] {
  let s = line.trim();
  if (s.startsWith('|')) s = s.slice(1);
  if (s.endsWith('|') && !s.endsWith('\\|')) s = s.slice(0, -1);
  const cells: string[] = [];
  let cur = '';
  for (let i = 0; i < s.length; i++) {
    st.steps++;
    const c = s[i];
    if (c === '\\' && s[i + 1] === '|') {
      cur += '|';
      i++;
    } else if (c === '|') {
      cells.push(cur.trim());
      cur = '';
    } else cur += c;
  }
  cells.push(cur.trim());
  return cells;
}

function delimiterRow(line: string | undefined, st: State): Align[] | null {
  if (line === undefined || line.indexOf('|') < 0 || indentOf(line) > 3) return null;
  const cells = splitCells(line, st);
  const align: Align[] = [];
  for (const c of cells) {
    if (!/^:?-+:?$/.test(c)) return null;
    const l = c.startsWith(':');
    const r = c.endsWith(':');
    align.push(l && r ? 'center' : r ? 'right' : l ? 'left' : null);
  }
  return align;
}

/** The column alignments when lines[i] starts a GFM table (a header row, then a matching delimiter row). */
function tableStart(lines: string[], i: number, st: State): Align[] | null {
  const head = lines[i];
  if (head.indexOf('|') < 0 || indentOf(head) > 3) return null;
  const align = delimiterRow(lines[i + 1], st);
  if (!align || splitCells(head, st).length !== align.length) return null;
  return align;
}

function fenceOpen(line: string): { indent: number; fence: string; lang: string } | null {
  const m = FENCE_OPEN.exec(line);
  if (!m) return null;
  const info = m[3];
  if (m[2][0] === '`' && info.indexOf('`') >= 0) return null;
  const lang = info.trim().split(/[ \t]/, 1)[0] ?? '';
  return { indent: m[1].length, fence: m[2], lang };
}

function isFenceClose(line: string, fence: string): boolean {
  const m = FENCE_CLOSE.exec(line);
  return m !== null && m[1][0] === fence[0] && m[1].length >= fence.length;
}

/** True when the line starts a block that ends a paragraph (lazy continuation stops there too). */
function interrupts(lines: string[], i: number, st: State): boolean {
  const line = lines[i];
  return (
    fenceOpen(line) !== null ||
    ATX.test(line) ||
    QUOTE.test(line) ||
    isThematicBreak(line, st) ||
    INTERRUPTING_ITEM.test(line) ||
    tableStart(lines, i, st) !== null
  );
}

interface Marker {
  ordered: boolean;
  /** The bullet character, or the ordered delimiter ('.' or ')'). */
  kind: string;
  start: number;
  /** Column where the item's content starts; continuation lines must be indented this far. */
  width: number;
  content: string;
}

function listMarker(line: string): Marker | null {
  const m = LIST_MARKER.exec(line);
  if (!m) return null;
  const end = m[0].length;
  const tok = m[2];
  const ordered = tok.length > 1 || /[0-9]/.test(tok);
  // Expand tabs right after the marker, counting columns from the marker's end.
  let i = end;
  let col = end;
  while (i < line.length && (line[i] === ' ' || line[i] === '\t')) {
    col = line[i] === '\t' ? col + 4 - (col % 4) : col + 1;
    i++;
  }
  const rest = line.slice(i);
  const spaces = col - end;
  let width: number;
  let content: string;
  if (rest === '') {
    width = end + 1;
    content = '';
  } else if (spaces >= 5) {
    width = end + 1;
    content = ' '.repeat(spaces - 1) + rest;
  } else {
    width = end + spaces;
    content = rest;
  }
  return {
    ordered,
    kind: ordered ? tok[tok.length - 1] : tok,
    start: ordered ? parseInt(tok, 10) : 1,
    width,
    content,
  };
}

/** Tracks whether a run of lines leaves a fenced code block open (so a line inside it is never lazy). */
class FenceTracker {
  private open: string | null = null;
  feed(line: string): void {
    if (this.open === null) {
      const f = fenceOpen(line);
      if (f) this.open = f.fence;
    } else if (isFenceClose(line, this.open)) this.open = null;
  }
  get inFence(): boolean {
    return this.open !== null;
  }
}

function literal(lines: string[], st: State): Block {
  st.fallbacks++;
  return { t: 'para', c: [{ t: 'raw', v: lines.join('\n') }] };
}

function parseBlocks(input: string[], depth: number, st: State): { blocks: Block[]; blankBetween: boolean } {
  const lines = input.map(expandLead);
  const blocks: Block[] = [];
  let blankBetween = false;
  let sawBlank = false;
  const emit = (b: Block): void => {
    if (sawBlank && blocks.length > 0) blankBetween = true;
    sawBlank = false;
    blocks.push(b);
  };
  let i = 0;
  while (i < lines.length) {
    st.steps++;
    const line = lines[i];
    if (BLANK.test(line)) {
      sawBlank = true;
      i++;
      continue;
    }

    const fence = fenceOpen(line);
    if (fence) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !isFenceClose(lines[i], fence.fence)) {
        st.steps++;
        const l = lines[i];
        body.push(l.slice(Math.min(fence.indent, indentOf(l))));
        i++;
      }
      i++; // the closing fence (or the end)
      emit({ t: 'code', lang: fence.lang, v: body.join('\n') });
      continue;
    }

    if (indentOf(line) >= 4) {
      const body: string[] = [];
      while (i < lines.length && (indentOf(lines[i]) >= 4 || BLANK.test(lines[i]))) {
        st.steps++;
        body.push(lines[i].slice(4));
        i++;
      }
      while (body.length > 0 && BLANK.test(body[body.length - 1])) body.pop();
      emit({ t: 'code', lang: '', v: body.join('\n') });
      continue;
    }

    const atx = ATX.exec(line);
    if (atx) {
      emit({ t: 'heading', level: atx[1].length, c: parseInline(headingText(line.slice(atx[0].length)), st) });
      i++;
      continue;
    }

    if (isThematicBreak(line, st)) {
      emit({ t: 'hr' });
      i++;
      continue;
    }

    // At the depth cap a quote or list marker is no longer a container: the line stays literal text.
    if (depth >= MAX_DEPTH && (QUOTE.test(line) || listMarker(line) !== null)) {
      emit(literal([line.trim()], st));
      i++;
      continue;
    }

    if (QUOTE.test(line)) {
      const inner: string[] = [];
      const ft = new FenceTracker();
      while (i < lines.length) {
        st.steps++;
        const l = lines[i];
        if (QUOTE.test(l)) {
          let rest = l.slice(l.indexOf('>') + 1);
          if (rest.startsWith(' ') || rest.startsWith('\t')) rest = rest.slice(1);
          inner.push(rest);
        } else if (
          !BLANK.test(l) && inner.length > 0 && !BLANK.test(inner[inner.length - 1]) && !ft.inFence &&
          !interrupts(lines, i, st) && listMarker(l) === null
        ) {
          inner.push(l.trimStart()); // lazy continuation of a paragraph
        } else break;
        ft.feed(inner[inner.length - 1]);
        i++;
      }
      emit({ t: 'quote', c: parseBlocks(inner, depth + 1, st).blocks });
      continue;
    }

    const first = listMarker(line);
    if (first) {
      const items: Block[][] = [];
      let tight = true;
      let gap = false;
      let marker: Marker | null = first;
      while (marker) {
        st.steps++;
        if (gap) tight = false;
        const content = [marker.content];
        const ft = new FenceTracker();
        ft.feed(marker.content);
        i++;
        // An item that starts with a blank line holds at most that one blank line.
        const emptyStart = marker.content === '' && (i >= lines.length || BLANK.test(lines[i]));
        while (i < lines.length && !emptyStart) {
          st.steps++;
          const l = lines[i];
          if (BLANK.test(l)) content.push('');
          else if (indentOf(l) >= marker.width) content.push(l.slice(marker.width));
          else if (
            !BLANK.test(content[content.length - 1]) && !ft.inFence && !interrupts(lines, i, st) &&
            listMarker(l) === null
          ) {
            content.push(l.trimStart()); // lazy continuation of a paragraph
          } else break;
          ft.feed(content[content.length - 1]);
          i++;
        }
        let trailing = 0;
        while (content.length > 1 && content[content.length - 1] === '') {
          content.pop();
          trailing++;
        }
        const parsed = parseBlocks(content, depth + 1, st);
        if (parsed.blankBetween) tight = false;
        items.push(parsed.blocks);
        gap = trailing > 0;
        const next: Marker | null = i < lines.length ? listMarker(lines[i]) : null;
        marker =
          next && next.ordered === first.ordered && next.kind === first.kind && !isThematicBreak(lines[i], st)
            ? next
            : null;
      }
      emit({ t: 'list', ordered: first.ordered, start: first.start, tight, items });
      // Blank lines after the last item separate the list from the next block.
      if (gap) sawBlank = true;
      continue;
    }

    const align = tableStart(lines, i, st);
    if (align) {
      const head = splitCells(line, st).map((c) => parseInline(c, st));
      const rows: Inline[][][] = [];
      i += 2;
      while (i < lines.length && !BLANK.test(lines[i]) && !interrupts(lines, i, st)) {
        st.steps++;
        rows.push(splitCells(lines[i], st).slice(0, align.length).map((c) => parseInline(c, st)));
        i++;
      }
      emit({ t: 'table', align, head, rows });
      continue;
    }

    if (REF_DEFINITION.test(line)) {
      emit(literal([line.trim()], st));
      i++;
      continue;
    }

    // A paragraph: lines up to a blank line or a line that starts another block.
    const para: string[] = [line.trimStart()];
    i++;
    let underline: string | null = null;
    while (i < lines.length) {
      st.steps++;
      const l = lines[i];
      if (BLANK.test(l)) break;
      if (SETEXT_UNDERLINE.test(l)) {
        underline = l.trim();
        i++;
        break;
      }
      if (interrupts(lines, i, st)) break;
      para.push(l.trimStart());
      i++;
    }
    emit({ t: 'para', c: parseInline(para.join('\n').trimEnd(), st) });
    if (underline !== null) emit(literal([underline], st));
  }
  return { blocks, blankBetween };
}

/** An ATX heading's text: trimmed, without a closing run of '#' (which must follow a space, or be all there is). */
function headingText(s: string): string {
  let t = s.trim();
  let k = t.length;
  while (k > 0 && t[k - 1] === '#') k--;
  if (k === 0) return '';
  if (k < t.length && (t[k - 1] === ' ' || t[k - 1] === '\t')) t = t.slice(0, k).trimEnd();
  return t;
}
