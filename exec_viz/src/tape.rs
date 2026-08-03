//! The attach surface: [`Viz`] is an [`Observer`] the app hands to its own `tick_obs`, plus the
//! shared read side the server scrubs. Cloning shares the tape.
//!
//! Live-first, so the recording *is* the storage: a live run can't be re-run, which is what the
//! old replay-by-rewinding-the-graph model assumed. Ticks land in a bounded buffer that thins with
//! age rather than forgetting its front; the per-node series the chart draws is downsampled online
//! and never dropped.

use std::{
	collections::{HashMap, VecDeque},
	sync::{Arc, Mutex, MutexGuard},
};

use trading_data_dag::{Fire, Ink, Observer, Plot};

use crate::api_types::{Activation, ActivationFrame, DayOut, GuideOut, InkOut, PlotOut, PointOut, SeriesOut, TopoNode};

#[derive(Clone)]
pub struct Viz(Arc<Mutex<Tape>>);

impl Viz {
	/// `price_node` names an OHLCV node — its recorded series *is* the candle pane, and it is skipped
	/// in the indicator panes so it draws once; `None` = no price pane. `capacity` bounds the
	/// retained tick count — see [`Tape::thin`] for what a run longer than that keeps — and
	/// `bucket_ms` is the chart's sample period.
	pub fn new(price_node: Option<&str>, capacity: usize, bucket_ms: i64) -> Self {
		assert!(capacity > 3 && bucket_ms > 0);
		Self(Arc::new(Mutex::new(Tape {
			price_node: price_node.map(str::to_string),
			capacity,
			bucket_ms,
			topology: Vec::new(),
			ticks: VecDeque::new(),
			opened: 0,
			stride: 1,
			sealed: false,
			series: Vec::new(),
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
			t.thin();
		}
		debug_assert!(t.ticks.len() < t.capacity);
		let abs = t.opened;
		t.opened += 1;
		t.ticks.push_back(Tick { abs, ts_ns, acts: Vec::new() });
		drop(t);
		self
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
	fn on(&mut self, node: &'static str, deps: &'static [&'static str], gates: &'static [bool], fire: Fire<'_>) {
		let mut t = self.lock();
		let i = t.idx;
		t.idx += 1;
		if t.topology.len() == i {
			// names rather than the positional flags: on the wire `gates` is a subset of `deps`, which is
			// what both readers test membership against.
			let gates: Vec<String> = deps.iter().zip(gates).filter(|(_, g)| **g).map(|(d, _)| trim(d)).collect();
			let deps: Vec<String> = deps.iter().map(|d| buffered(&trim(d), &t.topology)).collect();
			let node = TopoNode {
				node: trim(node),
				deps,
				gates,
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
				// `>=`, not `==`: a feed's timestamps do go backwards (a coarse lane landing on an exact
				// hour boundary weaves ahead of the tape around it), and one non-ascending point makes
				// lightweight-charts drop *every* series it holds.
				Some(p) if p.ts_ms >= bucket => p.vals = vals.to_vec(),
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
	/// Index among *all* ticks ever opened, so a tick's id survives every thinning pass.
	abs: usize,
	ts_ns: i64,
	acts: Vec<Act>,
}

pub(crate) struct Tape {
	price_node: Option<String>,
	capacity: usize,
	bucket_ms: i64,
	topology: Vec<TopoNode>,
	/// Ascending by `abs`, fewer than `capacity` of them — see [`Tape::thin`].
	ticks: VecDeque<Tick>,
	/// Ticks ever opened, thinned-away ones included.
	opened: usize,
	/// Spacing of the retained ticks outside the whole-kept tail; a power of two.
	stride: usize,
	/// The recording is over — see [`Tape::head`].
	sealed: bool,
	series: Vec<SeriesOut>,
	/// Ticks consumed; the frame describes the one that consumed `cursor - 1`. Absolute, so a
	/// thinning pass under a parked cursor cannot slide it.
	cursor: usize,
	ts_ns: i64,
	/// Per-tick step counter: step order is identical every tick, so it doubles as node id.
	idx: usize,
}

impl Tape {
	/// What the buffer keeps: the newest `capacity / 2` ticks whole, plus every `stride`-th tick over
	/// everything before them. So the freshest stretch is still tick-exact while the run stays
	/// walkable end to end — dropping the front instead, as a plain ring does, makes the beginning
	/// of a long recording unreachable for the rest of the run.
	///
	/// `stride` only ever doubles, so each pass keeps a subset of what the last one did, and each
	/// leaves a quarter of the buffer free — one O(capacity) pass per `capacity / 4` ticks.
	fn thin(&mut self) {
		while self.opened / self.stride > self.capacity / 4 {
			self.stride *= 2;
		}
		let whole = self.opened.saturating_sub(self.capacity / 2);
		let stride = self.stride;
		self.ticks.retain(|t| t.abs >= whole || t.abs % stride == 0);
	}

	/// Last addressable cursor. `at` opens a tick before the graph sweeps it, so until the recording
	/// is sealed the newest tick is still being written and is not a frame anyone may see.
	fn head(&self) -> usize {
		if self.sealed { self.opened } else { self.opened.saturating_sub(1) }
	}

	/// Last addressable *position*, `None` while the newest tick is the only one and still open.
	fn last(&self) -> Option<usize> {
		let n = if self.sealed { self.ticks.len() } else { self.ticks.len().saturating_sub(1) };
		n.checked_sub(1)
	}

	/// Position the cursor names — the nearest retained tick at or below it, since thinning drops
	/// ticks out from under a parked cursor.
	fn pos(&self) -> Option<usize> {
		let last = self.last()?;
		Some(self.ticks.partition_point(|t| t.abs < self.cursor).saturating_sub(1).min(last))
	}

	/// Parks the cursor on retained position `p`, clamped to what is addressable.
	fn park(&mut self, p: usize) {
		let Some(last) = self.last() else { return };
		self.cursor = self.ticks[p.min(last)].abs + 1;
	}

	/// Empty until the first tick closes: `Observer::on` grows this one node at a time *within* a
	/// tick, and a client topo-sorting a prefix would find a node whose deps aren't there yet.
	pub(crate) fn topology(&self) -> Vec<TopoNode> {
		if self.head() < 1 { Vec::new() } else { self.topology.clone() }
	}

	pub(crate) fn day(&self) -> DayOut {
		// A buffer's series is its source's, element for element — charting it would draw every
		// buffered pane twice. Consumers' deps are rerouted onto the source, so the client's depth
		// pass ranks a graph whose every name it can resolve.
		let src_of: HashMap<&str, &str> = self
			.series
			.iter()
			.filter(|s| s.node.starts_with("Buffer<"))
			.map(|s| (s.node.as_str(), s.deps.first().expect("a buffer has one dep").as_str()))
			.collect();
		// A typo in `price_node` would otherwise just quietly draw no candles; and the chart reads
		// the node it names positionally, as o·h·l·c·v.
		if let Some(p) = &self.price_node {
			match self.series.iter().find(|s| &s.node == p) {
				Some(s) => assert_eq!(s.dims.iter().product::<usize>(), 5, "price_node `{p}` must be an OHLCV bar"),
				// within the first tick the node may simply not have been stepped yet
				None => assert!(self.head() < 1, "price_node `{p}` names no node in the graph"),
			}
		}
		DayOut {
			series: self
				.series
				.iter()
				.filter(|s| !src_of.contains_key(s.node.as_str()))
				.map(|s| SeriesOut {
					deps: s.deps.iter().map(|d| src_of.get(d.as_str()).map_or(d.as_str(), |s| *s).to_string()).collect(),
					..s.clone()
				})
				.collect(),
			price_node: self.price_node.clone(),
		}
	}

	pub(crate) fn frame(&self) -> ActivationFrame {
		let tick = self.pos().map(|p| (p, &self.ticks[p]));
		ActivationFrame {
			tick: tick.map_or(0, |(_, t)| t.abs + 1),
			total: self.head(),
			sealed: self.sealed,
			pending: false,
			ts_ns: tick.map_or(0, |(_, t)| t.ts_ns),
			activations: tick.map_or_else(Vec::new, |(p, t)| {
				t.acts
					.iter()
					.zip(&self.topology)
					.enumerate()
					.map(|(i, (a, n))| {
						// A quiet node still holds its last value: show it, `fired` is what says it's live.
						let held = if a.vals.is_some() { a } else { self.held(i, p).unwrap_or(a) };
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

	/// Node `i`'s newest act at or before position `p` that fired; `None` if it never has. Searched
	/// rather than stamped, so what a scrubbed frame carries forward is a value the tape can still
	/// show — a remembered tick number would outlive the tick a thinning pass dropped.
	/// ponytail: linear scan, bounded by `capacity`; index the fires per node if a frame ever costs
	/// enough to feel.
	fn held(&self, i: usize, p: usize) -> Option<&Act> {
		self.ticks.iter().take(p + 1).rev().find_map(|t| t.acts.get(i).filter(|a| a.vals.is_some()))
	}

	/// `n` retained ticks on, not `n` absolute ones: in a thinned stretch the ticks between two
	/// retained ones are gone, and stepping over them would stall the cursor instead of moving it.
	pub(crate) fn step(&mut self, n: usize) -> ActivationFrame {
		let target = self.pos().map_or(0, |p| p.saturating_add(n));
		self.park(target);
		let mut f = self.frame();
		f.pending = !self.sealed && self.last().is_none_or(|l| target > l);
		f
	}

	pub(crate) fn seek(&mut self, tick: usize) -> ActivationFrame {
		self.park(self.ticks.partition_point(|t| t.abs < tick).saturating_sub(1));
		let mut f = self.frame();
		f.pending = !self.sealed && tick > self.head();
		f
	}

	/// Parks on the newest tick at or before `ts_ns` — what a click on the chart's time axis names.
	/// ponytail: linear, like [`Tape::held`]; a feed's tick timestamps are near-sorted but not
	/// guaranteed so, and a binary search would land off-by-a-batch on the ones that weave.
	pub(crate) fn seek_ts(&mut self, ts_ns: i64) -> ActivationFrame {
		self.park(self.ticks.iter().rposition(|t| t.ts_ns <= ts_ns).unwrap_or(0));
		let mut f = self.frame();
		f.pending = !self.sealed && self.ticks.back().is_some_and(|t| t.ts_ns < ts_ns);
		f
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
		let mut found = false;
		if let (Some(mut p), Some(last)) = (self.pos(), self.last()) {
			while p < last {
				p += 1;
				if hit(&self.ticks[p]) {
					found = true;
					break;
				}
			}
			self.park(p);
		}
		let mut f = self.frame();
		f.pending = !self.sealed && !found;
		f
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
			solo: p.solo,
			bars: p.bars,
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

/// Drops module paths at every depth, so a card reads as the types it names:
/// `Buffer<spl::nodes::Bar1m, dag::Horizon::Span(v_utils::primitives::timeframe::Timeframe(180000))>`
/// → `Buffer<Bar1m, Horizon::Span(Timeframe(180000))>`. A segment is a module iff it starts
/// lowercase — Rust's own convention, and the only thing telling `nodes::` from `Horizon::`, whose
/// variant would otherwise be stranded as a bare `Span(..)`. `type_name` strings are build-local,
/// display-only.
fn trim(name: &str) -> String {
	let mut out = String::with_capacity(name.len());
	// Start of the segment being accumulated: `::` rewinds to it when what precedes is a module.
	let mut seg = 0;
	let mut rest = name;
	while let Some(c) = rest.chars().next() {
		if let Some(after) = rest.strip_prefix("::") {
			match out[seg..].starts_with(|c: char| c.is_lowercase() || c == '_') {
				true => out.truncate(seg),
				false => {
					out.push_str("::");
					seg = out.len();
				}
			}
			rest = after;
			continue;
		}
		out.push(c);
		if !(c.is_alphanumeric() || c == '_') {
			seg = out.len();
		}
		rest = &rest[c.len_utf8()..];
	}
	out
}

#[cfg(test)]
mod tests {
	use trading_data_dag::Plot;

	use super::*;

	fn fire(vals: &[f64]) -> Fire<'_> {
		Fire {
			debug: &"",
			glance: &f64::NAN,
			dims: &[1],
			plots: &[Plot::DEFAULT],
			fires: 1,
			vals: Some(vals),
			dep_dims: &[],
			jac: None,
			exact_jac: None,
			formula: None,
			deriv: None,
			trace: None,
		}
	}

	#[test]
	fn a_backwards_tick_leaves_the_series_ascending() {
		let mut viz = Viz::new(None, 8, 60_000);
		for min in [2, 3, 2, 4] {
			let ts_ns = min * 60 * 1_000_000_000;
			viz.at(ts_ns).on("N", &[], &[], fire(&[min as f64]));
		}
		let day = viz.lock().day();
		let ts: Vec<i64> = day.series[0].points.iter().map(|p| p.ts_ms).collect();
		assert!(ts.windows(2).all(|w| w[0] < w[1]), "{ts:?}");
	}

	/// A run many times the capacity is still walkable end to end — the bug this replaced dropped the
	/// buffer's front, which left `seek(0)` landing wherever eviction happened to have reached.
	#[test]
	fn the_whole_run_stays_walkable_past_the_capacity() {
		let mut viz = Viz::new(None, 64, 60_000);
		for i in 0..5000 {
			viz.at(i * 60 * 1_000_000_000).on("N", &[], &[], fire(&[i as f64]));
		}
		viz.clone().seal();
		let mut t = viz.lock();
		assert_eq!(t.seek(0).tick, 1, "the recording's first tick is addressable");
		let mut walk = vec![t.frame().tick];
		loop {
			let tick = t.step(1).tick;
			if tick == *walk.last().expect("seeded") {
				break;
			}
			walk.push(tick);
		}
		assert_eq!(*walk.last().expect("seeded"), 5000, "and so is its last: {walk:?}");
		assert!(walk.windows(2).all(|w| w[0] < w[1]), "no step stands still: {walk:?}");
		// The freshest stretch is kept whole, so stepping through it moves one tick at a time.
		assert!(walk.windows(2).rev().take(8).all(|w| w[1] - w[0] == 1), "{walk:?}");
	}
}
