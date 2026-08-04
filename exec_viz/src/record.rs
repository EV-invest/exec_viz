//! Exec-time recording surface: the `runs/{run_id}/` layout + row schemas that scam_pump_liqs
//! and zero_fee_arb each hand-roll today, standardized here. Row shapes are modeled on
//! `scam_pump_liqs/viz/src/read.rs` (`OrderEventRow`, `DrawingRow`), which is the proven read-side
//! contract; the order/fill/drawing lanes are still schema-only.
//!
//! Under the `record` feature the run dir also holds the tape itself, written by the tape thread as
//! it absorbs — so a finished replay can be re-served without re-running the graph, and a crashed
//! one keeps what it got to.

use serde::{Deserialize, Serialize};

/// A run dir is `runs/{run_id}/` where `run_id = {version}_{config_hash}` — collision-proof
/// across logic and config versions.
pub const RUNS_DIR: &str = "runs";
pub const ORDERS_FILE: &str = "orders.parquet";
pub const FILLS_FILE: &str = "fills.parquet";
pub const DRAWINGS_FILE: &str = "drawings.parquet";

#[cfg(feature = "record")]
pub(crate) use tape::Sink;

/// One long-format self-drawn signal sample: strategy-side series (screener levels, degraders)
/// drawn onto the study chart without the viewer knowing their semantics. `colour` is a rendered
/// CSS colour string — the client applies it as-is.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DrawingRow {
	pub ts_ns: u64,
	pub label: String,
	pub value: f64,
	pub colour: String,
}

/// One execution: `ts_event_ns` is exchange fill time, `ts_init_ns` local receipt (their gap is
/// the fill→ack latency the exec view renders).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FillRow {
	pub client_order_id: String,
	pub ts_event_ns: u64,
	pub ts_init_ns: u64,
	pub price: f64,
	pub qty: f64,
	pub side: String,
	pub commission: Option<f64>,
}

/// One order-lifecycle event (`Initialized`/`Accepted`/`Updated`/`Canceled`/…). `_ns` suffixes
/// keep the unit visible across the JSON boundary. `price`/`qty` are the order's level on
/// `Initialized`/`Updated`, null elsewhere.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OrderEventRow {
	pub client_order_id: String,
	pub event: String,
	pub ts_event_ns: u64,
	pub ts_init_ns: u64,
	pub price: Option<f64>,
	pub qty: Option<f64>,
	pub side: String,
	pub order_type: String,
}

#[cfg(feature = "record")]
mod tape {
	use std::{
		fs::File,
		io::BufWriter,
		path::{Path, PathBuf},
		sync::Arc,
		time::{Duration, Instant},
	};

	use arrow::{
		array::{ArrayRef, Float64Builder, Int64Builder, ListBuilder, RecordBatch, StringBuilder, UInt32Builder, UInt64Builder},
		ipc::writer::StreamWriter,
	};

	use super::RUNS_DIR;
	use crate::{
		api_types::{SeriesOut, TopoNode},
		tape::Act,
	};

	/// The tape, long-format: one row per node per tick, `node` indexing `topology.json`.
	const TICKS_FILE: &str = "ticks.arrow";
	/// The chart's downsampled series, one row per node per bucket.
	const SERIES_FILE: &str = "series.arrow";
	const TOPOLOGY_FILE: &str = "topology.json";

	/// Bytes a batch accumulates before it is cut, and the longest a partial one waits — the
	/// flush-on-bytes-or-age shape of `trading_data_persistence::RotationPolicy`, minus the knob
	/// nobody has asked to turn.
	const BATCH_BYTES: usize = 8 << 20;
	const BATCH_AGE: Duration = Duration::from_secs(5);

	/// The run dir's write side, owned by the tape thread and stepped from [`crate::tape::Tape`] as it
	/// absorbs. Panics rather than degrades: a recording that cannot write is not a recording, and the
	/// run it is describing has not started trading on the strength of it.
	pub(crate) struct Sink {
		dir: PathBuf,
		ticks: Lane,
		cols: TickCols,
		rows: usize,
		bytes: usize,
		/// Set when a batch takes its first row, so an idle feed's partial batch still reaches disk.
		deadline: Option<Instant>,
		/// Nodes already named in `topology.json`, which is rewritten whole whenever the graph grows.
		named: usize,
	}

