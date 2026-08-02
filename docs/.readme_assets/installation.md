```sh
cargo add exec_viz
```

The front-end is a separate wasm bundle, built once and pointed at by an env var:

```sh
export EXEC_VIZ_WEB_DIR="$(nix run github:EV-invest/exec_viz)"  # prints where the bundle landed
```

`nix run .` builds `exec_viz_web` with a pinned `dioxus-cli`/`wasm-bindgen` pair and prints the bundle path — nothing else. Whoever wants a UI points `EXEC_VIZ_WEB_DIR` at it and starts their own binary; this repo has no business knowing their package names.

Without the env var the server panics on boot rather than serving a guessed-at (and silently stale) bundle from somewhere inside its own checkout.
