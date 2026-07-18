//! Demo-day runner: downstream code wiring `trading_data_demo`'s graph into the generic
//! replay server. `cargo r --example demo` → browse the served URL.

use std::path::PathBuf;

use clap::Parser;
use exec_viz::{AppConfig, SettingsFlags, api_types::BarOut};
use trading_data_demo::nodes::{Graph, Print};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
	// `Settings` derives a `--port` override from the config field — no manual args needed.
	#[clap(flatten)]
	settings_flags: SettingsFlags,
}

/// One boot-time pass over the day collecting closed `Bar1m` outs → the static chart payload.
fn day_bars(prints: &[Print]) -> Vec<BarOut> {
	let mut graph = Graph::default();
	let mut bars = Vec::new();
	for &p in prints {
		if let Some(b) = graph.tick(Some(p)).bar {
			bars.push(BarOut {
				ts_ms: b.ts_open / 1_000_000,
				open: b.open,
				high: b.high,
				low: b.low,
				close: b.close,
				volume: b.vol_quote,
			});
		}
	}
	bars
}

#[tokio::main]
async fn main() {
	tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
	let cli = Cli::parse();
	let cfg = match AppConfig::try_build(cli.settings_flags) {
		Ok(c) => c,
		Err(e) => {
			eprintln!("Error reading config: {e:?}");
			std::process::exit(1);
		}
	};

	// Reuse trading_data's demo cache: idempotent download+ingest on first boot, instant after.
	let cache = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../trading_data/tmp/demo_cache"));
	let catalog = trading_data_demo::ensure_catalog(&cache);
	let prints = trading_data_demo::load_prints(&catalog);
	let bars = day_bars(&prints);
	exec_viz::serve::<Graph>(cfg, prints, bars).await;
}
