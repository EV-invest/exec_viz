// exec_viz chart logic — the app-specific half of the lightweight-charts shim. The shared v_utils
// core (`lwc_core.js`) owns the chart instance and calls `draw(chart, data, viewSpec, lib)`; this module
// is "what we chart": the day's 1m candles + volume, one indicator pane per DAG layer (topo depth,
// recomputed here from each series' deps), a small gates pane, plus a replay-cursor vertical line the wasm side moves
// via `window.__execVizSetCursor(tsSec)` as the replay advances.
//
// Hue is renderer-owned: drawable elements are enumerated in topo order (a vector node takes LEN
// contiguous slots) and hues spread evenly over the wheel; a node's Sketch only tunes l/c/a.
//
// data     = the parsed /api/day payload ({ bars, series: [SeriesOut], price_node }).
// viewSpec = { theme }.

// Bound from `draw`'s `lib` argument rather than imported: v_utils' lwc_core owns the one
// lightweight-charts instance, and a second copy would not share the chart internals we reach into.
let ColorType, CrosshairMode, LineStyle, LineType, CandlestickSeries, HistogramSeries, LineSeries, createTextWatermark;

const GRID = "#1e2130";
const CANDLE = "rgba(255,255,255,0.5)";
const CURSOR = "rgba(224,176,64,0.9)";
const BUCKET_SEC = 60;
const MAIN_INK = { l: 0.72, c: 0.13, a: 1.0 };

// lightweight-charts' color parser predates oklch(); the browser's doesn't — round-trip through a
// 1×1 canvas to plain rgba.
const _colorCanvas = document.createElement("canvas");
_colorCanvas.width = _colorCanvas.height = 1;
const _colorCtx = _colorCanvas.getContext("2d", { willReadFrequently: true });
const _colorCache = new Map();
function oklch(ink, hue, k = 1) {
  const css = `oklch(${(ink.l * k).toFixed(3)} ${(ink.c * k).toFixed(3)} ${hue.toFixed(1)} / ${ink.a})`;
  let out = _colorCache.get(css);
  if (!out) {
    _colorCtx.clearRect(0, 0, 1, 1);
    _colorCtx.fillStyle = css;
    _colorCtx.fillRect(0, 0, 1, 1);
    const [r, g, b, a] = _colorCtx.getImageData(0, 0, 1, 1).data;
    out = `rgba(${r},${g},${b},${(a / 255).toFixed(3)})`;
    _colorCache.set(css, out);
  }
  return out;
}

function fmt(v) {
  if (v == null || Number.isNaN(v)) return "·";
  const a = Math.abs(v);
  if (a >= 1000) return v.toFixed(1);
  if (a >= 1) return v.toFixed(4);
  return v.toPrecision(4);
}

// `detailOf` is the row's own `Debug` view, shown only for the pane the crosshair is in: every
// series in the chart contributes a line to the one tooltip, and a multi-line view per node would
// bury the read it was written for.
function tipFrom(tip, rows, timeOf, textOf, pane, color, detailOf) {
  const m = new Map();
  for (const r of rows) {
    const t = textOf(r);
    if (t != null) m.set(timeOf(r), { text: t, detail: detailOf?.(r) });
  }
  tip.push({ map: m, pane, color });
}

