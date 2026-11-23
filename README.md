# lagless

Low-latency WebRTC data channels for real-time games, as a Node.js native module.
Rust ([webrtc-rs `rtc`](https://github.com/webrtc-rs/rtc), sans-I/O) + [napi-rs](https://napi.rs).

Browsers talk WebRTC data channels; the server side is where lag spikes come from on lossy links.
lagless is built so that **stale game state gets dropped, not queued**:

- **True-queue gate.** Unreliable sends are refused when bytes SCTP hasn't released yet (pending +
  in flight) would exceed 64 KiB. No hidden library buffer, so `send()` returning `false` is honest.
- **Pacer.** Per-peer send budget from the measured delivery rate and queue/RTT growth.
  Reliable sends always go and spend budget first; unreliable fills what the link can actually carry.
- **RACK-TLP** (RFC 8985) in SCTP. Loss is detected by time, not 3 dup SACKs, and a tail loss
  probe fires after ~2·SRTT instead of waiting for a 1 s RTO that collapses cwnd.
  Losses of unreliable chunks that won't be resent don't cut cwnd.

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
the queue near zero. Raw data and the harness are in [`bench/`](bench/).

## Usage

```js
const { Server } = require('lagless');

const server = new Server(); // defaults: unreliableLimit 65536, rack true, pacer true, bindAddress '0.0.0.0'

// signaling is yours: get the browser's offer (gather complete, non-trickle) over HTTP/WebSocket
const { peer, answer } = server.acceptOffer(offerJson, PUBLIC_IP, 0);
// -> send `answer` (JSON {type, sdp}) back to the browser

setInterval(() => {
  for (const m of server.drain()) handle(m.peer, m.label, m.data); // all inbound, one call
}, 1);

function tick() {
  server.send(peer, 'reliable', events, false);        // reliable: always queued
  for (const snap of snapshots) {
    if (!server.send(peer, 'state', snap, false)) {    // unreliable: false = dropped, keep it owed
      break;                                           // next tick sends fresher absolute state
    }
  }
}
// latency probes: server.send(peer, 'state', pong, true) bypasses gate and pacer
```

Browser side: `pc.createDataChannel('state', { ordered: false, maxRetransmits: 0 })` and
`pc.createDataChannel('reliable')`, wait for ICE gathering to complete, send `pc.localDescription`.

### API

| | |
|---|---|
| `new Server(opts?)` | `unreliableLimit` (bytes, 0 = off), `rack`, `pacer`, `rtoMin` (ms, default 1000), `bindAddress` |
| `acceptOffer(offerJson, advertiseIp, advertisePort)` | → `{ peer, answer, localPort }`. `advertisePort` 0 = the bound port |
| `send(peer, label, buf, exempt)` | → `boolean`. `false` only for unreliable channels (gate or budget). Throws if the channel isn't open |
| `budget(peer)` | bytes the pacer allows right now (negative after a reliable burst) |
| `drain()` | `[{ peer, label, data }]` since the last call |
| `stats(peer)` | `connected, closed, stackOutstanding, pendingCmd, dropped, pacedDrops, sentUnreliable, budget, rate, draining, srttMs, minRttMs, cwnd, inflight, pending, rackLost, rackProbes, t3Timeouts` |
| `close(peer)`, `closeAll()` | |

Unreliable messages should carry absolute state (full position, not deltas) so any one can be dropped.

## Limits (v1)

- One worker thread and one UDP socket per peer. No single-port mux yet.
- Host candidate only (no STUN/TURN gathering on the server); non-trickle offers.
- Browser to server direction still uses the browser's own SCTP (Chrome dcSCTP); the wins are server to client.
- Tested on Linux x64. The shaper does random loss only; no bandwidth-cap / bufferbloat tests yet.

## Build

```sh
npm install
npm run build        # napi build --platform --release
npm test             # loopback e2e against node-datachannel
npm run test:rust    # pacer unit tests + 214 vendored rtc-sctp tests
```

The patched `rtc` stack is vendored in [`vendor/`](vendor/README.md).

## License

MIT OR Apache-2.0
