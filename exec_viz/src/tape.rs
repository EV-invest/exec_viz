//! The attach surface: [`Viz`] is an [`Observer`] the app hands to its own `tick_obs`, plus the
//! shared read side the server scrubs. Cloning shares the tape.
//!
//! Live-first, so the recording *is* the storage: a live run can't be re-run, which is what the
//! old replay-by-rewinding-the-graph model assumed. Ticks land in a ring; the per-node series the
//! chart draws is downsampled online and never dropped.

use std::{
	collections::VecDeque,
	sync::{Arc, Mutex, MutexGuard},
};

use trading_data_dag::{Fire, Ink, Observer, Plot};

use crate::api_types::{Activation, ActivationFrame, BarOut, DayOut, GuideOut, InkOut, PlotOut, PointOut, SeriesOut, TopoNode};

#[derive(Clone)]
pub struct Viz(Arc<Mutex<Tape>>);

impl Viz {
	/// `price_node` backs the candles and is skipped in the indicator panes; `None` = no price
	/// pane. `capacity` is the retained tick count, `bucket_ms` the chart's sample period.
	pub fn new(price_node: Option<&str>, capacity: usize, bucket_ms: i64) -> Self {
		assert!(capacity > 0 && bucket_ms > 0);
		Self(Arc::new(Mutex::new(Tape {
			price_node: price_node.map(str::to_string),
			capacity,
			bucket_ms,
			topology: Vec::new(),
			ticks: VecDeque::new(),
			base: 0,
			sealed: false,
			last_fired: Vec::new(),
			series: Vec::new(),
			bars: Vec::new(),
			cursor: 0,
			ts_ns: 0,
			idx: 0,
		})))
	}

	/// Opens a tick and hands itself back as the observer: `graph.tick_obs(batches, viz.at(ts))`.
	pub fn at(&mut self, ts_ns: i64) -> &mut Self {
		let mut t = self.lock();
		t.ts_ns = ts_ns;
		t.idx = 0;
		if t.ticks.len() == t.capacity {
			t.ticks.pop_front();
			t.base += 1;
		}
		t.ticks.push_back(Tick { ts_ns, acts: Vec::new() });
		drop(t);
		self
	}

	/// A closed price bar for the chart's candle pane.
	pub fn bar(&self, bar: BarOut) {
		self.lock().bars.push(bar);
	}

	/// Ends the recording: the tick `at` opened last becomes addressable and `total` stops growing.
	/// By value, so the handle you recorded through is spent. A live feed never calls this.
	pub fn seal(self) {
		self.lock().sealed = true;
	}

