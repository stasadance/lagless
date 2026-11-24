---
title: Benchmarks
description: "lagless results vs node-datachannel at 200 ms RTT under packet loss."
---

## Setup

- **Server:** lagless (napi-rs over webrtc-rs `rtc`, fork `256ab18`). Only the **server to client** direction is lagless; **client to server** is Chrome's own SCTP (dcSCTP) in every row.
- **Baseline:** node-datachannel 0.33.6. "64 KiB + gate" is a verified 64 KiB SCTP send buffer (`sendBufferSize` + `maxMessageSize`, since libdatachannel floors the buffer at `maxMessageSize`, 256 KiB by default) plus dropping unreliable sends while anything is buffered.
- **Client:** real headless Chromium (dcSCTP). Channel `u` is unordered with `maxRetransmits: 0`, channel `r` is reliable and ordered.
- **Shaper:** userspace UDP, 100 ms each way, seeded loss in both directions. Optional bandwidth cap with a FIFO queue, Gilbert-Elliott bursty loss, jitter, RTT swing and several peers on one link.
- **Load:** 25 Hz ticks, 2 × 560 B unreliable state (27 KB/s) + 480 B reliable (12 KB/s). Ping every 200 ms on `u`.
- Same tick order (reliable first) for every server. Times in ms.

## lagless vs node-datachannel, 5% loss

Worse of 2 seeds, 30 s each.

| config | ping p50 | ping p95 | state age p95 | reliable age p95 |
|---|---|---|---|---|
| node-datachannel, naive | 12639 | 18386 | 18269 | 4041 |
| node-datachannel, 64 KiB + gate | 3841 | 6840 | 6730 | 4873 |
| **lagless, gate + RACK + pacer** | **214** | **341** | **293** | **821** |

## Network scenarios

1 seed, 20 s each, so read them as direction. Ping p50 / p95.

| scenario | node-datachannel | ndc 64 KiB + gate | lagless |
|---|---|---|---|
| 32 KB/s cap, 1 s queue (bufferbloat) | 5954 / 11035 | 2267 / 3443 | **271 / 440** |
| 48 KB/s cap, 500 ms queue, 2% loss | 5296 / 9780 | 3615 / 5068 | **237 / 377** |
| 8 peers on 240 KB/s, 1% loss (worst peer p95) | 11318 | 6766 | **464** |
| bursty loss (Gilbert-Elliott, ~2% in bursts of ~4) | 268 / 1814 | 219 / 1244 | **202 / 264** |
| 30 ms jitter (reordering), 1% loss | 4064 / 6508 | 3355 / 5650 | **240 / 390** |
| RTT swinging 200 → 600 ms, 1% loss | 9195 / 9632 | 859 / 6717 | **419 / 1083** |
| 400 ms RTT, 2% loss | 10820 / 13644 | 1614 / 7607 | **402 / 607** |

Weak spots: under bursty loss lagless's reliable age p95 is worse (749 vs 303 ms), and under RTT swings the pacer
drops most unreliable sends (71%) because it reads the swing as queueing. Full table:
[04-scenarios.md](https://github.com/stasadance/lagless/blob/main/bench/results/04-scenarios.md).

## Step by step, 2% and 5% loss (earlier runtime, 3 seeds × 45 s)

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

- Chrome client only. Browser to server is Chrome's dcSCTP everywhere; the numbers are about server to client.
- Scenario rows are 1 seed. Re-run with `SEEDS="1 2 3" bench/scenarios.sh` for tighter numbers.

Raw data and the harness are in [`bench/`](https://github.com/stasadance/lagless/tree/main/bench). See [Build from source](/lagless/docs/build/#benchmarks) to run it.
