//! What `capacity` buys and what it costs, measured rather than asserted — the `viz_cost.rs` →
//! `cost.json` → `cost.typ` shape from `trading_data`'s spl example, one crate over.
//!
//! The mix is synthetic on purpose: `exec_viz` cannot depend on a strategy that depends on it, so
//! the graph is built straight through the real [`Viz`]/[`Recorder`](exec_viz::Recorder) the way
//! `tape.rs`'s own tests do — a book-driven node firing every tick, a screener every 100, and
//! 1m/5m/1h bar analogues, at spl's measured ~106 B/tick of rendered card faces. What is *not*
//! synthetic is the tape: every reading below comes out of the same thinning pass and the same
//! handlers the browser drives.
//!
//! `cargo r --release -p exec_viz --example capacity`

use std::{fmt::Write as _, net::SocketAddr, path::Path, time::Instant};

use exec_viz::{
	Backpressure, Tape, Viz,
	api_types::{ActivationFrame, SeekReq, StepReq, StepUntilReq},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use trading_data_dag::{Fidelity, Fire, Glance, Observer as _, Plot};

/// spl's own, off its `cost.json`: one replayed trading day of BTC book + trades.
const TICKS_PER_DAY: usize = 159_210;
const DAYS: usize = 2;
/// Where a reader's own graph lands is what [`Viz::bytes`] is exported for; this is the one charted.
const GLANCE_BYTES_PER_TICK: usize = 106;
/// spl's `SCROLLBACK` sits third, so the table has a row that is the number in use rather than only
/// powers of two around it.
const CAPS: [usize; 5] = [2_048, 8_192, 20_000, 65_536, 262_144];
/// Frames per latency reading, parked across the middle of the tape — where a quiet node's
/// carry-forward scan is longest, which is what the reading is about.
const SAMPLES: usize = 64;
const BAR: usize = 44;

/// A minute of venue time in ticks, at spl's density — the unit the bar analogues are spaced by.
const MIN: usize = TICKS_PER_DAY / 1440;
/// The book node carries the whole per-tick face budget on its own: spl spreads it over four that
/// all fire on nearly every tick, and what the tape holds is the sum either way.
///
/// `Cap` is here because a mix without it measures the wrong thing. A frame carries every node's
/// standing value, and a node that has been quiet is found by searching back for its last fire — so
/// the cost of landing a cursor is set by the *rarest* node, not the busiest. spl's market cap fires
/// twice a day and its classification can go a whole one without firing; a mix whose rarest node
/// still fires hourly reports a latency nobody has.
const NODES: [Node; 6] = [
	Node {
		name: "Book",
		period: 1,
		face: GLANCE_BYTES_PER_TICK,
		dims: &[4],
	},
	Node {
		name: "Screen",
		period: 100,
		face: 24,
		dims: &[3],
	},
	Node {
		name: "Bar:1m",
		period: MIN,
		face: 24,
		dims: &[5],
	},
	Node {
		name: "Bar:5m",
		period: 5 * MIN,
		face: 24,
		dims: &[5],
	},
	Node {
		name: "Bar:1h",
		period: 60 * MIN,
		face: 24,
		dims: &[5],
	},
	Node {
		name: "Cap",
		period: TICKS_PER_DAY / 2,
		face: 24,
		dims: &[1],
	},
];
const CARD: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const BEGIN: &str = "<!-- capacity:begin -->";
const END: &str = "<!-- capacity:end -->";
fn main() {
	// SAFETY: the process is still single-threaded — the runtime below spawns the first thread, and
	// the tape thread comes later still. `serve_on` wants a built front-end and a sweep has no use for
	// one; every route it asks for answers JSON off the tape.
	unsafe { std::env::set_var("EXEC_VIZ_WEB_DIR", std::env::temp_dir()) };
	tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.expect("a current-thread runtime asks nothing of the system")
		.block_on(sweep());
}
/// A node of the synthetic mix: fires every `period` ticks, rendering `face` bytes when it does.
struct Node {
	name: &'static str,
	period: usize,
	face: usize,
	dims: &'static [usize],
}

/// One node's card face. Only its width is a measurement — the tape stores the rendering, not the
/// value — so every node renders a slice of one string.
struct Face(&'static str);
impl Glance for Face {
	fn glance(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		f.write_str(self.0)
	}
}

/// One sweep point.
struct Row {
	cap: usize,
	retained: usize,
	bytes: usize,
	/// Wall clock of the whole recording over its ticks. Under [`Backpressure::Block`] with a
	/// producer this trivial, what it prices is the tape thread.
	rec_ns: f64,
	seek_us: f64,
	/// Fires still reachable by `step_until`, positional with [`NODES`].
	reach: Vec<usize>,
}

async fn sweep() {
	let ticks = TICKS_PER_DAY * DAYS;
	let fires: Vec<usize> = NODES.iter().map(|n| ticks.div_ceil(n.period)).collect();
	let mut rows = Vec::new();
	let mut hop_us = 0.0;
	for cap in CAPS {
		let began = Instant::now();
		let viz = record(cap, ticks);
		let rec_ns = began.elapsed().as_secs_f64() * 1e9 / ticks as f64;
		let bytes = viz.bytes();
		let listener = Viz::bind(0).await;
		let addr = listener.local_addr().expect("a bound listener has an address");
		// Structured: the server future never returns, so `select!` ends exactly when the readings do
		// and drops it — no detached task outliving the measurement it was for.
		let row = tokio::select! {
			() = viz.serve_on(listener) => unreachable!("axum::serve returns only on an error it panics on"),
			(row, hop) = read(addr, cap, bytes, ticks, rec_ns) => { hop_us = hop; row }
		};
		eprintln!(
			"capacity {cap}: {} retained, {:.1} MB, {:.0}ns/tick, {:.0}µs",
			row.retained,
			row.bytes as f64 / 1e6,
			row.rec_ns,
			row.seek_us
		);
		rows.push(row);
	}

	let out = render(&rows, &fires, ticks, hop_us);
	print!("{out}");
	let docs = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs"));
	// Outside `.readme_assets/`: readme-fw warns on every file in there it does not know how to
	// assemble, and this is the raw artifact, not a section.
	std::fs::write(docs.join("capacity.txt"), &out).expect("write capacity.txt");
	splice(&docs.join(".readme_assets/other.md"), &out);
}

/// The whole run through the real recorder, sealed — so what is served afterwards is what the
/// thinning passes left, not a tape still being written.
fn record(cap: usize, ticks: usize) -> Viz {
	let (tape, mut rec) = Tape::new(None, cap, 60_000, Backpressure::Block);
	let viz = tape.viz();
	for i in 0..ticks {
		let mut r = rec.at(i as i64 * 1_000_000_000);
		for n in &NODES {
			let vals = [1.0, 2.0, 3.0, 4.0, 5.0];
			r.on(
				n.name,
				&[],
				&[],
				Fire {
					glance: &Face(&CARD[..n.face]),
					dims: n.dims,
					plots: &[Plot::DEFAULT],
					clock: None,
					fidelity: Fidelity::Exact,
					fires: 1,
					ran: i % n.period == 0,
					vals: (i % n.period == 0).then(|| &vals[..n.dims[0]]),
					dep_dims: &[],
					jac: None,
					exact_block: None,
					exact: false,
					formula: None,
					deriv: None,
					trace: None,
				},
			);
		}
	}
	rec.seal();
	viz
}

/// Every reading that is a replay op, taken over the served tape. Hands back the row and the bare
/// loopback round trip the latency in it should be read against.
async fn read(addr: SocketAddr, cap: usize, bytes: usize, ticks: usize, rec_ns: f64) -> (Row, f64) {
	let mut api = Api::connect(addr).await;

	// `step(n)` from the front parks at position `min(n, last)`, so the smallest `n` that reaches the
	// end names how many ticks the passes left — the one number the wire does not carry outright.
	let end = {
		api.seek(0).await;
		api.step(usize::MAX).await.tick
	};
	let (mut lo, mut hi) = (0usize, cap);
	while lo < hi {
		let mid = lo.midpoint(hi);
		api.seek(0).await;
		match api.step(mid).await.tick == end {
			true => hi = mid,
			false => lo = mid + 1,
		}
	}
	let retained = lo + 1;

	let mut hop = Vec::with_capacity(SAMPLES);
	for _ in 0..SAMPLES {
		let began = Instant::now();
		api.topology().await;
		hop.push(began.elapsed());
	}
	let mut seek = Vec::with_capacity(SAMPLES);
	for i in 0..SAMPLES {
		let began = Instant::now();
		// Walking rather than repeating one tick: nothing is cached, but a moving cursor is what the
		// reading is about, and a standing one invites the question.
		api.seek(ticks / 2 + i).await;
		seek.push(began.elapsed());
	}

	// A scan reports whether it reached anything, so the walk ends on the tape saying so rather than
	// on a cursor that stopped moving.
	let mut reach = Vec::new();
	for n in &NODES {
		let start = api.seek(0).await;
		let mut hits = usize::from(start.activations.iter().any(|a| a.node == n.name && a.fired));
		//LOOP: bounded by the retained fires of `n`, which is what it is counting.
		while api.step_until(n.name).await.found {
			hits += 1;
		}
		reach.push(hits);
	}
	(
		Row {
			cap,
			retained,
			bytes,
			rec_ns,
			seek_us: median(seek),
			reach,
		},
		median(hop),
	)
}

/// Median rather than mean: one scheduler hiccup over a loopback socket is not the tape's cost.
fn median(mut took: Vec<std::time::Duration>) -> f64 {
	took.sort_unstable();
	took[took.len() / 2].as_secs_f64() * 1e6
}

fn render(rows: &[Row], fires: &[usize], ticks: usize, hop_us: f64) -> String {
	let mut s = String::new();
	let per_tick = NODES.iter().map(|n| n.face as f64 / n.period as f64).sum::<f64>();
	let w = |s: &mut String, line: std::fmt::Arguments<'_>| s.write_fmt(line).expect("`String`'s `Write` is infallible");
	w(
		&mut s,
		format_args!(
			"{ticks} ticks ({DAYS} days at spl's {TICKS_PER_DAY}/day), {} nodes, {per_tick:.0} B/tick of card faces\n",
			NODES.len()
		),
	);
	w(&mut s, format_args!("fires over the run:"));
	for (n, f) in NODES.iter().zip(fires) {
		w(&mut s, format_args!("  {}={f}", n.name));
	}
	s.push_str("\n\n");

	let mut chart = |title: std::fmt::Arguments<'_>, val: &dyn Fn(&Row) -> f64, note: &dyn Fn(&Row, f64) -> String| {
		w(&mut s, title);
		let max = rows.iter().map(val).fold(0.0f64, f64::max);
		for r in rows {
			let v = val(r);
			let bar = ((v / max) * BAR as f64).round() as usize;
			w(&mut s, format_args!("  {:>7} │{:▬<width$} {}\n", r.cap, "", note(r, v), width = bar.max(1)));
		}
		s.push('\n');
	};
	chart(format_args!("footprint — Viz::bytes over the retained ticks\n"), &|r| r.bytes as f64 / 1e6, &|r, v| {
		format!("{v:.1} MB  ({} B/tick)", r.bytes / r.retained)
	});
	// What one keypress costs, socket included. An earlier version charted this minus the loopback hop,
	// which drew a prettier curve and told the reader a number they never wait for.
	chart(
		format_args!("reactivity — one /api/seek mid-tape, median of {SAMPLES}; the bare loopback hop under it is {hop_us:.0}µs\n"),
		&|r| r.seek_us,
		&|_, v| format!("{v:.0}µs"),
	);

	chart(
		format_args!("absorption — the whole recording's wall clock per tick, tape-thread bound under `Block`\n"),
		&|r| r.rec_ns,
		&|_, v| format!("{v:.0}ns"),
	);

	w(&mut s, format_args!("addressability — fires still reachable by `step_until`, against the run's own\n"));
	w(&mut s, format_args!("  {:>7} {:>9}", "capacity", "retained"));
	for n in &NODES {
		w(&mut s, format_args!(" {:>13}", n.name));
	}
	s.push('\n');
	for r in rows {
		w(&mut s, format_args!("  {:>7} {:>9}", r.cap, r.retained));
		for (hits, all) in r.reach.iter().zip(fires) {
			w(&mut s, format_args!(" {:>13}", format!("{hits}/{all}")));
		}
		s.push('\n');
	}
	s
}

/// Splices the block into the hand-written prose, so the prose can say "measured" and mean it.
fn splice(at: &Path, out: &str) {
	let md = std::fs::read_to_string(at).expect("other.md is this crate's, and checked in");
	let (head, rest) = md.split_once(BEGIN).unwrap_or_else(|| panic!("{} has no {BEGIN} marker", at.display()));
	let (_, tail) = rest.split_once(END).unwrap_or_else(|| panic!("{} has no {END} marker", at.display()));
	std::fs::write(at, format!("{head}{BEGIN}\n```\n{out}```\n{END}{tail}")).expect("write other.md");
}

/// The browser's door, over one keep-alive connection: the replay ops are the tape's own and this
/// crate exports none of them, which is exactly the position a consumer measuring their own graph is
/// in. Hand-rolled rather than an HTTP client dependency — every request here is a fixed-shape one
/// to a loopback socket, and the sweep issues tens of thousands of them.
struct Api {
	sock: tokio::net::TcpStream,
	buf: Vec<u8>,
}

impl Api {
	async fn connect(addr: SocketAddr) -> Self {
		let sock = tokio::net::TcpStream::connect(addr).await.expect("the server bound the address being asked for");
		sock.set_nodelay(true).expect("a connected TCP socket takes TCP_NODELAY");
		Self { sock, buf: Vec::new() }
	}

	async fn seek(&mut self, tick: usize) -> ActivationFrame {
		self.post("seek", &SeekReq { tick }).await
	}

	async fn step(&mut self, n: usize) -> ActivationFrame {
		self.post("step", &StepReq { n }).await
	}

	async fn step_until(&mut self, node: &str) -> ActivationFrame {
		self.post("step_until", &StepUntilReq { node: node.into() }).await
	}

	/// Read for its cost and not its answer: the cheapest handler there is, so what it prices is the
	/// socket rather than the tape.
	async fn topology(&mut self) {
		self.send("GET /api/topology HTTP/1.1\r\nHost: t\r\n\r\n").await;
		self.recv().await;
	}

	async fn post(&mut self, route: &str, body: &impl serde::Serialize) -> ActivationFrame {
		let body = serde_json::to_string(body).expect("a request shape serializes");
		let req = format!(
			"POST /api/{route} HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
			body.len()
		);
		self.send(&req).await;
		let body = self.recv().await;
		serde_json::from_slice(&body).unwrap_or_else(|e| panic!("/api/{route} answered {}: {e}", String::from_utf8_lossy(&body)))
	}

	async fn send(&mut self, req: &str) {
		self.sock.write_all(req.as_bytes()).await.expect("the served tape outlives the sweep reading it");
	}

	/// Length-delimited: axum states `content-length` on every response asked for here, so there is no
	/// chunked case to carry.
	async fn recv(&mut self) -> Vec<u8> {
		//LOOP: both are bounded by the response the server has already committed to sending.
		let head = loop {
			if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
				break i + 4;
			}
			self.fill().await;
		};
		let len: usize = std::str::from_utf8(&self.buf[..head])
			.expect("HTTP headers are ASCII")
			.to_ascii_lowercase()
			.split_once("content-length: ")
			.and_then(|(_, rest)| rest.split_once("\r\n"))
			.expect("axum length-delimits every response this asks for")
			.0
			.parse()
			.expect("a decimal length");
		while self.buf.len() < head + len {
			self.fill().await;
		}
		let body = self.buf[head..head + len].to_vec();
		self.buf.drain(..head + len);
		body
	}

	async fn fill(&mut self) {
		let was = self.buf.len();
		self.buf.resize(was + 16 * 1024, 0);
		let n = self.sock.read(&mut self.buf[was..]).await.expect("read the response the server committed to");
		assert!(n > 0, "the server closed the connection mid-sweep");
		self.buf.truncate(was + n);
	}
}
