//! The attach surface, in three types because there are three roles. [`Tape`] is the owner — one per
//! recording, not `Clone`, and what ends the recording and writes it down. [`Recorder`] is the
//! [`Observer`] the app hands to its own `tick_obs`, and the only write side. [`Viz`] is a read side,
//! shared: cloning one shares the tape, and none of them can end or save it.
//!
//! Live-first, so the recording *is* the storage: a live run can't be re-run, which is what the
//! old replay-by-rewinding-the-graph model assumed. Ticks land in a bounded buffer that thins with
//! age rather than forgetting its front; the per-node series the chart draws is downsampled online
//! and never dropped.
//!
//! The two halves do not share a thread. The graph's leg of a fire is one `Glance` appended to a
//! recycled column plus one channel push; naming the topology, bucketing the series, thinning and
//! the cost statistics all happen on the tape thread, off the trading core.
//!
//! Storage, though, need not be memory: [`Tape::save`] writes the tape down and [`Viz::load`] reads
//! one back, so a run can be looked at after the process that recorded it is gone. `load` hands back
//! a [`Viz`] and not a [`Tape`], because a file has no recording to end.

use std::{
	collections::{HashSet, VecDeque},
	fmt::Write as _,
	io::{BufReader, BufWriter, Read as _, Write as _},
	path::Path,
	sync::{
		Arc, Condvar, Mutex, MutexGuard,
		atomic::{AtomicUsize, Ordering},
		mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
	},
	thread::JoinHandle,
	time::Instant,
};

use serde::{Deserialize, Serialize};
use trading_data_dag::{Fidelity, Fire, Ink, Observer, Plot, Want};

use crate::{
	api_types::{Activation, ActivationFrame, DayOut, ExactBlock, FidelityOut, GuideOut, InkOut, PlotOut, PointOut, SeriesOut, TopoNode},
	cost::{Clock, Cost, TICK_STRIDE},
};

/// Ticks the tape thread may lag by before the handoff is felt. Deep enough that a thinning pass —
/// the one O(capacity) thing the consumer does — is absorbed rather than reflected back at the feed.
const QUEUE: usize = 1024;

/// Ticks the tape thread absorbs per lock acquisition, at most — bounded rather than open because it
/// is also the width a reader can be made to wait behind. Clamped at [`Tape::new`] to `capacity / 4`,
/// the room a thinning pass leaves, which is what keeps at most one such pass inside any one batch —
/// the sizing the recycle channel below rests on.
const BATCH: usize = 64;

/// What a tape file opens with, so a file that is not one is refused before it is decoded.
const TAPE_MAGIC: [u8; 8] = *b"EXECVIZT";
/// The layout of everything after the header. Node names are `type_name` strings and so build-local,
/// but their arrangement is this crate's — bump on any change to [`Acts`], [`Tick`] or [`TapeFile`].
/// Tagging with the build's commit instead would orphan every tape on an unrelated one.
const TAPE_SCHEMA: u32 = 2;

/// What a full handoff queue means for the tick being recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backpressure {
	/// Wait for room. A replay wants the whole tape and its feed is a file that is not going
	/// anywhere.
	///
	/// Which is why absorption is a thread rather than a future the app drives: here the producer
	/// waiting on the consumer is the trading core, so a caller that stopped polling would stall the
	/// graph [`QUEUE`] ticks later.
	Block,
	/// Drop the tick and count it. A live fill must never wait on a study aid — and a drop that
	/// nobody counted is indistinguishable from a quiet market, so [`ActivationFrame::dropped`]
	/// carries the tally.
	Drop,
}

/// The recording's owner: one per [`Tape::new`], and deliberately not `Clone`. There is one tape,
/// one end of it, and saving spends it — which is what lets the join handle be a plain field here
/// instead of a one-shot slot every reader shares mutable access to. Readers are [`Viz`]s taken off
/// it, and a [`Viz`] can neither end the recording nor write it down.
///
/// Dropping the owner *detaches* the tape thread rather than joining it. Joining here would hang
/// whenever the recorder is still alive — and the owner cannot reach the recorder's handoff to close
/// it — so the sanctioned ends are [`Tape::save`] and [`Recorder::seal`]. A detached thread still
/// terminates on its own the moment the recorder goes.
pub struct Tape {
	viz: Viz,
	join: JoinHandle<()>,
}

