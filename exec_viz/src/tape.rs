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

use trading_data_dag::{Fire, Ink, Observer, Sketch};

use crate::api_types::{Activation, ActivationFrame, BarOut, DayOut, GuideOut, InkOut, PointOut, SeriesOut, SketchOut, TopoNode};

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

	pub(crate) fn lock(&self) -> MutexGuard<'_, Tape> {
		self.0.lock().expect("tape mutex poisoned by a panicking observer")
	}
}

impl Observer for Viz {
	fn on(&mut self, node: &'static str, deps: &'static [&'static str], gates: &'static [&'static str], fire: Fire<'_>) {
		let mut t = self.lock();
		let i = t.idx;
		t.idx += 1;
		if t.topology.len() == i {
			let node = TopoNode {
				node: trim(node),
				deps: deps.iter().map(|d| trim(d)).collect(),
				gates: gates.iter().map(|g| trim(g)).collect(),
				dims: fire.dims.to_vec(),
				sketch: fire.sketch.into(),
			};
			t.series.push(SeriesOut {
				node: node.node.clone(),
				deps: node.deps.clone(),
				gates: node.gates.clone(),
				dims: node.dims.clone(),
				sketch: node.sketch.clone(),
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

		let act = Act {
			out: format!("{}", fire.glance),
			detail: clip(&format!("{:?}", fire.debug)),
			vals: fire.vals.map(<[f64]>::to_vec),
			jac: fire.jac.map(<[f64]>::to_vec),
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
	series: Vec<SeriesOut>,
	bars: Vec<BarOut>,
	/// Ticks consumed; the frame describes the one that consumed `cursor - 1`.
	cursor: usize,
	ts_ns: i64,
	/// Per-tick step counter: step order is identical every tick, so it doubles as node id.
	idx: usize,
}

impl Tape {
	pub(crate) fn topology(&self) -> Vec<TopoNode> {
		self.topology.clone()
	}

	pub(crate) fn day(&self) -> DayOut {
		DayOut {
			bars: self.bars.clone(),
			series: self.series.clone(),
			price_node: self.price_node.clone(),
		}
	}

	pub(crate) fn frame(&self) -> ActivationFrame {
		let tick = self.cursor.checked_sub(1).and_then(|i| i.checked_sub(self.base)).and_then(|i| self.ticks.get(i));
		ActivationFrame {
			tick: self.cursor,
			total: self.total(),
			ts_ns: tick.map_or(0, |t| t.ts_ns),
			activations: tick.map_or_else(Vec::new, |t| {
				t.acts
					.iter()
					.zip(&self.topology)
					.map(|(a, n)| Activation {
						node: n.node.clone(),
						deps: n.deps.clone(),
						gates: n.gates.clone(),
						out: a.out.clone(),
						detail: a.detail.clone(),
						fired: a.vals.is_some(),
						dims: n.dims.clone(),
						vals: a.vals.clone(),
						jac: a.jac.as_ref().map(|j| j.iter().map(|w| (!w.is_nan()).then_some(*w)).collect()),
					})
					.collect()
			}),
		}
	}

	pub(crate) fn step(&mut self, n: usize) -> ActivationFrame {
		self.cursor = self.cursor.saturating_add(n).clamp(self.base, self.total());
		self.frame()
	}

	pub(crate) fn seek(&mut self, tick: usize) -> ActivationFrame {
		self.cursor = tick.clamp(self.base, self.total());
		self.frame()
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
		self.cursor = self.cursor.max(self.base);
		while self.cursor < self.total() {
			self.cursor += 1;
			if hit(&self.ticks[self.cursor - 1 - self.base]) {
				break;
			}
		}
		self.frame()
	}

	fn total(&self) -> usize {
		self.base + self.ticks.len()
	}
}

impl From<&Sketch> for SketchOut {
	fn from(s: &Sketch) -> Self {
		let ink = |i: &Ink| InkOut { l: i.l, c: i.c, a: i.a };
		SketchOut {
			range: s.range,
			guides: s
				.guides
				.iter()
				.map(|g| GuideOut {
					label: g.label.to_string(),
					value: g.value,
					ink: ink(&g.ink),
				})
				.collect(),
			labels: s.labels.iter().map(|l| l.to_string()).collect(),
			inks: s.inks.iter().map(ink).collect(),
			overlay: s.overlay,
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
