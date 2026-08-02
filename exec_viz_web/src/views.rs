//! The single replay view: status bar, lwc candle chart of the day (with a moving replay
//! cursor), and the DAG activations panel.

use dioxus::prelude::*;
use exec_viz::api_types::TopoNode;
use futures::StreamExt as _;
use wasm_bindgen::{JsCast as _, closure::Closure};

use crate::{dag, keyboard, state};

const CHART_ID: &str = "exec-chart";

#[component]
pub fn Replay() -> Element {
	let topology = use_resource(state::fetch_topology);
	let mut day = use_resource(state::fetch_day);
	let mut banner = use_signal(|| Option::<String>::None);

	// Boot: pick up the server's current replay position (survives page reloads).
	use_future(|| async {
		state::refresh_status().await;
	});

	// Keyboard → action loop: keys land on a channel from the document listener, actions run
	// here inside the runtime.
	use_future(move || async move {
		let (tx, mut rx) = futures::channel::mpsc::unbounded::<String>();
		keyboard::install(tx);
		while let Some(key) = rx.next().await {
			// Spawned, not awaited: an op waiting on the tape to catch up would otherwise hold this
			// loop and freeze every other key for as long as it takes.
			let bar = bar_node(&topology);
			spawn(async move {
				handle_key(&key, bar.as_deref()).await;
			});
		}
	});

	// Free-run: poll-step while playing; `apply` flips PLAYING off only on error. Reaching the tape
	// head is not an end — under a live run the tape grows behind us, so a step that lands on the
	// head is just a poll, and the series it charts need re-fetching to keep up. Once sealed there
	// is nothing left to re-fetch.
	use_future(move || async move {
		let mut polls = 0u32;
		loop {
			gloo_timers::future::TimeoutFuture::new(50).await;
			polls += 1;
			let playing = *state::PLAYING.peek();
			if playing {
				state::step(*state::SPEED.peek()).await;
			}
			// `/api/day` is a full clone of every bar and series point under the tape lock, so the
			// idle rate is deliberately slow.
			let sealed = state::FRAME.peek().as_ref().is_some_and(|f| f.sealed);
			let period = if sealed {
				0
			} else if playing {
				20
			} else {
				100
			};
			if period != 0 && polls % period == 0 {
				day.restart();
			}
		}
	});

	// Chart click → seek. Same shape as the key loop, and for the same reason: the JS callback has no
	// dioxus runtime to await an API call in.
	use_future(move || async move {
		let (tx, mut rx) = futures::channel::mpsc::unbounded::<f64>();
		install_seek(tx);
		while let Some(ts_sec) = rx.next().await {
			spawn(async move {
				state::seek_ts((ts_sec * 1e9) as i64).await;
			});
		}
	});

	// Mount the chart when the day payload lands.
	use_effect(move || {
		if let Some(Ok(json)) = &*day.read() {
			let json = json.clone();
			spawn(async move {
				if let Some(el) = chart_el() {
					banner.set(v_utils::lwc::mount(el, "/lwc_draw.js", &json, r##"{"theme":"#131722"}"##).await);
				}
			});
		}
	});

	let frame = state::FRAME();
	let playing = state::PLAYING();
	let bar = bar_node(&topology);
	let bar_waiting = bar.as_deref().is_some_and(waiting);
	rsx! {
		div { class: "wrap",
			nav { class: "nav",
				span { "exec_viz" }
				match &frame {
					Some(f) => {
						let growing = if f.sealed { "" } else { "+" };
						rsx! {
							span { class: "pos", "event {f.tick}/{f.total}{growing}" }
							span { class: "pos", "{fmt_ts(f.ts_ns)}" }
						}
					}
					None => rsx! {
						span { class: "pos", "loading…" }
					},
				}
				span {
					class: if playing { "btn active" } else { "btn" },
					onclick: move |_| state::toggle_play(),
					if playing { "pause (p)" } else { "play (p)" }
				}
				span {
					class: if waiting("step") { "btn waiting" } else { "btn" },
					onclick: move |_| {
						spawn(async {
							state::step_one().await;
						});
					},
					"step (␣)"
					if waiting("step") { " ⟳" }
				}
				span {
					class: if waiting("seek") { "btn waiting" } else { "btn" },
					onclick: move |_| {
						spawn(async {
							state::seek(0).await;
						});
					},
					"⏮ (0)"
					if waiting("seek") { " ⟳" }
				}
				{
					let n_sel = state::SELECTED().len();
					rsx! {
						span {
							class: if waiting("change") { "btn waiting" } else if n_sel > 0 { "btn" } else { "btn off" },
							onclick: move |_| {
								spawn(async {
									state::step_until_change().await;
								});
							},
							if n_sel > 0 { "next Δ in {n_sel} sel (n)" } else { "next Δ (n): click nodes" }
							if waiting("change") { " ⟳" }
						}
					}
				}
				span { class: "pos", "speed {state::SPEED()} ev/poll (-/=)" }
				span {
					class: if bar_waiting || waiting("Classify") { "pos waiting" } else { "pos" },
					"b: next bar · c: next classify"
					if bar_waiting { " b⟳" }
					if waiting("Classify") { " c⟳" }
				}
			}
			if let Some(e) = state::ERROR() {
				div { class: "banner", "{e}" }
			}
			if let Some(msg) = banner() {
				div { class: "banner", "{msg}" }
			}
			div { class: "split",
				div { class: "chart-pane",
					div { class: "chart-host",
						div { id: CHART_ID, style: "position:absolute;inset:0" }
					}
				}
				div { class: "dag-pane",
					match &*topology.read() {
						None => rsx! {
							div { class: "loading", "loading…" }
						},
						Some(Err(e)) => rsx! {
							div { class: "error", "error: {e}" }
						},
						Some(Ok(t)) => rsx! {
							dag::DagPanel { topology: t.clone() }
						},
					}
				}
			}
		}
	}
}

/// `b` is "next bar", not a node name — the demo graph's clock is `Bar1m`, spl's is `Bars<1>`.
/// ponytail: first `Bar`-prefixed node in step order; ask the server for the price node if a graph
/// ever has two clocks and picks the wrong one.
fn bar_node(topology: &Resource<Result<Vec<TopoNode>, String>>) -> Option<String> {
	let t = topology.read();
	let Some(Ok(t)) = &*t else { return None };
	t.iter().map(|n| &n.node).find(|n| n.starts_with("Bar")).cloned()
}

/// The hook `lwc_draw.js` calls with the clicked bar's time, in seconds — the inbound twin of
/// `__execVizSetCursor`.
fn install_seek(tx: futures::channel::mpsc::UnboundedSender<f64>) {
	let cb = Closure::wrap(Box::new(move |ts_sec: f64| {
		let _ = tx.unbounded_send(ts_sec); // receiver lives as long as the page; a closed channel means teardown
	}) as Box<dyn FnMut(f64)>);
	if let Some(win) = web_sys::window() {
		js_sys::Reflect::set(&win, &wasm_bindgen::JsValue::from_str("__execVizSeek"), cb.as_ref()).expect("window takes properties");
	}
	// Program-lifetime hook: ownership handed to the JS runtime (standard wasm pattern).
	cb.forget();
}

fn waiting(key: &str) -> bool {
	state::WAITING().as_deref() == Some(key)
}

async fn handle_key(key: &str, bar: Option<&str>) {
	match key {
		" " => state::step_one().await,
		"p" => state::toggle_play(),
		"-" => state::speed_down(),
		"=" => state::speed_up(),
		"0" => state::seek(0).await,
		"b" =>
			if let Some(bar) = bar {
				state::step_until(bar).await;
			},
		"c" => state::step_until("Classify").await,
		"n" => state::step_until_change().await,
		_ => {}
	}
}

fn chart_el() -> Option<web_sys::HtmlElement> {
	web_sys::window()?.document()?.get_element_by_id(CHART_ID)?.dyn_into().ok()
}

fn fmt_ts(ts_ns: i64) -> String {
	if ts_ns == 0 {
		return "—".to_string();
	}
	String::from(js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts_ns as f64 / 1e6)).to_iso_string())
}
