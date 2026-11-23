---
title: Build from source
description: "How to build, test and release lagless from source."
---

lagless isn't published to npm yet. Prebuilt binaries for Linux x64/arm64, macOS x64/arm64 and Windows x64 will ship with the first release.

## Requirements

- Rust (stable) and Cargo
- Node.js 18+
- pnpm

## Build and test

```sh
pnpm install
pnpm build        # napi build --platform --release
pnpm test         # loopback e2e against node-datachannel
pnpm test:rust    # pacer unit tests + 214 vendored rtc-sctp tests
```

## The vendored rtc stack

[webrtc-rs/rtc](https://github.com/webrtc-rs/rtc) is vendored in `vendor/rtc` at upstream commit `0fbcaa7` (v0.21.1), with four patches from `vendor/patches/`:

| patch | what |
|---|---|
| 0001 | rtc-sctp tests bind IPv4 loopback (CI containers without IPv6) |
| 0002 | RACK-TLP loss detection (RFC 8985), behind `TransportConfig::with_rack` / `SettingEngine::with_sctp_rack`, plus a configurable RTO min |
| 0003 | Losses of chunks that won't be retransmitted don't cut cwnd |
| 0004 | `Association::transport_stats()` / `RTCPeerConnection::sctp_transport_stats()`: srtt, min RTT, cwnd, in-flight/pending/acked bytes, RACK and T3 counters |

## Benchmarks

```sh
pnpm add -D playwright && pnpm exec playwright install chromium
node bench/run.js rtc+rack+pace 1 5 1 45      # kind gate loss% seed seconds [rttMs]
bench/matrix.sh results.jsonl rtc+rack+pace rtc ndc
node bench/summarize.js results.jsonl
```

Kinds: `ndc` (node-datachannel), `rtc` (lagless, gate only), `+rack`, `+pace`, `+rto<ms>`.

## Release

```sh
pnpm version patch   # also bumps the npm/* platform packages
git push --follow-tags
```

CI builds all five platforms and publishes to npm with trusted publishing.
