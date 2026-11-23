---
title: Benchmarks
description: "lagless spike results vs node-datachannel at 200 ms RTT under packet loss."
---

## Setup

- **Server:** the lagless spike (napi-rs over webrtc-rs `rtc` @ `0fbcaa7`), single peer.
- **Baseline:** node-datachannel 0.33.6.
- **Client:** real headless Chromium (dcSCTP). Channel `u` is unordered with `maxRetransmits: 0`, channel `r` is reliable and ordered.
- **Shaper:** userspace UDP, 100 ms each way, seeded random loss in both directions. No rate cap.
- **Load:** 25 Hz ticks, 2 × 560 B unreliable state (27 KB/s) + 480 B reliable (12 KB/s). Ping every 200 ms on `u`.
- 45 s per run after a 3 s warmup, 3 seeds, median across seeds. Times in ms.

## lagless vs node-datachannel, 5% loss

| config | ping p50 | ping p95 | state age p95 | reliable age p95 |
|---|---|---|---|---|
| node-datachannel, naive | 13618 | 26331 | 25773 | 4700 |
| node-datachannel, 64 KiB + gate | 16211 | 21084 | 20865 | 4313 |
| lagless, gate | 228 | 485 | 465 | 5992 |
| lagless, gate + RACK v2 | 237 | 390 | 300 | 5061 |
| **lagless, gate + RACK v2 + pacer** | **209** | **354** | **273** | **811** |

## Step by step, 2% and 5% loss

| config | loss | ping p50 | ping p95 | state age p50 | state age p95 | reliable age p50 | reliable age p95 | goodput u/r KB/s | unrel dropped |
|---|---|---|---|---|---|---|---|---|---|
| gate | 2% | 234 | 698 | 212 | 633 | 2032 | 4462 | 26/11.8 | 4% |
| gate + RACK v2 | 2% | 262 | 355 | 202 | 294 | 4574 | 4952 | 19.7/10.9 | 25% |
| gate + RACK v2 + pacer | 2% | 210 | 335 | 143 | 258 | 387 | 837 | 17.8/12 | 32% |
| gate | 5% | 230 | 563 | 243 | 475 | 4866 | 7254 | 12.1/11.3 | 51% |
| gate + RACK v2 | 5% | 237 | 390 | 226 | 300 | 4812 | 5061 | 15/11.3 | 40% |
| gate + RACK v2 + pacer | 5% | 209 | 354 | 157 | 273 | 404 | 811 | 13.5/11.9 | 48% |

At 0% loss every config sits at about 207 ms ping and 103 ms state age.

## Reading it

- **Ping p50 ≈ RTT** with the pacer, so there's basically no standing queue.
- **p99 ping** stays under about 450 ms on every seed with RACK v2, vs up to 1.8 s without RACK.
- **Reliable age** drops from about 5 s to 0.4 s p50 once the pacer gives reliable its share first.
- **Cost:** about 5–10% less total goodput, taken from the unreliable stream. The offered 40 KB/s is more than a 5% loss link carries (the Mathis limit is about 33 KB/s at 200 ms), so some unreliable state has to be dropped. Dropping it beats queueing it.

## Caveats

- Single peer, Chrome client, random loss only. No bandwidth cap or bufferbloat test yet.
- The node-datachannel gate didn't reproduce an earlier in-game win with this harness. Treat those rows as "this harness".
- lagless adds about 6 ms at 0% loss from the spike's 1 ms poll loop.
