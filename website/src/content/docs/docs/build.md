---
title: Build from source
description: "How to build, test and release lagless from source."
---

The npm package ships prebuilt binaries for Linux x64/arm64 (glibc), macOS x64/arm64 and Windows x64. Build from source on other platforms, or to work on lagless itself.

## Requirements

- Rust (stable) and Cargo
- Node.js 18+
- pnpm

## Build and test

```sh
pnpm install
pnpm build        # native module (napi) + TypeScript wrapper (dist/)
pnpm typecheck    # lib, tests and bench
pnpm test         # loopback e2e against node-datachannel
pnpm test:rust    # pacer unit tests
```

## The rtc fork

The WebRTC stack is [stasadance/rtc](https://github.com/stasadance/rtc/tree/lagless), branch `lagless`: upstream [webrtc-rs/rtc](https://github.com/webrtc-rs/rtc) v0.21.1 (`0fbcaa7`) plus 4 commits. Its 214 rtc-sctp tests run there with `cargo test -p rtc-sctp`.

| commit | what |
|---|---|
| 1 | rtc-sctp tests bind IPv4 loopback (CI containers without IPv6) |
| 2 | RACK-TLP loss detection (RFC 8985), behind `TransportConfig::with_rack` / `SettingEngine::with_sctp_rack`, plus a configurable RTO min |
| 3 | Losses of chunks that won't be retransmitted don't cut cwnd |
| 4 | `Association::transport_stats()` / `RTCPeerConnection::sctp_transport_stats()`: srtt, min RTT, cwnd, in-flight/pending/acked bytes, RACK and T3 counters |

## Benchmarks

```sh
pnpm build && pnpm exec playwright install chromium   # or CHROMIUM_PATH=/path/to/chrome
node bench/run.mts rtc+rack+pace 1 5 1 45      # kind gate loss% seed seconds [rttMs]
bench/matrix.sh results.jsonl rtc+rack+pace rtc ndc
node bench/summarize.mts results.jsonl
```

Kinds: `ndc` (node-datachannel), `rtc` (lagless, gate only), `+rack`, `+pace`, `+rto<ms>`.

## Release

```sh
pnpm version patch
git push --follow-tags
```

CI builds all five platforms and publishes to npm with trusted publishing. The per-platform packages are generated in the release job.
