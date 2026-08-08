### Picking a `capacity`

`capacity` is the only number `Tape::new` asks for that has no right answer — it buys scrollback with
RAM, and the exchange rate is your graph's, not this crate's. What it constrains:

| | |
|---|---|
| `capacity / 2` | the newest ticks kept **whole** — the stretch that stays tick-exact |
| `capacity / 4` | the ceiling on the thinned *backbone*, and so how deep the squeeze goes |
| `capacity / 2` | the recycle channel's depth: one pass frees between a quarter and a half of the buffer, and the recorder takes all of it back rather than allocating |

Thinning is **per node, not per tick**: a pass raises the exponent of whichever node is claiming most
of the backbone until the backbone fits, so a node firing 288 times a day keeps all 288 while the
book-driven flood beside it is decimated. What that means for sizing — **a node's whole run stays
addressable as long as its fires fit inside its max-min fair share of `capacity / 4`.** Rare nodes
are never the ones cut, so in practice the share is the whole backbone minus what the handful of
greedy nodes settle at. In the block below, at `capacity` 20 000 the backbone is 5 000: the two nodes
that each want ~3 000 land on ~1 790 apiece, and everything rarer than that keeps every fire it ever
had. Read the other way: a 5-minute bar over three days is 864 fires and is safe from about 8 000;
over a month it is 8 640 and wants ~65 000.

**The price is memory, and only memory.** Recording cost does not move with `capacity` — the graph
thread's leg of a fire is the same two renderings either way — and neither does cursor latency: every
replay op is a binary search, so a keypress costs the same against 260 000 retained ticks as against
2 000. In the block below the whole latency column is the loopback socket; the tape's own share does
not clear the noise.

That is worth stating because it was not always true. A frame carries every node's standing value,
and a node that has been quiet is found by searching back for its last fire — which used to be a
linear scan. On a 42-node graph holding a day, one node that fires twice a day was enough to put a
scrub at 139ms, growing with wherever the cursor sat. `Inner::fired` indexes it, and the synthetic
mix below now carries a twice-a-day node so the number here cannot quietly go back to being one
nobody waits for.

The block is one synthetic graph on one machine. `Viz::bytes` is exported so the same reading can be
taken against yours.

<!-- capacity:begin -->
```
318420 ticks (2 days at spl's 159210/day), 6 nodes, 107 B/tick of card faces
fires over the run:  Book=318420  Screen=3185  Bar:1m=2895  Bar:5m=579  Bar:1h=49  Cap=4

footprint — Viz::bytes over the retained ticks
     2048 │▬ 1.2 MB  (691 B/tick)
     8192 │▬ 4.0 MB  (637 B/tick)
    20000 │▬▬▬▬ 9.8 MB  (594 B/tick)
    65536 │▬▬▬▬▬▬▬▬▬▬▬▬ 34.1 MB  (558 B/tick)
   262144 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 121.0 MB  (548 B/tick)

reactivity — one /api/seek mid-tape, median of 64; the bare loopback hop under it is 53µs
     2048 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 66µs
     8192 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 68µs
    20000 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 66µs
    65536 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 66µs
   262144 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 68µs

addressability — fires still reachable by `step_until`, against the run's own
  capacity  retained          Book        Screen        Bar:1m        Bar:5m        Bar:1h           Cap
     2048      1767   1767/318420      248/3185      229/2895       147/579         49/49           4/4
     8192      6316   6316/318420      689/3185      897/2895       579/579         49/49           4/4
    20000     16395  16395/318420     1793/3185     1781/2895       579/579         49/49           4/4
    65536     61059  61059/318420     3185/3185     2895/2895       579/579         49/49           4/4
   262144    220713 220713/318420     3185/3185     2895/2895       579/579         49/49           4/4
```
<!-- capacity:end -->

Produced by `cargo r --release -p exec_viz --example capacity`, which splices it in here and writes
it to `docs/capacity.txt` — the numbers are read, not typed.
