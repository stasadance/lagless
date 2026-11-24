# 04 — network scenarios + node-datachannel re-run (2026-11-24)

lagless at `71c9c4a` (one event-loop thread, one port) + rtc fork `256ab18`, node-datachannel 0.33.6
prebuilt, headless Chromium, 25 Hz ticks of 480 B reliable + 2 × 560 B unreliable per peer
(40 KB/s offered). `random5`: worse of 2 seeds × 30 s. Others: 1 seed × 20 s, so treat them as
direction, not decimals. Raw: [04-scenarios.jsonl](04-scenarios.jsonl). Times in ms.

Every row: **server → client** is the library under test, **client → server** is Chrome's dcSCTP.

What changed vs 01–03:
- Same tick order for all servers (reliable first, then unreliable).
- `ndc-gate` now really has a 64 KiB SCTP send buffer. Before, `setSctpSettings({ sendBufferSize: 65536 })`
  was silently raised to 256 KiB by libdatachannel (it floors the socket buffer at `maxMessageSize`);
  the adapter now also sets `maxMessageSize: 65536`. Checked by stalling the receiver: usrsctp accepts
  256 KiB without it and 64 KiB with it, prebuilt and from-source builds alike.
- The shaper gained a bandwidth cap with a FIFO queue, Gilbert-Elliott loss, jitter, RTT swing and
  multi-peer runs (see [bench/README.md](../README.md)).

| scenario | config | ping p50 | ping p95 | state age p50 | state age p95 | reliable age p50 | reliable age p95 | goodput u/r KB/s | unrel dropped | worst peer ping/rel p95 | seeds |
|---|---|---|---|---|---|---|---|---|---|---|---|
| random5 | rtc-gate-rack-pace | 214 | 341 | 160 | 293 | 441 | 821 | 13.3/12 | 53% |  | 2 |
| random5 | ndc-gate | 3841 | 6840 | 3636 | 6730 | 3969 | 4873 | 10.7/11.6 | 68% |  | 2 |
| random5 | ndc-naive | 12639 | 18386 | 11856 | 18269 | 3386 | 4041 | 11.9/11.8 | 0% |  | 2 |
| bloat | rtc-gate-rack-pace | 271 | 440 | 217 | 394 | 318 | 800 | 9.3/11.7 | 65% |  | 1 |
| bloat | ndc-gate | 2267 | 3443 | 1982 | 2769 | 1751 | 3270 | 10.8/11.7 | 62% |  | 1 |
| bloat | ndc-naive | 5954 | 11035 | 5379 | 10983 | 2098 | 3143 | 12.1/10.9 | 0% |  | 1 |
| bloat-loss | rtc-gate-rack-pace | 237 | 377 | 126 | 270 | 281 | 765 | 15.8/12.3 | 40% |  | 1 |
| bloat-loss | ndc-gate | 3615 | 5068 | 3117 | 4893 | 706 | 1162 | 14.4/11.7 | 43% |  | 1 |
| bloat-loss | ndc-naive | 5296 | 9780 | 5079 | 9770 | 589 | 1165 | 14.6/11.6 | 0% |  | 1 |
| contention8 | rtc-gate-rack-pace | 275 | 405 | 188 | 338 | 252 | 748 | 12.4/12.2 | 53% | 464 / 839 | 1 |
| contention8 | ndc-gate | 2344 | 5646 | 1881 | 4825 | 936 | 3046 | 15.6/11.3 | 38% | 6766 / 3706 | 1 |
| contention8 | ndc-naive | 5676 | 9844 | 5046 | 9506 | 666 | 2297 | 15.8/11.4 | 0% | 11318 / 3565 | 1 |
| bursty | rtc-gate-rack-pace | 202 | 264 | 102 | 226 | 162 | 749 | 21.1/11.8 | 19% |  | 1 |
| bursty | ndc-gate | 219 | 1244 | 102 | 1178 | 103 | 303 | 25.3/11.7 | 0% |  | 1 |
| bursty | ndc-naive | 268 | 1814 | 180 | 1670 | 111 | 363 | 26.2/11.8 | 0% |  | 1 |
| jitter | rtc-gate-rack-pace | 240 | 390 | 156 | 270 | 350 | 788 | 16.8/12 | 37% |  | 1 |
| jitter | ndc-gate | 3355 | 5650 | 2998 | 5469 | 516 | 1536 | 14.7/11.5 | 41% |  | 1 |
| jitter | ndc-naive | 4064 | 6508 | 3916 | 6352 | 240 | 881 | 18.6/12 | 0% |  | 1 |
| rtt-swing | rtc-gate-rack-pace | 419 | 1083 | 197 | 955 | 486 | 1749 | 7.2/11 | 71% |  | 1 |
| rtt-swing | ndc-gate | 859 | 6717 | 345 | 6627 | 2267 | 5236 | 13.6/11.9 | 46% |  | 1 |
| rtt-swing | ndc-naive | 9195 | 9632 | 8813 | 9328 | 2557 | 5447 | 15.4/12.3 | 0% |  | 1 |
| rtt400 | rtc-gate-rack-pace | 402 | 607 | 323 | 566 | 779 | 1299 | 6.2/11.2 | 73% |  | 1 |
| rtt400 | ndc-gate | 1614 | 7607 | 581 | 7613 | 4678 | 5686 | 6.3/10.6 | 72% |  | 1 |
| rtt400 | ndc-naive | 10820 | 13644 | 8386 | 13209 | 4460 | 5940 | 8.8/8.5 | 0% |  | 1 |

## Read
- **Bufferbloat** (`bloat`, 32 KB/s cap with a 1 s queue, below the 40 KB/s offered): node-datachannel fills
  the bottleneck queue to 1 s and tail-drops; lagless's pacer enters drain and keeps ping p50 at 271 ms. The
  "network queue > 60 ms" branch is now exercised (`draining: true`), though the bottleneck queue still peaked
  at ~460 ms once.
- **8 peers on one 240 KB/s link**: lagless worst peer ping p95 464 ms vs 6.8 s (gate) / 11.3 s (naive).
- **Bursty loss** (~2% in bursts of ~4): lagless has the best tail (ping p95 264 vs 1244 ms) but its
  reliable age p95 is worse (749 vs 303 ms) and it drops 19% of unreliable sends. Worth a look.
- **RTT swings** (200 → 600 ms every 20 s): lagless is best but weakest here: ping p95 1.08 s, 71% of
  unreliable dropped, 7 KB/s unreliable goodput. The pacer's min-RTT / drain rule reads the swing as queueing.
- **400 ms RTT, 2% loss**: ping p50 402 ms (≈ RTT) vs 1.6 s / 10.8 s; unreliable goodput is low for all three.
