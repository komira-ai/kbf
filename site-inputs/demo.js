// The demo player: a person's real workflow, replayed. It plays two recorded files on one
// clock: window.KBF_CAST (the terminal of the run: the typed command, the editor's files and
// every line the build client printed, at its relative time) and window.KBF_TRACE (what the
// farm did in the same window: leases per daemon, remote runs, cache hits). Every number on
// the page comes from them; nothing is generated. No inline style or markup from strings:
// the page's policy allows neither.
(function () {
  "use strict";
  var T = window.KBF_TRACE, C = window.KBF_CAST;
  var root = document.getElementById("demo");
  if (!T || !C || !root || T.schema !== 2 || C.schema !== 1) { return; }
  var SVGNS = "http://www.w3.org/2000/svg";
  var N = T.nodes, DUR = T.duration_ms;
  var grants = T.grants, remote = T.remote, hits = T.hits, lines = C.lines;
  var reduce = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var ED_MS = 9, CMD_MS = 45, PAUSE_MS = 600; // typing speed, wall clock, independent of the replay speed

  function $(id) { return document.getElementById(id); }
  function text(id, s) { var e = $(id); if (e) { e.textContent = s; } }
  function mmss(ms) {
    var s = Math.floor(ms / 1000), m = Math.floor(s / 60);
    return m + ":" + (s % 60 < 10 ? "0" : "") + (s % 60);
  }
  function svg(tag, attrs, parent, txt) {
    var e = document.createElementNS(SVGNS, tag);
    for (var k in attrs) { if (Object.prototype.hasOwnProperty.call(attrs, k)) { e.setAttribute(k, String(attrs[k])); } }
    if (txt !== undefined) { e.textContent = txt; }
    if (parent) { parent.appendChild(e); }
    return e;
  }

  // ---- the picture: built from the trace's node count ----
  var flow = $("flow");
  var LANE0 = 110, LANE_H = 68, RAIL_X = 968;
  var cy = [];
  for (var i = 0; i < N; i++) { cy.push(LANE0 + 28 + i * LANE_H); }
  var bottom = LANE0 + (N - 1) * LANE_H + 56;
  var mid = Math.round((cy[0] + cy[N - 1]) / 2);
  var fTop = Math.min(100, mid - 82), fH = Math.max(200, 2 * (mid - fTop));
  flow.setAttribute("viewBox", "0 0 990 " + Math.max(bottom + 24, fTop + fH + 24));
  svg("title", { id: "flow-t" }, flow, "How an action moves through kbf");
  svg("desc", { id: "flow-d" }, flow, "Buck2 sends actions to the frontend. Actions answered by the action cache come back from storage. The rest are granted to daemons, which read inputs from storage and write outputs back to it.");
  svg("path", { "class": "edge", d: "M150 " + mid + " L270 " + mid }, flow);
  svg("path", { "class": "edge", d: "M490 " + (mid - 30) + " L700 52" }, flow);
  for (i = 0; i < N; i++) { svg("path", { "class": "edge", d: "M490 " + mid + " L700 " + cy[i] }, flow); }
  svg("path", { "class": "edge", d: "M950 52 L" + RAIL_X + " 52 L" + RAIL_X + " " + cy[N - 1] + " L950 " + cy[N - 1] }, flow);
  for (i = 0; i < N - 1; i++) { svg("path", { "class": "edge", d: "M950 " + cy[i] + " L" + RAIL_X + " " + cy[i] }, flow); }
  svg("text", { "class": "lab", x: 158, y: mid - 12 }, flow, "actions");
  svg("text", { "class": "lab", x: 536, y: mid - 52, transform: "rotate(-24 536 " + (mid - 52) + ")" }, flow, "cache lookups");
  svg("text", { "class": "lab", x: 590, y: mid + 22 }, flow, "leases");
  var packetLayer = svg("g", {}, flow);
  var buck = svg("g", {}, flow);
  svg("rect", { "class": "node hot", x: 10, y: mid - 50, width: 140, height: 100, rx: 10 }, buck);
  svg("text", { "class": "t1", x: 80, y: mid - 3, "text-anchor": "middle" }, buck, "Buck2");
  svg("text", { "class": "t2", x: 80, y: mid + 20, "text-anchor": "middle" }, buck, "the build tool");
  var front = svg("g", { "class": "fe", id: "fe" }, flow);
  svg("rect", { "class": "node hot", x: 270, y: fTop, width: 220, height: fH, rx: 10 }, front);
  svg("text", { "class": "t1", x: 380, y: fTop + 36, "text-anchor": "middle" }, front, "frontend");
  svg("text", { "class": "t2", x: 380, y: fTop + 60, "text-anchor": "middle" }, front, "checks the cache,");
  svg("text", { "class": "t2", x: 380, y: fTop + 78, "text-anchor": "middle" }, front, "grants misses to daemons");
  svg("rect", { "class": "track", x: 305, y: fTop + fH - 62, width: 150, height: 10, rx: 5 }, front);
  var qbar = svg("rect", { "class": "qbar", x: 305, y: fTop + fH - 62, width: 0, height: 10, rx: 5 }, front);
  var qn = svg("text", { "class": "t2", x: 380, y: fTop + fH - 30, "text-anchor": "middle" }, front, "0 actions waiting");
  var store = svg("g", { "class": "st", id: "store" }, flow);
  svg("rect", { "class": "node cool", x: 700, y: 20, width: 250, height: 64, rx: 10 }, store);
  svg("text", { "class": "t1", x: 825, y: 48, "text-anchor": "middle" }, store, "storage");
  svg("text", { "class": "t2", x: 825, y: 68, "text-anchor": "middle" }, store, "CAS and action cache");
  var bars = [], runs = [];
  for (i = 0; i < N; i++) {
    var y = LANE0 + i * LANE_H;
    var g = svg("g", { "class": "dn" }, flow);
    svg("rect", { "class": "node", x: 700, y: y, width: 250, height: 56, rx: 10 }, g);
    svg("text", { "class": "t1", x: 716, y: y + 23 }, g, "daemon-" + (i + 1));
    runs.push(svg("text", { "class": "t2", x: 716, y: y + 44 }, g, "0 running"));
    svg("rect", { "class": "track", x: 790, y: y + 34, width: 150, height: 8, rx: 4 }, g);
    bars.push(svg("rect", { "class": "bar", x: 790, y: y + 34, width: 0, height: 8, rx: 4 }, g));
  }

  // Peak running leases of one daemon, for the size of its bar.
  var peak = 1, ev = [], cur = [];
  for (i = 0; i < grants.length; i++) { ev.push([grants[i][1], grants[i][0], 1], [grants[i][2], grants[i][0], -1]); }
  ev.sort(function (a, b) { return a[0] - b[0] || a[2] - b[2]; });
  for (i = 0; i < N; i++) { cur.push(0); }
  for (i = 0; i < ev.length; i++) { cur[ev[i][1]] += ev[i][2]; if (cur[ev[i][1]] > peak) { peak = cur[ev[i][1]]; } }

  // ---- the panes ----
  var buckText = C.editor.buck, bazelText = C.editor.bazel, command = C.command;
  var edBuck = $("ed-buck"), edBazel = $("ed-bazel"), term = $("term");
  text("ed-path", C.editor.path);
  edBazel.textContent = bazelText;
  function tab(which) {
    var isBuck = which === "buck";
    $("tab-buck").setAttribute("aria-selected", String(isBuck));
    $("tab-bazel").setAttribute("aria-selected", String(!isBuck));
    $("ed-buck-wrap").hidden = !isBuck;
    $("ed-bazel-wrap").hidden = isBuck;
    $("ed-bazel-note").hidden = isBuck;
  }
  $("tab-buck").addEventListener("click", function () { tab("buck"); });
  $("tab-bazel").addEventListener("click", function () { tab("bazel"); });

  // ---- the clock ----
  var phase = 0, phaseT = 0; // 0: typing the file, 1: typing the command, 2: the build
  var elapsed = 0, playing = false, last = 0, speed = 30;
  var packets = [], MAX_PACKETS = 36, flashUntil = 0, badUntil = 0;
  var shownLines = -1, lastEd = -1;

  function state(t) {
    var s = { granted: 0, running: 0, perNode: [], hits: 0, remoteDone: 0, failed: 0, waiting: 0 };
    for (var n = 0; n < N; n++) { s.perNode.push(0); }
    for (var k = 0; k < grants.length; k++) {
      var gr = grants[k];
      if (gr[1] > t) { break; }
      s.granted++;
      if (gr[2] > t) { s.running++; s.perNode[gr[0]]++; }
    }
    for (k = 0; k < remote.length; k++) {
      if (remote[k][1] <= t) { s.remoteDone++; if (remote[k][2] === 1) { s.failed++; } }
    }
    for (k = 0; k < hits.length; k++) { if (hits[k][0] <= t) { s.hits += hits[k][1]; } }
    var p = T.client.pending, waiting = 0;
    for (var j = 0; j < p.length; j++) { if (p[j][0] <= t) { waiting = p[j][1]; } else { break; } }
    s.waiting = waiting;
    return s;
  }

  function edChars() { return phase === 0 ? Math.min(buckText.length, Math.floor(phaseT / ED_MS)) : buckText.length; }
  function cmdChars() { return phase < 1 ? 0 : (phase === 1 ? Math.min(command.length, Math.floor(phaseT / CMD_MS)) : command.length); }

  function renderTerminal() {
    var n = 0;
    if (phase === 2) { while (n < lines.length && lines[n][0] <= elapsed) { n++; } }
    var key = phase * 100000 + cmdChars() * 1000 + n;
    if (key === shownLines) { return; }
    shownLines = key;
    var out = "$ " + command.slice(0, cmdChars());
    if (n > 0) { out += "\n" + lines.slice(0, n).map(function (l) { return l[1]; }).join("\n"); }
    term.textContent = out;
    term.scrollTop = term.scrollHeight;
  }

  function render() {
    var ec = edChars();
    if (ec !== lastEd) { edBuck.textContent = buckText.slice(0, ec); lastEd = ec; }
    renderTerminal();
    var t = phase === 2 ? elapsed : 0;
    var s = state(t);
    text("st-time", mmss(t) + " of " + mmss(DUR));
    text("st-waiting", String(s.waiting));
    text("st-running", String(s.running));
    text("st-hits", String(s.hits));
    text("st-remote", String(s.remoteDone));
    text("st-failed", String(s.failed));
    text("st-granted", String(s.granted));
    qbar.setAttribute("width", String(Math.round(150 * Math.min(1, s.waiting / 100))));
    qn.textContent = s.waiting + " actions waiting";
    for (var n = 0; n < N; n++) {
      bars[n].setAttribute("width", String(Math.round(150 * s.perNode[n] / peak)));
      runs[n].textContent = s.perNode[n] + " running";
    }
    var now = performance.now();
    $("store").setAttribute("class", "st" + (now < flashUntil ? " flash" : ""));
    $("fe").setAttribute("class", "fe" + (now < badUntil ? " bad" : ""));
    var scrub = $("scrub");
    if (scrub && document.activeElement !== scrub) { scrub.value = String(Math.round(1000 * t / DUR)); }
    var play = $("play");
    var done = phase === 2 && elapsed >= DUR;
    if (play) { play.textContent = playing ? "Pause" : (done ? "Replay" : "Play"); }
    $("skip").hidden = phase === 2;
  }

  // A packet runs along a polyline; where it crosses the frontend box it is hidden behind it.
  function spawn(points, cls) {
    if (reduce || packets.length >= MAX_PACKETS) { return; }
    var c = svg("circle", { r: 5, "class": "pkt " + cls }, packetLayer);
    var len = 0, segs = [];
    for (var i = 1; i < points.length; i++) {
      var d = Math.hypot(points[i][0] - points[i - 1][0], points[i][1] - points[i - 1][1]);
      segs.push(d); len += d;
    }
    packets.push({ el: c, pts: points, segs: segs, len: len, at: 0 });
  }
  function place(p) {
    var d = p.at * p.len, i = 0;
    while (i < p.segs.length - 1 && d > p.segs[i]) { d -= p.segs[i]; i++; }
    var f = p.segs[i] ? Math.min(1, d / p.segs[i]) : 1;
    p.el.setAttribute("cx", String(p.pts[i][0] + (p.pts[i + 1][0] - p.pts[i][0]) * f));
    p.el.setAttribute("cy", String(p.pts[i][1] + (p.pts[i + 1][1] - p.pts[i][1]) * f));
  }
  function advancePackets(dt) {
    for (var i = packets.length - 1; i >= 0; i--) {
      var p = packets[i];
      p.at += dt / 900;
      if (p.at >= 1) { p.el.remove(); packets.splice(i, 1); } else { place(p); }
    }
  }
  function clearPackets() { for (var i = packets.length - 1; i >= 0; i--) { packets[i].el.remove(); } packets = []; }

  // What happened in (from, to] of build time shows as motion.
  function emit(from, to) {
    var now = performance.now(), k;
    for (k = 0; k < grants.length; k++) {
      var gr = grants[k];
      if (gr[1] > to) { break; }
      if (gr[1] > from) { spawn([[150, mid], [270, mid], [490, mid], [700, cy[gr[0]]]], "act"); }
      if (gr[2] > from && gr[2] <= to && gr[3] === 0) { spawn([[950, cy[gr[0]]], [RAIL_X, cy[gr[0]]], [RAIL_X, 52], [950, 52]], "blob"); }
    }
    for (k = 0; k < hits.length; k++) {
      if (hits[k][0] > from && hits[k][0] <= to) {
        flashUntil = now + 450;
        spawn([[700, 52], [490, mid - 30], [270, mid], [150, mid]], "hit");
      }
    }
    for (k = 0; k < remote.length; k++) {
      if (remote[k][2] === 1 && remote[k][1] > from && remote[k][1] <= to) {
        badUntil = now + 500;
        spawn([[490, mid], [270, mid], [150, mid]], "bad");
      }
    }
  }

  function frame(now) {
    if (!playing) { return; }
    var dt = Math.min(100, now - last);
    last = now;
    if (phase === 0) {
      phaseT += dt;
      if (phaseT >= buckText.length * ED_MS + PAUSE_MS) { phase = 1; phaseT = 0; }
    } else if (phase === 1) {
      phaseT += dt;
      if (phaseT >= command.length * CMD_MS + PAUSE_MS) { phase = 2; phaseT = 0; }
    } else {
      var from = elapsed;
      elapsed = Math.min(DUR, elapsed + dt * speed);
      emit(from, elapsed);
      if (elapsed >= DUR) { playing = false; }
    }
    advancePackets(dt);
    render();
    if (playing) { window.requestAnimationFrame(frame); }
  }
  function play() {
    if (phase === 2 && elapsed >= DUR) { phase = reduce ? 2 : 0; phaseT = 0; elapsed = 0; clearPackets(); }
    playing = true; last = performance.now();
    window.requestAnimationFrame(frame);
    render();
  }
  function pause() { playing = false; render(); }

  $("play").addEventListener("click", function () { if (playing) { pause(); } else { play(); } });
  $("skip").addEventListener("click", function () { phase = 2; phaseT = 0; render(); });
  $("scrub").addEventListener("input", function () {
    phase = 2; phaseT = 0;
    elapsed = Math.round(DUR * Number($("scrub").value) / 1000);
    clearPackets();
    render();
  });
  var sp = $("speed");
  speed = Number(sp.value) || 30;
  sp.addEventListener("change", function () { speed = Number(sp.value); });

  // The facts that never change: the client's totals and the outcome.
  var c = T.client;
  text("tot-commands", String(c.commands));
  text("tot-cached", String(c.cached));
  text("tot-remote", String(c.remote));
  text("tot-local", String(c.local));
  text("tot-pct", String(Math.round(100 * c.cached / c.commands)));
  text("tot-failed", String(c.failed));
  text("tot-duration", mmss(DUR));
  text("tot-outcome", C.outcome);
  text("tot-nodes", String(N));
  text("tot-grants", String(grants.length));
  text("tot-binms", "100");
  root.setAttribute("data-ready", "true");
  tab("buck");

  // A link can open the replay at a second of the build: demo page `#t=250`.
  var jump = /^#t=(\d+)$/.exec(window.location.hash);
  if (jump) { phase = 2; elapsed = Math.min(DUR, Number(jump[1]) * 1000); }

  if (reduce || jump) {
    // Static: the finished file, the finished terminal, the totals. Play still replays the build.
    if (!jump) { phase = 2; elapsed = DUR; }
    render();
  } else {
    render();
    if ("IntersectionObserver" in window) {
      var seen = new IntersectionObserver(function (es) {
        if (es[0].isIntersecting) { seen.disconnect(); if (!playing && phase === 0 && phaseT === 0) { play(); } }
      }, { threshold: 0.1 });
      seen.observe(root);
    }
  }
})();
