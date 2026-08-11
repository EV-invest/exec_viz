//! Data layer + shared state: every cursor op, the current [`ActivationFrame`], and the free-run
//! knobs. Every frame that lands also moves the chart's replay cursor. What answers an op is
//! [`crate::transport`]'s business, not this module's.
//!
//! A tape may still be being written, so an op can name a tick that has not been recorded yet. The
//! tape says so (`pending`) rather than blocking, and [`WAITING`] is how the UI shows which control
//! is still chasing the feed. On a recording read back from a file none of that fires: it is sealed
//! before the first op reaches it.

use std::future::Future;

use ahash::{AHashMap, AHashSet};
use dioxus::prelude::*;
use exec_viz::api_types::{ActivationFrame, Op, TopoNode};
use wasm_bindgen::{JsCast as _, JsValue};

use crate::transport;

const VIEWS_KEY: &str = "exec-viz-views";
pub static FRAME: GlobalSignal<Option<ActivationFrame>> = Signal::global(|| None);
pub static PLAYING: GlobalSignal<bool> = Signal::global(|| false);
/// Cursor rides the tape's growing end. On by default so a page opened against a live run needs no
/// interaction; any explicit cursor op parks it.
pub static FOLLOW: GlobalSignal<bool> = Signal::global(|| true);
/// Events per free-run poll (one poll every 50ms).
pub static SPEED: GlobalSignal<usize> = Signal::global(|| 512);
pub static ERROR: GlobalSignal<Option<String>> = Signal::global(|| None);
/// Click-selected DAG nodes — the "skip to next change in any of these" set.
pub static SELECTED: GlobalSignal<AHashSet<String>> = Signal::global(AHashSet::new);
/// Key of the control whose op outran the recording and is re-issuing itself.
pub static WAITING: GlobalSignal<Option<String>> = Signal::global(|| None);
/// Explicit per-node chart-visibility overrides. Absent = the node's own default, which a node
/// declaring no plots asks to be off. Cached in localStorage, so it outlives both a reload and a
/// fresh run of the strategy — node names are what it keys on, and those are stable across runs.
/// Overrides rather than the hidden set: the meaning holds when a node's `PLOTS` changes upstream.
pub static VIEWS: GlobalSignal<AHashMap<String, bool>> = Signal::global(load_views);
/// The DAG node under the pointer with its own default, buffers resolved to the series they retain
/// — what `v` acts on.
pub static HOVERED: GlobalSignal<Option<(String, bool)>> = Signal::global(|| None);

pub fn shown(n: &TopoNode) -> bool {
	VIEWS.read().get(&n.node).copied().unwrap_or(!n.plots.is_empty())
}

pub fn toggle_view() {
	// nothing under the pointer is nothing to toggle
	let Some((node, default)) = HOVERED.peek().clone() else { return };
	let mut v = VIEWS.write();
	let now = v.get(&node).copied().unwrap_or(default);
	v.insert(node, !now);
	save_views(&v);
}

pub fn clear_views() {
	let mut v = VIEWS.write();
	v.clear();
	save_views(&v);
}

/// Empty until the recording's first tick closes: a half-built topology is withheld, and the
/// resource stays `None` (= "loading…") until there is a whole one.
pub async fn fetch_topology() -> Result<Vec<TopoNode>, String> {
	// A page opened ahead of the run has nothing to do but wait; a transport error still exits via `?`.
	//LOOP: bounded by the feed, not by us — the recording's first tick closes when it closes.
	loop {
		let t = transport::topology().await?;
		if !t.is_empty() {
			return Ok(t);
		}
		gloo_timers::future::TimeoutFuture::new(100).await;
	}
}
/// The raw chart payload — never parsed by Rust, handed straight to the chart shim.
pub async fn fetch_day() -> Result<String, String> {
	transport::day().await
}
pub async fn refresh_status() {
	apply(transport::op(Op::Status).await);
}
/// Free-run's stepper: takes whatever the tape has. Sitting at the head while playing is the feed
/// pacing us, not a wait, so `pending` is ignored and no control is marked.
pub async fn step(n: usize) {
	apply(transport::op(Op::Step { n }).await);
}
pub async fn step_one() {
	until_recorded("step", || transport::op(Op::Step { n: 1 })).await;
}
pub async fn seek(tick: usize) {
	until_recorded("seek", || transport::op(Op::Seek { tick })).await;
}
/// Where a click on the chart lands: the newest tick at or before the bar clicked. Shares `seek`'s
/// [`WAITING`] key — they are the same control, one addressed by tick and one by time.
pub async fn seek_ts(ts_ns: i64) {
	until_recorded("seek", || transport::op(Op::SeekTs { ts_ns })).await;
}
pub async fn step_until(node: &str) {
	let node = node.to_string();
	until_recorded(&node, || transport::op(Op::StepUntil { node: node.clone() })).await;
}
/// Skip to the next change in any selected node. No selection ⇒ no-op.
pub async fn step_until_change() {
	let nodes: Vec<String> = SELECTED.peek().iter().cloned().collect();
	if nodes.is_empty() {
		return;
	}
	until_recorded("change", || transport::op(Op::StepUntilChange { nodes: nodes.clone() })).await;
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
	*FOLLOW.write() = false;
}
/// Re-attach to the tape's end. Deliberately [`step`] and not [`until_recorded`]: a step past the
/// head is `pending` by definition, and retrying it would spin for as long as the feed runs.
pub async fn follow() {
	*FOLLOW.write() = true;
	step(usize::MAX).await;
}
pub fn speed_up() {
	let v = *SPEED.peek();
	*SPEED.write() = (v * 2).min(1 << 16);
}
pub fn speed_down() {
	let v = *SPEED.peek();
	*SPEED.write() = (v / 2).max(1);
}
/// No entry, no storage handle, or one written by an older shape: no overrides yet. That is
/// genuinely absent state rather than a swallowed error — the defaults are a complete answer.
fn load_views() -> AHashMap<String, bool> {
	let Ok(Some(store)) = local_storage() else { return AHashMap::new() };
	let Ok(Some(raw)) = store.get_item(VIEWS_KEY) else { return AHashMap::new() };
	serde_json::from_str(&raw).unwrap_or_default()
}

