use v_utils::macros as v_macros;

#[derive(Clone, Debug, Default, v_macros::MyConfigPrimitives, v_macros::Settings)]
pub struct AppConfig {
	/// HTTP port for the local replay server (overridable per-launch by `--port`).
	pub port: u16 = 59994,
}
