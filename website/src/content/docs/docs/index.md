---
title: Introduction
description: "lagless is a Node.js module for fast WebRTC data channels in io games: high packet loss, no ping spikes."
---

lagless is a Node.js server module for WebRTC data channels, built for real-time browser games. It's a native [napi-rs](https://napi.rs) binding over the [webrtc-rs `rtc`](https://github.com/webrtc-rs/rtc) sans-IO core, with a small TypeScript API on top.

The goal: ping stays near the real round-trip time even at 5% packet loss, instead of spiking to seconds.

## Install

```sh
pnpm add lagless
```

Node.js 18+. One package with prebuilt binaries bundled for Linux x64/arm64 (glibc and musl), macOS x64/arm64 and Windows x64. On anything else, [build from source](/lagless/docs/build/). Then head to the [API](/lagless/docs/api/).

## What you get

- **The true send queue.** Stack queue, in-flight bytes, cwnd, srtt and min RTT come straight from the SCTP association. No hidden buffer, so `bufferedAmount` doesn't lie.
- **Drop on backpressure.** An unreliable `send()` returns `false` when the data wasn't queued. Keep that state "owed" and send fresher state next tick.
- **Modern loss recovery.** RACK-TLP (RFC 8985) in our `rtc-sctp` patches, so a lost packet doesn't wait for a 1 s retransmission timeout.
- **Per-peer pacer.** A delay-based send budget keeps the network queue near zero, gives reliable messages their share first, and lets unreliable state fill the rest.
- **Probe exemption.** Pings skip the drop gate so they measure the real queue.

## Limits (v1)

- One host candidate per advertised IP. Behind a NAT, `publicAddress()` gets the mapped IP and port over STUN. Players behind strict NAT or UDP-blocking networks connect through a TURN server set in the browser's `iceServers`; the server needs nothing extra.
- Prebuilt targets: Linux x64/arm64 (glibc and musl), macOS x64/arm64, Windows x64. CI runs the end-to-end tests on every one of them (Linux x64 on each PR, the rest on main).
