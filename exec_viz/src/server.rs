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
use trading_data_dag::Dag;

use crate::{
	api_types::{ActivationFrame, BarOut, SeekReq, StepReq, StepUntilChangeReq, StepUntilReq, TopoNode},
	config::AppConfig,
	session::{ReplaySession, topology},
};

/// Compile-time root of the static assets dir (sibling of `src/`); only `lwc_draw.js` is served
/// from here — the rest of the front-end is the `dx build` output.
const ASSETS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets");

pub async fn serve<G>(cfg: AppConfig, events: Vec<G::Event>, bars: Vec<BarOut>)
where
	G: Dag + Send + 'static,
	G::Event: Send + Sync + 'static, {
	assert!(!events.is_empty(), "empty event stream");
	let day = serde_json::to_string(&DayPayload { bars }).expect("day payload serializes");
	tracing::info!(events = events.len(), "replay session ready");

	let state = AppState {
		session: Arc::new(Mutex::new(ReplaySession::<G>::new(events))),
		topology: Arc::new(topology::<G>()),
		day: Arc::new(day),
	};

	// Dev study tool relaunched constantly: `no-store` everywhere so a cached asset can't
	// silently break against a new API shape. All payloads here are small.
	let no_store = SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
	let web = web_dir();
	let app = Router::new()
		.route("/api/topology", get(handler_topology::<G>))
		.route("/api/day", get(handler_day::<G>))
		.route("/api/status", get(handler_status::<G>))
		.route("/api/step", post(handler_step::<G>))
		.route("/api/seek", post(handler_seek::<G>))
		.route("/api/step_until", post(handler_step_until::<G>))
		.route("/api/step_until_change", post(handler_step_until_change::<G>))
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
#[derive(Serialize)]
struct DayPayload {
	bars: Vec<BarOut>,
}

/// Root of the built front-end (`dx build` output). Overridable via `EXEC_VIZ_WEB_DIR`; defaults
/// to the debug `dx` bundle dir so `nix run` (build → serve) works as-is.
fn web_dir() -> PathBuf {
	std::env::var_os("EXEC_VIZ_WEB_DIR")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/dx/exec_viz/debug/web/public")))
}

struct AppState<G: Dag> {
	session: Arc<Mutex<ReplaySession<G>>>,
	topology: Arc<Vec<TopoNode>>,
	/// Pre-serialized `/api/day` body — computed once at boot, immutable.
	day: Arc<String>,
}
impl<G: Dag + Send + 'static> AppState<G>
where
	G::Event: Send + Sync + 'static,
{
	/// Run a session mutation off the async runtime: a backward seek re-runs the whole day
	/// (~1s), and free-run steps can be thousands of events.
	async fn with_session(&self, f: impl FnOnce(&mut ReplaySession<G>) -> ActivationFrame + Send + 'static) -> Json<ActivationFrame> {
		let mut guard = self.session.clone().lock_owned().await;
		Json(tokio::task::spawn_blocking(move || f(&mut guard)).await.expect("session task panicked"))
	}
}

// Manual: derive(Clone) would demand `G: Clone` — the fields are all Arcs.
impl<G: Dag> Clone for AppState<G> {
	fn clone(&self) -> Self {
		Self {
			session: self.session.clone(),
			topology: self.topology.clone(),
			day: self.day.clone(),
		}
	}
}

async fn handler_topology<G: Dag>(State(s): State<AppState<G>>) -> impl IntoResponse {
	Json(s.topology.as_ref().clone())
}

async fn handler_day<G: Dag>(State(s): State<AppState<G>>) -> impl IntoResponse {
	([(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], s.day.as_str().to_owned())
}

async fn handler_status<G: Dag>(State(s): State<AppState<G>>) -> impl IntoResponse {
	Json(s.session.lock().await.frame())
}

async fn handler_step<G: Dag + Send + 'static>(State(s): State<AppState<G>>, Json(req): Json<StepReq>) -> impl IntoResponse
where
	G::Event: Send + Sync + 'static, {
	s.with_session(move |sess| sess.step(req.n)).await
}

async fn handler_seek<G: Dag + Send + 'static>(State(s): State<AppState<G>>, Json(req): Json<SeekReq>) -> impl IntoResponse
where
	G::Event: Send + Sync + 'static, {
	s.with_session(move |sess| sess.seek(req.tick)).await
}

async fn handler_step_until<G: Dag + Send + 'static>(State(s): State<AppState<G>>, Json(req): Json<StepUntilReq>) -> impl IntoResponse
where
	G::Event: Send + Sync + 'static, {
	s.with_session(move |sess| sess.step_until(&req.node)).await
}

async fn handler_step_until_change<G: Dag + Send + 'static>(State(s): State<AppState<G>>, Json(req): Json<StepUntilChangeReq>) -> impl IntoResponse
where
	G::Event: Send + Sync + 'static, {
	s.with_session(move |sess| sess.step_until_change(&req.nodes)).await
}

/// SPA fallback: every non-asset, non-API *GET* serves the dx bundle's index.html (200) so a
/// client-side deep-link boots the wasm router. A non-GET here is an unknown API call (e.g. a
/// stale server missing a route the fresh bundle calls) — 404 it loudly instead of returning
/// HTML that the client then fails to parse as JSON, masking the real cause.
async fn handler_index(method: axum::http::Method) -> impl IntoResponse {
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
async fn handler_lwc_draw() -> impl IntoResponse {
	let path = PathBuf::from(ASSETS_DIR).join("lwc_draw.js");
	match tokio::fs::read_to_string(&path).await {
		Ok(s) => ([(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript"))], s).into_response(),
		Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("read {}: {e}", path.display())).into_response(),
	}
}
