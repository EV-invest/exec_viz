//! Wire shapes shared by the axum server (serializes) and the Dioxus web client (deserializes).
//! `#[cfg]`-neutral — both features compile this module. The fat `/api/day` chart payload is
//! deliberately absent from the client side: the web half holds it as an opaque JSON string and
//! hands it straight to the chart shim.

use serde::{Deserialize, Serialize};

/// One node of the static graph, in step (= topo) order. Roots have empty `deps`. `dims` is the
/// node's element shape (`[]` scalar); the client resolves dep dims by name from topology.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TopoNode {
	pub node: String,
	pub deps: Vec<String>,
	pub dims: Vec<usize>,
	pub sketch: SketchOut,
}

/// Serde mirror of `trading_data_dag::Ink`: l/c/a only — hue stays renderer-owned.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct InkOut {
	pub l: f64,
	pub c: f64,
	pub a: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GuideOut {
	pub label: String,
	pub value: f64,
	pub ink: InkOut,
}

/// Serde mirror of `trading_data_dag::Sketch`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SketchOut {
	pub range: Option<(f64, f64)>,
	pub guides: Vec<GuideOut>,
	pub labels: Vec<String>,
	pub inks: Vec<InkOut>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PointOut {
	pub ts_ms: i64,
	pub vals: Vec<f64>,
}

/// One node's full-day output sampled once per 1m bucket (last fired value wins). `deps` lets
/// the chart recompute topo depth without a second fetch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SeriesOut {
	pub node: String,
	pub deps: Vec<String>,
	pub dims: Vec<usize>,
	pub sketch: SketchOut,
	pub points: Vec<PointOut>,
}

/// The static `/api/day` chart payload — serialized once at boot, opaque to both the server and
/// the wasm client (the chart shim is the only reader).
#[derive(Clone, Debug, Serialize)]
pub struct DayOut {
	pub bars: Vec<BarOut>,
	pub series: Vec<SeriesOut>,
	/// Node whose values back the candles — the chart skips it in the indicator panes.
	pub price_node: String,
}

/// One node's output on the current tick. `out` is the compact `Display` (card face); `detail`
/// is the full `Debug` (hover tooltip). `vals` are the flattened elements (`None` = didn't fire);
/// `jac` is the row-major `vals.len() × sum(dep lens)` local Jacobian, entries `None` where the
/// engine saw no signal (NaN doesn't survive serde_json).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Activation {
	pub node: String,
	pub deps: Vec<String>,
	pub out: String,
	pub detail: String,
	pub fired: bool,
	pub dims: Vec<usize>,
	pub vals: Option<Vec<f64>>,
	pub jac: Option<Vec<Option<f64>>>,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StepUntilChangeReq {
	pub nodes: Vec<String>,
}
