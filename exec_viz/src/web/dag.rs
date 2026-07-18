//! The DAG activations panel: DOM-inspector-style, present-moment only. One column per topo
//! level; each node card renders its element grid (shape from `TopoNode::dims`), cells heat-
//! colored by per-element running min/max. An SVG overlay draws element→element edges weighted
//! by the tick's finite-difference Jacobian — values and sensitivities on the computation graph
//! itself, à la Jane Street's "Computations that differentiate, debug and document themselves".

use std::collections::HashMap;

use dioxus::prelude::*;

use crate::{
	api_types::{Activation, TopoNode},
	web::state,
};

const DAG_ID: &str = "dag-root";

#[component]
pub fn DagPanel(topology: Vec<TopoNode>) -> Element {
	let mut hover = use_signal(|| Hover::None);
	let mut ranges = use_signal(HashMap::<String, Vec<(f64, f64)>>::new);
	let mut edges = use_signal(Vec::<Edge>::new);

	// `level(node) = 1 + max(level(deps))`, roots 0 — one pass works because the server sends
	// nodes in step (= topo) order.
	let mut level: HashMap<String, usize> = HashMap::new();
	let mut cols: Vec<Vec<TopoNode>> = Vec::new();
	for n in &topology {
		let l = n.deps.iter().map(|d| level.get(d).expect("topo order: dep precedes node") + 1).max().unwrap_or(0);
		level.insert(n.node.clone(), l);
		if cols.len() <= l {
			cols.resize_with(l + 1, Vec::new);
		}
		cols[l].push(n.clone());
	}

	let topo = topology.clone();
	use_effect(move || {
		let Some(frame) = state::FRAME() else { return };
		{
			let mut r = ranges.write();
			for a in &frame.activations {
				if let Some(vals) = &a.vals {
					let e = r.entry(a.node.clone()).or_insert_with(|| vec![(f64::INFINITY, f64::NEG_INFINITY); vals.len()]);
					for (i, v) in vals.iter().enumerate() {
						e[i].0 = e[i].0.min(*v);
						e[i].1 = e[i].1.max(*v);
					}
				}
			}
		}
		let dep_lens: HashMap<String, usize> = topo.iter().map(|n| (n.node.clone(), n.dims.iter().product())).collect();
		let acts = frame.activations.clone();
		spawn(async move {
			// measure one timer tick after the DOM patch so fresh cell rects are non-zero
			gloo_timers::future::TimeoutFuture::new(0).await;
			edges.set(measure_edges(&acts, &dep_lens));
		});
	});

	let frame = state::FRAME();
	let acts: HashMap<String, (bool, String, String, Option<Vec<f64>>)> = frame
		.iter()
		.flat_map(|f| f.activations.iter())
		.map(|a| (a.node.clone(), (a.fired, a.out.clone(), a.detail.clone(), a.vals.clone())))
		.collect();
	let hovered_deps: Vec<String> = hover().node().and_then(|h| topology.iter().find(|n| n.node == h).map(|n| n.deps.clone())).unwrap_or_default();
	let hovered_node: Option<String> = hover().node().map(str::to_string);

	rsx! {
		div { class: "dag", id: DAG_ID,
			for col in cols {
				div { class: "dag-col",
					for n in col {
						{
							let (fired, out, detail, vals) = acts.get(&n.node).cloned().unwrap_or((false, String::new(), String::new(), None));
							let dep_hl = hovered_deps.contains(&n.node);
							let selected = state::SELECTED().contains(&n.node);
							let class = format!(
								"dag-card{}{}{}",
								if fired { " lit" } else { "" },
								if dep_hl { " dep" } else { "" },
								if selected { " sel" } else { "" },
							);
							let name = n.node.clone();
							let clicked = n.node.clone();
							let node = n.node.clone();
							let len: usize = n.dims.iter().product();
							// row-major: cols = last dim; rank ≥ 3 reads as stacked 2D slices
							let gcols = n.dims.last().copied().unwrap_or(1);
							let ranges_r = ranges.read();
							let node_ranges = ranges_r.get(&n.node);
							rsx! {
								div {
									key: "{node}",
									class: "{class}",
									onmouseenter: move |_| hover.set(Hover::Card(name.clone())),
									onmouseleave: move |_| hover.set(Hover::None),
									onclick: move |_| state::toggle_select(&clicked),
									div { class: "dag-name", "{node}" }
									div { class: "dag-out", "{out}" }
									div {
										class: "dag-grid",
										style: "grid-template-columns: repeat({gcols}, minmax(24px, 1fr));",
										for i in 0..len {
											{
												let cell_class = if vals.is_some() { "dag-cell" } else { "dag-cell dim" };
												let (txt, heat) = match vals.as_ref().map(|v| v[i]) {
													Some(v) => (fmt_val(v), heat(node_ranges.and_then(|r| r.get(i)), v)),
													None => (String::new(), 0.0),
												};
												let enter = n.node.clone();
												let leave = n.node.clone();
												rsx! {
													div {
														id: "dagel-{node}-{i}",
														class: "{cell_class}",
														style: "background: rgba(38, 166, 154, {heat});",
														onmouseenter: move |_| hover.set(Hover::Cell(enter.clone(), i)),
														onmouseleave: move |_| hover.set(Hover::Card(leave.clone())),
														"{txt}"
													}
												}
											}
										}
									}
									if !detail.is_empty() && hovered_node.as_deref() == Some(node.as_str()) {
										div { class: "dag-tip", "{detail}" }
									}
								}
							}
						}
					}
				}
			}
			svg { class: "dag-edges",
				{
					let h = hover();
					rsx! {
						for e in edges.read().iter().filter(move |e| match &h {
							Hover::None => true,
							Hover::Card(n) => e.to.0 == *n || e.from.0 == *n,
							Hover::Cell(n, i) => (e.to.0 == *n && e.to.1 == *i) || (e.from.0 == *n && e.from.1 == *i),
						}) {
							line {
								key: "{e.from.0}-{e.from.1}-{e.to.0}-{e.to.1}",
								x1: "{e.x1}",
								y1: "{e.y1}",
								x2: "{e.x2}",
								y2: "{e.y2}",
								stroke: if e.w > 0.0 { "#26a69a" } else { "#ef5350" },
								stroke_width: "{1.0 + 2.0 * e.mag}",
								opacity: "{0.15 + 0.85 * e.mag}",
							}
						}
					}
				}
			}
		}
	}
}
#[derive(Clone, PartialEq)]
enum Hover {
	None,
	Card(String),
	Cell(String, usize),
}

