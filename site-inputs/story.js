// The Story page. Two enhancements, both optional: (1) on a wide screen, a diagram pinned beside
// the timeline that follows the step you are reading; (2) the panel of fixed slots against the
// memory ladder, drawn from window.KBF_MEM, the real peaks of real actions. Without scripts, or
// under reduced motion, the page is plain sections with one diagram each. No inline style or
// markup from strings.
(function () {
  "use strict";
  var SVGNS = "http://www.w3.org/2000/svg";
  var reduce = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  function el(tag, attrs, parent, txt) {
    var e = document.createElement(tag);
    for (var k in attrs) { if (Object.prototype.hasOwnProperty.call(attrs, k)) { e.setAttribute(k, String(attrs[k])); } }
    if (txt !== undefined) { e.textContent = txt; }
    if (parent) { parent.appendChild(e); }
    return e;
  }
  function svg(tag, attrs, parent, txt) {
    var e = document.createElementNS(SVGNS, tag);
    for (var k in attrs) { if (Object.prototype.hasOwnProperty.call(attrs, k)) { e.setAttribute(k, String(attrs[k])); } }
    if (txt !== undefined) { e.textContent = txt; }
    if (parent) { parent.appendChild(e); }
    return e;
  }
  function num(n) { return Number(n).toLocaleString("en-US"); }
  function gib(x) { return x >= 100 ? num(Math.round(x)) : (Math.round(x * 10) / 10).toLocaleString("en-US"); }

  // ---- 1. the pinned diagram ----
  var story = document.getElementById("story");
  var wide = window.matchMedia && window.matchMedia("(min-width: 64rem)");
  if (story && !reduce && wide && wide.matches && "IntersectionObserver" in window) {
    var steps = story.querySelectorAll(".step");
    var fig = el("figure", { "class": "pinned-fig", "aria-hidden": "true" }, story);
    var current = 0;
    var show = function (i) {
      if (i === current) { return; }
      current = i;
      var art = steps[i].querySelector(".step-art svg");
      while (fig.firstChild) { fig.removeChild(fig.firstChild); }
      if (art) { fig.appendChild(art.cloneNode(true)); }
      Array.prototype.forEach.call(steps, function (s, j) { s.classList.toggle("active", j === i); });
    };
    story.classList.add("pinned");
    current = -1;
    show(0);
    var obs = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) { show(Array.prototype.indexOf.call(steps, e.target)); }
      });
    }, { rootMargin: "-35% 0px -55% 0px" });
    Array.prototype.forEach.call(steps, function (s) { obs.observe(s); });
  }

  // ---- 2. the panel: the model's numbers are computed by `cargo xtask site mem` and tested there ----
  var M = window.KBF_MEM;
  var slots = document.getElementById("slots");
  if (!M || !slots || M.schema !== 1 || !M.model) { return; }
  var peaks = M.peaks_mib.map(function (m) { return m / 1024; }); // GiB, ascending
  var N = peaks.length, MD = M.model, SLOT = MD.slot_gib;
  var RUNGS = MD.ladder.map(function (r) { return r.rung_gib; });
  function g(mib) { return gib(mib / 1024); }

  function setText(cls, s) {
    Array.prototype.forEach.call(document.getElementsByClassName(cls), function (e) { e.textContent = s; });
  }
  function say(id, s) { document.getElementById(id).textContent = s; }
  setText("mem-ok", num(N));
  setText("mem-source", "recorded test-farm builds");
  var P = M.percentiles_mib;
  say("mem-line",
    "The median action peaks at " + gib(P.p50 / 1024) + " GiB, 1 in 10 above " + gib(P.p90 / 1024) +
    " GiB, 1 in 100 above " + gib(P.p99 / 1024) + " GiB, and the largest at " + gib(P.max / 1024) + " GiB.");
  say("real-oom",
    "Separately, on the kbf farm itself the daemon logged " + num(M.oom) + " of " + num(M.actions) +
    " actions in these builds as killed for memory. They are in neither scheme: the peak recorded for a killed action is where it was stopped, not what it needed.");

  // the histogram: log2 axis from 0.25 GiB, one bin per power of two, the first bin "up to 0.25"
  var h = document.getElementById("hist");
  var X0 = 40, BIN_W = 100, BASE = 220, TOP = 24, edges = [0.25, 0.5, 1, 2, 4, 8, 16, 32, 64];
  var counts = edges.map(function () { return 0; });
  peaks.forEach(function (p) {
    for (var i = 0; i < edges.length; i++) { if (p <= edges[i]) { counts[i]++; return; } }
  });
  var maxc = Math.max.apply(null, counts);
  function xOf(v) { return X0 + BIN_W + BIN_W * Math.log2(v / 0.25); }
  svg("line", { "class": "axis", x1: X0, y1: BASE, x2: X0 + BIN_W * edges.length, y2: BASE }, h);
  counts.forEach(function (c, i) {
    var bh = Math.round((BASE - TOP - 20) * Math.sqrt(c / maxc));
    var x = X0 + i * BIN_W;
    if (c > 0) {
      svg("rect", { "class": "bar-h", x: x + 6, y: BASE - bh, width: BIN_W - 12, height: bh }, h);
      svg("text", { x: x + BIN_W / 2, y: BASE - bh - 5, "text-anchor": "middle" }, h, num(c));
    }
    svg("text", { x: x + BIN_W, y: BASE + 14, "text-anchor": "middle" }, h, String(edges[i]));
  });
  svg("text", { x: X0, y: BASE + 14, "text-anchor": "middle" }, h, "0");
  svg("text", { x: X0 + BIN_W * edges.length, y: BASE + 30, "text-anchor": "end" }, h, "peak memory, GiB (log scale)");
  var sx = xOf(SLOT);
  svg("line", { "class": "cls", x1: sx, y1: TOP, x2: sx, y2: BASE }, h);
  svg("text", { "class": "cls-t", x: sx + 4, y: TOP + 10 }, h, "slot " + SLOT);
  RUNGS.forEach(function (r) {
    var x = xOf(r);
    svg("path", { "class": "rung", d: "M" + (x - 6) + " " + (BASE - 2) + " L" + (x + 6) + " " + (BASE - 2) + " L" + x + " " + (BASE - 12) + " Z" }, h);
    svg("text", { "class": "rung-t", x: x + 8, y: BASE - 4 }, h, "rung " + r);
  });

  function table(id, head, rows) {
    var t = document.getElementById(id);
    while (t.firstChild) { t.removeChild(t.firstChild); }
    var tr = el("tr", {}, el("thead", {}, t));
    head.forEach(function (c) { el("th", { scope: "col" }, tr, c); });
    var body = el("tbody", {}, t);
    rows.forEach(function (r) {
      var row = el("tr", {}, body);
      r.forEach(function (c, i) { el(i === 0 ? "th" : "td", i === 0 ? { scope: "row" } : {}, row, c); });
    });
  }

  // left: one fixed slot
  var S = MD.slot;
  table("fx-table", ["", "Actions", "Booked GiB", "Peak GiB", "Idle GiB"], [
    ["Run in the " + SLOT + " GiB slot", num(S.fit), g(S.booked_mib), g(S.used_mib), g(S.idle_mib)],
    ["Peak above " + SLOT + " GiB", num(S.killed), "", "", ""]
  ]);
  say("fx-note",
    "Every action books " + SLOT + " GiB. The median action peaks at " + gib(P.p50 / 1024) + " GiB and leaves about " +
    gib(SLOT - P.p50 / 1024) + " GiB of its slot idle; across all " + num(S.fit) + " actions that run, " + g(S.idle_mib) +
    " GiB are idle. " + num(S.killed) + " actions peak above " + SLOT + " GiB (the largest at " + gib(P.max / 1024) +
    " GiB): the slot kills them on every try, so a build that needs one cannot finish.");

  // right: the ladder
  var rows = [], tb = 0, ti = 0, tk = 0, tu = 0, tn = 0, climbers = 0;
  MD.ladder.forEach(function (r, i) {
    rows.push([r.rung_gib + " GiB", num(r.actions), g(r.booked_mib), g(r.used_mib), g(r.idle_mib), num(r.kills)]);
    tb += r.booked_mib; ti += r.idle_mib; tk += r.kills; tu += r.used_mib; tn += r.actions;
    if (i > 0) { climbers += r.actions; }
  });
  rows.push(["All", num(tn), g(tb), g(tu), g(ti), num(tk)]);
  table("lad-table", ["Rung it ends on", "Actions", "Booked GiB", "Peak GiB", "Idle GiB", "Kills on the first run"], rows);
  say("lad-note",
    num(climbers) + " actions peak above the first rung (" + RUNGS[0] + " GiB). Each is killed once on its first run, queued again one rung up, and finishes there; its action key remembers the floor, so it is never killed again. " +
    (MD.beyond_top === 0 ? "No action in this sample is impossible: the largest fits the " + RUNGS[RUNGS.length - 1] + " GiB top rung, and the ladder ends on a whole host for anything bigger, as long as a machine has the memory." : num(MD.beyond_top) + " actions peak above the top rung and run on a whole host."));
  say("cmp-line",
    "For the same " + num(N) + " actions, the slot books " + g(S.booked_mib) + " GiB for the " + num(S.fit) + " that run (and cannot run " + num(S.killed) +
    "); the ladder books " + g(tb) + " GiB and runs all " + num(tn) + ".");
})();
