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

**The tape is the storage.** A live run cannot be re-run, so ticks are kept rather than replayed-by-rewinding. Past `capacity`, the buffer *thins* instead of dropping its front: the newest `capacity / 2` ticks stay whole, and everything older is decimated by *fire* rather than by index — each node keeps one fire in `2^k`, and `k` rises only for whichever node is claiming most of the backbone. So a run many times the capacity stays walkable end to end with its freshest stretch still tick-exact, and the rare event a human scrubs for — the 5-minute bar, the classification — survives the squeeze that the book flood absorbs. See *Picking a `capacity`* below for what to size it to. The per-node series the chart draws is downsampled online into `bucket_ms` buckets and never dropped.

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

Two things the nav says about a thinned stretch. Where a step covers more than one absolute tick it shows a `×n` next to the position — the resolution the tape is admitting to, rather than a stepper that looks like it is skipping. And a scan that walks a sealed recording to its end without a hit leaves the cursor where it was and reports that it found nothing, instead of answering a failed search by jumping to the end of the run.

An op can name a tick that has not been recorded yet. The server says so (`pending`) rather than blocking, and the control that outran the feed shows a `⟳` while it re-issues itself.

#### HTTP

`GET /api/topology` · `GET /api/day` · `GET /api/status`, and `POST /api/{step,seek,seek_ts,step_until,step_until_change}`. Shapes live in `exec_viz::api_types`, which both halves compile — the wasm client depends on `exec_viz` with `default-features = false`, leaving just the wire types and none of the axum/tokio server half.

### Picking a `capacity`

`capacity` is the only number `Viz::new` asks for that has no right answer — it is RAM traded
against reactivity, and the exchange rate is your graph's, not this crate's. What it constrains:

| | |
|---|---|
| `capacity / 2` | the newest ticks kept **whole** — the stretch that stays tick-exact |
| `capacity / 4` | the ceiling on the thinned *backbone*, and so how deep the squeeze goes |
| `capacity / 2` | the recycle channel's depth: one pass frees between a quarter and a half of the buffer, and the recorder takes all of it back rather than allocating |
| `capacity` | the bound on the carry-forward scan a frame does for every quiet node, which is what landing a cursor costs |

Thinning is **per node, not per tick**: a pass raises the exponent of whichever node is claiming most
of the backbone until the backbone fits, so a node firing 288 times a day keeps all 288 while the
book-driven flood beside it is decimated. What that means for sizing — **a node's whole run stays
addressable as long as its fires fit inside its max-min fair share of `capacity / 4`.** Rare nodes
are never the ones cut, so in practice the share is the whole backbone minus what the handful of
greedy nodes settle at. In the block below, at `capacity` 20 000 the backbone is 5 000: the two nodes
that each want ~3 000 land on ~1 790 apiece, and everything rarer than that keeps every fire it ever
had. Read the other way: a 5-minute bar over three days is 864 fires and is safe from about 8 000;
over a month it is 8 640 and wants ~65 000.

Bigger is not free, and the price is not where it looks. Recording cost does not move with
`capacity` at all — the graph thread's leg of a fire is the same two renderings either way. What
moves is the cost of *landing* a cursor, because a frame carries every node's standing value and a
node quiet for a while is found by scanning back for it.

The block below is one synthetic graph on one machine. `Viz::bytes` is exported so the same reading
can be taken against yours.

<!-- capacity:begin -->
```
318420 ticks (2 days at spl's 159210/day), 5 nodes, 107 B/tick of card faces
fires over the run:  Book=318420  Screen=3185  Bar:1m=2895  Bar:5m=579  Bar:1h=49

footprint — Viz::bytes over the retained ticks
     2048 │▬ 3.0 MB  (1477 B/tick)
     8192 │▬▬ 5.4 MB  (818 B/tick)
    20000 │▬▬▬▬ 11.3 MB  (665 B/tick)
    65536 │▬▬▬▬▬▬▬▬▬▬▬ 33.8 MB  (554 B/tick)
   262144 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 132.6 MB  (524 B/tick)

reactivity — /api/seek mid-tape *above* a bare 53µs loopback hop, median of 64
     2048 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 13µs
     8192 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 13µs
    20000 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 13µs
    65536 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 14µs
   262144 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 34µs

addressability — fires still reachable by `step_until`, against the run's own
  capacity  retained          Book        Screen        Bar:1m        Bar:5m        Bar:1h
     2048      2007   2007/318420      251/3185      231/2895       147/579         49/49
     8192      6557   6557/318420      691/3185      899/2895       579/579         49/49
    20000     16951  16951/318420     1796/3185     1784/2895       579/579         49/49
    65536     61047  61047/318420     3185/3185     2895/2895       579/579         49/49
   262144    252884 252884/318420     3185/3185     2895/2895       579/579         49/49
```
<!-- capacity:end -->

Produced by `cargo r --release -p exec_viz --example capacity`, which splices it in here and writes
it to `docs/capacity.txt` — the numbers are read, not typed.


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

