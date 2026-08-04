# exec_viz
![Minimum Supported Rust Version](https://img.shields.io/badge/nightly-1.92+-ab6000.svg)
[<img alt="crates.io" src="https://img.shields.io/crates/v/exec_viz.svg?color=fc8d62&logo=rust" height="20" style=flat-square>](https://crates.io/crates/exec_viz)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs&style=flat-square" height="20">](https://docs.rs/exec_viz)
![Lines Of Code](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/valeratrades/b48e6f02c61942200e7d1e3eeabf9bcb/raw/exec_viz-loc.json)
<br>
[<img alt="ci errors" src="https://img.shields.io/github/actions/workflow/status/EV-invest/exec_viz/errors.yml?branch=main&style=for-the-badge&style=flat-square&label=errors&labelColor=420d09" height="20">](https://github.com/EV-invest/exec_viz/actions?query=branch%3Amain) <!--NB: Won't find it if repo is private-->
[<img alt="ci warnings" src="https://img.shields.io/github/actions/workflow/status/EV-invest/exec_viz/warnings.yml?branch=main&style=for-the-badge&style=flat-square&label=warnings&labelColor=d16002" height="20">](https://github.com/EV-invest/exec_viz/actions?query=branch%3Amain) <!--NB: Won't find it if repo is private-->

Tick-by-tick replay for a [`trading_data`](https://github.com/EV-invest/trading_data) computation DAG — one computation structure, several interpretations of it.

Production evaluates the graph through `step`. Point `exec_viz` at the same tick chain and it runs under a recording `Observer` instead, so the browser can walk the run back: layers lighting up as they fire, element values and their local Jacobians drawn on the computation graph itself, next to a candle chart of the same day.

It is a **library, not a runner**. The app owns the graph, the feed and the runtime; it attaches a `Recorder`, hands it to its own `tick_obs`, and serves the `Viz` on a port of its choosing. Every handler reads whatever has been recorded so far, so the server runs *alongside* the recording — a live feed is scrubbable while it is still arriving.
<!-- markdownlint-disable -->
<details>
<summary>
<h2>Installation</h2>
</summary>

```sh
cargo add exec_viz
```

The front-end is a separate wasm bundle, built once and pointed at by an env var:

```sh
export EXEC_VIZ_WEB_DIR="$(nix run github:EV-invest/exec_viz)"  # prints where the bundle landed
```

`nix run .` builds `exec_viz_web` with a pinned `dioxus-cli`/`wasm-bindgen` pair and prints the bundle path — nothing else. Whoever wants a UI points `EXEC_VIZ_WEB_DIR` at it and starts their own binary; this repo has no business knowing their package names.

Without the env var the server panics on boot rather than serving a guessed-at (and silently stale) bundle from somewhere inside its own checkout.

</details>
<!-- markdownlint-restore -->

## Usage
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

**Optionally, the tape is also a file.** With the `record` feature, `Viz::recorded(.., root, run_id)` has the tape thread write `{root}/runs/{run_id}/` as it absorbs: `ticks.arrow` (one row per node per tick — *every* tick, thinned-away ones included), `series.arrow` and `topology.json`. Batches are cut on bytes-or-age and flushed as they are cut, so a run that dies keeps everything up to its last cut. Off by default: it is `arrow` that costs, not the writing.

**The tape is the storage.** A live run cannot be re-run, so ticks are kept rather than replayed-by-rewinding. Past `capacity`, the buffer *thins* instead of dropping its front: the newest `capacity / 2` ticks stay whole, and everything older is decimated to every `stride`-th tick (`stride` only doubles). A run many times the capacity stays walkable end to end, with the freshest stretch still tick-exact — where a plain ring would make the beginning of a long recording unreachable for the rest of the run. The per-node series the chart draws is downsampled online into `bucket_ms` buckets and never dropped.

#### The view

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

#### HTTP

`GET /api/topology` · `GET /api/day` · `GET /api/status`, and `POST /api/{step,seek,seek_ts,step_until,step_until_change}`. Shapes live in `exec_viz::api_types`, which both halves compile — the wasm client depends on `exec_viz` with `default-features = false`, leaving just the wire types and none of the axum/tokio server half.



<br>

<sup>
	This repository follows <a href="https://github.com/valeratrades/.github/tree/master/best_practices">my best practices</a> and <a href="https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md">Tiger Style</a> (except "proper capitalization for acronyms": (VsrState, not VSRState) and formatting). For project's architecture, see <a href="./docs/ARCHITECTURE.md">ARCHITECTURE.md</a>.
</sup>

#### License

<sup>
	Licensed under <a href="LICENSE">Blue Oak 1.0.0</a>
</sup>

<br>

<sub>
	Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>

