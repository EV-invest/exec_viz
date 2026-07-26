//! DAG-activations replay for any `trading_data` graph: one computation structure, multiple
//! interpretations. Prod evals via `step`; here the same tick chain runs under a recording
//! [`trading_data_dag::Observer`], and the browser replays it tick-by-tick — layers lighting up,
//! values flowing — next to a candle chart.
//!
//! A library, not a runner. The app owns the graph, the feed and the runtime; it attaches a
//! [`Viz`], hands it to its own `tick_obs`, and awaits [`Viz::serve`] on a port of its choosing:
//!
//! ```ignore
//! let mut viz = Viz::new(Some("Bar1m"), 100_000, 60_000);
//! let out = graph.tick_obs(batches, viz.at(ts_ns));
//! viz.clone().serve(port).await;
//! ```

pub mod api_types;

#[cfg(feature = "server")]
pub mod record;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
mod tape;
#[cfg(feature = "server")]
pub use tape::Viz;

#[cfg(feature = "web")]
mod web;
#[cfg(feature = "web")]
pub use web::launch as launch_web;
