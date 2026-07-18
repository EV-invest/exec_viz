//! The Dioxus wasm front-end entry (`dx` builds this bin with `--no-default-features --features
//! web`). The server side is library-only: downstream code calls [`exec_viz::serve`] with its
//! own graph — see `examples/demo.rs`.

fn main() {
	exec_viz::launch_web();
}
