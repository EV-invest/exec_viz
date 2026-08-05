//! The attach surface: [`Recorder`] is the [`Observer`] the app hands to its own `tick_obs`, and
//! [`Viz`] is the shared read side the server scrubs. Cloning a [`Viz`] shares the tape.
//!
//! Live-first, so the recording *is* the storage: a live run can't be re-run, which is what the
//! old replay-by-rewinding-the-graph model assumed. Ticks land in a bounded buffer that thins with
//! age rather than forgetting its front; the per-node series the chart draws is downsampled online
//! and never dropped.
//!
//! The two halves do not share a thread. The graph's leg of a fire is one `Glance` appended to a
//! recycled column plus one channel push; naming the topology, bucketing the series, thinning and
//! the cost statistics all happen on the tape thread, off the trading core.

use std::{
	collections::{HashMap, VecDeque},
	fmt::Write as _,
	sync::{
		Arc, Mutex, MutexGuard,
		atomic::{AtomicUsize, Ordering},
		mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
	},
	thread::JoinHandle,
	time::Instant,
};

use trading_data_dag::{Fire, Ink, Observer, Plot, Want};

use crate::{
	api_types::{Activation, ActivationFrame, DayOut, GuideOut, InkOut, PlotOut, PointOut, SeriesOut, TopoNode},
	cost::{Clock, Cost, TICK_STRIDE},
};

/// Ticks the tape thread may lag by before the handoff is felt. Deep enough that a thinning pass —
/// the one O(capacity) thing the consumer does — is absorbed rather than reflected back at the feed.
const QUEUE: usize = 1024;

/// What a full handoff queue means for the tick being recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backpressure {
	/// Wait for room. A replay wants the whole tape and its feed is a file that is not going
	/// anywhere.
	Block,
	/// Drop the tick and count it. A live fill must never wait on a study aid — and a drop that
	/// nobody counted is indistinguishable from a quiet market, so [`ActivationFrame::dropped`]
	/// carries the tally.
	Drop,
}

#[derive(Clone)]
pub struct Viz(Arc<Mutex<Tape>>);

impl Viz {
	/// `price_node` names an OHLCV node — its recorded series *is* the candle pane, and it is skipped
	/// in the indicator panes so it draws once; `None` = no price pane. Pass the node's own
	/// [`Cell::NAME`](trading_data_dag::Cell::NAME) rather than a literal: a name is a `&'static str`
	/// the graph already holds, and a hand-spelled one that matches nothing draws an empty chart.
	/// `capacity` bounds the retained tick count — see [`Tape::thin`] for what a run longer than that
	/// keeps — and `bucket_ms` is the chart's sample period.
	///
	/// Spawns the tape thread and hands back the read side and the one write side: a tape has many
	/// readers and exactly one recorder, which is why they are different types.
	pub fn new(price_node: Option<&'static str>, capacity: usize, bucket_ms: i64, mode: Backpressure) -> (Self, Recorder) {
		assert!(capacity > 3 && bucket_ms > 0);
		let dropped = Arc::new(AtomicUsize::new(0));
		let viz = Self(Arc::new(Mutex::new(Tape {
			price_node: price_node.map(str::to_string),
			capacity,
			bucket_ms,
			topology: Vec::new(),
			cost: Vec::new(),
			ticks: VecDeque::new(),
			opened: 0,
			stride: 1,
			sealed: false,
			dropped: dropped.clone(),
			series: Vec::new(),
			cursor: 0,
		})));
		let (tx, rx) = sync_channel(QUEUE);
		// A thinning pass keeps the newest half whole, so it can never free more than `capacity / 2` at
		// once and a return channel that deep takes any one pass entire. At [`QUEUE`] it took a seventh
		// of one, and the recorder allocated a fresh tick's columns for six ticks in every seven.
		let (back, recycle) = sync_channel(capacity / 2);
		let tape = viz.clone();
		let join = std::thread::Builder::new()
			.name("exec_viz tape".into())
			.spawn(move || {
				for msg in rx {
					let freed = tape.lock().absorb(msg);
					// A full return channel is the recorder saying it is not allocating fast enough to
					// want them back; dropping them here is exactly what it would have done.
					freed.into_iter().try_for_each(|acts| back.try_send(acts)).ok();
				}
				tape.lock().sealed = true;
			})
			.expect("spawn the tape thread");
		(
			viz,
			Recorder {
				tx,
				recycle,
				join,
				mode,
				raw: Vec::new(),
				meta: None,
				acts: Acts::default(),
				spans: Vec::new(),
				clock: Clock::new(),
				opened: 0,
				dropped,
				ts_ns: 0,
				idx: 0,
				timed: false,
			},
		)
	}

