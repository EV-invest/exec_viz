//! The DAG activations panel: DOM-inspector-style, present-moment only. One column per topo
//! level, node cards showing the latest `Debug` out, lighting up (with a CSS re-flash keyed on
//! tick) when they fire. Hover highlights a node's direct deps. No graph lib.

use std::collections::HashMap;

use dioxus::prelude::*;

use crate::{api_types::TopoNode, web::state};

#[component]
pub fn DagPanel(topology: Vec<TopoNode>) -> Element {
	let mut hover = use_signal(|| Option::<String>::None);

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

	let frame = state::FRAME();
	let tick = frame.as_ref().map(|f| f.tick).unwrap_or(0);
	let acts: HashMap<String, (bool, String)> = frame.iter().flat_map(|f| f.activations.iter()).map(|a| (a.node.clone(), (a.fired, a.out.clone()))).collect();
	let hovered_deps: Vec<String> = hover().and_then(|h| topology.iter().find(|n| n.node == h).map(|n| n.deps.clone())).unwrap_or_default();

	rsx! {
		div { class: "dag",
			for col in cols {
				div { class: "dag-col",
					for n in col {
						{
							let (fired, out) = acts.get(&n.node).cloned().unwrap_or((false, String::new()));
							let dep_hl = hovered_deps.contains(&n.node);
							let class = format!("dag-card{}{}", if fired { " lit" } else { "" }, if dep_hl { " dep" } else { "" });
							let name = n.node.clone();
							let node = n.node.clone();
							rsx! {
								div {
									// key on tick so the CSS flash re-triggers on every firing
									key: "{node}-{tick}",
									class: "{class}",
									onmouseenter: move |_| hover.set(Some(name.clone())),
									onmouseleave: move |_| hover.set(None),
									div { class: "dag-name", "{node}" }
									div { class: "dag-out", title: "{out}", "{out}" }
								}
							}
						}
					}
				}
			}
		}
	}
}