impl Tape {
	/// `price_node` names an OHLCV node — its recorded series *is* the candle pane, and it is skipped
	/// in the indicator panes so it draws once; `None` = no price pane. Pass the node's own
	/// [`Cell::NAME`](trading_data_dag::Cell::NAME) rather than a literal: a name is a `&'static str`
	/// the graph already holds, and a hand-spelled one that matches nothing draws an empty chart.
	/// `capacity` bounds the retained tick count — see [`Inner::thin`] for what a run longer than that
	/// keeps — and `bucket_ms` is the chart's sample period.
	///
	/// Spawns the tape thread and hands back the owner and the one write side: a tape has many
	/// readers and exactly one recorder, which is why they are different types.
	pub fn new(price_node: Option<&'static str>, capacity: usize, bucket_ms: i64, mode: Backpressure) -> (Self, Recorder) {
		assert!(capacity > 3 && bucket_ms > 0);
		let dropped = Arc::new(AtomicUsize::new(0));
		let viz = Viz {
			inner: Arc::new(Mutex::new(Inner {
				price_node: price_node.map(str::to_string),
				capacity,
				bucket_ms,
				topology: Vec::new(),
				cost: Vec::new(),
				ticks: VecDeque::new(),
				opened: 0,
				fires: Vec::new(),
				keep: Vec::new(),
				fired: Vec::new(),
				sealed: false,
				dropped: dropped.clone(),
				series: Vec::new(),
				cursor: 0,
			})),
			done: Arc::new(Condvar::new()),
		};
		let (tx, rx) = sync_channel(QUEUE);
		// A thinning pass keeps the newest half whole, so it can never free more than `capacity / 2` at
		// once and a return channel that deep takes any one pass entire. At [`QUEUE`] it took a seventh
		// of one, and the recorder allocated a fresh tick's columns for six ticks in every seven.
		let (back, recycle) = sync_channel(capacity / 2);
		let tape = viz.clone();
		let batch_cap = BATCH.min(capacity / 4).max(1);
		let join = std::thread::Builder::new()
			.name("exec_viz tape".into())
			.spawn(move || {
				let mut batch = Vec::with_capacity(batch_cap);
				let mut freed = Vec::new();
				//LOOP: the recv is the only way in, and it fails once every recorder is gone.
				// Blocking, so an idle tape parks rather than spins.
				while let Ok(first) = rx.recv() {
					batch.push(first);
					// Only what is already queued, never waited for: a feed at a few ticks a second sees
					// batches of one, and the lock is amortized exactly when there is a backlog to amortize
					// it over — which is when the latency it trades away is already gone.
					batch.extend(rx.try_iter().take(batch_cap - 1));
					{
						let mut t = tape.lock();
						for msg in batch.drain(..) {
							freed.append(&mut t.absorb(msg));
						}
					}
					// A full return channel is the recorder saying it is not allocating fast enough to
					// want them back; dropping them here is exactly what it would have done.
					for acts in freed.drain(..) {
						if back.try_send(acts).is_err() {
							break;
						}
					}
				}
				tape.lock().sealed = true;
				// The end of the recording, announced rather than only joinable: a recorder has no owner
				// to reach, so this is what [`Recorder::seal`] waits on.
				tape.done.notify_all();
			})
			.expect("spawn the tape thread");
		(
			Tape { viz: viz.clone(), join },
			Recorder {
				tx,
				recycle,
				viz,
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

	/// A reader — what the server is handed, and what every replay op goes through. Cheap to clone.
	pub fn viz(&self) -> Viz {
		self.viz.clone()
	}

	/// Blocks until the recording is over — every recorder dropped or sealed, every handed-off tick
	/// absorbed — then writes the tape. What lands is what the tape held: a thinned run saves thinned,
	/// because that is the recording.
	///
	/// By value: a tape is written down once, and spending the owner is what says so.
	pub fn save(self, path: impl AsRef<Path>) -> std::io::Result<()> {
		self.viz.wait_sealed();
		// Immediate — the notify above was the thread's last act — and it is what makes "the recording
		// is over" mean the thread is done touching the tape, not merely that it set a flag.
		self.join.join().expect("the tape thread only ends by running out of ticks");
		self.viz.write(path)
	}
}

/// A read side, shared: what the server scrubs and what every replay op goes through. Cloning shares
/// the tape. It cannot end the recording or save it — that is [`Tape`]'s, and there is one of those.
#[derive(Clone)]
pub struct Viz {
	inner: Arc<Mutex<Inner>>,
	/// Signalled once, by the tape thread's last act. What [`Recorder::seal`] waits on, since the join
	/// handle belongs to the single owner and a recorder has no owner to reach.
	done: Arc<Condvar>,
}

impl Viz {
	/// What the retained ticks cost in memory, allocations included — the reading `capacity` is
	/// actually bought with. Exported because the README's advice is measured against one synthetic
	/// graph, and this is what lets it be re-measured against yours; `exec_viz`'s own `capacity`
	/// example is the first such reader.
	///
	/// The per-node chart series and the topology are not in it: neither is bounded by `capacity`.
	pub fn bytes(&self) -> usize {
		let t = self.lock();
		t.ticks.iter().map(Tick::bytes).sum()
	}

	/// Waits for the tape thread to announce the end of the recording. Idempotent, and a no-op on a
	/// tape read back from a file, which is sealed before anyone can ask.
	fn wait_sealed(&self) {
		let mut t = self.lock();
		while !t.sealed {
			t = self.done.wait(t).unwrap_or_else(std::sync::PoisonError::into_inner);
		}
	}

	/// The write half of [`Tape::save`], with the waiting already done.
	fn write(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
		let t = self.lock();
		// What is *addressable*, not what is held: a `total` naming a tick the file does not carry
		// would be a tape that reads as truncated mid-walk rather than as one that ends.
		let last = t.last();
		let file = TapeFile {
			price_node: t.price_node.clone(),
			capacity: t.capacity,
			bucket_ms: t.bucket_ms,
			topology: t.topology.clone(),
			cost: t.cost.clone(),
			ticks: t.ticks.iter().take(last.map_or(0, |l| l + 1)).cloned().collect(),
			opened: t.head(),
			fires: t.fires.clone(),
			keep: t.keep.clone(),
			dropped: t.dropped.load(Ordering::Relaxed),
			series: t.series.clone(),
			cursor: t.cursor,
		};

		let mut w = BufWriter::new(std::fs::File::create(path)?);
		w.write_all(&TAPE_MAGIC)?;
		w.write_all(&TAPE_SCHEMA.to_le_bytes())?;
		rmp_serde::encode::write(&mut w, &file).map_err(std::io::Error::other)?;
		w.flush()
	}

	/// A tape read back. Sealed by definition — a file does not grow — so there is no [`Recorder`]
	/// and every replay op is addressable from the first request.
	pub fn load(path: impl AsRef<Path>) -> std::io::Result<Self> {
		let path = path.as_ref();
		let mut r = BufReader::new(std::fs::File::open(path)?);
		let mut magic = [0u8; 8];
		r.read_exact(&mut magic)?;
		if magic != TAPE_MAGIC {
			return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{} is not an exec_viz tape", path.display())));
		}
		let mut schema = [0u8; 4];
		r.read_exact(&mut schema)?;
		let schema = u32::from_le_bytes(schema);
		if schema != TAPE_SCHEMA {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				format!("{} is a schema-{schema} tape and this build reads schema {TAPE_SCHEMA} — re-record it", path.display()),
			));
		}
		let file: TapeFile = rmp_serde::decode::from_read(&mut r).map_err(std::io::Error::other)?;
		let ticks: VecDeque<Tick> = file.ticks.into();

		Ok(Self {
			inner: Arc::new(Mutex::new(Inner {
				price_node: file.price_node,
				capacity: file.capacity,
				bucket_ms: file.bucket_ms,
				topology: file.topology,
				cost: file.cost,
				opened: file.opened,
				fires: file.fires,
				keep: file.keep,
				fired: index(&ticks),
				ticks,
				sealed: true,
				dropped: Arc::new(AtomicUsize::new(file.dropped)),
				series: file.series,
				cursor: file.cursor,
			})),
			done: Arc::new(Condvar::new()),
		})
	}

	pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
		// Served concurrently with the recording it describes: a panicking handler must not cost the
		// run its tape. Every op leaves the tape consistent, so the inner value is still readable.
		self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
	}
}

