---
title: How it works
description: "lagless internals: the rtc sans-IO core, RACK-TLP and the per-peer pacer."
---

```
TypeScript API (Server extends EventEmitter)
  send(peer, label, buf, { exempt }) -> bool (false = dropped, keep owed)
  budget(peer)                       -> bytes allowed right now
  stats(peer)                        -> true queue, cwnd, srtt, minRtt, counters
  on("message" / "open" / "channelopen" / "close") -> pushed in batches from the native thread
napi-rs bridge
Rust core, one event-loop thread (mio) + one UDP port shared by all peers
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
- **App-limited samples.** If the app sent less than the budget allowed and no queue built, the sample measures the app, not the link. Like BBR, it only enters the max filter when it raises it, so a quiet stretch doesn't ratchet the rate down and full sends get the old rate back at once.
- **Drain vs probe.** If outstanding bytes exceed rate × (min RTT + 40 ms) + 2 MTU, or the network queue (srtt − min RTT) is over 60 ms, the rate drops to 0.75 × the last sample. Otherwise it's 1.15 × the recent max. Floor 8 KB/s, start 64 KB/s (`pacerMinRate`, `pacerStartRate`).
- **Path RTT changes.** If two drain samples don't bring srtt down, the RTT rise isn't a queue we built (mobile RTT swing, route change). The pacer takes the current srtt as its new base instead of draining to the floor.
- **Token bucket** (0.1 s deep, `pacerBurstMs`) per peer. Reliable sends always go and spend tokens. Unreliable sends need tokens or `send()` returns `false`. Pongs are exempt.
- **Server-wide egress cap** (`egressRate`, off by default). One more token bucket shared by all peers, for a host uplink that many peers share. Same rules: reliable always goes, unreliable needs tokens. It isn't fair across peers: whoever sends first in a tick gets the budget, so rotate the send order if that matters.

The result: reliable gets its share first, unreliable fills what the link carries, and ping sits at the RTT.