function attachTooltip(div, chart, tip, state) {
  const tt = document.createElement("div");
  tt.className = "tooltip";
  tt.style.display = "none";
  div.appendChild(tt);
  state.tt = tt;

  const onCross = (param) => {
    if (param.time === undefined || !param.point) {
      tt.style.display = "none";
      return;
    }
    const lines = [];
    for (const e of tip) if (e.map.has(param.time)) lines.push({ ...e.map.get(param.time), pane: e.pane, color: e.color });
    if (lines.length === 0) {
      tt.style.display = "none";
      return;
    }
    tt.replaceChildren(...lines.flatMap((l) => {
      const d = document.createElement("div");
      if (Array.isArray(l.text)) {
        for (const seg of l.text) {
          const s = document.createElement("span");
          s.textContent = seg.text;
          if (seg.color) s.style.color = seg.color;
          d.appendChild(s);
        }
      } else {
        d.textContent = l.text;
        if (l.color) d.style.color = l.color;
      }
      const own = param.paneIndex == null || l.pane === param.paneIndex;
      if (!own) d.style.opacity = "0.35";
      if (!own || !l.detail) return [d];
      const det = document.createElement("div");
      det.className = "detail";
      det.textContent = l.detail;
      return [d, det];
    }));
    tt.style.display = "block";
  };
  chart.subscribeCrosshairMove(onCross);
  state.crosshair = onCross;

  const onMove = (e) => {
    const r = div.getBoundingClientRect();
    let x = e.clientX - r.left + 14;
    let y = e.clientY - r.top + 14;
    if (x + tt.offsetWidth > r.width) x = e.clientX - r.left - tt.offsetWidth - 8;
    if (y + tt.offsetHeight > r.height) y = Math.max(0, e.clientY - r.top - tt.offsetHeight - 8);
    tt.style.left = `${x}px`;
    tt.style.top = `${y}px`;
  };
  div.addEventListener("mousemove", onMove);
  state.ttMove = onMove;
}

// Vertical replay-position line on the price pane. `set(tSec)` snaps to the bar grid and requests
// a repaint through the primitive's own update channel.
class CursorPrimitive {
  constructor() { this._t = null; }
  attached({ chart, requestUpdate }) { this._chart = chart; this._req = requestUpdate; }
  detached() {}
  updateAllViews() {}
  paneViews() { return [{ zOrder: () => "top", renderer: () => this._renderer() }]; }
  set(tSec) {
    this._t = Math.floor(tSec / BUCKET_SEC) * BUCKET_SEC;
    if (this._req) this._req();
  }
  _renderer() {
    const t = this._t;
    if (t == null) return null;
    const x = this._chart.timeScale().timeToCoordinate(t);
    if (x == null) return null;
    return {
      draw: (target) => target.useMediaCoordinateSpace(({ context: ctx, mediaSize }) => {
        ctx.strokeStyle = CURSOR;
        ctx.lineWidth = 1;
        ctx.beginPath();
        ctx.moveTo(x, 0);
        ctx.lineTo(x, mediaSize.height);
        ctx.stroke();
      }),
    };
  }
}

function teardown(chart) {
  const st = chart.__ev;
  if (!st) return;
  for (const s of st.series) chart.removeSeries(s);
  if (st.crosshair) chart.unsubscribeCrosshairMove(st.crosshair);
  if (st.ttMove) chart.chartElement().removeEventListener("mousemove", st.ttMove);
  if (st.click) chart.unsubscribeClick(st.click);
  if (st.tt) st.tt.remove();
  chart.__ev = null;
}

