//! axum router + JSON handlers over a [`Viz`] tape. Router shape (nested ServeDir + SPA
//! `fallback`) mirrors `scam_pump_liqs/viz/src/server.rs`; the tape is a single mutex — this is a
//! single-user study tool. Runtime-free: `serve` is a plain future, so whoever owns the graph also
//! owns where the server runs. Any boot failure is a loud panic — no fallbacks.

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
	api_types::{SeekReq, StepReq, StepUntilChangeReq, StepUntilReq},
	tape::Viz,
};

/// Compile-time root of the static assets dir (sibling of `src/`); only `lwc_draw.js` is served
/// from here — the rest of the front-end is the `dx build` output.
const ASSETS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");

impl Viz {
	/// Serves the UI until the future is dropped. Cursor ops are plain scans over the tape, so
	/// every handler is cheap enough to run inline on the async runtime.
	pub async fn serve(self, port: u16) {
		// Dev study tool relaunched constantly: `no-store` everywhere so a cached asset can't
		// silently break against a new API shape. All payloads here are small.
		let no_store = SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
		let web = web_dir();
		let app = Router::new()
			.route("/api/topology", get(topology))
			.route("/api/day", get(day))
			.route("/api/status", get(status))
			.route("/api/step", post(step))
			.route("/api/seek", post(seek))
			.route("/api/step_until", post(step_until))
			.route("/api/step_until_change", post(step_until_change))
			.route("/lwc_draw.js", get(lwc_draw))
			.layer(no_store)
			.nest_service("/wasm", ServeDir::new(web.join("wasm")))
			.nest_service("/assets", ServeDir::new(web.join("assets")))
			.fallback(index)
			.with_state(self);

		let addr = SocketAddr::from(([127, 0, 0, 1], port));
		let listener = tokio::net::TcpListener::bind(addr).await.unwrap_or_else(|e| panic!("bind {addr}: {e}"));
		println!("exec_viz: http://{addr}/");
		axum::serve(listener, app).await.expect("axum serve");
	}
}

/// Root of the built front-end (`dx build` output). Overridable via `EXEC_VIZ_WEB_DIR`; defaults
/// to the debug `dx` bundle dir so a build-then-run wrapper works as-is.
fn web_dir() -> PathBuf {
	std::env::var_os("EXEC_VIZ_WEB_DIR")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/dx/exec_viz/debug/web/public")))
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
	let path = web_dir().join("index.html");
	match tokio::fs::read_to_string(&path).await {
		Ok(s) => Html(s).into_response(),
		Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("read {}: {e}", path.display())).into_response(),
	}
}

/// The chart shim's app half, served at the root URL the wasm side dynamically imports it from
/// (`v_utils::lwc::mount(el, "/lwc_draw.js", …)`). Read live so an edit lands on the next reload.
async fn lwc_draw() -> impl IntoResponse {
	let path = PathBuf::from(ASSETS_DIR).join("lwc_draw.js");
	match tokio::fs::read_to_string(&path).await {
		Ok(s) => ([(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript"))], s).into_response(),
		Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("read {}: {e}", path.display())).into_response(),
	}
}
