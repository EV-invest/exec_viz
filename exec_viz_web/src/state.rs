//! Data layer + shared state: all `/api/*` calls, the current [`ActivationFrame`], and the
//! free-run knobs. Every frame that lands also moves the chart's replay cursor.
//!
//! The tape is served while it is still being written, so an op can name a tick that has not been
//! recorded yet. The server says so (`pending`) rather than blocking, and [`WAITING`] is how the UI
//! shows which control is still chasing the feed.

use std::{collections::HashSet, future::Future};

use dioxus::prelude::*;
use exec_viz::api_types::{ActivationFrame, SeekReq, SeekTsReq, StepReq, StepUntilChangeReq, StepUntilReq, TopoNode};
use gloo_net::http::Request;
use wasm_bindgen::{JsCast as _, JsValue};

pub static FRAME: GlobalSignal<Option<ActivationFrame>> = Signal::global(|| None);
pub static PLAYING: GlobalSignal<bool> = Signal::global(|| false);
/// Events per free-run poll (one poll every 50ms).
pub static SPEED: GlobalSignal<usize> = Signal::global(|| 512);
pub static ERROR: GlobalSignal<Option<String>> = Signal::global(|| None);
/// Click-selected DAG nodes — the "skip to next change in any of these" set.
pub static SELECTED: GlobalSignal<HashSet<String>> = Signal::global(HashSet::new);
/// Key of the control whose op outran the recording and is re-issuing itself.
pub static WAITING: GlobalSignal<Option<String>> = Signal::global(|| None);
/// Empty until the recording's first tick closes: the server withholds a half-built topology, and
/// the resource stays `None` (= "loading…") until there is a whole one.
pub async fn fetch_topology() -> Result<Vec<TopoNode>, String> {
	// A page opened ahead of the run has nothing to do but wait; a transport error still exits via `?`.
	//LOOP: bounded by the feed, not by us — the recording's first tick closes when it closes.
	loop {
		let t: Vec<TopoNode> = get_json("/api/topology").await?;
		if !t.is_empty() {
			return Ok(t);
		}
		gloo_timers::future::TimeoutFuture::new(100).await;
	}
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
/// Free-run's stepper: takes whatever the tape has. Sitting at the head while playing is the feed
/// pacing us, not a wait, so `pending` is ignored and no control is marked.
pub async fn step(n: usize) {
	apply(post_json("/api/step", &StepReq { n }).await);
}
pub async fn step_one() {
	until_recorded("step", || async { post_json("/api/step", &StepReq { n: 1 }).await }).await;
}
pub async fn seek(tick: usize) {
	until_recorded("seek", || async move { post_json("/api/seek", &SeekReq { tick }).await }).await;
}
/// Where a click on the chart lands: the newest tick at or before the bar clicked. Shares `seek`'s
/// [`WAITING`] key — they are the same control, one addressed by tick and one by time.
pub async fn seek_ts(ts_ns: i64) {
	until_recorded("seek", || async move { post_json("/api/seek_ts", &SeekTsReq { ts_ns }).await }).await;
}
pub async fn step_until(node: &str) {
	let node = node.to_string();
	until_recorded(&node, || async { post_json("/api/step_until", &StepUntilReq { node: node.clone() }).await }).await;
}
/// Skip to the next change in any selected node. No selection ⇒ no-op.
pub async fn step_until_change() {
	let nodes: Vec<String> = SELECTED.peek().iter().cloned().collect();
	if nodes.is_empty() {
		return;
	}
	until_recorded("change", || async { post_json("/api/step_until_change", &StepUntilChangeReq { nodes: nodes.clone() }).await }).await;
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
pub fn speed_up() {
	let v = *SPEED.peek();
	*SPEED.write() = (v * 2).min(1 << 16);
}
pub fn speed_down() {
	let v = *SPEED.peek();
	*SPEED.write() = (v / 2).max(1);
}
/// Bumped by each retrying op, so a newer one supersedes an older one's loop.
static GENERATION: GlobalSignal<u64> = Signal::global(|| 0);

/// Re-issues `op` every 100ms while the tape cannot yet satisfy it, holding `key` in [`WAITING`].
/// Re-issuing is what makes this correct without server-side state: `seek` is absolute, a scan
/// resumes from wherever it got to, and a `pending` step advanced nothing.
async fn until_recorded<F, Fut>(key: &str, op: F)
where
	F: Fn() -> Fut,
	Fut: Future<Output = Result<ActivationFrame, String>>, {
	let generation = *GENERATION.peek() + 1;
	*GENERATION.write() = generation;
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