/// The write side, and the only one: the graph thread's whole share of the recording. Not `Clone` —
/// it owns the handoff to the tape thread and the buffers being recycled across it.
pub struct Recorder {
	tx: SyncSender<TickMsg>,
	/// Drained columns coming back from the tape with their capacities intact, which is what makes a
	/// fire's rendering a memcpy rather than an allocation.
	recycle: Receiver<Acts>,
	/// Only ever waited on — see [`Recorder::seal`].
	viz: Viz,
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
		let Recorder { tx, viz, .. } = self;
		drop(tx);
		viz.wait_sealed();
	}
}

/// One opened tick's observer. Everything it writes goes into the recorder's recycled buffers; the
/// tick crosses to the tape thread when this drops.
pub struct Rec<'a>(&'a mut Recorder);
/// [`Inner`]'s persisted fields. Separate rather than serialized in place because two of the tape's
/// are not data: `dropped` is shared with a recorder that no longer exists on the read-back side, and
/// `sealed` is what a file is.
#[derive(Deserialize, Serialize)]
struct TapeFile {
	price_node: Option<String>,
	capacity: usize,
	bucket_ms: i64,
	topology: Vec<TopoNode>,
	cost: Vec<Cost>,
	ticks: Vec<Tick>,
	opened: usize,
	fires: Vec<usize>,
	keep: Vec<u8>,
	dropped: usize,
	series: Vec<SeriesOut>,
	cursor: usize,
}

