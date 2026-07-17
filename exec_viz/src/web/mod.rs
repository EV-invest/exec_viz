//! Dioxus WASM front-end. Built with `--no-default-features --features web --target wasm32`
//! (via `dx`); the server paths never compile here.

mod app;
mod dag;
mod keyboard;
mod state;
mod views;

pub use app::launch;
