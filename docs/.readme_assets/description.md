Tick-by-tick replay for a [`trading_data`](https://github.com/EV-invest/trading_data) computation DAG — one computation structure, several interpretations of it.

Production evaluates the graph through `step`. Point `exec_viz` at the same tick chain and it runs under a recording `Observer` instead, so the browser can walk the run back: layers lighting up as they fire, element values and their local Jacobians drawn on the computation graph itself, next to a candle chart of the same day.

It is a **library, not a runner**. The app owns the graph, the feed and the runtime; it attaches a `Viz`, hands it to its own `tick_obs`, and serves it on a port of its choosing. Every handler reads whatever has been recorded so far, so the server runs *alongside* the recording — a live feed is scrubbable while it is still arriving.