	pub(crate) fn lock(&self) -> MutexGuard<'_, Tape> {
		// Served concurrently with the recording it describes: a panicking handler must not cost the
		// run its tape. Every op leaves the tape consistent, so the inner value is still readable.
		self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

/// The write side, and the only one: the graph thread's whole share of the recording. Not `Clone` —
/// it owns the handoff to the tape thread and the buffers being recycled across it.
pub struct Recorder {
	tx: SyncSender<TickMsg>,
	/// Drained columns coming back from the tape with their capacities intact, which is what makes a
	/// fire's rendering a memcpy rather than an allocation.
	recycle: Receiver<Acts>,
	join: JoinHandle<()>,
	mode: Backpressure,
	/// The stepped nodes' names, positional. The per-fire step-order check compares against this, and
	/// its length is what says a node is new and still owes its [`Meta`].
	raw: Vec<&'static str>,
	/// Nodes stepped for the first time, held until the tick they appeared on is sent.
	meta: Option<Vec<Meta>>,
	acts: Acts,
	spans: Vec<f64>,
	/// Step-order span clock — see [`Cost`].
	clock: Clock,
	opened: usize,
	/// Shared with the tape rather than sent with a tick: a burst of drops can run to the end of the
	/// run, and then there is no next tick to carry the tally on.
	dropped: Arc<AtomicUsize>,
	ts_ns: i64,
	/// Per-tick step counter: step order is identical every tick, so it doubles as node id.
	idx: usize,
	timed: bool,
}

impl Recorder {
	/// Opens a tick and hands back the observer for it:
	/// `graph.tick_obs(ts, batches, &mut recorder.at(ts))`. Dropping the returned [`Rec`] — at the end
	/// of that statement — is what hands the finished tick to the tape thread.
	pub fn at(&mut self, ts_ns: i64) -> Rec<'_> {
		self.ts_ns = ts_ns;
		self.idx = 0;
		self.timed = self.opened % TICK_STRIDE == 0;
		self.opened += 1;
		// Tested before it is cleared: an empty buffer is one `Drop` handed off, and asking the recycle
		// channel for a replacement it never took back drains it for nothing every tick.
		if self.acts.ends.is_empty() {
			self.acts = self.recycle.try_recv().unwrap_or_default();
		}
		self.acts.clear();
		if self.timed {
			self.spans.clear();
			// the sweep has not started, so the span the first node will close begins here.
			self.clock.mark(Instant::now());
		}
		Rec(self)
	}

	/// Ends the recording: every tick still in flight is absorbed, the last one becomes addressable
	/// and `total` stops growing. By value, so the handle you recorded through is spent. A live feed
	/// never calls this — dropping the recorder says the same thing without the wait.
	pub fn seal(self) {
		let Recorder { tx, join, .. } = self;
		drop(tx);
		join.join().expect("the tape thread only ends by running out of ticks");
	}
}

/// One opened tick's observer. Everything it writes goes into the recorder's recycled buffers; the
/// tick crosses to the tape thread when this drops.
pub struct Rec<'a>(&'a mut Recorder);

