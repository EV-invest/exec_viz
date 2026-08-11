🌐 **[Live demo](https://ev-invest.github.io/exec_viz/)** — no setup, runs in the browser. A recorded `scam_pump_liqs` day, scrubbable: `space` steps, `b` jumps to the next bar, `c` to the next classification, a click on the chart seeks to that time.

Tick-by-tick replay for a [`trading_data`](https://github.com/EV-invest/trading_data) computation DAG — one computation structure, several interpretations of it.

Production evaluates the graph through `step`. Point `exec_viz` at the same tick chain and it runs under a recording `Observer` instead, so the browser can walk the run back: layers lighting up as they fire, element values and their local Jacobians drawn on the computation graph itself, next to a candle chart of the same day.

It is a **library, not a runner**. The app owns the graph, the feed and the runtime; it attaches a `Recorder`, hands it to its own `tick_obs`, and serves the `Viz` on a port of its choosing. Every handler reads whatever has been recorded so far, so the server runs *alongside* the recording — a live feed is scrubbable while it is still arriving.

The demo above has no server at all. `Tape::save` writes a recording down and `Viz::from_bytes` reads it back sealed, so the same front-end built with `--features demo` fetches one file and runs the cursor in the browser's own heap. Which is what it has to do: a `Viz` has exactly one cursor, so a hosted server would hand every visitor the same one to fight over.
