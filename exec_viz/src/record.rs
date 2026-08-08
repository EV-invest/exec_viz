//! Exec-time recording surface: the `runs/{run_id}/` layout + row schemas that scam_pump_liqs
//! and zero_fee_arb each hand-roll today, standardized here. Row shapes are modeled on
//! `scam_pump_liqs/viz/src/read.rs` (`OrderEventRow`, `DrawingRow`), which is the proven read-side
//! contract; the order/fill/drawing lanes are still schema-only.

use serde::{Deserialize, Serialize};

/// A run dir is `runs/{run_id}/` where `run_id = {version}_{config_hash}` — collision-proof
/// across logic and config versions.
pub const RUNS_DIR: &str = "runs";
/// One situation's saved [`Viz`](crate::Viz) tape — see [`Viz::save`](crate::Viz::save).
pub const TAPE_FILE: &str = "tape.bin";
pub const ORDERS_FILE: &str = "orders.parquet";
pub const FILLS_FILE: &str = "fills.parquet";
pub const DRAWINGS_FILE: &str = "drawings.parquet";

/// One long-format self-drawn signal sample: strategy-side series (screener levels, degraders)
/// drawn onto the study chart without the viewer knowing their semantics. `colour` is a rendered
/// CSS colour string — the client applies it as-is.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DrawingRow {
	pub ts_ns: u64,
	pub label: String,
	pub value: f64,
	pub colour: String,
}

/// One execution: `ts_event_ns` is exchange fill time, `ts_init_ns` local receipt (their gap is
/// the fill→ack latency the exec view renders).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FillRow {
	pub client_order_id: String,
	pub ts_event_ns: u64,
	pub ts_init_ns: u64,
	pub price: f64,
	pub qty: f64,
	pub side: String,
	pub commission: Option<f64>,
}

/// One order-lifecycle event (`Initialized`/`Accepted`/`Updated`/`Canceled`/…). `_ns` suffixes
/// keep the unit visible across the JSON boundary. `price`/`qty` are the order's level on
/// `Initialized`/`Updated`, null elsewhere.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OrderEventRow {
	pub client_order_id: String,
	pub event: String,
	pub ts_event_ns: u64,
	pub ts_init_ns: u64,
	pub price: Option<f64>,
	pub qty: Option<f64>,
	pub side: String,
	pub order_type: String,
}
