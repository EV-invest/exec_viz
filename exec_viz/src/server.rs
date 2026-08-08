//! axum router + JSON handlers over a [`Viz`] tape. Router shape (nested ServeDir + SPA
//! `fallback`) mirrors `scam_pump_liqs/viz/src/server.rs`; the tape is a single mutex — this is a
//! single-user study tool. Runtime-free: `serve` is a plain future taking the port it binds, so
//! whoever owns the graph also owns where the server runs. Any boot failure is a loud panic — no
//! fallbacks.

use std::{net::SocketAddr, path::PathBuf};

use axum::{
	Json, Router,
	extract::State,
	http::{HeaderValue, StatusCode, header},
	response::{Html, IntoResponse},
	routing::{get, post},
};
use tower_http::{services::ServeDir, set_header::SetResponseHeaderLayer};

use crate::{
	api_types::{SeekReq, SeekTsReq, StepReq, StepUntilChangeReq, StepUntilReq},
	tape::Viz,
};

impl Viz {
	/// Bound separately from [`Viz::serve_on`] so a caller that records and serves concurrently can
	/// hold an open port — and print its URL — before it starts the work the server describes.
	pub async fn bind(port: u16) -> tokio::net::TcpListener {
		let addr = SocketAddr::from(([127, 0, 0, 1], port));
		tokio::net::TcpListener::bind(addr).await.unwrap_or_else(|e| panic!("bind {addr}: {e}"))
	}

	/// Serves the UI until the future is dropped. Cursor ops are plain scans over the tape, so every
	/// handler is cheap enough to run inline on the async runtime.
	pub async fn serve_on(self, listener: tokio::net::TcpListener) {
		// Dev study tool relaunched constantly: `no-store` everywhere so a cached asset can't
		// silently break against a new API shape. All payloads here are small.
		let no_store = SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
		let web = web_dir().expect(UNSET);
		let app = Router::new()
			.route("/api/topology", get(topology))
			.route("/api/day", get(day))
			.route("/api/status", get(status))
			.route("/api/step", post(step))
			.route("/api/seek", post(seek))
			.route("/api/seek_ts", post(seek_ts))
			.route("/api/step_until", post(step_until))
			.route("/api/step_until_change", post(step_until_change))
			.route("/lwc_draw.js", get(lwc_draw))
			.nest_service("/wasm", ServeDir::new(web.join("wasm")))
			.nest_service("/assets", ServeDir::new(web.join("assets")))
			.fallback(index)
			.layer(no_store)
			.with_state(self);

		axum::serve(listener, app).await.expect("axum serve");
	}
}

const UNSET: &str = "EXEC_VIZ_WEB_DIR: point it at a `dx build -p exec_viz_web` bundle";

/// Root of a built `exec_viz_web` bundle. The app supplies it — this crate ships no front-end, and
/// guessing at a path inside its own checkout is how a stale bundle gets served silently. `None`
/// rather than a panic so an app that records with a UI when there is one, and without when there
/// is not, can ask instead of reading this crate's env var behind its back.
pub fn web_dir() -> Option<PathBuf> {
	std::env::var_os("EXEC_VIZ_WEB_DIR").map(PathBuf::from)
}

async fn topology(State(v): State<Viz>) -> impl IntoResponse {
	Json(v.lock().topology())
}

async fn day(State(v): State<Viz>) -> impl IntoResponse {
	Json(v.lock().day())
}

async fn status(State(v): State<Viz>) -> impl IntoResponse {
	Json(v.lock().frame())
}

async fn step(State(v): State<Viz>, Json(req): Json<StepReq>) -> impl IntoResponse {
	Json(v.lock().step(req.n))
}

async fn seek(State(v): State<Viz>, Json(req): Json<SeekReq>) -> impl IntoResponse {
	Json(v.lock().seek(req.tick))
}

async fn seek_ts(State(v): State<Viz>, Json(req): Json<SeekTsReq>) -> impl IntoResponse {
	Json(v.lock().seek_ts(req.ts_ns))
}

async fn step_until(State(v): State<Viz>, Json(req): Json<StepUntilReq>) -> impl IntoResponse {
	Json(v.lock().step_until(&req.node))
}

async fn step_until_change(State(v): State<Viz>, Json(req): Json<StepUntilChangeReq>) -> impl IntoResponse {
	Json(v.lock().step_until_change(&req.nodes))
}

/// SPA fallback: every non-asset, non-API *GET* serves the dx bundle's index.html (200) so a
/// client-side deep-link boots the wasm router. A non-GET here is an unknown API call (e.g. a
/// stale server missing a route the fresh bundle calls) — 404 it loudly instead of returning
/// HTML that the client then fails to parse as JSON, masking the real cause.
async fn index(method: axum::http::Method) -> impl IntoResponse {
	if method != axum::http::Method::GET {
		return (StatusCode::NOT_FOUND, format!("no such route for {method}")).into_response();
	}
	let path = web_dir().expect(UNSET).join("index.html");
	match tokio::fs::read_to_string(&path).await {
		Ok(s) => Html(s).into_response(),
		Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("read {}: {e}", path.display())).into_response(),
	}
}

/// The chart shim's app half, served at the root URL the wasm side dynamically imports it from
/// (`v_utils::lwc::mount(el, "/lwc_draw.js", …)`).
async fn lwc_draw() -> impl IntoResponse {
	([(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript"))], include_str!("../assets/lwc_draw.js"))
}
