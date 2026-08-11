//! What one node's step costs, estimated online from the observer's own clock.
//!
//! `Observer::on` is called once per node in step order, so the span between one node's exit and
//! the next one's entry *is* that next node's step — no framework instrumentation, and nothing in
//! `trading_data_dag`, which has no `std::` to read a clock with.
//!
//! Only one tick in [`TICK_STRIDE`] is clocked. `Instant::now()` is a vDSO read costing ~25ns on a
//! `tsc` clocksource, but an MMIO read costing ~1.5µs on `hpet` — and two per fire on every tick
//! measured 21s over a day on an hpet box, four times what the whole recording costs. A cost
//! estimate is a study aid; it does not need every tick, and it must not be the reason the replay is
//! slow. A clocked tick clocks *every* node in it, so the spans within it are still whole.
//!
//! Samples are heavy right-tailed: a step preempted by the scheduler measures the scheduler, not
//! the node. So the series tracked is the **minimum of each block of [`BLOCK`]**, which answers
//! "what does this node cost" rather than "what did the machine do to it".
//!
//! The level over those minima is an EMA whose gain is the steady-state Kalman gain of a local-level
//! model, so how fast it follows is read off the data instead of picked: `α = (−λ + √(λ² + 4λ))/2`
//! for signal-to-noise `λ = q/r`. Differencing makes the series MA(1) — `Var(Δy) = q + 2r` and
//! `Cov(Δy_t, Δy_{t−1}) = −r` — so both variances come off moments of `Δy`, with nothing to tune.

#[cfg(feature = "server")]
use std::time::Instant;

/// Samples per block minimum. Large enough that a block usually contains an unpreempted step,
/// small enough that a node's cost changing mid-run is still visible within a few hundred ticks.
#[cfg(feature = "server")]
const BLOCK: usize = 16;
/// Ticks between clocked ticks — see the module note on what a clock read costs.
#[cfg(feature = "server")]
pub(crate) const TICK_STRIDE: usize = 64;
/// Block minima needed before the moments mean anything; until then the gain is a plain warm-up.
#[cfg(feature = "server")]
const WARM: f64 = 8.0;
#[cfg(feature = "server")]
const WARM_GAIN: f64 = 0.3;

#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
pub(crate) struct Cost {
	block_n: usize,
	block_min: f64,
	/// The EMA and, on the same recursion over 1s, its bias correction. Kept as a pair rather than
	/// as `(1−α)^t`, because α is re-estimated every step and a fixed-α correction would be wrong
	/// from the first re-estimate on.
	ema: f64,
	weight: f64,
	prev_y: Option<f64>,
	prev_d: Option<f64>,
	nd: f64,
	sum_d: f64,
	sum_d2: f64,
	ndd: f64,
	sum_dd: f64,
}

impl Cost {
	#[cfg(feature = "server")]
	pub(crate) fn sample(&mut self, ns: f64) {
		self.block_min = if self.block_n == 0 { ns } else { self.block_min.min(ns) };
		self.block_n += 1;
		if self.block_n < BLOCK {
			return;
		}
		self.block_n = 0;
		let y = self.block_min;

		if let Some(p) = self.prev_y {
			let d = y - p;
			self.nd += 1.0;
			self.sum_d += d;
			self.sum_d2 += d * d;
			if let Some(pd) = self.prev_d {
				self.ndd += 1.0;
				self.sum_dd += d * pd;
			}
			self.prev_d = Some(d);
		}
		self.prev_y = Some(y);

		let a = self.gain();
		self.ema += a * (y - self.ema);
		self.weight += a * (1.0 - self.weight);
	}

	/// Nanoseconds per step. `None` until the first block closes — a node stepped a handful of times
	/// has no estimate, and reporting the raw first sample as one would be reporting the scheduler.
	pub(crate) fn ns(&self) -> Option<f64> {
		(self.weight > 0.0).then(|| self.ema / self.weight)
	}

	#[cfg(feature = "server")]
	fn gain(&self) -> f64 {
		if self.ndd < WARM {
			return WARM_GAIN;
		}
		let mean = self.sum_d / self.nd;
		let var = (self.sum_d2 / self.nd - mean * mean).max(0.0);
		let cov = self.sum_dd / self.ndd - mean * mean;
		// `r = −Cov₁`, so a non-negative covariance says the differenced series carries no measurement
		// noise this model can name: nothing to smooth through, the level is whatever came in.
		if cov >= 0.0 {
			return 1.0;
		}
		let lambda = (var + 2.0 * cov).max(0.0) / -cov;
		let a = (-lambda + (lambda * lambda + 4.0 * lambda).sqrt()) / 2.0;
		// `q = 0` ⇒ a constant level under noise ⇒ `α = 0`, which would freeze the estimate at zero.
		// The running mean is that model's optimum, and `1/t` under the bias correction *is* it.
		a.max(1.0 / self.nd.max(1.0))
	}
}

/// The step-order clock: `mark` closes the span that ended here and opens the next one.
#[cfg(feature = "server")]
pub(crate) struct Clock(Instant);

#[cfg(feature = "server")]
impl Clock {
	pub(crate) fn new() -> Self {
		Clock(Instant::now())
	}

	/// Nanoseconds since the last `mark`, and restarts. Call on entry to `on` for the span the node
	/// just spent stepping, and on exit so the observer's own work is not charged to the next node.
	pub(crate) fn mark(&mut self, now: Instant) -> f64 {
		let ns = now.duration_since(self.0).as_nanos() as f64;
		self.0 = now;
		ns
	}
}

#[cfg(all(test, feature = "server"))]
mod tests {
	use super::*;

	/// The two things the estimator is for: it converges on a level buried in noise, and it is not
	/// dragged up by the right tail that preemption puts in every real sample stream.
	#[test]
	fn the_level_is_the_floor_not_the_mean() {
		let mut c = Cost::default();
		let mut state = 12_345u64;
		let mut rand = || {
			state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
			(state >> 33) as f64 / (1u64 << 31) as f64
		};
		let mut raw = Vec::new();
		for _ in 0..BLOCK * 400 {
			// a 100ns step, with one sample in ten preempted into the tail.
			let s = if rand() < 0.1 { 100.0 + 5_000.0 * rand() } else { 100.0 + 20.0 * rand() };
			raw.push(s);
			c.sample(s);
		}
		let mean = raw.iter().sum::<f64>() / raw.len() as f64;
		let got = c.ns().expect("400 blocks closed");
		assert!(got > 90.0 && got < 140.0, "estimate {got} is not the ~100ns floor");
		assert!(mean > 300.0, "the tail must be there for the test to mean anything: mean {mean}");
	}

	/// A cold start reports the level, not a fraction of it — which is what the `weight` recursion
	/// buys, and what a plain zero-seeded EMA gets wrong for its first few hundred samples.
	#[test]
	fn a_cold_start_is_not_biased_toward_zero() {
		let mut c = Cost::default();
		assert!(c.ns().is_none(), "no estimate before the first block closes");
		for _ in 0..BLOCK {
			c.sample(500.0);
		}
		assert_eq!(c.ns(), Some(500.0));
	}
}
