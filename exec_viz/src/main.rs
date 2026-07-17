//! `exec_viz` entry — one binary, two build modes selected by feature: the axum replay server
//! (`--features server`, the default) and the Dioxus wasm front-end (`--no-default-features
//! --features web`, built by `dx`). Same crate, the feature picks which `main` body compiles.

fn main() {
	#[cfg(feature = "web")]
	exec_viz::launch_web();
	#[cfg(feature = "server")]
	server::main();
}

#[cfg(feature = "server")]
mod server {
	use clap::Parser;
	use exec_viz::{AppConfig, SettingsFlags};

	#[derive(Parser)]
	#[command(author, version, about, long_about = None)]
	struct Cli {
		// `Settings` derives a `--port` override from the config field — no manual args needed.
		#[clap(flatten)]
		settings_flags: SettingsFlags,
	}

	#[tokio::main]
	pub async fn main() {
		tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
		let cli = Cli::parse();
		let cfg = match AppConfig::try_build(cli.settings_flags) {
			Ok(c) => c,
			Err(e) => {
				eprintln!("Error reading config: {e:?}");
				std::process::exit(1);
			}
		};
		exec_viz::serve(cfg).await;
	}
}
