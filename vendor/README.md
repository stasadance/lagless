# vendor/rtc

[webrtc-rs/rtc](https://github.com/webrtc-rs/rtc), the sans-I/O WebRTC stack, at upstream commit
`0fbcaa74d41c90c00bf89f6389484205be467a17` (v0.21.1, 2026-11-11), with the patches in `patches/` applied.

| patch | what |
|---|---|
| 0001 | rtc-sctp tests bind IPv4 loopback (CI containers without IPv6) |
| 0002 | RACK-TLP loss detection (RFC 8985) in rtc-sctp, behind `TransportConfig::with_rack` / `SettingEngine::with_sctp_rack`; configurable RTO min |
| 0003 | RACK: losses of chunks that will not be retransmitted (unreliable, retries or lifetime exhausted) do not cut cwnd |
| 0004 | `Association::transport_stats()` / `RTCPeerConnection::sctp_transport_stats()`: srtt, min RTT, cwnd, in-flight/pending/acked bytes, RACK and T3 counters |

Trimmed from upstream: `examples/`, `benchmarks/`, `docs/`, `tests/`, and the root crate's examples and
dev-dependencies (they pull in the old `webrtc` crate). Library code is otherwise unchanged.

To refresh: check out upstream at the commit above, `git am patches/*.patch`, copy the tree here, re-apply the trim.

Upstream is dual-licensed MIT / Apache-2.0 (`rtc/LICENSE-MIT`, `rtc/LICENSE-APACHE`).
