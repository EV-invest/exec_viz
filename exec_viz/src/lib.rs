#![feature(default_field_values)]
//! DAG-activations replay for any `trading_data` graph: one computation structure, multiple
//! interpretations. Prod evals via `step`; here the same tick chain runs under a recording
//! [`trading_data_dag::Observer`], and the browser replays it tick-by-tick — layers lighting up,
//! values flowing — next to a candle chart.
//!
//! A library, not a runner. The app owns the graph, the feed and the runtime; it attaches a
//! [`Recorder`], hands it to its own `tick_obs`, and drives [`Viz::serve_on`] on a port of its
//! choosing. Every handler reads whatever has been recorded so far, so the server can run
//! *alongside* the recording — [`Viz::bind`] is separate precisely so the URL answers before the
//! work begins:
//!
//! ```ignore
//! let (viz, mut rec) = Viz::new(Some(<Bar1m as Cell>::NAME), 100_000, 60_000, Backpressure::Block);
//! let server = viz.clone().serve_on(Viz::bind(port).await);
//! tokio::join!(server, async {
//!     let out = graph.tick_obs(ts_ns, batches, &mut rec.at(ts_ns));
//!     rec.seal(); // a finite recording says so; a live feed just drops the recorder
//! });
//! ```
//!
//! The recording runs on its own thread: the graph's leg of it is two renderings into a recycled
//! buffer and one channel push. [`Backpressure`] is what a full handoff means — a replay waits for
//! the tape, a live feed does not.
//!
//! It owns no front-end either: the browser half is the sibling `exec_viz_web` bin, and the app
//! points `EXEC_VIZ_WEB_DIR` at a built bundle of it.

pub mod api_types;

#[cfg(feature = "server")]
mod cost;
#[cfg(feature = "server")]
pub mod record;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
mod tape;
#[cfg(feature = "server")]
pub use server::web_dir;
#[cfg(feature = "server")]
pub use tape::{Backpressure, Rec, Recorder, Tape, Viz};