impl Observer for Rec<'_> {
	/// A Jacobian is read by exactly one thing — [`Tape::frame`], for the single tick a client is
	/// parked on — and [`Tape::thin`] will drop most of a long run, so the cheap answer would be to
	/// ask for one only on a tick that survives. It cannot be known here: at record time every tick
	/// is still in the whole-kept tail, and the strides [`Tape::thin`] will grow through depend on how
	/// much longer the run goes. Guessing costs fidelity on a tick a client can still seek to.
	fn want(&self) -> Want {
		Want::Jac
	}

	fn on(&mut self, node: &'static str, deps: &'static [&'static str], gates: &'static [bool], fire: Fire<'_>) {
		let entry = self.0.timed.then(Instant::now);
		let r = &mut *self.0;
		let i = r.idx;
		r.idx += 1;
		if r.raw.len() == i {
			r.raw.push(node);
			r.meta.get_or_insert_default().push(Meta {
				node,
				deps,
				gates,
				dims: fire.dims,
				plots: fire.plots,
				clock_ms: fire.clock.map(|tf| tf.duration().as_millis() as i64),
			});
		} else {
			assert_eq!(r.raw[i], node, "step order shifted between ticks");
		}
		// closed *before* this fire is recorded, so what it prices is the node's step and not the tape.
		if let Some(entry) = entry {
			let ns = r.clock.mark(entry);
			r.spans.push(ns);
		}

		// A node's columns growing is what says it fired, so both have to grow by something. Every
		// `Flat` in the tree has `LEN >= 1` and a Jacobian that ran is `out_len × dep_len` of nodes
		// that each flatten to at least one slot — stated here once rather than stored 840,000 times.
		assert!(fire.dims.iter().product::<usize>() > 0, "{node} flattens to nothing");
		assert!(fire.jac.is_none_or(|j| !j.is_empty()), "{node}'s Jacobian ran and came back empty");
		if let Some(vals) = fire.vals {
			write!(r.acts.outs, "{}", fire.glance).expect("`String`'s `Write` is infallible");
			r.acts.vals.extend_from_slice(vals);
		}
		if let Some(jac) = fire.jac {
			r.acts.jac.extend_from_slice(jac);
		}
		r.acts.close();

		if r.timed {
			r.clock.mark(Instant::now());
		}
	}
}

impl Drop for Rec<'_> {
	fn drop(&mut self) {
		// An unwinding sweep left a half-recorded tick, and the assert below would trade the panic
		// that explains it for a double one out of this destructor.
		if std::thread::panicking() {
			return;
		}
		let r = &mut *self.0;
		assert_eq!(r.idx, r.acts.ends.len(), "every node reports on every tick");
		let msg = TickMsg {
			ts_ns: r.ts_ns,
			acts: std::mem::take(&mut r.acts),
			meta: r.meta.take(),
			spans: r.timed.then(|| r.spans.clone()),
		};
		let gone = "the tape thread outlives every recorder but a sealed one";
		match r.mode {
			Backpressure::Block => r.tx.send(msg).expect(gone),
			Backpressure::Drop => match r.tx.try_send(msg) {
				Ok(()) => {}
				// Kept, not discarded: the buffers are this tick's to reuse on the next one, and the
				// meta is the only copy of a node's identity there will ever be.
				Err(TrySendError::Full(msg)) => {
					r.dropped.fetch_add(1, Ordering::Relaxed);
					r.acts = msg.acts;
					r.meta = msg.meta;
				}
				Err(TrySendError::Disconnected(_)) => panic!("{gone}"),
			},
		}
	}
}

/// A node's identity, sent the one tick it first steps on. Every field is `&'static`, so what
/// crosses the channel is pointers and the naming happens on the tape thread.
struct Meta {
	node: &'static str,
	deps: &'static [&'static str],
	gates: &'static [bool],
	dims: &'static [usize],
	plots: &'static [Plot],
	clock_ms: Option<i64>,
}

struct TickMsg {
	ts_ns: i64,
	acts: Acts,
	/// `None` once the graph is whole, which after the first tick it is.
	meta: Option<Vec<Meta>>,
	/// Per-node step spans, present only on the clocked ticks — see [`crate::cost`].
	spans: Option<Vec<f64>>,
}

