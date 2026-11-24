<p align="center">
  <img alt="lagless" src="docs/assets/logo-light.svg#gh-light-mode-only" width="320">
  <img alt="lagless" src="docs/assets/logo-dark.svg#gh-dark-mode-only" width="320">
</p>

<p align="center">Low-latency WebRTC data channels for io games. Rust core, Node.js API.</p>

<p align="center">
  <a href="https://www.npmjs.com/package/lagless"><img alt="npm" src="https://img.shields.io/npm/v/lagless?color=1238c4"></a>
  <a href="LICENSE"><img alt="MIT license" src="https://img.shields.io/npm/l/lagless?color=1238c4"></a>
</p>

<p align="center">
  <a href="https://stasadance.github.io/lagless/">Website</a> ·
  <a href="https://stasadance.github.io/lagless/docs/">Docs</a> ·
  <a href="https://stasadance.github.io/lagless/docs/benchmarks/">Benchmarks</a>
</p>

<p align="center">
  <img alt="At 5% packet loss, classic SCTP ping is 13.6 s and lagless ping is 209 ms" src="docs/assets/hero-light.png#gh-light-mode-only" width="720">
  <img alt="At 5% packet loss, classic SCTP ping is 13.6 s and lagless ping is 209 ms" src="docs/assets/hero-dark.png#gh-dark-mode-only" width="720">
</p>

Browsers talk WebRTC data channels, and on lossy links the server side is where lag spikes come from.
lagless is built so that **stale game state gets dropped, not queued**. Built on
[webrtc-rs `rtc`](https://github.com/webrtc-rs/rtc) (sans-I/O) and [napi-rs](https://napi.rs).

## Features

- **True-queue gate.** Unreliable sends are refused when bytes SCTP hasn't released yet (pending +
  in flight) would exceed 64 KiB. No hidden library buffer, so `send()` returning `false` is honest.
- **Pacer.** Per-peer send budget from the measured delivery rate and queue/RTT growth.
  Reliable sends always go and spend budget first; unreliable fills what the link can actually carry.
- **RACK-TLP** (RFC 8985) in SCTP. Loss is detected by time, not 3 dup SACKs, and a tail loss
  probe fires after ~2·SRTT instead of waiting for a 1 s RTO that collapses cwnd.
  Losses of unreliable chunks that won't be resent don't cut cwnd.
- **Honest pings.** Latency probes bypass the gate and the pacer, so ping measures the real queue.
- **Stats you can act on.** Per-peer queue bytes, cwnd, srtt, min RTT, pacer rate and RACK/RTO counters.
- **TypeScript API** on npm, with prebuilt binaries for Linux x64/arm64, macOS x64/arm64 and Windows x64.

## Install

```sh
pnpm add lagless
```

Prebuilt binaries for Linux x64 and arm64 (glibc), macOS x64 and arm64, and Windows x64 ship inside
the package, so there's no compile step and no per-platform packages. Node >= 18.

## Usage

```ts
import { Server } from 'lagless';

const server = new Server({ port: 7000 }); // one UDP port for every peer; defaults: unreliableLimit 65536, rack true, pacer true

// signaling is yours: get the browser's offer (gather complete, non-trickle) over HTTP/WebSocket
const { peer, answer } = await server.accept(offer, PUBLIC_IP); // or [PUBLIC_IPV4, PUBLIC_IPV6]
// -> send `answer` ({ type, sdp }) back to the browser

server.on('message', ({ peer, label, data }) => handle(peer, label, data));
server.on('channelopen', ({ peer, label }) => { /* can send on `label` now */ });
server.on('close', ({ peer, reason }) => forget(peer)); // 'local' | 'closed' | 'failed'; peer is already removed

function tick() {
  server.send(peer, 'reliable', events);            // reliable: always queued
  for (const snap of snapshots) {
    if (!server.send(peer, 'state', snap)) break;   // unreliable: false = dropped, keep it owed;
  }                                                 // next tick sends fresher absolute state
}
// latency probes: server.send(peer, 'state', pong, { exempt: true }) bypasses gate and pacer
```

Browser side: `pc.createDataChannel('state', { ordered: false, maxRetransmits: 0 })` and
`pc.createDataChannel('reliable')`, wait for ICE gathering to complete, send `pc.localDescription`.

### API