	impl Sink {
		pub(crate) fn new(root: &Path, run_id: &str) -> Self {
			let dir = root.join(RUNS_DIR).join(run_id);
			std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("run dir `{}`: {e}", dir.display()));
			Self {
				ticks: Lane::at(dir.join(TICKS_FILE)),
				dir,
				cols: TickCols::new(),
				rows: 0,
				bytes: 0,
				deadline: None,
				named: 0,
			}
		}

		/// Rewritten whole rather than appended to: the graph is named once in practice, and a reader
		/// that finds a half-run's `topology.json` must find all of what its tick rows index.
		pub(crate) fn name(&mut self, topology: &[TopoNode]) {
			if topology.len() == self.named {
				return;
			}
			let path = self.dir.join(TOPOLOGY_FILE);
			let json = serde_json::to_vec(topology).expect("`TopoNode` is plain data");
			std::fs::write(&path, json).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
			self.named = topology.len();
		}

		pub(crate) fn tick(&mut self, abs: usize, ts_ns: i64, acts: &[Act]) {
			for (i, a) in acts.iter().enumerate() {
				self.cols.abs.append_value(abs as u64);
				self.cols.ts_ns.append_value(ts_ns);
				self.cols.node.append_value(i as u32);
				self.cols.out.append_value(&a.out);
				self.cols.detail.append_value(&a.detail);
				let (vals, jac) = (a.vals.as_deref(), a.jac.as_deref());
				list(&mut self.cols.vals, vals);
				list(&mut self.cols.jac, jac);
				self.bytes += a.out.len() + a.detail.len() + 8 * (vals.map_or(0, <[f64]>::len) + jac.map_or(0, <[f64]>::len)) + 24;
			}
			self.rows += acts.len();
			let deadline = *self.deadline.get_or_insert_with(|| Instant::now() + BATCH_AGE);
			if self.bytes >= BATCH_BYTES || Instant::now() >= deadline {
				self.cut();
			}
		}

		/// Closes the run: the open batch, then the series as one batch — it is buckets rather than
		/// ticks, and small enough that keeping it whole until here costs nothing.
		pub(crate) fn finish(mut self, series: &[SeriesOut]) {
			self.cut();
			self.ticks.close();

			let mut node = UInt32Builder::new();
			let mut ts_ms = Int64Builder::new();
			let mut vals = ListBuilder::new(Float64Builder::new());
			let mut detail = StringBuilder::new();
			for (i, s) in series.iter().enumerate() {
				for p in &s.points {
					node.append_value(i as u32);
					ts_ms.append_value(p.ts_ms);
					list(&mut vals, Some(&p.vals));
					detail.append_value(&p.detail);
				}
			}
			let mut lane = Lane::at(self.dir.join(SERIES_FILE));
			lane.write(&batch([
				("node", Arc::new(node.finish()) as ArrayRef),
				("ts_ms", Arc::new(ts_ms.finish())),
				("vals", Arc::new(vals.finish())),
				("detail", Arc::new(detail.finish())),
			]));
			lane.close();
		}

