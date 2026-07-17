use dioxus::prelude::*;

use crate::web::views;

pub fn launch() {
	dioxus::launch(App);
}

#[component]
fn App() -> Element {
	rsx! {
		document::Style { {include_str!("../../assets/app.css")} }
		views::Replay {}
	}
}
