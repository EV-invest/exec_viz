#![feature(default_field_values)]
//! DAG-activations replay for any [`trading_data_dag::Dag`]: one computation structure, multiple
//! interpretations. Prod evals via `step`; here the same tick chain runs under a recording
//! [`trading_data_dag::Observer`], and the browser replays an event stream tick-by-tick — layers
//! lighting up, values flowing — next to a candle chart.
//!
//! No trace persistence: **determinism is the storage**. The server holds a replay session
//! (events + graph + cursor) and recomputes activation frames on demand; backward seek is a
//! fresh graph re-run from 0 (~1s for a full day).

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
	session::day_series,
};

#[cfg(feature = "web")]
mod web;
#[cfg(feature = "web")]
pub use web::launch as launch_web;