impl Observer for Rec<'_> {
	/// A Jacobian is read by exactly one thing — [`Inner::frame`], for the single tick a client is
	/// parked on — and [`Inner::thin`] will drop most of a long run, so the cheap answer would be to
	/// ask for one only on a tick that survives. It cannot be known here: at record time every tick
	/// is still in the whole-kept tail, and the strides [`Inner::thin`] will grow through depend on how
	/// much longer the run goes. Guessing costs fidelity on a tick a client can still seek to.
	fn want(&self, _: &'static str) -> Want {
		Want::Exact
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
				fidelity: fire.fidelity,
				deriv: fire.deriv.map(|d| d.to_string()),
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
		if let Some((block, widths)) = fire.exact_block {
			r.acts.block.extend_from_slice(block);
			r.acts.widths.extend_from_slice(widths);
		}
		r.acts.exact.push(fire.exact);
		r.acts.ran.push(fire.ran);
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
	/// Both are the kernel's, not the tick's, so they are recorded here once rather than per fire.
	fidelity: Fidelity,
	deriv: Option<String>,
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
#[derive(Clone, Default, Deserialize, Serialize)]
struct Acts {
	outs: String,
	vals: Vec<f64>,
	jac: Vec<f64>,
	block: Vec<f64>,
	widths: Vec<usize>,
	/// How this tick's Jacobian was reached, positional with `ends`. Per tick rather than per node
	/// because a node that did not fire drew nothing at all.
	exact: Vec<bool>,
	/// Whether the sweep advanced each node, positional with `ends`. Not derivable from the columns:
	/// a clocked node between publications grows none of them and was stepped all the same.
	ran: Vec<bool>,
	/// Positional with `topology`.
	ends: Vec<Ends>,
}
impl Acts {
	fn get(&self, i: usize) -> Option<ActRef<'_>> {
		let end = *self.ends.get(i)?;
		let start = if i == 0 { Ends::default() } else { self.ends[i - 1] };
		let span = |a: u32, b: u32| (b > a).then_some(a as usize..b as usize);
		Some(ActRef {
			out: &self.outs[start.out as usize..end.out as usize],
			ran: self.ran[i],
			vals: span(start.vals, end.vals).map(|r| &self.vals[r]),
			jac: span(start.jac, end.jac).map(|r| &self.jac[r]),
			block: span(start.block, end.block).map(|r| (&self.block[r], &self.widths[start.widths as usize..end.widths as usize])),
			exact: self.exact[i],
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
			block: end(self.block.len()),
			widths: end(self.widths.len()),
		});
	}

	fn clear(&mut self) {
		self.outs.clear();
		self.vals.clear();
		self.jac.clear();
		self.block.clear();
		self.widths.clear();
		self.exact.clear();
		self.ran.clear();
		self.ends.clear();
	}
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
struct Ends {
	out: u32,
	vals: u32,
	jac: u32,
	block: u32,
	widths: u32,
}

/// One node's slice of a tick. `vals: None` is the unfired reading — a fired node's columns always
/// grow, which is the invariant [`Rec::on`] asserts so this does not have to store a flag.
#[derive(Clone, Copy)]
struct ActRef<'a> {
	out: &'a str,
	ran: bool,
	vals: Option<&'a [f64]>,
	jac: Option<&'a [f64]>,
	block: Option<(&'a [f64], &'a [usize])>,
	exact: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct Tick {
	/// Index among *all* ticks ever opened, so a tick's id survives every thinning pass.
	abs: usize,
	ts_ns: i64,
	acts: Acts,
	/// What [`Inner::thin`] decides by, positional with `topology`: `0` where the node did not fire,
	/// else `1 + trailing_zeros` of its 0-based fire ordinal. So "keep every `2^k`-th fire of node
	/// `i`" is the single comparison `ranks[i] > k`, the kept sets nest as `k` grows, and a node's
	/// very first fire (ordinal 0, and the saturating rank that gives) survives every `k`.
	///
	/// It rides here and not on [`Acts`] because those buffers are the graph thread's, recycled
	/// across the handoff; the ordinal it counts is the tape's.
	ranks: Vec<u8>,
}
impl Tick {
	/// Capacities rather than lengths: the columns arrive from the recycle channel already grown, and
	/// what a tick occupies is what it holds onto.
	fn bytes(&self) -> usize {
		let a = &self.acts;
		let col = a.outs.capacity() + (a.vals.capacity() + a.jac.capacity() + a.block.capacity()) * size_of::<f64>();
		size_of::<Self>() + col + a.widths.capacity() * size_of::<usize>() + a.exact.capacity() + a.ends.capacity() * size_of::<Ends>() + self.ranks.capacity()
	}
}

pub(crate) struct Inner {
	price_node: Option<String>,
	capacity: usize,
	bucket_ms: i64,
	topology: Vec<TopoNode>,
	/// `topology`'s per-node step-cost estimates, positional with it.
	cost: Vec<Cost>,
	/// Ascending by `abs`, fewer than `capacity` of them — see [`Inner::thin`].
	ticks: VecDeque<Tick>,
	/// Ticks ever absorbed, thinned-away ones included.
	opened: usize,
	/// Per-node fire count so far, positional with `topology` — what [`Tick::ranks`] is read off.
	fires: Vec<usize>,
	/// Per-node "keep one fire in `2^keep[i]`", positional with `topology` and only ever raised —
	/// see [`Inner::thin`].
	keep: Vec<u8>,
	/// Per node, ascending, the `ticks` *positions* it fired on — what [`Inner::held`] binary-searches
	/// instead of scanning. Positions and not `abs` ids because between thinning passes `ticks` only
	/// grows at the back, so a position already recorded cannot move; [`Inner::thin`], which is the
	/// one thing that does move them, rebuilds this in the walk it was already making.
	///
	/// Derived, so it is not in [`TapeFile`] — [`Viz::load`] rebuilds it rather than trusting a file
	/// to agree with the ticks beside it.
	fired: Vec<Vec<usize>>,
	/// The recording is over — see [`Inner::head`].
	sealed: bool,
	/// The recorder's drop tally, shared — see [`Recorder::dropped`].
	dropped: Arc<AtomicUsize>,
	series: Vec<SeriesOut>,
	/// Ticks consumed; the frame describes the one that consumed `cursor - 1`. Absolute, so a
	/// thinning pass under a parked cursor cannot slide it.
	cursor: usize,
}

impl Inner {
	/// Takes one finished tick onto the tape, and hands back the act buffers a thinning pass freed.
	fn absorb(&mut self, msg: TickMsg) -> Vec<Acts> {
		let TickMsg { ts_ns, acts, meta, spans } = msg;
		for m in meta.into_iter().flatten() {
			// names rather than the positional flags: on the wire `gates` is a subset of `deps`, which is
			// what both readers test membership against.
			let gates: Vec<String> = m.deps.iter().zip(m.gates).filter(|(_, g)| **g).map(|(d, _)| trim(d)).collect();
			let deps: Vec<String> = m.deps.iter().map(|d| trim(d)).collect();
			let topo = TopoNode {
				node: trim(m.node),
				deps,
				gates,
				dims: m.dims.to_vec(),
				plots: m.plots.iter().map(PlotOut::from).collect(),
				cost_ns: None,
				fidelity: m.fidelity.into(),
				deriv: m.deriv,
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
			self.fires.push(0);
			self.keep.push(0);
		}
		if let Some(spans) = spans {
			assert_eq!(spans.len(), self.cost.len(), "a clocked tick clocks every node in it");
			for (c, ns) in self.cost.iter_mut().zip(spans) {
				c.sample(ns);
			}
		}

		let ms = ts_ns / 1_000_000;
		let bucket = ms - ms.rem_euclid(self.bucket_ms);
		let mut ranks = vec![0u8; self.series.len()];
		for (i, s) in self.series.iter_mut().enumerate() {
			let Some(vals) = acts.get(i).and_then(|a| a.vals) else { continue };
			// Saturated so that ordinal 0 — whose `trailing_zeros` is the word width — outranks every
			// `keep`, which is what keeps the front of a long recording addressable.
			ranks[i] = 1 + self.fires[i].trailing_zeros().min(63) as u8;
			self.fires[i] += 1;
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
		let p = self.ticks.len();
		// After the push, so `p` is the position the tick lands on. Appending only ever extends each
		// node's list at the back, which is what keeps them ascending without a sort.
		self.fired.resize(ranks.len(), Vec::new());
		for (i, r) in ranks.iter().enumerate() {
			if *r > 0 {
				self.fired[i].push(p);
			}
		}
		self.ticks.push_back(Tick { abs, ts_ns, acts, ranks });
		freed
	}

	/// What the buffer keeps: the newest `capacity / 2` ticks whole, plus — over everything before
	/// them — every tick some node fired on at a rank the squeeze has not yet reached. So the
	/// freshest stretch is still tick-exact while the run stays walkable end to end; dropping the
	/// front instead, as a plain ring does, makes the beginning of a long recording unreachable for
	/// the rest of the run.
	///
	/// Decimating by tick index would be proximity to the buffer rather than to the problem: what a
	/// human scrubs a tape for is the rare event, and a fixed stride destroys precisely those — a
	/// 5-minute bar firing 288 times a day survives a stride of 64 nine times. So the squeeze is
	/// max-min fair instead: raise the greediest node's exponent until the pre-tail union fits, and
	/// a node that never dominates never loses a fire.
	///
	/// `keep` only ever rises, so each pass keeps a subset of what the last one did, and each leaves
	/// a quarter of the buffer free — one O(capacity) pass per `capacity / 4` ticks.
	///
	/// The dropped ticks' act buffers go back to the recorder rather than being freed here.
	fn thin(&mut self) -> Vec<Acts> {
		let whole = self.opened.saturating_sub(self.capacity / 2);
		let pre = self.ticks.iter().take_while(|t| t.abs < whole);
		let mut counts = vec![0usize; self.keep.len()];
		// ponytail: recounts rather than tracking retained counts incrementally — it runs once per
		// `capacity / 4` ticks and past the first pass converges in zero or one iterations.
		//LOOP: each iteration halves one node's share of the backbone, so the union strictly falls.
		loop {
			counts.iter_mut().for_each(|c| *c = 0);
			let mut union = 0;
			for t in pre.clone() {
				let mut any = false;
				for (i, &r) in t.ranks.iter().enumerate() {
					if r > self.keep[i] {
						counts[i] += 1;
						any = true;
					}
				}
				union += usize::from(any);
			}
			if union <= self.capacity / 4 {
				break;
			}
			let (i, n) = counts.iter().enumerate().max_by_key(|(_, n)| **n).expect("a non-empty union came from some node");
			assert!(*n > 0, "the union counts ticks no node claims");
			self.keep[i] += 1;
		}

		let mut freed = Vec::new();
		let mut kept = VecDeque::with_capacity(self.ticks.len());
		for t in self.ticks.drain(..) {
			match t.abs >= whole || t.ranks.iter().zip(&self.keep).any(|(r, k)| r > k) {
				true => kept.push_back(t),
				false => freed.push(t.acts),
			}
		}
		self.fired = index(kept.iter());
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
		// buffered pane twice. Nothing names one in dep position (`Buffering<C, R>` forwards
		// `Cell::NAME` to `C`'s), so dropping the pane leaves no dangling edge behind.
		let buffers: HashSet<&str> = self.series.iter().map(|s| s.node.as_str()).filter(|n| n.starts_with("Buffer<")).collect();
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
			series: self.series.iter().filter(|s| !buffers.contains(s.node.as_str())).cloned().collect(),
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
			found: true,
			// At the buffer's front there is no previous retained tick, so what it stands for is
			// everything from the start of the run — which for an unthinned front is the `1` it reads as.
			gap: tick.map_or(1, |(p, t)| t.abs + 1 - p.checked_sub(1).map_or(0, |q| self.ticks[q].abs + 1)),
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
							ran: a.ran,
							fired: a.vals.is_some(),
							dims: n.dims.clone(),
							vals: held.vals.map(|v| v.iter().map(|x| x.is_finite().then_some(*x)).collect()),
							jac: a.jac.map(|j| j.iter().map(|w| (!w.is_nan()).then_some(*w)).collect()),
							exact_block: a.block.map(|(cols, widths)| ExactBlock {
								cols: cols.iter().map(|w| (!w.is_nan()).then_some(*w)).collect(),
								widths: widths.to_vec(),
							}),
							cost_ns: self.cost[i].ns(),
							exact: a.exact,
						})
					})
					.collect()
			}),
		}
	}

	/// Node `i`'s newest act at or before position `p` that fired; `None` if it never has. Searched
	/// rather than stamped, so what a scrubbed frame carries forward is a value the tape can still
	/// show — a remembered tick number would outlive the tick a thinning pass dropped.
	///
	/// Searched over [`Inner::fired`] rather than back over the ticks: a frame calls this once per
	/// quiet node, and a node that has not fired for a long stretch — a twice-a-day market cap, a
	/// classification that never triggers — walked the whole retained tape for it. That put a scrub
	/// at 139ms on a 42-node graph holding a day, growing linearly with where the cursor sat.
	fn held(&self, i: usize, p: usize) -> Option<ActRef<'_>> {
		let fired = self.fired.get(i)?;
		let q = fired[..fired.partition_point(|&q| q <= p)].last()?;
		self.ticks[*q].acts.get(i).filter(|a| a.vals.is_some())
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
	/// ponytail: linear, like [`Inner::held`]; a feed's tick timestamps are near-sorted but not
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
			// A name the graph does not carry is still a search that reached nothing — `c` pressed before
			// the run has a `Classify` reads the same to the user as one that never fires again.
			None => ActivationFrame { found: false, ..self.frame() },
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

	/// A search that reaches the end of a *sealed* recording without a hit leaves the cursor alone:
	/// there is no further Δ to walk to, and parking at `last` would answer a failed search by
	/// teleporting the user to the end of the run. While the tape is still growing `last` *is* the
	/// resume point — [`crate::api_types::ActivationFrame::pending`] says the op will be re-issued
	/// from there — so there it parks.
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
			if found || !self.sealed {
				self.park(p);
			}
		}
		let mut f = self.frame();
		f.pending = !self.sealed && !found;
		f.found = found;
		f
	}
}

