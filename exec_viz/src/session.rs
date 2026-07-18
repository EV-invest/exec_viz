//! Replay session over an event stream: events + graph + cursor. No trace persistence —
//! determinism is the storage. Forward = tick; backward = fresh graph re-run from 0.

use trading_data_dag::{Dag, Fire, Observer, Stamped};

use crate::api_types::{Activation, ActivationFrame, TopoNode};

/// Step order IS topo order: one throwaway tick under a collector yields the static graph shape.
pub fn topology<G: Dag>() -> Vec<TopoNode> {
	let mut c = Collect::default();
	G::default().tick_obs(None, &mut c);
	c.0.into_iter()
		.map(|a| TopoNode {
			node: a.node,
			deps: a.deps,
			dims: a.dims,
		})
		.collect()
}
pub struct ReplaySession<G: Dag> {
	events: Vec<G::Event>,
	graph: G,
	/// Number of consumed events; the frame describes the tick that consumed `events[cursor-1]`.
	cursor: usize,
	last: Vec<Activation>,
}
impl<G: Dag> ReplaySession<G> {
	pub fn new(events: Vec<G::Event>) -> Self {
		Self {
			events,
			graph: G::default(),
			cursor: 0,
			last: Vec::new(),
		}
	}

	pub fn frame(&self) -> ActivationFrame {
		ActivationFrame {
			tick: self.cursor,
			total: self.events.len(),
			ts_ns: self.cursor.checked_sub(1).map(|i| self.events[i].ts_ns()).unwrap_or(0),
			activations: self.last.clone(),
		}
	}

	fn advance_one(&mut self) {
		let mut c = Collect::default();
		self.graph.tick_obs(Some(self.events[self.cursor]), &mut c);
		self.cursor += 1;
		self.last = c.0;
	}

	/// All but the final tick run under the `()` observer — zero flattening/FD cost — so bulk
	/// steps and seeks stay cheap; only the landing tick is fully observed.
	pub fn step(&mut self, n: usize) -> ActivationFrame {
		let last = (self.cursor + n).min(self.events.len());
		while self.cursor + 1 < last {
			self.graph.tick_obs(Some(self.events[self.cursor]), &mut ());
			self.cursor += 1;
		}
		if self.cursor < last {
			self.advance_one();
		}
		self.frame()
	}

	/// Backward seek = fresh graph + re-run from 0 (<1s for a full day).
	pub fn seek(&mut self, tick: usize) -> ActivationFrame {
		if tick < self.cursor {
			self.graph = G::default();
			self.cursor = 0;
			self.last.clear();
		}
		self.step(tick - self.cursor)
	}

	/// Advance until `node` (trimmed name) fires, or the stream ends.
	pub fn step_until(&mut self, node: &str) -> ActivationFrame {
		while self.cursor < self.events.len() {
			self.advance_one();
			if self.last.iter().any(|a| a.fired && a.node == node) {
				break;
			}
		}
		self.frame()
	}

	/// Advance until any of `nodes` fires with an out *different from its value at call time*
	/// (so a node stuck emitting the same value is skipped through to its next actual change),
	/// or the stream ends.
	pub fn step_until_change(&mut self, nodes: &[String]) -> ActivationFrame {
		let baseline: std::collections::HashMap<String, String> = self.last.iter().filter(|a| nodes.contains(&a.node)).map(|a| (a.node.clone(), a.out.clone())).collect();
		while self.cursor < self.events.len() {
			self.advance_one();
			if self.last.iter().any(|a| a.fired && nodes.contains(&a.node) && baseline.get(&a.node) != Some(&a.out)) {
				break;
			}
		}
		self.frame()
	}
}

/// Generics-aware last-`::`-segment: `nodes::Rsi<14>` → `Rsi<14>` (path segments inside `<>`
/// are left as-is). `type_name` strings are build-local, display-only.
fn trim(name: &str) -> String {
	let mut depth = 0u32;
	let mut start = 0;
	let b = name.as_bytes();
	for i in 0..b.len() {
		match b[i] {
			b'<' => depth += 1,
			b'>' => depth -= 1,
			b':' if depth == 0 && b.get(i + 1) == Some(&b':') => start = i + 2,
			_ => {}
		}
	}
	name[start..].to_string()
}

#[derive(Default)]
struct Collect(Vec<Activation>);

impl Observer for Collect {
	fn on(&mut self, node: &'static str, deps: &'static [&'static str], fire: Fire<'_>) {
		self.0.push(Activation {
			node: trim(node),
			deps: deps.iter().map(|d| trim(d)).collect(),
			out: format!("{}", fire.glance),
			detail: format!("{:?}", fire.debug),
			fired: fire.vals.is_some(),
			dims: fire.dims.to_vec(),
			vals: fire.vals.map(<[f64]>::to_vec),
			jac: fire.jac.map(|j| j.iter().map(|w| if w.is_nan() { None } else { Some(*w) }).collect()),
		});
	}
}
