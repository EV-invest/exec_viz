//! The single replay view: status bar, lwc candle chart of the day (with a moving replay
//! cursor), and the DAG activations panel.

use dioxus::prelude::*;
use futures::StreamExt as _;
use wasm_bindgen::JsCast as _;

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
	use_future(|| async {
		let (tx, mut rx) = futures::channel::mpsc::unbounded::<String>();
		keyboard::install(tx);
		while let Some(key) = rx.next().await {
			handle_key(&key).await;
		}
	});

	// Free-run: poll-step while playing; `apply` flips PLAYING off only on error. Reaching the
	// tape head is not an end — under a live run the tape grows behind us, so a step that lands
	// on the head is just a poll, and the series it charts need re-fetching to keep up.
	use_future(move || async move {
		let mut polls = 0u32;
		loop {
			gloo_timers::future::TimeoutFuture::new(50).await;
			if *state::PLAYING.peek() {
				let n = *state::SPEED.peek();
				state::step(n).await;
				polls += 1;
				if polls % 20 == 0 {
					day.restart();
				}
			}
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
	rsx! {
		div { class: "wrap",
			nav { class: "nav",
				span { "exec_viz" }
				match &frame {
					Some(f) => rsx! {
						span { class: "pos", "event {f.tick}/{f.total}" }
						span { class: "pos", "{fmt_ts(f.ts_ns)}" }
					},
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
					class: "btn",
					onclick: move |_| {
						spawn(async {
							state::step(1).await;
						});
					},
					"step (␣)"
				}
				span {
					class: "btn",
					onclick: move |_| {
						spawn(async {
							state::seek(0).await;
						});
					},
					"⏮ (0)"
				}
				{
					let n_sel = state::SELECTED().len();
					rsx! {
						span {
							class: if n_sel > 0 { "btn" } else { "btn off" },
							onclick: move |_| {
								spawn(async {
									state::step_until_change().await;
								});
							},
							if n_sel > 0 { "next Δ in {n_sel} sel (n)" } else { "next Δ (n): click nodes" }
						}
					}
				}
				span { class: "pos", "speed {state::SPEED()} ev/poll (-/=)" }
				span { class: "pos", "b: next bar · c: next classify" }
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

async fn handle_key(key: &str) {
	match key {
		" " => state::step(1).await,
		"p" => state::toggle_play(),
		"-" => state::speed_down(),
		"=" => state::speed_up(),
		"0" => state::seek(0).await,
		"b" => state::step_until("Bar1m").await,
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
