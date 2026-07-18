// exec_viz chart logic — the app-specific half of the lightweight-charts shim. The shared v_utils
// core (`lwc_core.js`) owns the chart instance and calls `draw(chart, data, viewSpec)`; this module
// is "what we chart": the day's 1m candles + volume, one indicator pane per DAG layer (topo depth,
// recomputed here from each series' deps), plus a replay-cursor vertical line the wasm side moves
// via `window.__execVizSetCursor(tsSec)` as the replay advances.
//
// Hue is renderer-owned: drawable elements are enumerated in topo order (a vector node takes LEN
// contiguous slots) and hues spread evenly over the wheel; a node's Sketch only tunes l/c/a.
//
// data     = the parsed /api/day payload ({ bars, series: [SeriesOut], price_node }).
// viewSpec = { theme }.

import { ColorType, CrosshairMode, LineStyle, CandlestickSeries, HistogramSeries, LineSeries, createTextWatermark } from "lightweight-charts";

const GRID = "#1e2130";
const LONG = "rgba(38,166,154,1.0)";
const SHORT = "rgba(239,83,80,1.0)";
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
  chart.__ev = null;
}

// One pane per DAG layer below price+volume; all of a layer's nodes drawn together, each on its
// own price scale (layers mix units — RSI 0–100 next to λ ~1e-6).
function addIndicatorPanes(chart, data, st) {
  const series = data.series ?? [];
  const depth = new Map();
  for (const s of series) depth.set(s.node, s.deps.length ? 1 + Math.max(...s.deps.map((d) => depth.get(d))) : 0);
  const len = (s) => s.dims.reduce((a, b) => a * b, 1);
  // roots (depth 0) and the candle source are the price chart itself, not indicators.
  const drawable = series.filter((s) => depth.get(s.node) >= 1 && s.node !== data.price_node);

  let slots = 0;
  const slot0 = new Map();
  for (const s of drawable) { slot0.set(s.node, slots); slots += len(s); }
  const hue = (s, i) => (360 * (slot0.get(s.node) + i)) / Math.max(slots, 1);
  const ink = (s, i) => s.sketch.inks[i] ?? MAIN_INK;

  for (const d of [...new Set(drawable.map((s) => depth.get(s.node)))].sort((a, b) => a - b)) {
    const pane = chart.panes().length;
    const nodes = drawable.filter((s) => depth.get(s.node) === d);
    for (const s of nodes) {
      const n = len(s);
      const opts = { priceScaleId: `ind-${s.node}`, lastValueVisible: false, priceLineVisible: false };
      if (s.sketch.range) {
        const [minValue, maxValue] = s.sketch.range;
        opts.autoscaleInfoProvider = () => ({ priceRange: { minValue, maxValue } });
      }
      let guideHost = null;
      if (n > 1) {
        // stacked histogram: per-point cumulative segments, largest drawn first so each later
        // (smaller) one paints on top; l·c darken with the segment's own weight.
        const segs = Array.from({ length: n }, () => []);
        for (const p of s.points) {
          let cum = 0;
          for (let k = 0; k < n; k++) {
            const v = p.vals[k];
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
        const line = chart.addSeries(LineSeries, { ...opts, color: oklch(ink(s, 0), hue(s, 0)), lineWidth: 1 }, pane);
        line.setData(s.points.filter((p) => Number.isFinite(p.vals[0])).map((p) => ({ time: p.ts_ms / 1000, value: p.vals[0] })));
        st.series.push(line);
        guideHost = line;
      }
      for (const g of s.sketch.guides) {
        guideHost.createPriceLine({ price: g.value, color: oklch(g.ink, hue(s, 0)), lineWidth: 1, lineStyle: LineStyle.Dotted, axisLabelVisible: false, title: g.label });
      }
    }
    const text = nodes.map((s) => (s.sketch.labels.length ? `${s.node} (${s.sketch.labels.join(" · ")})` : s.node)).join("   ");
    createTextWatermark(chart.panes()[pane], { horzAlign: "left", vertAlign: "top", lines: [{ text, color: "rgba(150,160,180,0.55)", fontSize: 10 }] });
  }
}

export function draw(chart, data, viewSpec) {
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

  const st = { series: [] };
  const candle = chart.addSeries(CandlestickSeries, { upColor: LONG, downColor: SHORT, borderVisible: false, wickUpColor: LONG, wickDownColor: SHORT }, 0);
  candle.setData(data.bars.map((b) => ({ time: b.ts_ms / 1000, open: b.open, high: b.high, low: b.low, close: b.close })));
  st.series.push(candle);

  const vol = chart.addSeries(HistogramSeries, { color: "rgba(120,120,180,0.5)", priceScaleId: "right", priceFormat: { type: "volume" }, lastValueVisible: false, priceLineVisible: false }, 1);
  vol.setData(data.bars.map((b) => ({ time: b.ts_ms / 1000, value: b.volume })));
  st.series.push(vol);

  addIndicatorPanes(chart, data, st);

  chart.panes().forEach((p, i) => p.setStretchFactor(i === 0 ? 3 : 1));

  const cursor = new CursorPrimitive();
  candle.attachPrimitive(cursor);
  window.__execVizSetCursor = (tsSec) => cursor.set(tsSec);

  chart.__ev = st;
  chart.timeScale().fitContent();
}
