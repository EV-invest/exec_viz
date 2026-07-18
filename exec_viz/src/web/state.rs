//! Data layer + shared state: all `/api/*` calls, the current [`ActivationFrame`], and the
//! free-run knobs. Every frame that lands also moves the chart's replay cursor.

use std::collections::{HashMap, HashSet};

use dioxus::prelude::*;
use gloo_net::http::Request;
use wasm_bindgen::{JsCast as _, JsValue};

use crate::api_types::{ActivationFrame, SeekReq, StepReq, StepUntilChangeReq, StepUntilReq, TopoNode};

pub static FRAME: GlobalSignal<Option<ActivationFrame>> = Signal::global(|| None);
pub static PLAYING: GlobalSignal<bool> = Signal::global(|| false);
/// Events per free-run poll (one poll every 50ms).
pub static SPEED: GlobalSignal<usize> = Signal::global(|| 512);
pub static ERROR: GlobalSignal<Option<String>> = Signal::global(|| None);
/// Click-selected DAG nodes — the "skip to next change in any of these" set.
pub static SELECTED: GlobalSignal<HashSet<String>> = Signal::global(HashSet::new);
pub static GATES_VISIBLE: GlobalSignal<bool> = Signal::global(|| false);
/// Gate names — union of the topology's `gates` lists (a gate nobody consumes gates nothing).
pub static GATES: GlobalSignal<Vec<String>> = Signal::global(Vec::new);
/// Per-gate (tick, state) transitions of the frames seen this session — free-run lands sparse
/// frames, so between-poll flips are unseen; a server-side full-day trace endpoint is the
/// upgrade if that ever matters.
pub static GATE_HIST: GlobalSignal<HashMap<String, Vec<(usize, bool)>>> = Signal::global(HashMap::new);

pub async fn fetch_topology() -> Result<Vec<TopoNode>, String> {
	let topo: Vec<TopoNode> = get_json("/api/topology").await?;
	let mut gates: Vec<String> = topo.iter().flat_map(|n| n.gates.iter().cloned()).collect();
	gates.sort();
	gates.dedup();
	*GATES.write() = gates;
	Ok(topo)
}

/// Raw `/api/day` body — never parsed by Rust, handed straight to the chart shim.
pub async fn fetch_day() -> Result<String, String> {
	let resp = Request::get("/api/day").send().await.map_err(|e| e.to_string())?;
	let text = resp.text().await.map_err(|e| e.to_string())?;
	if resp.ok() { Ok(text) } else { Err(text) }
}

pub async fn refresh_status() {
	apply(get_json("/api/status").await);
}

pub async fn step(n: usize) {
	apply(post_json("/api/step", &StepReq { n }).await);
}

pub async fn seek(tick: usize) {
	apply(post_json("/api/seek", &SeekReq { tick }).await);
}

pub async fn step_until(node: &str) {
	apply(post_json("/api/step_until", &StepUntilReq { node: node.to_string() }).await);
}

/// Skip to the next change in any selected node. No selection ⇒ no-op.
pub async fn step_until_change() {
	let nodes: Vec<String> = SELECTED.peek().iter().cloned().collect();
	if nodes.is_empty() {
		return;
	}
	apply(post_json("/api/step_until_change", &StepUntilChangeReq { nodes }).await);
}

pub fn toggle_select(node: &str) {
	let mut sel = SELECTED.write();
	if !sel.remove(node) {
		sel.insert(node.to_string());
	}
}

pub fn toggle_play() {
	let v = *PLAYING.peek();
	*PLAYING.write() = !v;
}

pub fn toggle_gates() {
	let v = *GATES_VISIBLE.peek();
	*GATES_VISIBLE.write() = !v;
}

pub fn speed_up() {
	let v = *SPEED.peek();
	*SPEED.write() = (v * 2).min(1 << 16);
}

pub fn speed_down() {
	let v = *SPEED.peek();
	*SPEED.write() = (v / 2).max(1);
}

fn apply(res: Result<ActivationFrame, String>) {
	match res {
		Ok(f) => {
			set_cursor(f.ts_ns);
			record_gates(&f);
			if f.tick >= f.total {
				*PLAYING.write() = false;
			}
			*FRAME.write() = Some(f);
		}
		Err(e) => {
			*PLAYING.write() = false;
			*ERROR.write() = Some(e);
		}
	}
}

fn record_gates(f: &ActivationFrame) {
	let gates = GATES.peek();
	if gates.is_empty() {
		return;
	}
	let mut hist = GATE_HIST.write();
	for g in gates.iter() {
		let v = hist.entry(g.clone()).or_default();
		// backward seek: rewind history past the landed tick
		while v.last().is_some_and(|&(t, _)| t >= f.tick) {
			v.pop();
		}
		let Some(a) = f.activations.iter().find(|a| a.node == *g) else { continue };
		let open = a.vals.as_ref().is_some_and(|vs| vs[0] != 0.0);
		if v.last().map(|&(_, b)| b) != Some(open) {
			v.push((f.tick, open));
		}
	}
}

/// Move the chart's replay-position line via the hook `lwc_draw.js` leaves on `window`.
/// Best-effort: the chart may not be mounted yet (boot ordering), which is fine — the next
/// frame will land once it is.
fn set_cursor(ts_ns: i64) {
	if ts_ns == 0 {
		return;
	}
	let Some(win) = web_sys::window() else { return };
	if let Some(f) = js_sys::Reflect::get(&win, &JsValue::from_str("__execVizSetCursor"))
		.ok()
		.as_ref()
		.and_then(|f| f.dyn_ref::<js_sys::Function>())
	{
		let _ = f.call1(&JsValue::NULL, &JsValue::from_f64(ts_ns as f64 / 1e9)); // draw errors surface in the console, not here
	}
}

/// GET + parse JSON, surfacing a non-2xx body verbatim (the server sends plain-text errors, so a
/// raw `.json()` would mask them as an opaque parse failure).
async fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
	let resp = Request::get(url).send().await.map_err(|e| e.to_string())?;
	if !resp.ok() {
		return Err(resp.text().await.unwrap_or_default());
	}
	resp.json().await.map_err(|e| e.to_string())
}

async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(url: &str, body: &B) -> Result<T, String> {
	let resp = Request::post(url).json(body).map_err(|e| e.to_string())?.send().await.map_err(|e| e.to_string())?;
	if !resp.ok() {
		return Err(resp.text().await.unwrap_or_default());
	}
	resp.json().await.map_err(|e| e.to_string())
}
