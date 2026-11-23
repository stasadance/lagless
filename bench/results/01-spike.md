# 01 — spike: rtc + napi-rs + true-queue gate (2026-11-22)

**Verdict: GO.** rtc (webrtc-rs sans-IO) + napi-rs + true-queue drop gate kills the state/ping spikes. At 200 ms RTT / 5% loss: ping **228 / 485 ms** (p50/p95) vs Germs best ndc (~0.5 / 1.2–2.6 s) and naive ndc in this harness (13.6 / 26 s). Hits the target p95 ≈ RTT + one loss recovery.

**Catch: reliable stream starves under overload.** Offered 40 KB/s > loss-limited capacity (~25 KB/s at 5%). Gate drops 45% of unreliable, and reliable age climbs to ~5 s. In rtc-naive, reliable goodput went to 0 because unreliable hogs the SCTP scheduler. Fix is phase 2: pacer + priority/budget so reliable gets a guaranteed share. Goodput ceiling is the same as usrsctp (neither has RACK), so RACK is still the phase 1 item.

## Setup
- Server: `native/` napi-rs addon over webrtc-rs `rtc` @ 0fbcaa7 (ICE-lite, 1 thread + UDP socket per peer, 1 ms poll). Gate = drop unreliable when **true** outstanding (SCTP pending + inflight, `outstanding_bytes()`) + len > 64 KiB. Pong exempt.
- Baseline: node-datachannel 0.33.6. Gate = sendBufferSize 64 KiB + skip unreliable while any bufferedAmount > 0 (Germs #413).
- Client: real headless Chromium (dcSCTP), channels `u` (unordered, maxRetransmits 0) + `r` (reliable ordered).
- Shaper: userspace UDP, 100 ms each way, random loss both directions, seeded.
- Load: 25 Hz ticks, 2×560 B unreliable state (27 KB/s) + 480 B reliable (12 KB/s). Ping every 200 ms on `u`.
- 45 s per run after a 3 s warmup, 3 seeds, median across seeds. Ages = one-way (send stamp → client receive), same host clock.

## Table (median of 3 seeds, ms)
| config | loss | ping p50 | ping p95 | state age p50 | state age p95 | reliable age p50 | reliable age p95 | goodput u/r KB/s | unrel dropped | seeds |
|---|---|---|---|---|---|---|---|---|---|---|
| rtc-gate | 0% | 208 | 209 | 103 | 107 | 103 | 107 | 27.3/12 | 0% | 3 |
| rtc-naive | 0% | 208 | 209 | 103 | 106 | 103 | 106 | 27.3/12 | 0% | 3 |
| ndc-gate | 0% | 202 | 203 | 101 | 102 | 101 | 102 | 27.2/12 | 0% | 3 |
| ndc-naive | 0% | 202 | 203 | 101 | 102 | 101 | 102 | 27.3/12 | 0% | 3 |
| rtc-gate | 2% | 242 | 462 | 200 | 408 | 2204 | 4273 | 25.2/11.7 | 5% | 3 |
| rtc-naive | 2% | 264 | 616 | 190 | 538 | 1962 | 5923 | 26.6/10.3 | 0% | 3 |
| ndc-gate | 2% | 2256 | 4630 | 2105 | 4518 | 212 | 1000 | 24/11.9 | 0% | 3 |
| ndc-naive | 2% | 1607 | 3222 | 1497 | 3099 | 216 | 999 | 25.9/11.9 | 0% | 3 |
| rtc-gate | 5% | 228 | 485 | 241 | 465 | 4812 | 5992 | 13.6/11.3 | 45% | 3 |
| rtc-naive | 5% | 2348 | 3810 | 2381 | 3725 | 7810 | 8827 | 24.2/0 | 0% | 3 |
| ndc-gate | 5% | 16211 | 21084 | 13528 | 20865 | 2448 | 4313 | 11/11 | 41% | 3 |
| ndc-naive | 5% | 13618 | 26331 | 12073 | 25773 | 1239 | 4700 | 10.5/10.6 | 0% | 3 |

## Caveats
- The ndc gate did NOT reproduce the Germs win here (with a Chrome client it was worse than naive). Possible causes: the sendBufferSize setting isn't taking effect in the prebuilt 0.33.6, or dcSCTP behaves differently from the ndc client Germs used. Not verified because ndc exposes no SNDBUF getter. Treat the ndc rows as "this harness", not as Germs numbers.
- rtc adds ~6 ms at 0% loss (1 ms poll loop + JS drain interval). Fixable later (event-driven wake).
- Single peer only. No port mux yet.

## Reproduce
`bench/matrix.sh out.jsonl rtc ndc` with `GATE=0` and `GATE=1`.