/// One tick's acts. Node identity lives once in `topology`; a tick keeps only what varies, and it
/// keeps it by column: a per-node `String` and two `Vec`s were three heap pointers each and the tape
/// holds `capacity` ticks of them. Step order is fixed and asserted, so the columns are appended
/// front to back and a node is a set of ends into each.
#[derive(Default)]
struct Acts {
	outs: String,
	vals: Vec<f64>,
	jac: Vec<f64>,
	/// Positional with `topology`.
	ends: Vec<Ends>,
}
impl Acts {
	fn get(&self, i: usize) -> Option<ActRef<'_>> {
		let end = *self.ends.get(i)?;
		let start = if i == 0 { Ends { out: 0, vals: 0, jac: 0 } } else { self.ends[i - 1] };
		let span = |a: u32, b: u32| (b > a).then_some(a as usize..b as usize);
		Some(ActRef {
			out: &self.outs[start.out as usize..end.out as usize],
			vals: span(start.vals, end.vals).map(|r| &self.vals[r]),
			jac: span(start.jac, end.jac).map(|r| &self.jac[r]),
		})
	}

	/// Closes the node being appended. The ends are asserted rather than widened: a tick that
	/// overflows one is a graph that changed shape, not a tape that should quietly keep going.
	fn close(&mut self) {
		let end = |n: usize| u32::try_from(n).expect("one tick's columns are far under 4G");
		self.ends.push(Ends {
			out: end(self.outs.len()),
			vals: end(self.vals.len()),
			jac: end(self.jac.len()),
		});
	}

	fn clear(&mut self) {
		self.outs.clear();
		self.vals.clear();
		self.jac.clear();
		self.ends.clear();
	}
}

#[derive(Clone, Copy)]
struct Ends {
	out: u32,
	vals: u32,
	jac: u32,
}

/// One node's slice of a tick. `vals: None` is the unfired reading — a fired node's columns always
/// grow, which is the invariant [`Rec::on`] asserts so this does not have to store a flag.
#[derive(Clone, Copy)]
struct ActRef<'a> {
	out: &'a str,
	vals: Option<&'a [f64]>,
	jac: Option<&'a [f64]>,
}

struct Tick {
	/// Index among *all* ticks ever opened, so a tick's id survives every thinning pass.
	abs: usize,
	ts_ns: i64,
	acts: Acts,
}

pub(crate) struct Tape {
	price_node: Option<String>,
	capacity: usize,
	bucket_ms: i64,
	topology: Vec<TopoNode>,
	/// `topology`'s per-node step-cost estimates, positional with it.
	cost: Vec<Cost>,
	/// Ascending by `abs`, fewer than `capacity` of them — see [`Tape::thin`].
	ticks: VecDeque<Tick>,
	/// Ticks ever absorbed, thinned-away ones included.
	opened: usize,
	/// Spacing of the retained ticks outside the whole-kept tail; a power of two.
	stride: usize,
	/// The recording is over — see [`Tape::head`].
	sealed: bool,
	/// The recorder's drop tally, shared — see [`Recorder::dropped`].
	dropped: Arc<AtomicUsize>,
	series: Vec<SeriesOut>,
	/// Ticks consumed; the frame describes the one that consumed `cursor - 1`. Absolute, so a
	/// thinning pass under a parked cursor cannot slide it.
	cursor: usize,
}

