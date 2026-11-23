---
title: Build from source
description: "How to build the lagless native addon from source."
---

lagless carries patches on top of webrtc-rs `rtc`, so building from source is a supported path. Prebuilt binaries will ship with v1.

## Requirements

- Rust (stable) and Cargo
- Node.js 22+
- A checkout of [webrtc-rs/rtc](https://github.com/webrtc-rs/rtc) with the lagless patches applied

## rtc patches

The spike runs on `rtc` at commit `0fbcaa7` with four patches:

1. `rtc-sctp` tests bind IPv4 loopback (for containers with no IPv6).
2. RACK-TLP loss detection (RFC 8985), off by default (`TransportConfig::with_rack`, `SettingEngine::with_sctp_rack`), plus `with_rto_min`.
3. RACK doesn't cut cwnd for losses of chunks that won't be retransmitted.
4. Live SCTP sender state exposed for apps (`Association::transport_stats()`, `RTCPeerConnection::sctp_transport_stats()`).

## Build the spike

```sh
# native/Cargo.toml expects webrtc-rs/rtc at ../vendor/rtc (commit 0fbcaa7, patches applied)
cd native
cargo build --release
cp target/release/libspike_native.so ../harness/spike_native.node
```

## Run the benchmark matrix

```sh
cd harness
npm i node-datachannel
./matrix.sh
node summarize.js results.jsonl
```

The harness runs a real server, a headless Chromium client and a userspace UDP shaper (one-way delay and seeded random loss in both directions).
