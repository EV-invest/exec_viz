//! The single replay view: status bar, lwc candle chart of the day (with a moving replay
//! cursor), and the DAG activations panel.

use dioxus::prelude::*;
use futures::StreamExt as _;
use wasm_bindgen::JsCast as _;

use crate::web::{dag, keyboard, state};

const CHART_ID: &str = "exec-chart";

#[component]
pub fn Replay() -> Element {
	let topology = use_resource(state::fetch_topology);
	let day = use_resource(state::fetch_day);
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

	// Free-run: poll-step while playing; `apply` flips PLAYING off at day end or on error.
	use_future(|| async {
		loop {
			gloo_timers::future::TimeoutFuture::new(50).await;
			if *state::PLAYING.peek() {
				let n = *state::SPEED.peek();
				state::step(n).await;
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
				span { class: "pos", "b: next bar · c: next classify · g: gates" }
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
					if state::GATES_VISIBLE() {
						GatesPanel {}
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
		"g" => state::toggle_gates(),
		_ => {}
	}
}

/// All gates share this one pane: per gate its current state and a logic-analyzer square wave
/// of the transitions seen this session (x = tick).
#[component]
fn GatesPanel() -> Element {
	let tick = state::FRAME().map(|f| f.tick).unwrap_or(0);
	let hist = state::GATE_HIST();
	rsx! {
		div { class: "gates-pane",
			for g in state::GATES() {
				{
					let tr = hist.get(&g).cloned().unwrap_or_default();
					let open = tr.last().is_some_and(|&(_, b)| b);
					rsx! {
						div { class: "gate-row", key: "{g}",
							span { class: if open { "gate-state open" } else { "gate-state" }, if open { "open" } else { "closed" } }
							span { class: "gate-name", "{g}" }
							svg { class: "gate-wave", view_box: "0 0 1000 30", preserve_aspect_ratio: "none",
								polyline {
									points: "{wave_points(&tr, tick)}",
									fill: "none",
									stroke: "#26a69a",
									stroke_width: "1.5",
									vector_effect: "non-scaling-stroke",
								}
							}
						}
					}
				}
			}
		}
	}
}

/// Square-wave polyline over (tick, state) transitions: x ∈ [0, tick] → [0, 1000], open = high.
fn wave_points(tr: &[(usize, bool)], tick: usize) -> String {
	let x = |t: usize| t as f64 / tick.max(1) as f64 * 1000.0;
	let y = |b: bool| if b { 4 } else { 26 };
	let mut pts = String::new();
	let mut prev = None;
	for &(t, b) in tr {
		if let Some(pb) = prev {
			pts.push_str(&format!("{:.1},{} ", x(t), y(pb)));
		}
		pts.push_str(&format!("{:.1},{} ", x(t), y(b)));
		prev = Some(b);
	}
	if let Some(pb) = prev {
		pts.push_str(&format!("{:.1},{}", x(tick), y(pb)));
	}
	pts
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