impl Tape {
	/// Takes one finished tick onto the tape, and hands back the act buffers a thinning pass freed.
	fn absorb(&mut self, msg: TickMsg) -> Vec<Acts> {
		let TickMsg { ts_ns, acts, meta, spans } = msg;
		for m in meta.into_iter().flatten() {
			// names rather than the positional flags: on the wire `gates` is a subset of `deps`, which is
			// what both readers test membership against.
			let gates: Vec<String> = m.deps.iter().zip(m.gates).filter(|(_, g)| **g).map(|(d, _)| trim(d)).collect();
			let deps: Vec<String> = m.deps.iter().map(|d| buffered(&trim(d), &self.topology)).collect();
			let topo = TopoNode {
				node: trim(m.node),
				deps,
				gates,
				dims: m.dims.to_vec(),
				plots: m.plots.iter().map(PlotOut::from).collect(),
				cost_ns: None,
			};
			self.series.push(SeriesOut {
				node: topo.node.clone(),
				deps: topo.deps.clone(),
				gates: topo.gates.clone(),
				dims: topo.dims.clone(),
				plots: topo.plots.clone(),
				points: Vec::new(),
				clock_ms: m.clock_ms,
			});
			self.topology.push(topo);
			self.cost.push(Cost::default());
		}
		if let Some(spans) = spans {
			assert_eq!(spans.len(), self.cost.len(), "a clocked tick clocks every node in it");
			for (c, ns) in self.cost.iter_mut().zip(spans) {
				c.sample(ns);
			}
		}

		let ms = ts_ns / 1_000_000;
		let bucket = ms - ms.rem_euclid(self.bucket_ms);
		for (i, s) in self.series.iter_mut().enumerate() {
			let Some(vals) = acts.get(i).and_then(|a| a.vals) else { continue };
			match s.points.last_mut() {
				// `>=`, not `==`: a feed's timestamps do go backwards (a coarse lane landing on an exact
				// hour boundary weaves ahead of the tape around it), and one non-ascending point makes
				// lightweight-charts drop *every* series it holds.
				Some(p) if p.ts_ms >= bucket => {
					p.vals.clear();
					p.vals.extend_from_slice(vals);
				}
				_ => s.points.push(PointOut { ts_ms: bucket, vals: vals.to_vec() }),
			}
		}

		let freed = if self.ticks.len() == self.capacity { self.thin() } else { Vec::new() };
		debug_assert!(self.ticks.len() < self.capacity);
		let abs = self.opened;
		self.opened += 1;
		self.ticks.push_back(Tick { abs, ts_ns, acts });
		freed
	}

	/// What the buffer keeps: the newest `capacity / 2` ticks whole, plus every `stride`-th tick over
	/// everything before them. So the freshest stretch is still tick-exact while the run stays
	/// walkable end to end — dropping the front instead, as a plain ring does, makes the beginning
	/// of a long recording unreachable for the rest of the run.
	///
	/// `stride` only ever doubles, so each pass keeps a subset of what the last one did, and each
	/// leaves a quarter of the buffer free — one O(capacity) pass per `capacity / 4` ticks.
	///
	/// The dropped ticks' act buffers go back to the recorder rather than being freed here.
	fn thin(&mut self) -> Vec<Acts> {
		while self.opened / self.stride > self.capacity / 4 {
			self.stride *= 2;
		}
		let whole = self.opened.saturating_sub(self.capacity / 2);
		let stride = self.stride;
		let mut freed = Vec::new();
		let mut kept = VecDeque::with_capacity(self.ticks.len());
		for t in self.ticks.drain(..) {
			match t.abs >= whole || t.abs % stride == 0 {
				true => kept.push_back(t),
				false => freed.push(t.acts),
			}
		}
		self.ticks = kept;
		freed
	}

	/// Last addressable cursor. Until the recording is sealed the recorder is still filling a tick
	/// past the newest absorbed one, so the head is held one back rather than flickering.
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

