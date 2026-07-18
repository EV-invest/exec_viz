//! Global keyboard control: a document keydown listener that forwards our keys into a channel;
//! the component side drains it inside the dioxus runtime (async API calls need runtime context
//! a bare JS closure doesn't have).

use futures::channel::mpsc::UnboundedSender;
use wasm_bindgen::{JsCast as _, closure::Closure};

const KEYS: [&str; 9] = [" ", "p", "-", "=", "0", "b", "c", "n", "g"];

pub fn install(tx: UnboundedSender<String>) {
	let cb = Closure::wrap(Box::new(move |e: web_sys::KeyboardEvent| {
		// A held modifier means a different chord — never ours.
		if e.ctrl_key() || e.meta_key() || e.alt_key() {
			return;
		}
		let key = e.key();
		if KEYS.contains(&key.as_str()) {
			e.prevent_default();
			let _ = tx.unbounded_send(key); // receiver lives as long as the page; a closed channel means teardown
		}
	}) as Box<dyn FnMut(_)>);
	if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
		let _ = doc.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
	}
	// Program-lifetime listener: ownership handed to the JS runtime (standard wasm pattern).
	cb.forget();
}
