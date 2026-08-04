#![feature(default_field_values)]
//! DAG-activations replay for any `trading_data` graph: one computation structure, multiple
//! interpretations. Prod evals via `step`; here the same tick chain runs under a recording
//! [`trading_data_dag::Observer`], and the browser replays it tick-by-tick — layers lighting up,
//! values flowing — next to a candle chart.
//!
//! A library, not a runner. The app owns the graph, the feed and the runtime; it attaches a
//! [`Viz`], hands it to its own `tick_obs`, and drives [`Viz::serve_on`] on a port of its choosing.
//! Every handler reads whatever has been recorded so far, so the server can run *alongside* the
//! recording — [`Viz::bind`] is separate precisely so the URL answers before the work begins:
//!
//! ```ignore
//! let mut viz = Viz::new(Some(<Bar1m as Cell>::NAME), 100_000, 60_000);
//! let server = viz.clone().serve_on(Viz::bind(port).await);
//! tokio::join!(server, async {
//!     let out = graph.tick_obs(ts_ns, batches, &mut viz.at(ts_ns));
//!     viz.seal(); // a finite recording says so; a live feed never does
//! });
//! ```
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
pub use tape::{Rec, Viz};
