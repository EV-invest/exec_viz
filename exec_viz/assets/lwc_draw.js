// exec_viz chart logic — the app-specific half of the lightweight-charts shim. The shared v_utils
// core (`lwc_core.js`) owns the chart instance and calls `draw(chart, data, viewSpec)`; this module
// is "what we chart": the day's 1m candles + volume, plus a replay-cursor vertical line the wasm
// side moves via `window.__execVizSetCursor(tsSec)` as the replay advances.
//
// data     = the parsed /api/day payload ({ bars: [{ts_ms, open, high, low, close, volume}] }).
// viewSpec = { theme }.

import { ColorType, CrosshairMode, CandlestickSeries, HistogramSeries } from "lightweight-charts";

const GRID = "#1e2130";
const LONG = "rgba(38,166,154,1.0)";
const SHORT = "rgba(239,83,80,1.0)";
const CURSOR = "rgba(224,176,64,0.9)";
const BUCKET_SEC = 60;

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

  const candle = chart.addSeries(CandlestickSeries, { upColor: LONG, downColor: SHORT, borderVisible: false, wickUpColor: LONG, wickDownColor: SHORT }, 0);
  candle.setData(data.bars.map((b) => ({ time: b.ts_ms / 1000, open: b.open, high: b.high, low: b.low, close: b.close })));

  const vol = chart.addSeries(HistogramSeries, { color: "rgba(120,120,180,0.5)", priceScaleId: "right", priceFormat: { type: "volume" }, lastValueVisible: false, priceLineVisible: false }, 1);
  vol.setData(data.bars.map((b) => ({ time: b.ts_ms / 1000, value: b.volume })));

  chart.panes().forEach((p, i) => p.setStretchFactor(i === 0 ? 3 : 1));

  const cursor = new CursorPrimitive();
  candle.attachPrimitive(cursor);
  window.__execVizSetCursor = (tsSec) => cursor.set(tsSec);

  chart.__ev = { series: [candle, vol] };
  chart.timeScale().fitContent();
}