impl Hover {
	fn node(&self) -> Option<&str> {
		match self {
			Hover::None => None,
			Hover::Card(n) | Hover::Cell(n, _) => Some(n),
		}
	}
}

/// One jac entry, resolved to measured pixel endpoints relative to the `.dag` origin. `mag` is
/// `|w| / max|w|` over the destination node's jac — piecewise-constant nodes spike to ±1/h at
/// threshold crossings, and this per-node normalization absorbs them.
#[derive(Clone, PartialEq)]
struct Edge {
	from: (String, usize),
	to: (String, usize),
	x1: f64,
	y1: f64,
	x2: f64,
	y2: f64,
	w: f64,
	mag: f64,
}

/// Resolves every non-zero jac entry of the frame to pixel endpoints: dep concat slot →
/// (dep node, local element) via prefix sums over the deps' lens, both cells measured against
/// the `.dag` origin so the overlay is scroll-invariant.
fn measure_edges(acts: &[Activation], dep_lens: &HashMap<String, usize>) -> Vec<Edge> {
	let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
		return Vec::new();
	};
	let Some(root) = doc.get_element_by_id(DAG_ID) else {
		return Vec::new();
	};
	let root_rect = root.get_bounding_client_rect();
	let mut out = Vec::new();
	for a in acts {
		let Some(jac) = &a.jac else { continue };
		let out_len: usize = a.dims.iter().product();
		let lens: Vec<usize> = a.deps.iter().map(|d| *dep_lens.get(d).expect("dep present in topology")).collect();
		let total: usize = lens.iter().sum();
		assert_eq!(jac.len(), out_len * total, "jac shape mismatch for {}", a.node);
		let max_w = jac.iter().flatten().fold(0.0_f64, |m, w| m.max(w.abs()));
		if max_w == 0.0 {
			continue;
		}
		for i in 0..out_len {
			for j in 0..total {
				let Some(w) = jac[i * total + j] else { continue };
				if w == 0.0 {
					continue;
				}
				let (mut dep_idx, mut local) = (0, j);
				while local >= lens[dep_idx] {
					local -= lens[dep_idx];
					dep_idx += 1;
				}
				let from = doc.get_element_by_id(&format!("dagel-{}-{local}", a.deps[dep_idx]));
				let to = doc.get_element_by_id(&format!("dagel-{}-{i}", a.node));
				let (Some(from_el), Some(to_el)) = (from, to) else { continue };
				let fr = from_el.get_bounding_client_rect();
				let tr = to_el.get_bounding_client_rect();
				if fr.width() == 0.0 || tr.width() == 0.0 {
					continue; // mid-remount: the next frame re-measures
				}
				out.push(Edge {
					from: (a.deps[dep_idx].clone(), local),
					to: (a.node.clone(), i),
					x1: fr.right() - root_rect.left(),
					y1: fr.top() + fr.height() / 2.0 - root_rect.top(),
					x2: tr.left() - root_rect.left(),
					y2: tr.top() + tr.height() / 2.0 - root_rect.top(),
					w,
					mag: w.abs() / max_w,
				});
			}
		}
	}
	out
}

fn heat(range: Option<&(f64, f64)>, v: f64) -> f64 {
	let t = match range {
		Some((lo, hi)) if hi > lo => ((v - lo) / (hi - lo)).clamp(0.0, 1.0),
		_ => 0.5,
	};
	0.12 + 0.55 * t
}

fn fmt_val(v: f64) -> String {
	if v == 0.0 {
		"0".to_string()
	} else if v.abs() >= 1000.0 {
		format!("{v:.0}")
	} else if v.abs() >= 1.0 {
		format!("{v:.2}")
	} else {
		format!("{v:.4}")
	}
}
