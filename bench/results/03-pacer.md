# 03 — per-peer send pacer (2026-11-22)

**Result: the 5 s reliable lag is gone.** Under overload (40 KB/s offered on a link that carries about 25–30 KB/s), reliable message age drops from about 4.8 s to **~0.4 s p50 / ~0.8 s p95**. Ping p50 sits at the RTT (~209 ms), and p95 / p99 stay at or below RACK v2 (p99 ≤ 414 ms on every seed). The cost is about 5–10% less total goodput, which comes out of the unreliable stream.

## What it is (src/pacer.rs)
- **Delivery rate:** bytes the peer acked, sampled over windows of at least one RTT (SACKs arrive in bursts, so 50 ms samples overshot by 4x). A max filter is kept over the last ~5 samples.
- **Drain vs probe:** if outstanding bytes > rate × (min RTT + 40 ms) + 2 MTU, or the network queue (srtt − min RTT) > 60 ms, the rate drops to 0.75 × the last delivery sample. Otherwise it's 1.15 × the recent max. Floor 8 KB/s, start 64 KB/s.
- **Token bucket** (0.1 s deep) per peer:
  - Reliable sends always go and spend tokens, so they can push the budget negative.
  - Unreliable sends need tokens, or `send()` returns false and the state is "owed".
  - Pongs are exempt.
  - Net effect: reliable gets its share first and unreliable fills whatever the link carries.
- **API:**
  - `new Server({ pacer: true })`
  - `server.budget(peer)` gives the bytes allowed right now.
  - `stats(peer)` now has rate, draining, srtt, minRtt, cwnd, inflight, pending, rackLost, rackProbes, t3Timeouts, pacedDrops.
- **rtc patch 0004:** `Association::transport_stats()` / `RTCPeerConnection::sctp_transport_stats()` (srtt, RACK min RTT, cwnd, in-flight/pending, acked bytes, counters).
- **Unit test:** a fluid-link sim at 20 / 60 / 300 KB/s and 200 ms RTT reaches full goodput with a standing queue of 60–180 ms (≤ ~3.6 KB on the slow link).

## Chrome matrix (median of 3 seeds, gate + RACK v2 in both, 200 ms RTT)
The tick order changed for both configs: reliable sends first, then unreliable. So both were re-run together.

| config | loss | ping p50 | ping p95 | state age p50 | state age p95 | reliable age p50 | reliable age p95 | goodput u/r KB/s | unrel dropped | seeds |
|---|---|---|---|---|---|---|---|---|---|---|
| rtc-gate-rack-pace | 0% | 207 | 208 | 103 | 106 | 103 | 106 | 27.3/12 | 0% | 3 |
| rtc-gate-rack | 0% | 208 | 209 | 103 | 106 | 103 | 106 | 27.3/12 | 0% | 3 |
| rtc-gate-rack-pace | 2% | 210 | 335 | 143 | 258 | 387 | 837 | 17.8/12 | 32% | 3 |
| rtc-gate-rack | 2% | 251 | 345 | 194 | 291 | 4382 | 4913 | 21.4/11 | 22% | 3 |
| rtc-gate-rack-pace | 5% | 209 | 354 | 157 | 273 | 404 | 811 | 13.5/11.9 | 48% | 3 |
| rtc-gate-rack | 5% | 254 | 367 | 230 | 302 | 4756 | 5052 | 15.6/11.1 | 37% | 3 |

Reliable age p95 per seed (ms), pacer vs no pacer: 2% loss 837 / 803 / 868 vs 4976 / 4913 / 4895. 5% loss 836 / 748 / 811 vs 5035 / 5052 / 5068.

## Read
- **Reliable is fixed.** About 0.4 s p50 is one RTT plus some loss recovery.
- **State updates are fresher:** state age p50 is 143–157 ms (one-way 100 ms + queue) vs 194–230 ms without the pacer.
- **Ping p50 ≈ RTT** (209–210 ms), so there's basically no standing queue. That's the whole point.
- **Cost:** total goodput 29.8 vs 32.4 KB/s at 2%, 25.4 vs 26.7 at 5%. More unreliable sends get dropped (32% / 48%), because reliable now takes its full 12 KB/s first. The pacer runs the link a little under capacity to keep the queue near zero.
- **Caveats:** single peer, Chrome client, a userspace shaper with random loss only. No bandwidth cap or bufferbloat test yet: the shaper has no rate limit, so "network queue" was never exercised, only sender queue.