// One pane per DAG layer below price+volume; all of a layer's nodes drawn together, each on its
// own price scale (layers mix units — RSI 0–100 next to λ ~1e-6), except plots asking to be `solo`,
// which take a pane of their own right under their layer's. Gate nodes get one dedicated pane at
// the bottom instead: 0/1 square waves on the shared time axis.
function addIndicatorPanes(chart, data, st) {
  const series = data.series ?? [];
  const depth = new Map();
  // the server contracts hidden nodes out of `deps`, so every name resolves; a miss would otherwise
  // go `undefined` → NaN → silently dropped by the `>= 1` filter below, taking its consumers with it.
  const dep = (d) => {
    if (!depth.has(d)) throw new Error(`series dep "${d}" is not a drawn node`);
    return depth.get(d);
  };
  // A gate is an upstream edge too, and the DAG panel draws it on its consumer's card, so it
  // shares that depth rather than sitting one before it. Same rule both sides, or a gated node
  // pane-hops relative to where the panel puts it.
  for (const s of series) {
    const up = [...s.deps.filter((d) => !s.gates.includes(d)).map((d) => dep(d) + 1), ...s.gates.map(dep)];
    depth.set(s.node, up.length ? Math.max(...up) : 0);
  }
  const len = (s) => s.dims.reduce((a, b) => a * b, 1);
  // One drawable per plot, not per node: scale is what shares an axis, so a node whose out mixes
  // units (a quantity next to a price) draws as several, each picking its own `slots` of `vals`.
  const explode = (s) => s.plots.map((plot, pi) => ({
    node: s.node,
    key: `${s.node}#${pi}`,
    plot,
    slots: plot.slots.length ? plot.slots : Array.from({ length: len(s) }, (_, k) => k),
    points: s.points,
  }));
  // roots (depth 0) and the candle source are the price chart itself, not indicators.
  const drawable = series.filter((s) => depth.get(s.node) >= 1 && s.node !== data.price_node).flatMap(explode);
  // a gate nobody consumes gates nothing (same rule as the DAG panel)
  const gateSet = new Set(series.flatMap((s) => s.gates));
  const gates = drawable.filter((s) => gateSet.has(s.node) && !s.plot.overlay);

  let slots = 0;
  const slot0 = new Map();
  for (const s of drawable) { slot0.set(s.key, slots); slots += s.slots.length; }
  const hue = (s, i) => (360 * (slot0.get(s.key) + i)) / Math.max(slots, 1);
  const ink = (s, i) => s.plot.inks[i] ?? MAIN_INK;
  // `k` indexes the plot's own elements; `slots[k]` is where that element sits in the node's flat out.
  const val = (s, p, k) => p.vals[s.slots[k]];

  const overlays = drawable.filter((s) => s.plot.overlay);
  const indicators = drawable.filter((s) => !gateSet.has(s.node) && !s.plot.overlay);
  const drawLayer = (nodes) => {
    if (!nodes.length) return;
    const pane = chart.panes().length;
    for (const s of nodes) {
      const n = s.slots.length;
      const opts = { priceScaleId: `ind-${s.key}`, lastValueVisible: false, priceLineVisible: false };
      if (s.plot.range) {
        const [minValue, maxValue] = s.plot.range;
        opts.autoscaleInfoProvider = () => ({ priceRange: { minValue, maxValue } });
      }
      const label = (k) => s.plot.labels[k] ?? (n > 1 ? `[${k}]` : "");
      tipFrom(st.tip, s.points, (p) => p.ts_ms / 1000,
        (p) => [
          { text: s.node, color: oklch(ink(s, 0), hue(s, 0)) },
          ...Array.from({ length: n }, (_, k) => ({ text: `  ${label(k) ? label(k) + " " : ""}${fmt(val(s, p, k))}`, color: oklch(ink(s, k), hue(s, k)) })),
        ],
        pane, null, (p) => p.detail);
      let guideHost = null;
      if (s.plot.bars) {
        // stacked histogram: per-point cumulative segments, largest drawn first so each later
        // (smaller) one paints on top; l·c darken with the segment's own weight.
        const segs = Array.from({ length: n }, () => []);
        for (const p of s.points) {
          let cum = 0;
          for (let k = 0; k < n; k++) {
            const v = val(s, p, k);
            if (!Number.isFinite(v)) continue;
            cum += v;
            const w = Math.max(0, Math.min(1, v));
            segs[k].push({ time: p.ts_ms / 1000, value: cum, color: oklch(ink(s, k), hue(s, k), 0.4 + 0.6 * w) });
          }
        }
        for (let k = n - 1; k >= 0; k--) {
          const h = chart.addSeries(HistogramSeries, { ...opts, color: oklch(ink(s, k), hue(s, k)) }, pane);
          h.setData(segs[k]);
          st.series.push(h);
          guideHost = h;
        }
      } else {
        for (let k = 0; k < n; k++) {
          const line = chart.addSeries(LineSeries, { ...opts, color: oklch(ink(s, k), hue(s, k)), lineWidth: 1 }, pane);
          line.setData(s.points.filter((p) => Number.isFinite(val(s, p, k))).map((p) => ({ time: p.ts_ms / 1000, value: val(s, p, k) })));
          st.series.push(line);
          guideHost = line;
        }
      }
      for (const g of s.plot.guides) {
        guideHost.createPriceLine({ price: g.value, color: oklch(g.ink, hue(s, 0)), lineWidth: 1, lineStyle: LineStyle.Dotted, axisLabelVisible: false, title: g.label });
      }
    }
    const text = nodes.map((s) => (s.plot.labels.length ? `${s.node} (${s.plot.labels.join(" · ")})` : s.node)).join("   ");
    createTextWatermark(chart.panes()[pane], { horzAlign: "left", vertAlign: "top", lines: [{ text, color: "rgba(150,160,180,0.55)", fontSize: 10 }] });
  };
  for (const d of [...new Set(indicators.map((s) => depth.get(s.node)))].sort((a, b) => a - b)) {
    const layer = indicators.filter((s) => depth.get(s.node) === d);
    // shared pane first, then each solo claimant directly under it — a layer of nothing but solo
    // plots opens no shared pane at all.
    drawLayer(layer.filter((s) => !s.plot.solo));
    for (const s of layer.filter((s) => s.plot.solo)) drawLayer([s]);
  }

  // price-denominated series drawn on the candle pane (pane 0), on the shared price scale.
  for (const s of overlays) {
    const n = s.slots.length;
    const label = (k) => s.plot.labels[k] ?? (n > 1 ? `[${k}]` : "");
    tipFrom(st.tip, s.points, (p) => p.ts_ms / 1000,
      (p) => [
        { text: s.node, color: oklch(ink(s, 0), hue(s, 0)) },
        ...Array.from({ length: n }, (_, k) => ({ text: `  ${label(k) ? label(k) + " " : ""}${fmt(val(s, p, k))}`, color: oklch(ink(s, k), hue(s, k)) })),
      ],
      0, null, (p) => p.detail);
    let guideHost = null;
    for (let k = 0; k < n; k++) {
      const line = chart.addSeries(LineSeries, { priceScaleId: "right", lastValueVisible: false, priceLineVisible: false, color: oklch(ink(s, k), hue(s, k)), lineWidth: 1 }, 0);
      line.setData(s.points.filter((p) => Number.isFinite(val(s, p, k))).map((p) => ({ time: p.ts_ms / 1000, value: val(s, p, k) })));
      st.series.push(line);
      guideHost = line;
    }
    for (const g of s.plot.guides) {
      guideHost.createPriceLine({ price: g.value, color: oklch(g.ink, hue(s, 0)), lineWidth: 1, lineStyle: LineStyle.Dotted, axisLabelVisible: false, title: g.label });
    }
  }

  if (gates.length) {
    const pane = chart.panes().length;
    st.gatePane = pane;
    for (const s of gates) {
      const color = oklch(ink(s, 0), hue(s, 0));
      const line = chart.addSeries(LineSeries, {
        priceScaleId: `ind-${s.key}`,
        lastValueVisible: false,
        priceLineVisible: false,
        color,
        lineWidth: 1,
        lineType: LineType.WithSteps,
        // fixed 0/1 frame with margin so the square wave never rescales
        autoscaleInfoProvider: () => ({ priceRange: { minValue: -0.25, maxValue: 1.25 } }),
      }, pane);
      line.setData(s.points.filter((p) => Number.isFinite(val(s, p, 0))).map((p) => ({ time: p.ts_ms / 1000, value: val(s, p, 0) })));
      st.series.push(line);
      tipFrom(st.tip, s.points, (p) => p.ts_ms / 1000, (p) => `${s.node} ${val(s, p, 0) !== 0 ? "open" : "closed"}`, pane, color);
    }
    createTextWatermark(chart.panes()[pane], { horzAlign: "left", vertAlign: "top", lines: [{ text: gates.map((s) => `⏻ ${s.node}`).join("   "), color: "rgba(150,160,180,0.55)", fontSize: 10 }] });
  }
}

