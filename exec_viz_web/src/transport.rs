//! Where the three reads land. The app-served bundle asks the axum half over JSON; the `demo`
//! bundle fetches the recording once and answers out of a `Viz` in this heap.
//!
//! Which one is a build-time choice because they are two artifacts, not two modes — and because a
//! `Viz` has exactly one cursor. A hosted server would hand every visitor the same one to fight
//! over; a tape per wasm heap is what makes two tabs scrub independently.

#[cfg(feature = "demo")]
pub use local::{day, op, topology};
#[cfg(not(feature = "demo"))]
pub use remote::{day, op, topology};

#[cfg(not(feature = "demo"))]
mod remote {
	use exec_viz::api_types::{ActivationFrame, Op, TopoNode};
	use gloo_net::http::Request;

	pub async fn topology() -> Result<Vec<TopoNode>, String> {
		let resp = Request::get("api/topology").send().await.map_err(|e| e.to_string())?;
		text_or_json(resp).await
	}

	/// Raw body — never parsed by Rust, handed straight to the chart shim.
	pub async fn day() -> Result<String, String> {
		let resp = Request::get("api/day").send().await.map_err(|e| e.to_string())?;
		let text = resp.text().await.map_err(|e| e.to_string())?;
		if resp.ok() { Ok(text) } else { Err(text) }
	}

	pub async fn op(op: Op) -> Result<ActivationFrame, String> {
		let resp = Request::post("api/op").json(&op).map_err(|e| e.to_string())?.send().await.map_err(|e| e.to_string())?;
		text_or_json(resp).await
	}

	/// A non-2xx body is surfaced verbatim: the server sends plain-text errors, so a raw `.json()`
	/// would mask them as an opaque parse failure.
	async fn text_or_json<T: serde::de::DeserializeOwned>(resp: gloo_net::http::Response) -> Result<T, String> {
		if !resp.ok() {
			return Err(resp.text().await.unwrap_or_default());
		}
		resp.json().await.map_err(|e| e.to_string())
	}
}

#[cfg(feature = "demo")]
mod local {
	use std::cell::RefCell;

	use exec_viz::{
		Viz,
		api_types::{ActivationFrame, Op, TopoNode},
	};
	use gloo_net::http::Request;

	/// Relative, so the same bundle finds it under `dx serve` at `/` and under a Pages project site
	/// at `/exec_viz/`.
	const TAPE_URL: &str = "demo.tape";

	pub async fn topology() -> Result<Vec<TopoNode>, String> {
		Ok(viz().await?.topology())
	}

	pub async fn day() -> Result<String, String> {
		// The one place this side serializes anything: the chart shim eats JSON text, and the wire
		// shape it eats is the server's, so the payload is built the same way here.
		serde_json::to_string(&viz().await?.day()).map_err(|e| e.to_string())
	}

	pub async fn op(op: Op) -> Result<ActivationFrame, String> {
		Ok(viz().await?.dispatch(op))
	}

	/// The tape, fetched once. Wasm is single-threaded, so a second caller arriving mid-fetch is a
	/// flag and a park rather than a lock — and it has to be one tape: three boot reads race for it,
	/// and three `Viz`es would be three cursors of which only one is ever read.
	async fn viz() -> Result<Viz, String> {
		enum Claim {
			Held(Viz),
			Wait,
			Mine,
		}
		//LOOP: parks only while another caller's fetch is in flight, so it ends when that one does.
		loop {
			let claim = TAPE.with_borrow_mut(|t| match t {
				Load::Held(v) => Claim::Held(v.clone()),
				Load::Fetching => Claim::Wait,
				Load::Cold => {
					*t = Load::Fetching;
					Claim::Mine
				}
			});
			match claim {
				Claim::Held(v) => return Ok(v),
				Claim::Wait => gloo_timers::future::TimeoutFuture::new(20).await,
				Claim::Mine => {
					let got = fetch().await;
					// Back to `Cold` on failure rather than left claimed: the parked callers would otherwise
					// spin forever on a fetch that is never coming, and each of them wants to report the error.
					TAPE.with_borrow_mut(|t| {
						*t = match &got {
							Ok(v) => Load::Held(v.clone()),
							Err(_) => Load::Cold,
						}
					});
					return got;
				}
			}
		}
	}

	async fn fetch() -> Result<Viz, String> {
		let resp = Request::get(TAPE_URL).send().await.map_err(|e| e.to_string())?;
		if !resp.ok() {
			return Err(format!("{TAPE_URL}: {} {}", resp.status(), resp.status_text()));
		}
		let bytes = resp.binary().await.map_err(|e| e.to_string())?;
		// Schema included — a tape written at another one must say so rather than be reinterpreted.
		Viz::from_bytes(&bytes).map_err(|e| format!("{TAPE_URL}: {e}"))
	}

	enum Load {
		Cold,
		Fetching,
		Held(Viz),
	}

	thread_local! {
		static TAPE: RefCell<Load> = const { RefCell::new(Load::Cold) };
	}
}
