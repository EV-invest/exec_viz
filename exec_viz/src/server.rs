//! axum router + JSON handlers for the replay app. Router shape (nested ServeDir + SPA
//! `fallback`) mirrors `scam_pump_liqs/viz/src/server.rs`; the session is a single mutex — this
//! is a single-user study tool. Any boot failure is a loud panic — no fallbacks.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
	Json, Router,
	extract::State,
	http::{HeaderValue, StatusCode, header},
	response::{Html, IntoResponse},
	routing::{get, post},
};
use serde::Serialize;
use tokio::sync::Mutex;
use tower_http::{services::ServeDir, set_header::SetResponseHeaderLayer};

use crate::{
	api_types::{ActivationFrame, BarOut, SeekReq, StepReq, StepUntilReq, TopoNode},
	config::AppConfig,
	session::{ReplaySession, day_bars, topology},
};

/// Compile-time root of the static assets dir (sibling of `src/`); only `lwc_draw.js` is served
/// from here — the rest of the front-end is the `dx build` output.
const ASSETS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");

#[derive(Serialize)]
struct DayPayload {
	bars: Vec<BarOut>,
}

pub async fn serve(cfg: AppConfig) {
	// Reuse trading_data's demo cache: idempotent download+ingest on first boot, instant after.
	let cache = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../trading_data/tmp/demo_cache"));
	let catalog = trading_data_demo::ensure_catalog(&cache);
	let prints = Arc::new(trading_data_demo::load_prints(&catalog));
	assert!(!prints.is_empty(), "demo day produced no prints");
	let day = serde_json::to_string(&DayPayload { bars: day_bars(&prints) }).expect("day payload serializes");
	tracing::info!(prints = prints.len(), "replay session ready");

	let state = AppState {
		session: Arc::new(Mutex::new(ReplaySession::new(prints))),
		topology: Arc::new(topology()),
		day: Arc::new(day),
	};

	// Dev study tool relaunched constantly: `no-store` everywhere so a cached asset can't
	// silently break against a new API shape. All payloads here are small.
	let no_store = SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	let web = web_dir();
	let app = Router::new()
		.route("/api/topology", get(handler_topology))
		.route("/api/day", get(handler_day))
		.route("/api/status", get(handler_status))
		.route("/api/step", post(handler_step))
		.route("/api/seek", post(handler_seek))
		.route("/api/step_until", post(handler_step_until))
		.route("/lwc_draw.js", get(handler_lwc_draw))
		.layer(no_store)
		.nest_service("/wasm", ServeDir::new(web.join("wasm")))
		.nest_service("/assets", ServeDir::new(web.join("assets")))
		.fallback(handler_index)
		.with_state(state);

	let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
	let listener = tokio::net::TcpListener::bind(addr).await.unwrap_or_else(|e| panic!("bind {addr}: {e}"));
	println!("http://{addr}/");
	axum::serve(listener, app).await.expect("axum serve");
}

/// Root of the built front-end (`dx build` output). Overridable via `EXEC_VIZ_WEB_DIR`; defaults
/// to the debug `dx` bundle dir so `nix run` (build → serve) works as-is.
fn web_dir() -> PathBuf {
	std::env::var_os("EXEC_VIZ_WEB_DIR")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/dx/exec_viz/debug/web/public")))
}

#[derive(Clone)]
struct AppState {
	session: Arc<Mutex<ReplaySession>>,
	topology: Arc<Vec<TopoNode>>,
	/// Pre-serialized `/api/day` body — computed once at boot, immutable.
	day: Arc<String>,
}

impl AppState {
	/// Run a session mutation off the async runtime: a backward seek re-runs the whole day
	/// (~1s), and free-run steps can be thousands of events.
	async fn with_session(&self, f: impl FnOnce(&mut ReplaySession) -> ActivationFrame + Send + 'static) -> Json<ActivationFrame> {
		let mut guard = self.session.clone().lock_owned().await;
		Json(tokio::task::spawn_blocking(move || f(&mut guard)).await.expect("session task panicked"))
	}
}

async fn handler_topology(State(s): State<AppState>) -> impl IntoResponse {
	Json(s.topology.as_ref().clone())
}

async fn handler_day(State(s): State<AppState>) -> impl IntoResponse {
	([(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], s.day.as_str().to_owned())
}

async fn handler_status(State(s): State<AppState>) -> impl IntoResponse {
	Json(s.session.lock().await.frame())
}

async fn handler_step(State(s): State<AppState>, Json(req): Json<StepReq>) -> impl IntoResponse {
	s.with_session(move |sess| sess.step(req.n)).await
}

async fn handler_seek(State(s): State<AppState>, Json(req): Json<SeekReq>) -> impl IntoResponse {
	s.with_session(move |sess| sess.seek(req.tick)).await
}

async fn handler_step_until(State(s): State<AppState>, Json(req): Json<StepUntilReq>) -> impl IntoResponse {
	s.with_session(move |sess| sess.step_until(&req.node)).await
}

/// SPA fallback: every non-asset, non-API path serves the dx bundle's index.html (200).
async fn handler_index() -> impl IntoResponse {
	let path = web_dir().join("index.html");
	match tokio::fs::read_to_string(&path).await {
		Ok(s) => Html(s).into_response(),
		Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("read {}: {e}", path.display())).into_response(),
	}
}

/// The chart shim's app half, served at the root URL the wasm side dynamically imports it from
/// (`v_utils::lwc::mount(el, "/lwc_draw.js", …)`). Read live so an edit lands on the next reload.
async fn handler_lwc_draw() -> impl IntoResponse {
	let path = PathBuf::from(ASSETS_DIR).join("lwc_draw.js");
	match tokio::fs::read_to_string(&path).await {
		Ok(s) => ([(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript"))], s).into_response(),
		Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("read {}: {e}", path.display())).into_response(),
	}
}