	pub(crate) fn lock(&self) -> MutexGuard<'_, Tape> {
		// Served concurrently with the recording it describes: a panicking handler must not cost the
		// run its tape. Every op leaves the tape consistent, so the inner value is still readable.
		self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

impl Observer for Viz {
	fn on(&mut self, node: &'static str, deps: &'static [&'static str], gates: &'static [&'static str], fire: Fire<'_>) {
		let mut t = self.lock();
		let i = t.idx;
		t.idx += 1;
		if t.topology.len() == i {
			let deps: Vec<String> = deps.iter().map(|d| buffered(&trim(d), &t.topology)).collect();
			let node = TopoNode {
				node: trim(node),
				deps,
				gates: gates.iter().map(|g| trim(g)).collect(),
				dims: fire.dims.to_vec(),
				plots: fire.plots.iter().map(PlotOut::from).collect(),
			};
			t.series.push(SeriesOut {
				node: node.node.clone(),
				deps: node.deps.clone(),
				gates: node.gates.clone(),
				dims: node.dims.clone(),
				plots: node.plots.clone(),
				points: Vec::new(),
			});
			t.topology.push(node);
		} else {
			assert_eq!(t.topology[i].node, trim(node), "step order shifted between ticks");
		}

		if let Some(vals) = fire.vals {
			let ms = t.ts_ns / 1_000_000;
			let bucket = ms - ms.rem_euclid(t.bucket_ms);
			match t.series[i].points.last_mut() {
				Some(p) if p.ts_ms == bucket => p.vals = vals.to_vec(),
				_ => t.series[i].points.push(PointOut { ts_ms: bucket, vals: vals.to_vec() }),
			}
		}

		if t.last_fired.len() == i {
			t.last_fired.push(None);
		}
		if fire.vals.is_some() {
			let tick = t.total() - 1;
			t.last_fired[i] = Some(tick);
		}
		let act = Act {
			out: format!("{}", fire.glance),
			detail: clip(&format!("{:?}", fire.debug)),
			vals: fire.vals.map(<[f64]>::to_vec),
			jac: fire.jac.map(<[f64]>::to_vec),
			last_fired: t.last_fired[i],
		};
		t.ticks.back_mut().expect("`Viz::at` opens the tick before the graph steps").acts.push(act);
	}
}

/// Node identity lives once in `topology`; a tick keeps only what varies.
struct Act {
	out: String,
	detail: String,
	vals: Option<Vec<f64>>,
	jac: Option<Vec<f64>>,
	/// Absolute tick of this node's latest fire as of *this* tick (`Some(self)` when it fired), so
	/// a scrubbed frame carries forward the value that was standing then, not the live one.
	last_fired: Option<usize>,
}

struct Tick {
	ts_ns: i64,
	acts: Vec<Act>,
}

pub(crate) struct Tape {
	price_node: Option<String>,
	capacity: usize,
	bucket_ms: i64,
	topology: Vec<TopoNode>,
	/// ponytail: fixed-capacity ring — scrollback ends at `base`. Persist ticks to disk if a
	/// full-day-plus scrub ever matters.
	ticks: VecDeque<Tick>,
	/// Absolute index of `ticks[0]`; grows as the ring evicts, so tick numbers stay stable.
	base: usize,
	/// The recording is over — see [`Tape::head`].
	sealed: bool,
	/// Per node (step index): absolute tick of its latest fire, stamped into each `Act`.
	last_fired: Vec<Option<usize>>,
	series: Vec<SeriesOut>,
	bars: Vec<BarOut>,
	/// Ticks consumed; the frame describes the one that consumed `cursor - 1`.
	cursor: usize,
	ts_ns: i64,
	/// Per-tick step counter: step order is identical every tick, so it doubles as node id.
	idx: usize,
}

impl Tape {
	/// Last addressable cursor. `at` opens a tick before the graph sweeps it, so until the recording
	/// is sealed the newest tick is still being written and is not a frame anyone may see.
	fn head(&self) -> usize {
		if self.sealed { self.total() } else { self.total().saturating_sub(1) }
	}

	/// Empty until the first tick closes: `Observer::on` grows this one node at a time *within* a
	/// tick, and a client topo-sorting a prefix would find a node whose deps aren't there yet.
	pub(crate) fn topology(&self) -> Vec<TopoNode> {
		if self.head() < 1 { Vec::new() } else { self.topology.clone() }
	}

	pub(crate) fn day(&self) -> DayOut {
		DayOut {
			bars: self.bars.clone(),
			// A buffer's series is its source's, element for element — charting it would draw every
			// buffered pane twice.
			series: self.series.iter().filter(|s| !s.node.starts_with("Buffer<")).cloned().collect(),
			price_node: self.price_node.clone(),
		}
	}

	pub(crate) fn frame(&self) -> ActivationFrame {
		let tick = self.cursor.checked_sub(1).and_then(|i| i.checked_sub(self.base)).and_then(|i| self.ticks.get(i));
		ActivationFrame {
			tick: self.cursor,
			total: self.head(),
			sealed: self.sealed,
			pending: false,
			ts_ns: tick.map_or(0, |t| t.ts_ns),
			activations: tick.map_or_else(Vec::new, |t| {
				t.acts
					.iter()
					.zip(&self.topology)
					.enumerate()
					.map(|(i, (a, n))| {
						// A quiet node still holds its last value: show it, `fired` is what says it's live.
						let held = if a.vals.is_some() { a } else { self.held(i, a.last_fired).unwrap_or(a) };
						Activation {
							node: n.node.clone(),
							deps: n.deps.clone(),
							gates: n.gates.clone(),
							out: held.out.clone(),
							detail: held.detail.clone(),
							fired: a.vals.is_some(),
							dims: n.dims.clone(),
							vals: held.vals.as_ref().map(|v| v.iter().map(|x| x.is_finite().then_some(*x)).collect()),
							jac: a.jac.as_ref().map(|j| j.iter().map(|w| (!w.is_nan()).then_some(*w)).collect()),
						}
					})
					.collect()
			}),
		}
	}

	/// Node `i`'s act on the tick it last fired; `None` once that tick has been evicted.
	fn held(&self, i: usize, last_fired: Option<usize>) -> Option<&Act> {
		let act = self.ticks.get(last_fired?.checked_sub(self.base)?)?.acts.get(i)?;
		assert!(act.vals.is_some(), "`last_fired` points at a tick where the node fired");
		Some(act)
	}

	pub(crate) fn step(&mut self, n: usize) -> ActivationFrame {
		let target = self.cursor.saturating_add(n);
		self.cursor = self.bound(target);
		let mut f = self.frame();
		f.pending = !self.sealed && target > self.head();
		f
	}

	pub(crate) fn seek(&mut self, tick: usize) -> ActivationFrame {
		self.cursor = self.bound(tick);
		let mut f = self.frame();
		f.pending = !self.sealed && tick > self.head();
		f
	}

	/// `base` itself is not a cursor: `frame` describes the tick *before* the cursor, so the oldest
	/// one still in the ring is reached at `base + 1`.
	fn bound(&self, tick: usize) -> usize {
		tick.clamp((self.base + 1).min(self.head()), self.head())
	}

	/// Advance until `node` (trimmed name) fires, or the recording ends.
	pub(crate) fn step_until(&mut self, node: &str) -> ActivationFrame {
		match self.topology.iter().position(|n| n.node == node) {
			Some(i) => self.scan(|t| t.acts.get(i).is_some_and(|a| a.vals.is_some())),
			None => self.frame(),
		}
	}

	/// Advance until any of `nodes` fires with an out *different from its value at call time* (so a
	/// node stuck emitting the same value is skipped through to its next actual change).
	pub(crate) fn step_until_change(&mut self, nodes: &[String]) -> ActivationFrame {
		let watched: Vec<usize> = self.topology.iter().enumerate().filter(|(_, n)| nodes.contains(&n.node)).map(|(i, _)| i).collect();
		let baseline: Vec<Option<String>> = {
			let now = self.frame();
			watched.iter().map(|&i| now.activations.get(i).map(|a| a.out.clone())).collect()
		};
		self.scan(|t| {
			watched
				.iter()
				.zip(&baseline)
				.any(|(&i, was)| t.acts.get(i).is_some_and(|a| a.vals.is_some() && Some(&a.out) != was.as_ref()))
		})
	}

	fn scan(&mut self, hit: impl Fn(&Tick) -> bool) -> ActivationFrame {
		self.cursor = self.bound(self.cursor);
		let mut found = false;
		while self.cursor < self.head() {
			self.cursor += 1;
			if hit(&self.ticks[self.cursor - 1 - self.base]) {
				found = true;
				break;
			}
		}
		let mut f = self.frame();
		f.pending = !self.sealed && !found;
		f
	}

	fn total(&self) -> usize {
		self.base + self.ticks.len()
	}
}

impl From<&Plot> for PlotOut {
	fn from(p: &Plot) -> Self {
		let ink = |i: &Ink| InkOut { l: i.l, c: i.c, a: i.a };
		PlotOut {
			slots: p.slots.to_vec(),
			range: p.range,
			guides: p
				.guides
				.iter()
				.map(|g| GuideOut {
					label: g.label.to_string(),
					value: g.value,
					ink: ink(&g.ink),
				})
				.collect(),
			labels: p.labels.iter().map(|l| l.to_string()).collect(),
			inks: p.inks.iter().map(ink).collect(),
			overlay: p.overlay,
		}
	}
}

/// ponytail: the hover tooltip is a study aid, and a root's `Debug` is its whole arrival batch —
/// unclipped, one tick of a busy feed outweighs a thousand quiet ones.
fn clip(detail: &str) -> String {
	const MAX: usize = 256;
	match detail.char_indices().nth(MAX) {
		Some((i, _)) => format!("{}…", &detail[..i]),
		None => detail.to_string(),
	}
}

/// `Buffering<X, J>` names a *shape*, not a frame node — the node the client must draw the edge to
/// is the `Buffer<X, K>` that serves it. A buffer always precedes its consumers in step order, so
/// `topology` already holds it. Non-`Buffering` deps pass through.
fn buffered(dep: &str, topology: &[TopoNode]) -> String {
	let Some(inner) = dep.strip_prefix("Buffering<").and_then(|s| s.strip_suffix('>')) else {
		return dep.to_string();
	};
	// `J` is a `usize` literal, so the last comma is the top-level one.
	let series = inner[..inner.rfind(',').expect("Buffering<C, J> has two arguments")].trim_end();
	let prefix = format!("Buffer<{series},");
	topology
		.iter()
		.map(|t| &t.node)
		.find(|n| n.starts_with(&prefix))
		.unwrap_or_else(|| panic!("{dep} has no `Buffer<{series}, _>` ahead of it in the graph"))
		.clone()
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
