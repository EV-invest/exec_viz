### Picking a `capacity`

`capacity` is the only number `Viz::new` asks for that has no right answer — it is RAM traded
against reactivity, and the exchange rate is your graph's, not this crate's. What it constrains:

| | |
|---|---|
| `capacity / 2` | the newest ticks kept **whole** — the stretch that stays tick-exact |
| `capacity / 4` | the ceiling on the thinned *backbone*, and so how deep the squeeze goes |
| `capacity / 2` | the recycle channel's depth: one pass frees between a quarter and a half of the buffer, and the recorder takes all of it back rather than allocating |
| `capacity` | the bound on the carry-forward scan a frame does for every quiet node, which is what landing a cursor costs |

Thinning is **per node, not per tick**: a pass raises the exponent of whichever node is claiming most
of the backbone until the backbone fits, so a node firing 288 times a day keeps all 288 while the
book-driven flood beside it is decimated. What that means for sizing — **a node's whole run stays
addressable as long as its fires fit inside its max-min fair share of `capacity / 4`.** Rare nodes
are never the ones cut, so in practice the share is the whole backbone minus what the handful of
greedy nodes settle at. In the block below, at `capacity` 20 000 the backbone is 5 000: the two nodes
that each want ~3 000 land on ~1 790 apiece, and everything rarer than that keeps every fire it ever
had. Read the other way: a 5-minute bar over three days is 864 fires and is safe from about 8 000;
over a month it is 8 640 and wants ~65 000.

Bigger is not free, and the price is not where it looks. Recording cost does not move with
`capacity` at all — the graph thread's leg of a fire is the same two renderings either way. What
moves is the cost of *landing* a cursor, because a frame carries every node's standing value and a
node quiet for a while is found by scanning back for it.

The block below is one synthetic graph on one machine. `Viz::bytes` is exported so the same reading
can be taken against yours.

<!-- capacity:begin -->
```
318420 ticks (2 days at spl's 159210/day), 5 nodes, 107 B/tick of card faces
fires over the run:  Book=318420  Screen=3185  Bar:1m=2895  Bar:5m=579  Bar:1h=49

footprint — Viz::bytes over the retained ticks
     2048 │▬ 1.4 MB  (679 B/tick)
     8192 │▬ 4.2 MB  (639 B/tick)
    20000 │▬▬▬ 10.0 MB  (592 B/tick)
    65536 │▬▬▬▬▬▬▬▬▬▬▬ 34.0 MB  (557 B/tick)
   262144 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 138.2 MB  (546 B/tick)

reactivity — /api/seek mid-tape *above* a bare 55µs loopback hop, median of 64
     2048 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 11µs
     8192 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 12µs
    20000 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 13µs
    65536 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 13µs
   262144 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 31µs

absorption — the whole recording's wall clock per tick, tape-thread bound under `Block`
     2048 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 567ns
     8192 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 706ns
    20000 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 956ns
    65536 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 1085ns
   262144 │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ 1334ns

addressability — fires still reachable by `step_until`, against the run's own
  capacity  retained          Book        Screen        Bar:1m        Bar:5m        Bar:1h
     2048      2007   2007/318420      251/3185      231/2895       147/579         49/49
     8192      6557   6557/318420      691/3185      899/2895       579/579         49/49
    20000     16951  16951/318420     1796/3185     1784/2895       579/579         49/49
    65536     61047  61047/318420     3185/3185     2895/2895       579/579         49/49
   262144    252884 252884/318420     3185/3185     2895/2895       579/579         49/49
```
<!-- capacity:end -->

Produced by `cargo r --release -p exec_viz --example capacity`, which splices it in here and writes
it to `docs/capacity.txt` — the numbers are read, not typed.