fn save_views(views: &AHashMap<String, bool>) {
	let Ok(Some(store)) = local_storage() else { return };
	let json = serde_json::to_string(views).expect("a map of string to bool serializes");
	store.set_item(VIEWS_KEY, &json).expect("a handle localStorage granted takes a write this small");
}

fn local_storage() -> Result<Option<web_sys::Storage>, JsValue> {
	web_sys::window().expect("wasm32 target always runs in a browser").local_storage()
}
/// Bumped by each retrying op, so a newer one supersedes an older one's loop.
static GENERATION: GlobalSignal<u64> = Signal::global(|| 0);

/// Re-issues `op` every 100ms while the tape cannot yet satisfy it, holding `key` in [`WAITING`].
/// Re-issuing is what makes this correct without state on the answering side: `seek` is absolute, a
/// scan resumes from wherever it got to, and a `pending` step advanced nothing. A sealed tape never
/// answers `pending`, so this runs its body once.
async fn until_recorded<F, Fut>(key: &str, op: F)
where
	F: Fn() -> Fut,
	Fut: Future<Output = Result<ActivationFrame, String>>, {
	let generation = *GENERATION.peek() + 1;
	*GENERATION.write() = generation;
	// Naming a tick is taking the cursor by hand — every explicit op routes through here, and none of
	// them want the free-run loop yanking the cursor back to the tape's end afterwards.
	*FOLLOW.write() = false;
	*WAITING.write() = Some(key.to_string());
	// Exits on `!pending`, on an error through `apply`, or when a newer op supersedes this one.
	//LOOP: re-issues until the tape holds the tick asked for — how long that takes is the feed's to say.
	loop {
		let res = op().await;
		// A newer op owns the cursor and `WAITING` now; applying this would jump the cursor back to
		// where a superseded request left it.
		if *GENERATION.peek() != generation {
			return;
		}
		let pending = matches!(&res, Ok(f) if f.pending);
		apply(res);
		if !pending {
			break;
		}
		gloo_timers::future::TimeoutFuture::new(100).await;
	}
	*WAITING.write() = None;
}

fn apply(res: Result<ActivationFrame, String>) {
	match res {
		Ok(f) => {
			// A retry loop re-fetches the same frame ten times a second; writing it would re-run the
			// DAG's layout pass each time for a render that cannot differ.
			let same = FRAME.peek().as_ref() == Some(&f);
			if same {
				return;
			}
			set_cursor(f.ts_ns);
			*FRAME.write() = Some(f);
		}
		Err(e) => {
			*PLAYING.write() = false;
			*ERROR.write() = Some(e);
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
	let win = web_sys::window().expect("wasm32 target always runs in a browser");
	let hook = js_sys::Reflect::get(&win, &JsValue::from_str("__execVizSetCursor")).expect("window takes property reads");
	// The one genuinely absent case: no chart mounted yet, so nothing to point at.
	if let Some(f) = hook.dyn_ref::<js_sys::Function>() {
		let _ = f.call1(&JsValue::NULL, &JsValue::from_f64(ts_ns as f64 / 1e9)); // draw errors surface in the console, not here
	}
}