export function draw(chart, data, viewSpec, lib) {
  ({ ColorType, CrosshairMode, LineStyle, LineType, CandlestickSeries, HistogramSeries, LineSeries, createTextWatermark } = lib);
  teardown(chart);
  const theme = viewSpec.theme || "#131722";
  document.documentElement.style.background = theme;
  document.body.style.background = theme;
  chart.applyOptions({
    layout: { background: { type: ColorType.Solid, color: theme }, textColor: "#d0d4dc", fontSize: 11 },
    grid: { vertLines: { color: GRID }, horzLines: { color: GRID } },
    crosshair: { mode: CrosshairMode.Normal },
    rightPriceScale: { borderColor: GRID },
    timeScale: { timeVisible: true, secondsVisible: false, borderColor: GRID, minBarSpacing: 0.001 },
  });

  const st = { series: [], tip: [], tt: null, ttMove: null, crosshair: null, click: null };
  // The candle pane is a node's own recording, not a channel of its own: the price node's five
  // slots read o·h·l·c·v. Bucketed like every other series, so a candle and the indicators derived
  // from it sit on one x.
  const bars = (data.series ?? []).find((s) => s.node === data.price_node)?.points ?? [];
  const candle = chart.addSeries(CandlestickSeries, { upColor: CANDLE, downColor: CANDLE, borderVisible: false, wickUpColor: CANDLE, wickDownColor: CANDLE }, 0);
  candle.setData(bars.map((b) => ({ time: b.ts_ms / 1000, open: b.vals[0], high: b.vals[1], low: b.vals[2], close: b.vals[3] })));
  st.series.push(candle);
  tipFrom(st.tip, bars, (b) => b.ts_ms / 1000, (b) => `O ${fmt(b.vals[0])}  H ${fmt(b.vals[1])}  L ${fmt(b.vals[2])}  C ${fmt(b.vals[3])}`, 0, CANDLE);

  const vol = chart.addSeries(HistogramSeries, { color: "rgba(120,120,180,0.5)", priceScaleId: "right", priceFormat: { type: "volume" }, lastValueVisible: false, priceLineVisible: false }, 1);
  vol.setData(bars.map((b) => ({ time: b.ts_ms / 1000, value: b.vals[4] })));
  st.series.push(vol);
  tipFrom(st.tip, bars, (b) => b.ts_ms / 1000, (b) => `V ${fmt(b.vals[4])}`, 1, "rgba(120,120,180,0.9)");

  addIndicatorPanes(chart, data, st);
  attachTooltip(chart.chartElement(), chart, st.tip, st);

  chart.panes().forEach((p, i) => p.setStretchFactor(i === 0 ? 3 : i === st.gatePane ? 0.5 : 1));

  const cursor = new CursorPrimitive();
  candle.attachPrimitive(cursor);
  window.__execVizSetCursor = (tsSec) => cursor.set(tsSec);

  // Click anywhere on the time axis to send the replay there; the wasm side owns the hook, and a
  // draw before it boots simply has nowhere to send the click yet.
  st.click = (param) => {
    if (param.time != null) window.__execVizSeek?.(param.time);
  };
  chart.subscribeClick(st.click);

  chart.__ev = st;
  // Only the first draw with anything in it frames the data: under a live feed `draw` re-runs on
  // every refetch, and fitting again would throw away whatever the user had zoomed to. Kept off
  // `__ev`, which `teardown` clears at the top of every draw.
  if (!chart.__evFitted && bars.length) {
    chart.__evFitted = true;
    chart.timeScale().fitContent();
  }
}
