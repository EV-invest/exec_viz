Attach a `Recorder` to the graph you already have, and serve its `Viz`. `bind` is separate from `serve_on` precisely so the URL answers — and prints — *before* the work it describes begins:

```rust,ignore
use exec_viz::{Backpressure, Viz};

// `price_node` names an OHLCV node, whose series is the candle pane; `capacity` bounds the
// retained ticks; `bucket_ms` is the chart's sample period.
let (viz, mut rec) = Viz::new(Some(<Bar1m as Cell>::NAME), 100_000, 60_000, Backpressure::Block);

let listener = Viz::bind(8080).await;
println!("http://{}", listener.local_addr()?);

tokio::join!(viz.clone().serve_on(listener), async {
    for (ts_ns, batches) in feed {
        graph.tick_obs(batches, &mut rec.at(ts_ns));
    }
    rec.seal(); // a finite recording says so; a live feed just drops the recorder
});
```

`rec.at(ts)` opens a tick and hands back the observer, so the graph's own sweep is the thing being recorded — there is no second evaluation path to keep in step with the first. Nothing else goes in: the candles are read off `price_node`'s own `[open, high, low, close, volume]` recording, so a bar the graph already computes is not also held as an output to draw it. `seal` takes `self`, so the handle you recorded through is spent and `total` stops growing.

**The recording is not on your thread.** A finished tick crosses to a tape thread over a bounded channel, and that thread does the naming, bucketing, thinning and cost accounting; the graph pays a `Display`, a clipped `Debug` and one push, into buffers the tape hands back. `Backpressure` says what a full channel means: `Block` for a replay, which wants the whole tape and whose feed will wait, and `Drop` for a live run, where a fill must never queue behind a study aid — dropped ticks are counted into `ActivationFrame::dropped` rather than passing for a quiet market.

**The tape is the storage.** A live run cannot be re-run, so ticks are kept rather than replayed-by-rewinding. Past `capacity`, the buffer *thins* instead of dropping its front: the newest `capacity / 2` ticks stay whole, and everything older is decimated to every `stride`-th tick (`stride` only doubles). A run many times the capacity stays walkable end to end, with the freshest stretch still tick-exact — where a plain ring would make the beginning of a long recording unreachable for the rest of the run. The per-node series the chart draws is downsampled online into `bucket_ms` buckets and never dropped.

### The view

Two [dockviewers](https://github.com/EV-invest/dockviewers) tiles you can resize, tab and maximize — a lightweight-charts candle pane with a moving replay cursor, and the DAG activations panel. Clicking the chart seeks the tape to that bar; `Alt+S` caches the arrangement per screen band.

The DAG panel is DOM-inspector-style and present-moment only: one column per topo level, each node card rendering its element grid (shape from the node's `dims`), cells heat-colored by per-element running min/max. An SVG overlay draws element→element edges weighted by that tick's finite-difference Jacobian. A node that stayed quiet still shows its last value — `fired` is what says it is live this tick.

| key | |
|---|---|
| `␣` | step one tick |
| `p` | play / pause |
| `-` `=` | slower / faster (events per 50ms poll) |
| `0` | seek to the start |
| `b` | skip to the next bar |
| `c` | skip to the next `Classify` |
| `n` | skip to the next change in the nodes you clicked |

An op can name a tick that has not been recorded yet. The server says so (`pending`) rather than blocking, and the control that outran the feed shows a `⟳` while it re-issues itself.

### HTTP

`GET /api/topology` · `GET /api/day` · `GET /api/status`, and `POST /api/{step,seek,seek_ts,step_until,step_until_change}`. Shapes live in `exec_viz::api_types`, which both halves compile — the wasm client depends on `exec_viz` with `default-features = false`, leaving just the wire types and none of the axum/tokio server half.