impl From<Fidelity> for FidelityOut {
	fn from(f: Fidelity) -> Self {
		match f {
			Fidelity::Exact => FidelityOut::Exact,
			Fidelity::Partial(omits) => FidelityOut::Partial { omits: omits.to_string() },
			Fidelity::Opaque(why) => FidelityOut::Opaque { why: why.to_string() },
		}
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

/// Per node, the positions in `ticks` it fired on — [`Inner::fired`] built from scratch, which is
/// what both the thinning pass and a tape read back off disk need. Ascending because the ticks are.
fn index<'a>(ticks: impl IntoIterator<Item = &'a Tick>) -> Vec<Vec<usize>> {
	let mut fired: Vec<Vec<usize>> = Vec::new();
	for (p, t) in ticks.into_iter().enumerate() {
		fired.resize(fired.len().max(t.ranks.len()), Vec::new());
		for (i, r) in t.ranks.iter().enumerate() {
			if *r > 0 {
				fired[i].push(p);
			}
		}
	}
	fired
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
			fidelity: Fidelity::Exact,
			ran: true,
			fires: 1,
			vals: Some(vals),
			dep_dims: &[],
			jac: None,
			exact_block: None,
			exact: false,
			formula: None,
			deriv: None,
			trace: None,
		}
	}

	#[test]
	fn a_backwards_tick_leaves_the_series_ascending() {
		let (tape, mut rec) = Tape::new(None, 8, 60_000, Backpressure::Block);
		let viz = tape.viz();
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
		let (tape, mut rec) = Tape::new(None, 4096, 60_000, Backpressure::Block);
		let viz = tape.viz();
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
		let (tape, mut rec) = Tape::new(None, 8, 60_000, Backpressure::Drop);
		let viz = tape.viz();
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
		let (tape, mut rec) = Tape::new(None, 64, 60_000, Backpressure::Block);
		let viz = tape.viz();
		for i in 0..5000 {
			rec.at(i * 60 * 1_000_000_000).on("N", &[], &[], fire(&[i as f64]));
		}
		rec.seal();
		let walk = walk(&viz);
		assert_eq!(walk.first(), Some(&1), "the recording's first tick is addressable");
		assert_eq!(walk.last(), Some(&5000), "and so is its last: {walk:?}");
		assert!(walk.windows(2).all(|w| w[0] < w[1]), "no step stands still: {walk:?}");
		// The freshest stretch is kept whole, so stepping through it moves one tick at a time.
		assert!(walk.windows(2).rev().take(8).all(|w| w[1] - w[0] == 1), "{walk:?}");
	}

	/// The reported bug: thinning by tick index keeps ticks at fixed absolute positions, so a node
	/// that fires rarely loses its fires to the stride — spl's 5-minute bar fired 576 times over the
	/// window and 9 of them stayed addressable. Thinning by *fire* rank, the squeeze lands on the
	/// node that can afford it.
	#[test]
	fn a_rare_node_keeps_its_fires_past_the_capacity() {
		const RARE: usize = 500;
		const TICKS: usize = 200_000;
		// `B`'s whole run has to fit under the backbone ceiling for "keeps every fire" to be a thing the
		// tape can offer at all — 400 fires against 1024, the same order of headroom spl's 5-minute bar
		// has (576 fires against 5000).
		let (tape, mut rec) = Tape::new(None, 4096, 60_000, Backpressure::Block);
		let viz = tape.viz();
		for i in 0..TICKS {
			let mut r = rec.at(i as i64 * 60 * 1_000_000_000);
			r.on("A", &[], &[], fire(&[i as f64]));
			// Two `on`s or one is what says a node fired, so `B`'s quiet ticks report nothing at all.
			r.on(
				"B",
				&["A"],
				&[false],
				Fire {
					vals: (i % RARE == 0).then_some(&[1.0]),
					..fire(&[1.0])
				},
			);
		}
		rec.seal();

		let mut t = viz.lock();
		t.seek(0);
		let mut reached = vec![t.frame().tick - 1];
		//LOOP: walks B's fires; `step_until` stands still once there are none left, which ends it.
		loop {
			let at = t.step_until("B").tick - 1;
			if at == *reached.last().expect("seeded") {
				break;
			}
			reached.push(at);
		}
		let want: Vec<usize> = (0..TICKS).step_by(RARE).collect();
		assert_eq!(reached, want, "every one of B's {} fires is addressable", want.len());
	}

	/// A search is not a seek: reaching the end of a sealed recording without a hit used to park the
	/// cursor at that end, so a mistyped node name threw away where the user was reading.
	#[test]
	fn a_scan_that_finds_nothing_stays_put() {
		let (tape, mut rec) = Tape::new(None, 64, 60_000, Backpressure::Block);
		let viz = tape.viz();
		for i in 0..500 {
			rec.at(i * 60 * 1_000_000_000).on("N", &[], &[], fire(&[i as f64]));
		}
		rec.seal();

		let mut t = viz.lock();
		let was = t.seek(300).tick;
		let f = t.step_until("Absent");
		assert_eq!(f.tick, was, "a search for a node the graph does not carry moved the cursor");
		assert!(!f.found, "and it reported as though it had reached one");
		// `N` fires every tick, so the one op that *is* a scan with nothing left to find is a scan
		// started from the last tick.
		t.seek(usize::MAX);
		let end = t.frame().tick;
		let f = t.step_until("N");
		assert_eq!(f.tick, end);
		assert!(!f.found && !f.pending, "a sealed tape with no hit is an answer, not a wait: {f:?}");
	}

	/// A tape read back is the tape that was recorded. Past the capacity on purpose: what survives a
	/// save is what the thinning passes left, so the walk is the one thing that can say the file holds
	/// the same recording rather than merely the same ticks.
	#[test]
	fn a_saved_tape_reads_back_as_the_one_recorded() {
		let (tape, mut rec) = Tape::new(Some("P"), 64, 60_000, Backpressure::Block);
		let viz = tape.viz();
		for i in 0..5000 {
			let mut r = rec.at(i * 60 * 1_000_000_000);
			r.on(
				"P",
				&[],
				&[],
				Fire {
					dims: &[5],
					..fire(&[1.0, 2.0, 0.5, i as f64, 10.0])
				},
			);
			r.on("N", &["P"], &[false], fire(&[i as f64]));
		}
		rec.seal();

		let path = std::env::temp_dir().join("exec_viz_round_trip.bin");
		tape.save(&path).expect("save");
		let back = Viz::load(&path).expect("load");
		std::fs::remove_file(&path).expect("written above");

		assert_eq!(back.lock().topology(), viz.lock().topology());
		let (was, is) = (viz.lock().day(), back.lock().day());
		assert_eq!(is.price_node, was.price_node);
		assert_eq!(is.series, was.series);
		assert_eq!(walk(&back), walk(&viz));
	}

	/// `seek(0)`, then `step(1)` to a standstill: every position the tape is addressable at, in order.
	fn walk(viz: &Viz) -> Vec<usize> {
		let mut t = viz.lock();
		let mut walk = vec![t.seek(0).tick];
		loop {
			let tick = t.step(1).tick;
			if tick == *walk.last().expect("seeded") {
				return walk;
			}
			walk.push(tick);
		}
	}
}
