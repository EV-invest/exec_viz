#![feature(default_field_values)]
//! The Dioxus wasm front-end over `exec_viz`'s wire types, built by `dx build -p exec_viz_web`.
//! Nothing here runs a server: `exec_viz` is a library, and the app that owns the graph also owns
//! the runtime, the port and this bundle — see `trading_data`'s examples.

mod dag;
mod keyboard;
mod state;
mod transport;
mod views;

use dioxus::prelude::*;

fn main() {
	dioxus::launch(App);
}

#[component]
fn App() -> Element {
	rsx! {
		document::Style { {include_str!("../assets/app.css")} }
		views::Replay {}
	}
}
