// The demo player: a person's real workflow, replayed. It plays two recorded files on one clock: the cast (the
// terminal of the run: the typed command, the editor's files and every line the build client printed, at its relative
// time) and the trace (what the farm did in the same window: leases per daemon, remote runs, cache hits). Every number
// on the page comes from them; nothing is generated. No inline style or markup from strings: the page's policy allows
// neither.

import cast from './data/cast.js';
import trace from './data/trace.js';

type Point = [number, number];
interface Packet {
  el: SVGElement;
  pts: Point[];
  segs: number[];
  len: number;
  at: number;
}
interface Totals {
  granted: number;
  running: number;
  perNode: number[];
  hits: number;
  remoteDone: number;
  failed: number;
  waiting: number;
}

const SVGNS = 'http://www.w3.org/2000/svg';
const ED_MS = 9, CMD_MS = 45, PAUSE_MS = 600; // typing speed, wall clock, independent of the replay speed

const $ = (id: string): HTMLElement => {
  const e = document.getElementById(id);
  if (!e) throw new Error(`demo: #${id} is missing`);
  return e;
};
const text = (id: string, s: string): void => {
  const e = document.getElementById(id);
  if (e) e.textContent = s;
};
function mmss(ms: number): string {
  const s = Math.floor(ms / 1000), m = Math.floor(s / 60);
  return m + ':' + (s % 60 < 10 ? '0' : '') + (s % 60);
}
function svg(tag: string, attrs: Record<string, string | number>, parent?: Element, txt?: string): SVGElement {
  const e = document.createElementNS(SVGNS, tag) as SVGElement;
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
  if (txt !== undefined) e.textContent = txt;
  if (parent) parent.appendChild(e);
  return e;
}

