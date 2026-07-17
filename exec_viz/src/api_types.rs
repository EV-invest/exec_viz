//! Wire shapes shared by the axum server (serializes) and the Dioxus web client (deserializes).
//! `#[cfg]`-neutral — both features compile this module. The fat `/api/day` chart payload is
//! deliberately absent from the client side: the web half holds it as an opaque JSON string and
//! hands it straight to the chart shim.

use serde::{Deserialize, Serialize};

/// One node of the static graph, in step (= topo) order. Roots have empty `deps`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TopoNode {
	pub node: String,
	pub deps: Vec<String>,
}

/// One node's output on the current tick. `out` is the node's `Debug` rendering — every node is
/// drawable for free; `fired = out != "None"` (multi-rate nodes yield `None` off-cadence).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Activation {
	pub node: String,
	pub deps: Vec<String>,
	pub out: String,
	pub fired: bool,
}

/// Replay position + the last tick's activations. `tick` counts consumed events (0 = nothing
/// replayed yet); `ts_ns` is the last consumed print's timestamp (0 at start).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ActivationFrame {
	pub tick: usize,
	pub total: usize,
	pub ts_ns: i64,
	pub activations: Vec<Activation>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct BarOut {
	pub ts_ms: i64,
	pub open: f64,
	pub high: f64,
	pub low: f64,
	pub close: f64,
	pub volume: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct StepReq {
	pub n: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct SeekReq {
	pub tick: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StepUntilReq {
	pub node: String,
}
