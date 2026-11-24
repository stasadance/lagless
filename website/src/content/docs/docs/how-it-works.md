---
title: How it works
description: "lagless internals: the rtc sans-IO core, RACK-TLP and the per-peer pacer."
---

```
TypeScript API (Server extends EventEmitter)
  send(peer, label, buf, { exempt }) -> bool (false = dropped, keep owed)
  budget(peer)                       -> bytes allowed right now
  stats(peer)                        -> true queue, cwnd, srtt, minRtt, counters
  on("message")                      -> fed by one batched native drain per ms
napi-rs bridge
Rust core, one worker thread + one UDP socket per peer
  webrtc-rs rtc (stasadance/rtc fork): ICE-lite, DTLS, SCTP + RACK-TLP, exposed sender state
  own UDP loop and clock (sans-IO)
  true-queue gate (64 KiB) + per-peer pacer
```

## Why `rtc`

webrtc-rs `rtc` is a sans-IO port of Pion. It doesn't use usrsctp, so the whole SCTP send queue is in Rust and lagless can read it. We bring the sockets and the clock.

## RACK-TLP

Classic SCTP needs three duplicate SACKs, or a retransmission timeout (1000 ms minimum in `rtc`), to notice a loss. With unreliable game traffic that means long stalls.

lagless patches in RFC 8985:

- **Time-based loss marking.** A chunk is lost once it was sent more than a reordering window before the newest delivered chunk.
- **Tail loss probe.** PTO = 2 × SRTT (+200 ms when one chunk is in flight), never later than the RTO. It resends the highest outstanding chunk, or sends FORWARD-TSN if everything outstanding was abandoned.
- **Lost retransmissions** are detected too, with one cwnd cut per recovery episode.
- **No cwnd cut for abandoned chunks.** Losing an unreliable chunk that won't be resent doesn't halve cwnd. Reliable loss still does.

In deterministic sims at 100 ms RTT, tail loss recovers in 605 ms vs 1.2 s, and loss without three dup SACKs in 375 ms vs 1.25 s.

## Pacer

- **Delivery rate** is sampled from acked bytes over windows of at least one RTT, with a max filter over the last ~5 samples.
- **Drain vs probe.** If outstanding bytes exceed rate × (min RTT + 40 ms) + 2 MTU, or the network queue (srtt − min RTT) is over 60 ms, the rate drops to 0.75 × the last sample. Otherwise it's 1.15 × the recent max. Floor 8 KB/s, start 64 KB/s.
- **Token bucket** (0.1 s deep) per peer. Reliable sends always go and spend tokens. Unreliable sends need tokens or `send()` returns `false`. Pongs are exempt.

The result: reliable gets its share first, unreliable fills what the link carries, and ping sits at the RTT.
