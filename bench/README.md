# bench

Real headless Chromium client → userspace UDP shaper (one-way delay, seeded random loss both ways) →
server (lagless or node-datachannel). 25 Hz ticks: 2 × 560 B unreliable state + 480 B reliable per tick,
ping every 200 ms on the unreliable channel. Reports ping, one-way state/reliable age (same host clock)
and goodput.

```sh
npm i --no-save playwright && npx playwright install chromium
node bench/run.js rtc+rack+pace 1 5 1 45      # kind gate loss% seed seconds [rttMs]
bench/matrix.sh results.jsonl rtc+rack+pace rtc ndc   # LOSSES, SEEDS, DUR, RTT, GATE env
node bench/summarize.js results.jsonl
```

Kinds: `ndc` (node-datachannel), `rtc` (lagless, gate only), `+rack`, `+pace`, `+rto<ms>`.

Write-ups and raw results from development: [results/](results/)
(01 spike, 02 RACK-TLP, 03 pacer). Note: 03 switched the tick to send reliable before unreliable,
so compare 03 rows with each other, not across files.