| | |
|---|---|
| `new Server(opts?)` | `unreliableLimit` (bytes, 0 = off), `rack`, `pacer`, `rtoMin` (ms, default 1000), `bindAddress` (default `'0.0.0.0'`, `'::'` = dual-stack), `port` (default 0 = any) |
| `localPort` | the UDP port all peers share |
| `accept(offer, advertiseIp, advertisePort = 0)` | async. `offer` is `{ type, sdp }` or its JSON; `advertiseIp` is one IP or an array (one host candidate each, IPv4 and/or IPv6). → `Promise<{ peer, answer, localPort }>`. `advertisePort` 0 = `localPort` |
| `send(peer, label, data, { exempt }?)` | → `boolean`. `false` only for unreliable channels (gate or budget). Throws if the channel isn't open |
| `on('message', ({ peer, label, data }) => …)` | every inbound message, all peers (pushed from the native thread, no polling) |
| `on('open' \| 'channelopen' \| 'close' \| 'error', …)` | `{ peer }` connected; `{ peer, label }` channel open; `{ peer, reason }` peer gone and removed; `PeerError` (`.peer`), only emitted when an `error` listener exists |
| `budget(peer)` | bytes the pacer allows right now (negative after a reliable burst) |
| `stats(peer)` | `null` once the peer is gone. `connected, closed, stackOutstanding, pendingCmd, dropped, pacedDrops, sentUnreliable, budget, rate, draining, srttMs, minRttMs, cwnd, inflight, pending, rackLost, rackProbes, t3Timeouts` |
| `closePeer(peer)`, `close()` | graceful: SCTP ABORT + DTLS close_notify, so the browser notices at once. `close()` also stops the native thread |
| `native` | the raw Rust binding |

Unreliable messages should carry absolute state (full position, not deltas) so any one can be dropped.

## Results

Headless Chromium client, 200 ms RTT, random loss both ways, 40 KB/s offered
(27 KB/s unreliable state + 12 KB/s reliable), median of 3 seeds. Ping rides the unreliable channel.

| 5% loss | ping p50 / p95 | state age p50 / p95 | reliable age p50 / p95 |
|---|---|---|---|
| node-datachannel 0.33.6 | 13.6 s / 26.3 s | 12.1 s / 25.8 s | 1.2 s / 4.7 s |
| node-datachannel + 64 KiB buf + drop gate | 16.2 s / 21.1 s | 13.5 s / 20.9 s | 2.4 s / 4.3 s |
| **lagless** (gate + RACK + pacer) | **209 ms / 354 ms** | **157 ms / 273 ms** | **404 ms / 811 ms** |

node-datachannel rows are from the first run of the harness, which sent unreliable before reliable
each tick; the lagless row sends reliable first. At 2% loss lagless is 210 / 335 ms ping and 387 / 837 ms reliable age; at 0% it is 207 / 208 ms.
The pacer runs the link slightly under capacity (about 5–10% less goodput than RACK alone) to keep
the queue near zero. Write-ups and the harness are in [`bench/`](bench/).

## Limits (v1)

- Host candidate only (no STUN/TURN gathering on the server); non-trickle offers.
- Browser to server direction still uses the browser's own SCTP (Chrome dcSCTP); the wins are server to client.
- Builds for all five targets; end-to-end tests run on Linux x64 only. No musl (Alpine) build. The shaper does random loss only; no bandwidth-cap / bufferbloat tests yet.

## Build

```sh
pnpm install
pnpm build        # native module (napi) + TypeScript wrapper (dist/)
pnpm typecheck    # lib, tests and bench
pnpm test         # loopback e2e against node-datachannel
pnpm test:rust    # pacer unit tests
```

### rtc fork

The WebRTC stack is [stasadance/rtc](https://github.com/stasadance/rtc/tree/lagless), branch `lagless`:
upstream [webrtc-rs/rtc](https://github.com/webrtc-rs/rtc) v0.21.1 (`0fbcaa7`) plus 4 commits.
Its rtc-sctp tests (214) run there: `cargo test -p rtc-sctp`.

| commit | what |
|---|---|
| 1 | rtc-sctp tests bind IPv4 loopback (CI containers without IPv6) |
| 2 | RACK-TLP loss detection (RFC 8985) in rtc-sctp, behind `TransportConfig::with_rack` / `SettingEngine::with_sctp_rack`; configurable RTO min |
| 3 | RACK: losses of chunks that will not be retransmitted (unreliable, retries or lifetime exhausted) do not cut cwnd |
| 4 | `Association::transport_stats()` / `RTCPeerConnection::sctp_transport_stats()`: srtt, min RTT, cwnd, in-flight/pending/acked bytes, RACK and T3 counters |

## Release

Bump `version` in `package.json` and merge to `main`. The release workflow builds all five platforms,
puts every binary in the one `lagless` package and publishes it to npm with
[trusted publishing](https://docs.npmjs.com/trusted-publishers) (no token, no tag). A push whose version
is already on npm publishes nothing.

## License

MIT. The `rtc` dependency is MIT OR Apache-2.0 (upstream).