		fn cut(&mut self) {
			if self.rows == 0 {
				return;
			}
			let c = &mut self.cols;
			self.ticks.write(&batch([
				("abs", Arc::new(c.abs.finish()) as ArrayRef),
				("ts_ns", Arc::new(c.ts_ns.finish())),
				("node", Arc::new(c.node.finish())),
				("out", Arc::new(c.out.finish())),
				("detail", Arc::new(c.detail.finish())),
				("vals", Arc::new(c.vals.finish())),
				("jac", Arc::new(c.jac.finish())),
			]));
			self.rows = 0;
			self.bytes = 0;
			self.deadline = None;
		}
	}

	struct TickCols {
		abs: UInt64Builder,
		ts_ns: Int64Builder,
		node: UInt32Builder,
		out: StringBuilder,
		detail: StringBuilder,
		vals: ListBuilder<Float64Builder>,
		jac: ListBuilder<Float64Builder>,
	}

	impl TickCols {
		fn new() -> Self {
			Self {
				abs: UInt64Builder::new(),
				ts_ns: Int64Builder::new(),
				node: UInt32Builder::new(),
				out: StringBuilder::new(),
				detail: StringBuilder::new(),
				vals: ListBuilder::new(Float64Builder::new()),
				jac: ListBuilder::new(Float64Builder::new()),
			}
		}
	}

	/// A null list, not an empty one: the node did not fire, which is a different thing from firing
	/// with nothing to say.
	fn list(b: &mut ListBuilder<Float64Builder>, v: Option<&[f64]>) {
		match v {
			Some(v) => {
				b.values().append_slice(v);
				b.append(true);
			}
			None => b.append(false),
		}
	}

	/// Schema taken from the arrays rather than declared: the builders are the only thing that decides
	/// what a column holds, and a second spelling of it is a second thing to keep in step.
	fn batch<const N: usize>(cols: [(&str, ArrayRef); N]) -> RecordBatch {
		RecordBatch::try_from_iter_with_nullable(cols.map(|(n, a)| (n, a, true))).expect("the builders are appended in lockstep")
	}

	/// One `.arrow` IPC stream. The writer is opened by the first batch, so a lane that never gets one
	/// leaves no file behind.
	struct Lane {
		path: PathBuf,
		writer: Option<StreamWriter<BufWriter<File>>>,
	}

	impl Lane {
		fn at(path: PathBuf) -> Self {
			Self { path, writer: None }
		}

		fn write(&mut self, batch: &RecordBatch) {
			let Self { path, writer } = self;
			if writer.is_none() {
				let f = ok(path, File::create(&path));
				*writer = Some(ok(path, StreamWriter::try_new(BufWriter::new(f), &batch.schema())));
			}
			let w = writer.as_mut().expect("just opened");
			ok(path, w.write(batch));
			// Per batch, not per run: what makes a killed run's stream readable up to its last cut.
			ok(path, w.flush());
		}

		/// Writes the end-of-stream marker. A run that dies without reaching this leaves a stream a
		/// reader stops at rather than one it rejects.
		fn close(&mut self) {
			let Self { path, writer } = self;
			if let Some(w) = writer {
				ok(path, w.finish());
			}
		}
	}

	fn ok<T>(path: &Path, r: Result<T, impl std::fmt::Display>) -> T {
		r.unwrap_or_else(|e| panic!("{}: {e}", path.display()))
	}

	#[cfg(test)]
	mod tests {
		use arrow::{
			array::{Array, Float64Array, ListArray, UInt64Array},
			ipc::reader::StreamReader,
		};
		use trading_data_dag::{Fire, Observer as _, Plot};

		use super::*;
		use crate::{Backpressure, Viz};

		/// The buffer thins; the file does not. A run many times its capacity is the case a persisted
		/// tape exists for, so what the file must hold is every tick the graph stepped — including the
		/// ones a client can no longer seek to, which are the ones re-running the graph was the only
		/// other way to get back.
		#[test]
		fn the_file_keeps_what_the_buffer_thinned_away() {
			let root = std::env::temp_dir().join("exec_viz_record_roundtrip");
			// Left behind by a previous run of this test, and a stale `ticks.arrow` would be appended to.
			std::fs::remove_dir_all(&root).ok();
			let stepped = 400;
			let (viz, mut rec) = Viz::recorded(None, 8, 60_000, Backpressure::Block, &root, "run");
			for i in 0..stepped {
				rec.at(i as i64 * 60 * 1_000_000_000).on("N", &[], &[], fire(&[i as f64]));
			}
			rec.seal();
			assert!(viz.lock().frame().tick < stepped, "the buffer never thinned, so this measured nothing");

			let dir = root.join(RUNS_DIR).join("run");
			let topology: Vec<TopoNode> = serde_json::from_slice(&std::fs::read(dir.join(TOPOLOGY_FILE)).unwrap()).unwrap();
			assert_eq!(topology.len(), 1);

			let mut got = Vec::new();
			for batch in StreamReader::try_new(File::open(dir.join(TICKS_FILE)).unwrap(), None).unwrap() {
				let batch = batch.unwrap();
				let abs = batch.column(0).as_any().downcast_ref::<UInt64Array>().unwrap();
				let vals = batch.column(5).as_any().downcast_ref::<ListArray>().unwrap();
				for i in 0..batch.num_rows() {
					let v = vals.value(i);
					got.push((abs.value(i) as usize, v.as_any().downcast_ref::<Float64Array>().unwrap().value(0)));
				}
			}
			assert_eq!(got.len(), stepped, "one row per node per tick, and there is one node");
			assert!(got.iter().enumerate().all(|(i, &(abs, v))| abs == i && v == i as f64), "{:?}", &got[..8]);
		}

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
	}
}
