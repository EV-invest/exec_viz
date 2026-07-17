#![feature(default_field_values)]
//! DAG-activations replay for `trading_data`'s step-graph: one computation structure, multiple
//! interpretations. Prod evals via `step`; here the same tick chain runs under a recording
//! [`trading_data::Observer`], and the browser replays a day event-by-event — layers lighting up
//! per tick, values flowing — next to the day's candle chart.
//!
//! No trace persistence: **determinism is the storage**. The server holds a replay session
//! (prints + graph + cursor) and recomputes activation frames on demand; backward seek is a
//! fresh graph re-run from 0 (~1s for the full day).

pub mod api_types;

#[cfg(feature = "server")]
mod config;
#[cfg(feature = "server")]
pub mod record;
#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
mod session;
#[cfg(feature = "server")]
pub use {
	config::{AppConfig, SettingsFlags},
	server::serve,
};

#[cfg(feature = "web")]
mod web;
#[cfg(feature = "web")]
pub use web::launch as launch_web;
