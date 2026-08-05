//! The single replay view: status bar, then the two dock tiles — lwc candle chart of the day (with
//! a moving replay cursor) and the DAG activations panel — the user can resize, tab and maximize.

use dioxus::prelude::*;
use dockviewers_dioxus::{Config, DockPanel, Group, GroupId, MinSize, PackedApi, PackedArea, PanelId};
use exec_viz::api_types::TopoNode;
use futures::StreamExt as _;
use wasm_bindgen::{JsCast as _, closure::Closure};

use crate::{dag, keyboard, state};

const CHART_ID: &str = "exec-chart";
const CHART_PANEL: &str = "chart";
const DAG_PANEL: &str = "dag";

#[component]
pub fn Replay() -> Element {
	let topology = use_resource(state::fetch_topology);
	let mut day = use_resource(state::fetch_day);
	let banner = use_signal(|| Option::<String>::None);
	let mut api = use_signal(|| None::<PackedApi>);

	// The panes read their feeds from context: `panels` is built once (the overlay keys by panel id,
	// so a tile that moves keeps its component — and the chart keeps its JS state).
	use_context_provider(|| topology);
	use_context_provider(|| day);
	use_context_provider(|| banner);
	// A tile's `+` asks the host for another panel; this view's set is fixed, so the only thing to
	// offer is whichever of the two was closed.
	use_context_provider(|| {
		Callback::new(move |gid: GroupId| {
			let mut a = api().expect("a tile exists to be clicked, so the seed already ran");
			let live = a.tab_ids();
			for id in [CHART_PANEL, DAG_PANEL] {
				if !live.iter().any(|p| p.0 == id) {
					a.add_tab(gid, PanelId(id.into()));
				}
			}
		})
	});
	let panels = use_signal(|| {
		vec![
			DockPanel {
				id: PanelId(CHART_PANEL.into()),
				title: "chart".into(),
				content: rsx! {
					ChartPane {}
				},
			},
			DockPanel {
				id: PanelId(DAG_PANEL.into()),
				title: "dag".into(),
				content: rsx! {
					DagPane {}
				},
			},
		]
	});

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
	// is nothing left to re-fetch, and nothing to follow either: a finished recording opens where
	// it was left, not at its end.
	use_future(move || async move {
		let mut polls = 0u32;
		loop {
			gloo_timers::future::TimeoutFuture::new(50).await;
			polls += 1;
			let playing = *state::PLAYING.peek();
			let sealed = state::FRAME.peek().as_ref().is_some_and(|f| f.sealed);
			let following = *state::FOLLOW.peek() && !sealed;
			if following {
				state::step(usize::MAX).await;
			} else if playing {
				state::step(*state::SPEED.peek()).await;
			}
			// `/api/day` is a full clone of every bar and series point under the tape lock, so the
			// idle rate is deliberately slow.
			let period = if sealed {
				0
			} else if playing || following {
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

	let frame = state::FRAME();
	let playing = state::PLAYING();
	let following = state::FOLLOW() && !frame.as_ref().is_some_and(|f| f.sealed);
	let bar = bar_node(&topology);
	let bar_waiting = bar.as_deref().is_some_and(waiting);
	rsx! {
		// Inline, not in app.css: the dock measures its container on mount, before the injected
		// stylesheet lands — from the sheet these two boxes are zero-height on that one frame.
		div { class: "wrap", style: "height:100vh; display:flex; flex-direction:column;",
			nav { class: "nav",
				span { "exec_viz" }
				match &frame {
					Some(f) => {
						let growing = if f.sealed { "" } else { "+" };
						rsx! {
							span { class: "pos", "event {f.tick}/{f.total}{growing}" }
							span { class: "pos", "{fmt_ts(f.ts_ns)}" }
							// A tape the run outran has holes in it; saying so is the difference between
							// reading a gap as a quiet market and reading it as a gap.
							if f.dropped > 0 {
								span { class: "pos warn", "{f.dropped} dropped" }
							}
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
					class: if following { "btn active" } else { "btn" },
					onclick: move |_| {
						spawn(async {
							state::follow().await;
						});
					},
					"live (l)"
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
				span {
					class: if state::VIEWS().is_empty() { "btn off" } else { "btn" },
					onclick: move |_| state::clear_views(),
					"reset views (⇧C)"
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
			div { class: "dock-host", style: "flex:1 1 auto; min-height:0; position:relative;",
				PackedArea {
					panels,
					config: Some(dock_config()),
					on_band: Some(Callback::new(move |a: PackedApi| {
						api.set(Some(a));
						if !a.restored() {
							seed(a);
						}
					})),
				}
			}
		}
	}
}

/// `Alt+S` caches the live arrangement per screen band in this browser, and `on_band` gets it back
/// on the next load instead of re-seeding. There is nothing to publish anywhere, so no `on_save`.
fn dock_config() -> Config {
	Config {
		storage_key: Some("exec-viz-layout".into()),
		// Two tiles, one tab each — the bar is pure overhead here, so halve the default and give the
		// pixels to the chart.
		title_h_rem: 1.0,
		..Default::default()
	}
}

/// Fresh-layout seed: the ratio the old fixed split had, both tiles full height.
fn seed(mut api: PackedApi) {
	let cols = api.cols().max(2);
	let chart_w = (cols * 9 / 16).max(1);
	let rows = Config::default().rows;
	for (id, w) in [(CHART_PANEL, chart_w), (DAG_PANEL, cols - chart_w)] {
		let gid = api.mint_group_id();
		api.place(Group::new(gid, PanelId(id.into())), w, rows, MinSize::Rem { w: 12.0, h: 8.0 });
	}
}

/// Mounting waits on the day payload, which lands long after this pane is on screen — so the host
/// element the shim needs is always there by the time the effect fires.
#[component]
fn ChartPane() -> Element {
	let day = use_context::<Resource<Result<String, String>>>();
	let topology = use_context::<Resource<Result<Vec<TopoNode>, String>>>();
	let mut banner = use_context::<Signal<Option<String>>>();
	use_effect(move || {
		// Read unconditionally, and before the day gate: this is what subscribes the effect to a
		// toggle, and re-mounting is the redraw path (`draw` tears its own series down at the top).
		let hidden: Vec<String> = match &*topology.read() {
			Some(Ok(t)) => t.iter().filter(|n| !state::shown(n)).map(|n| n.node.clone()).collect(),
			_ => Vec::new(),
		};
		if let Some(Ok(json)) = &*day.read() {
			let json = json.clone();
			let spec = serde_json::json!({ "theme": "#131722", "hidden": hidden }).to_string();
			spawn(async move {
				let el = chart_el().expect("the chart host renders with this pane");
				banner.set(v_utils::lwc::mount(el, "/lwc_draw.js", &json, &spec).await);
			});
		}
	});
	rsx! {
		div { class: "chart-host",
			div { id: CHART_ID, style: "position:absolute;inset:0" }
		}
	}
}

#[component]
fn DagPane() -> Element {
	let topology = use_context::<Resource<Result<Vec<TopoNode>, String>>>();
	rsx! {
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
		"l" => state::follow().await,
		"v" => state::toggle_view(),
		"C" => state::clear_views(),
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
