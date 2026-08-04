//! Wire shapes shared by the axum server (serializes) and the Dioxus web client (deserializes).
//! `#[cfg]`-neutral — both features compile this module. The fat `/api/day` chart payload is
//! deliberately absent from the client side: the web half holds it as an opaque JSON string and
//! hands it straight to the chart shim.

use serde::{Deserialize, Serialize};

/// One node of the static graph, in step (= topo) order. Roots have empty `deps`. `dims` is the
/// node's element shape (`[]` scalar); the client resolves dep dims by name from topology.
/// `gates` are the node's control edges — the client's gate set is the union over all nodes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TopoNode {
	pub node: String,
	pub deps: Vec<String>,
	pub gates: Vec<String>,
	pub dims: Vec<usize>,
	pub plots: Vec<PlotOut>,
	/// Estimated nanoseconds this node spends per step — what ranks the graph by cost. `None` until
	/// it has been stepped enough times for the estimate to be about the node rather than the
	/// machine; see the recorder's `cost` module for what "estimated" means here.
	pub cost_ns: Option<f64>,
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

/// Serde mirror of `trading_data_dag::Plot`. `slots` indexes the node's flattened elements
/// (empty = all of them), so a node whose out mixes units draws as several plots, each on its
/// own scale, rather than as several nodes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PlotOut {
	pub slots: Vec<usize>,
	pub range: Option<(f64, f64)>,
	pub guides: Vec<GuideOut>,
	pub labels: Vec<String>,
	pub inks: Vec<InkOut>,
	pub overlay: bool,
	pub solo: bool,
	pub bars: bool,
	pub candles: bool,
}

/// `detail` is the bucket's last fired `Debug` — the node's own composed view of the out, which the
/// chart tooltip shows in place of nothing when the crosshair is in that node's pane. A flat row of
/// `vals` is all the tooltip could otherwise say, and a node that wrote a `Debug` wrote it for
/// exactly this read.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PointOut {
	pub ts_ms: i64,
	pub vals: Vec<f64>,
	pub detail: String,
}

/// One node's full-day output sampled once per 1m bucket (last fired value wins). `deps` lets
/// the chart recompute topo depth without a second fetch; `gates` routes gate nodes to the
/// chart's dedicated gates pane.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SeriesOut {
	pub node: String,
	pub deps: Vec<String>,
	pub gates: Vec<String>,
	pub dims: Vec<usize>,
	pub plots: Vec<PlotOut>,
	pub points: Vec<PointOut>,
}

/// The `/api/day` chart payload, opaque to both the server and the wasm client (the chart shim is
/// the only reader). Snapshotted per request — under a live feed it keeps growing.
#[derive(Clone, Debug, Serialize)]
pub struct DayOut {
	pub series: Vec<SeriesOut>,
	/// Node whose recorded series *is* the candle pane, read positionally as o·h·l·c·v — the chart
	/// draws it there and skips it in the indicator panes. `None` when the graph has no price node,
	/// and then nothing is skipped.
	pub price_node: Option<String>,
}

/// One node's standing output as of the current tick: `fired` says whether it was *this* tick's
/// work, while `out`/`detail`/`vals` carry the last fired ones forward (`None` only before the
/// node's first fire, or once that tick has left the ring). `out` is the compact `Display` (card
/// face), `detail` the full `Debug` (hover tooltip), `vals` the flattened elements — a slot is
/// `None` where the node left it empty. `jac` is the tick's own row-major `vals.len() × sum(dep
/// lens)` local Jacobian — never carried forward — with entries `None` where the engine saw no
/// signal (NaN doesn't survive serde_json).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Activation {
	pub node: String,
	pub deps: Vec<String>,
	pub gates: Vec<String>,
	pub out: String,
	pub detail: String,
	pub fired: bool,
	pub dims: Vec<usize>,
	pub vals: Option<Vec<Option<f64>>>,
	pub jac: Option<Vec<Option<f64>>>,
	/// As [`TopoNode::cost_ns`] — carried here too so a card can show it without a second fetch.
	pub cost_ns: Option<f64>,
}

/// Replay position + the last tick's activations. `tick` counts consumed events (0 = nothing
/// replayed yet); `ts_ns` is the last consumed print's timestamp (0 at start).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ActivationFrame {
	pub tick: usize,
	pub total: usize,
	/// The recording is over: `total` will not grow again.
	pub sealed: bool,
	/// Ticks the graph stepped that the tape never got, because the recorder was running under
	/// `Backpressure::Drop` and the handoff was full. Non-zero means the run outpaced its own viz —
	/// the gap is in the tape, and this is what says so rather than it reading as a quiet market.
	pub dropped: usize,
	/// This op ran out of recorded ticks — re-issuing it later will get further.
	pub pending: bool,
	pub ts_ns: i64,
	pub activations: Vec<Activation>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct StepReq {
	pub n: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct SeekReq {
	pub tick: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct SeekTsReq {
	pub ts_ns: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StepUntilReq {
	pub node: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StepUntilChangeReq {
	pub nodes: Vec<String>,
}