function player(T: TraceData, C: CastData, root: HTMLElement): void {
  const N = T.nodes, DUR = T.duration_ms;
  const grants = T.grants, remote = T.remote, hits = T.hits, lines = C.lines;
  const reduce = !!window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  // ---- the picture: built from the trace's node count ----
  const flow = $('flow');
  const LANE0 = 110, LANE_H = 68, RAIL_X = 968;
  const cy: number[] = [];
  for (let i = 0; i < N; i++) cy.push(LANE0 + 28 + i * LANE_H);
  const bottom = LANE0 + (N - 1) * LANE_H + 56;
  const mid = Math.round((cy[0] + cy[N - 1]) / 2);
  const fTop = Math.min(100, mid - 82), fH = Math.max(200, 2 * (mid - fTop));
  flow.setAttribute('viewBox', '0 0 990 ' + Math.max(bottom + 24, fTop + fH + 24));
  svg('title', { id: 'flow-t' }, flow, 'How an action moves through kbf');
  svg('desc', { id: 'flow-d' }, flow, 'Buck2 sends actions to the frontend. Actions answered by the action cache come back from storage. The rest are granted to daemons, which read inputs from storage and write outputs back to it.');
  svg('path', { class: 'edge', d: 'M150 ' + mid + ' L270 ' + mid }, flow);
  svg('path', { class: 'edge', d: 'M490 ' + (mid - 30) + ' L700 52' }, flow);
  for (let i = 0; i < N; i++) svg('path', { class: 'edge', d: 'M490 ' + mid + ' L700 ' + cy[i] }, flow);
  svg('path', { class: 'edge', d: 'M950 52 L' + RAIL_X + ' 52 L' + RAIL_X + ' ' + cy[N - 1] + ' L950 ' + cy[N - 1] }, flow);
  for (let i = 0; i < N - 1; i++) svg('path', { class: 'edge', d: 'M950 ' + cy[i] + ' L' + RAIL_X + ' ' + cy[i] }, flow);
  svg('text', { class: 'lab', x: 158, y: mid - 12 }, flow, 'actions');
  svg('text', { class: 'lab', x: 536, y: mid - 52, transform: 'rotate(-24 536 ' + (mid - 52) + ')' }, flow, 'cache lookups');
  svg('text', { class: 'lab', x: 590, y: mid + 22 }, flow, 'leases');
  const packetLayer = svg('g', {}, flow);
  const buck = svg('g', {}, flow);
  svg('rect', { class: 'node hot', x: 10, y: mid - 50, width: 140, height: 100, rx: 10 }, buck);
  svg('text', { class: 't1', x: 80, y: mid - 3, 'text-anchor': 'middle' }, buck, 'Buck2');
  svg('text', { class: 't2', x: 80, y: mid + 20, 'text-anchor': 'middle' }, buck, 'the build tool');
  const front = svg('g', { class: 'fe', id: 'fe' }, flow);
  svg('rect', { class: 'node hot', x: 270, y: fTop, width: 220, height: fH, rx: 10 }, front);
  svg('text', { class: 't1', x: 380, y: fTop + 36, 'text-anchor': 'middle' }, front, 'frontend');
  svg('text', { class: 't2', x: 380, y: fTop + 60, 'text-anchor': 'middle' }, front, 'checks the cache,');
  svg('text', { class: 't2', x: 380, y: fTop + 78, 'text-anchor': 'middle' }, front, 'grants misses to daemons');
  svg('rect', { class: 'track', x: 305, y: fTop + fH - 62, width: 150, height: 10, rx: 5 }, front);
  const qbar = svg('rect', { class: 'qbar', x: 305, y: fTop + fH - 62, width: 0, height: 10, rx: 5 }, front);
  const qn = svg('text', { class: 't2', x: 380, y: fTop + fH - 30, 'text-anchor': 'middle' }, front, '0 actions waiting');
  const store = svg('g', { class: 'st', id: 'store' }, flow);
  svg('rect', { class: 'node cool', x: 700, y: 20, width: 250, height: 64, rx: 10 }, store);
  svg('text', { class: 't1', x: 825, y: 48, 'text-anchor': 'middle' }, store, 'storage');
  svg('text', { class: 't2', x: 825, y: 68, 'text-anchor': 'middle' }, store, 'CAS and action cache');
  const bars: SVGElement[] = [], runs: SVGElement[] = [];
  for (let i = 0; i < N; i++) {
    const y = LANE0 + i * LANE_H;
    const g = svg('g', { class: 'dn' }, flow);
    svg('rect', { class: 'node', x: 700, y, width: 250, height: 56, rx: 10 }, g);
    svg('text', { class: 't1', x: 716, y: y + 23 }, g, 'daemon-' + (i + 1));
    runs.push(svg('text', { class: 't2', x: 716, y: y + 44 }, g, '0 running'));
    svg('rect', { class: 'track', x: 790, y: y + 34, width: 150, height: 8, rx: 4 }, g);
    bars.push(svg('rect', { class: 'bar', x: 790, y: y + 34, width: 0, height: 8, rx: 4 }, g));
  }

  // Peak running leases of one daemon, for the size of its bar.
  let peak = 1;
  const ev: [number, number, number][] = [];
  const cur: number[] = [];
  for (const gr of grants) ev.push([gr[1], gr[0], 1], [gr[2], gr[0], -1]);
  ev.sort((a, b) => a[0] - b[0] || a[2] - b[2]);
  for (let i = 0; i < N; i++) cur.push(0);
  for (const e of ev) {
    cur[e[1]] += e[2];
    if (cur[e[1]] > peak) peak = cur[e[1]];
  }

  // ---- the panes ----
  const buckText = C.editor.buck, bazelText = C.editor.bazel, command = C.command;
  const edBuck = $('ed-buck'), edBazel = $('ed-bazel'), term = $('term');
  text('ed-path', C.editor.path);
  edBazel.textContent = bazelText;
  function tab(which: 'buck' | 'bazel'): void {
    const isBuck = which === 'buck';
    $('tab-buck').setAttribute('aria-selected', String(isBuck));
    $('tab-bazel').setAttribute('aria-selected', String(!isBuck));
    $('ed-buck-wrap').hidden = !isBuck;
    $('ed-bazel-wrap').hidden = isBuck;
    $('ed-bazel-note').hidden = isBuck;
  }
  $('tab-buck').addEventListener('click', () => tab('buck'));
  $('tab-bazel').addEventListener('click', () => tab('bazel'));

  // ---- the clock ----
  let phase = 0, phaseT = 0; // 0: typing the file, 1: typing the command, 2: the build
  let elapsed = 0, playing = false, last = 0, speed = 30;
  let packets: Packet[] = [];
  const MAX_PACKETS = 36;
  let flashUntil = 0, badUntil = 0;
  let shownLines = -1, lastEd = -1;

  function state(t: number): Totals {
    const s: Totals = { granted: 0, running: 0, perNode: [], hits: 0, remoteDone: 0, failed: 0, waiting: 0 };
    for (let n = 0; n < N; n++) s.perNode.push(0);
    for (const gr of grants) {
      if (gr[1] > t) break;
      s.granted++;
      if (gr[2] > t) {
        s.running++;
        s.perNode[gr[0]]++;
      }
    }
    for (const r of remote) {
      if (r[1] <= t) {
        s.remoteDone++;
        if (r[2] === 1) s.failed++;
      }
    }
    for (const h of hits) if (h[0] <= t) s.hits += h[1];
    for (const p of T.client.pending) {
      if (p[0] <= t) s.waiting = p[1];
      else break;
    }
    return s;
  }

  const edChars = (): number => (phase === 0 ? Math.min(buckText.length, Math.floor(phaseT / ED_MS)) : buckText.length);
  const cmdChars = (): number => (phase < 1 ? 0 : phase === 1 ? Math.min(command.length, Math.floor(phaseT / CMD_MS)) : command.length);

  function renderTerminal(): void {
    let n = 0;
    if (phase === 2) {
      while (n < lines.length && lines[n][0] <= elapsed) n++;
    }
    const key = phase * 100000 + cmdChars() * 1000 + n;
    if (key === shownLines) return;
    shownLines = key;
    let out = '$ ' + command.slice(0, cmdChars());
    if (n > 0) out += '\n' + lines.slice(0, n).map((l) => l[1]).join('\n');
    term.textContent = out;
    term.scrollTop = term.scrollHeight;
  }

  function render(): void {
    const ec = edChars();
    if (ec !== lastEd) {
      edBuck.textContent = buckText.slice(0, ec);
      lastEd = ec;
    }
    renderTerminal();
    const t = phase === 2 ? elapsed : 0;
    const s = state(t);
    text('st-time', mmss(t) + ' of ' + mmss(DUR));
    text('st-waiting', String(s.waiting));
    text('st-running', String(s.running));
    text('st-hits', String(s.hits));
    text('st-remote', String(s.remoteDone));
    text('st-failed', String(s.failed));
    text('st-granted', String(s.granted));
    qbar.setAttribute('width', String(Math.round(150 * Math.min(1, s.waiting / 100))));
    qn.textContent = s.waiting + ' actions waiting';
    for (let n = 0; n < N; n++) {
      bars[n].setAttribute('width', String(Math.round((150 * s.perNode[n]) / peak)));
      runs[n].textContent = s.perNode[n] + ' running';
    }
    const now = performance.now();
    $('store').setAttribute('class', 'st' + (now < flashUntil ? ' flash' : ''));
    $('fe').setAttribute('class', 'fe' + (now < badUntil ? ' bad' : ''));
    const scrub = $('scrub') as HTMLInputElement;
    if (document.activeElement !== scrub) scrub.value = String(Math.round((1000 * t) / DUR));
    const done = phase === 2 && elapsed >= DUR;
    $('play').textContent = playing ? 'Pause' : done ? 'Replay' : 'Play';
    $('skip').hidden = phase === 2;
  }

  // A packet runs along a polyline; where it crosses the frontend box it is hidden behind it.
  function spawn(points: Point[], cls: string): void {
    if (reduce || packets.length >= MAX_PACKETS) return;
    const c = svg('circle', { r: 5, class: 'pkt ' + cls }, packetLayer);
    let len = 0;
    const segs: number[] = [];
    for (let i = 1; i < points.length; i++) {
      const d = Math.hypot(points[i][0] - points[i - 1][0], points[i][1] - points[i - 1][1]);
      segs.push(d);
      len += d;
    }
    packets.push({ el: c, pts: points, segs, len, at: 0 });
  }
  function place(p: Packet): void {
    let d = p.at * p.len, i = 0;
    while (i < p.segs.length - 1 && d > p.segs[i]) {
      d -= p.segs[i];
      i++;
    }
    const f = p.segs[i] ? Math.min(1, d / p.segs[i]) : 1;
    p.el.setAttribute('cx', String(p.pts[i][0] + (p.pts[i + 1][0] - p.pts[i][0]) * f));
    p.el.setAttribute('cy', String(p.pts[i][1] + (p.pts[i + 1][1] - p.pts[i][1]) * f));
  }
  function advancePackets(dt: number): void {
    for (let i = packets.length - 1; i >= 0; i--) {
      const p = packets[i];
      p.at += dt / 900;
      if (p.at >= 1) {
        p.el.remove();
        packets.splice(i, 1);
      } else place(p);
    }
  }
  function clearPackets(): void {
    for (let i = packets.length - 1; i >= 0; i--) packets[i].el.remove();
    packets = [];
  }

  // What happened in (from, to] of build time shows as motion.
  function emit(from: number, to: number): void {
    const now = performance.now();
    for (const gr of grants) {
      if (gr[1] > to) break;
      if (gr[1] > from) spawn([[150, mid], [270, mid], [490, mid], [700, cy[gr[0]]]], 'act');
      if (gr[2] > from && gr[2] <= to && gr[3] === 0) spawn([[950, cy[gr[0]]], [RAIL_X, cy[gr[0]]], [RAIL_X, 52], [950, 52]], 'blob');
    }
    for (const h of hits) {
      if (h[0] > from && h[0] <= to) {
        flashUntil = now + 450;
        spawn([[700, 52], [490, mid - 30], [270, mid], [150, mid]], 'hit');
      }
    }
    for (const r of remote) {
      if (r[2] === 1 && r[1] > from && r[1] <= to) {
        badUntil = now + 500;
        spawn([[490, mid], [270, mid], [150, mid]], 'bad');
      }
    }
  }

  function frame(now: number): void {
    if (!playing) return;
    const dt = Math.min(100, now - last);
    last = now;
    if (phase === 0) {
      phaseT += dt;
      if (phaseT >= buckText.length * ED_MS + PAUSE_MS) {
        phase = 1;
        phaseT = 0;
      }
    } else if (phase === 1) {
      phaseT += dt;
      if (phaseT >= command.length * CMD_MS + PAUSE_MS) {
        phase = 2;
        phaseT = 0;
      }
    } else {
      const from = elapsed;
      elapsed = Math.min(DUR, elapsed + dt * speed);
      emit(from, elapsed);
      if (elapsed >= DUR) playing = false;
    }
    advancePackets(dt);
    render();
    if (playing) window.requestAnimationFrame(frame);
  }
  function play(): void {
    if (phase === 2 && elapsed >= DUR) {
      phase = reduce ? 2 : 0;
      phaseT = 0;
      elapsed = 0;
      clearPackets();
    }
    playing = true;
    last = performance.now();
    window.requestAnimationFrame(frame);
    render();
  }
  function pause(): void {
    playing = false;
    render();
  }

  $('play').addEventListener('click', () => (playing ? pause() : play()));
  $('skip').addEventListener('click', () => {
    phase = 2;
    phaseT = 0;
    render();
  });
  $('scrub').addEventListener('input', () => {
    phase = 2;
    phaseT = 0;
    elapsed = Math.round((DUR * Number(($('scrub') as HTMLInputElement).value)) / 1000);
    clearPackets();
    render();
  });
  const sp = $('speed') as HTMLSelectElement;
  speed = Number(sp.value) || 30;
  sp.addEventListener('change', () => {
    speed = Number(sp.value);
  });

  // The facts that never change: the client's totals and the outcome.
  const c = T.client;
  text('tot-commands', String(c.commands));
  text('tot-cached', String(c.cached));
  text('tot-remote', String(c.remote));
  text('tot-local', String(c.local));
  text('tot-pct', String(Math.round((100 * c.cached) / c.commands)));
  text('tot-failed', String(c.failed));
  text('tot-duration', mmss(DUR));
  text('tot-outcome', C.outcome);
  text('tot-nodes', String(N));
  text('tot-grants', String(grants.length));
  text('tot-binms', '100');
  root.setAttribute('data-ready', 'true');
  tab('buck');

  // A link can open the replay at a second of the build: demo page `#t=250`.
  const jump = /^#t=(\d+)$/.exec(window.location.hash);
  if (jump) {
    phase = 2;
    elapsed = Math.min(DUR, Number(jump[1]) * 1000);
  }

  if (reduce || jump) {
    // Static: the finished file, the finished terminal, the totals. Play still replays the build.
    if (!jump) {
      phase = 2;
      elapsed = DUR;
    }
    render();
  } else {
    render();
    if ('IntersectionObserver' in window) {
      const seen = new IntersectionObserver(
        (es) => {
          if (es[0].isIntersecting) {
            seen.disconnect();
            if (!playing && phase === 0 && phaseT === 0) play();
          }
        },
        { threshold: 0.1 },
      );
      seen.observe(root);
    }
  }
}

const root = document.getElementById('demo');
if (root && trace.schema === 2 && cast.schema === 1) player(trace, cast, root);