	/// Empty until the first tick is addressable, so what a client topo-sorts is a graph with a frame
	/// behind it.
	pub(crate) fn topology(&self) -> Vec<TopoNode> {
		if self.head() < 1 {
			return Vec::new();
		}
		// stamped on the way out, not stored: the cost is a live estimate and `topology` is the one
		// thing on the tape that does not change after the first tick.
		self.topology.iter().zip(&self.cost).map(|(n, c)| TopoNode { cost_ns: c.ns(), ..n.clone() }).collect()
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
			dropped: self.dropped.load(Ordering::Relaxed),
			pending: false,
			ts_ns: tick.map_or(0, |(_, t)| t.ts_ns),
			activations: tick.map_or_else(Vec::new, |(p, t)| {
				self.topology
					.iter()
					.enumerate()
					.filter_map(|(i, n)| {
						let a = t.acts.get(i)?;
						// A quiet node still holds its last value: show it, `fired` is what says it's live.
						let held = if a.vals.is_some() { a } else { self.held(i, p).unwrap_or(a) };
						Some(Activation {
							node: n.node.clone(),
							deps: n.deps.clone(),
							gates: n.gates.clone(),
							out: held.out.to_string(),
							fired: a.vals.is_some(),
							dims: n.dims.clone(),
							vals: held.vals.map(|v| v.iter().map(|x| x.is_finite().then_some(*x)).collect()),
							jac: a.jac.map(|j| j.iter().map(|w| (!w.is_nan()).then_some(*w)).collect()),
							cost_ns: self.cost[i].ns(),
						})
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
	fn held(&self, i: usize, p: usize) -> Option<ActRef<'_>> {
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
				.any(|(&i, was)| t.acts.get(i).is_some_and(|a| a.vals.is_some() && was.as_deref() != Some(a.out)))
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
			labels: p.labels.iter().map(|axis| axis.iter().map(|l| l.to_string()).collect()).collect(),
			inks: p.inks.iter().map(ink).collect(),
			overlay: p.overlay,
			solo: p.solo,
			bars: p.bars,
			candles: p.candles,
		}
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
			glance: &f64::NAN,
			dims: &[1],
			plots: &[Plot::DEFAULT],
			clock: None,
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
		let (viz, mut rec) = Viz::new(None, 8, 60_000, Backpressure::Block);
		for min in [2, 3, 2, 4] {
			let ts_ns = min * 60 * 1_000_000_000;
			rec.at(ts_ns).on("N", &[], &[], fire(&[min as f64]));
		}
		rec.seal();
		let day = viz.lock().day();
		let ts: Vec<i64> = day.series[0].points.iter().map(|p| p.ts_ms).collect();
		assert!(ts.windows(2).all(|w| w[0] < w[1]), "{ts:?}");
	}

	/// What the observer times is the span *before* it was called — the next node's step — and not the
	/// recording it just did. A recorder charging its own work forward would read every node alike.
	#[test]
	fn a_slow_step_is_charged_to_the_node_that_took_it() {
		let (viz, mut rec) = Viz::new(None, 4096, 60_000, Backpressure::Block);
		// a few blocks' worth of clocked ticks, which is what it takes for an estimate to exist at all.
		for i in 0..TICK_STRIDE * 32 {
			let mut r = rec.at(i as i64 * 60 * 1_000_000_000);
			r.on("Fast", &[], &[], fire(&[0.0]));
			std::thread::sleep(std::time::Duration::from_micros(100));
			r.on("Slow", &["Fast"], &[false], fire(&[1.0]));
		}
		rec.seal();
		let cost: Vec<f64> = viz.lock().topology().iter().map(|n| n.cost_ns.expect("blocks closed")).collect();
		assert!(cost[1] > 50_000.0, "the slept step reads as {}ns", cost[1]);
		assert!(cost[0] * 4.0 < cost[1], "the fast step reads as {}ns next to {}ns", cost[0], cost[1]);
	}

	/// A tick the handoff had no room for is one the tape will never hold, so the only thing that can
	/// keep it from reading as a quiet market is the count — which every landing tick carries, so a
	/// burst of drops is still reported by whatever follows it.
	#[test]
	fn a_dropped_tick_is_counted_rather_than_lost() {
		let (viz, mut rec) = Viz::new(None, 8, 60_000, Backpressure::Drop);
		let stepped = QUEUE + 64;
		// The tape thread cannot absorb what it is blocked on, so the queue fills and stays full.
		let held = viz.lock();
		for i in 0..stepped as i64 {
			rec.at(i * 60 * 1_000_000_000).on("N", &[], &[], fire(&[i as f64]));
		}
		drop(held);
		rec.seal();
		let f = viz.lock().frame();
		assert!(f.dropped > 0, "the queue never filled, so this measured nothing");
		assert_eq!(f.dropped + f.total, stepped, "every stepped tick is either on the tape or counted");
	}

	/// A run many times the capacity is still walkable end to end — the bug this replaced dropped the
	/// buffer's front, which left `seek(0)` landing wherever eviction happened to have reached.
	#[test]
	fn the_whole_run_stays_walkable_past_the_capacity() {
		let (viz, mut rec) = Viz::new(None, 64, 60_000, Backpressure::Block);
		for i in 0..5000 {
			rec.at(i * 60 * 1_000_000_000).on("N", &[], &[], fire(&[i as f64]));
		}
		rec.seal();
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
