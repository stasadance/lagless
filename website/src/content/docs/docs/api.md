---
title: API
description: "The lagless server API: accept, send, on('message'), budget and stats."
---

## Quick start

```ts
import { Server } from "lagless";

const server = new Server(); // unreliableLimit 65536, rack true, pacer true, bindAddress "0.0.0.0"

// Signaling is yours: get the browser's offer (gathering complete, non-trickle) over HTTP or WebSocket.
const { peer, answer } = server.accept(offer, PUBLIC_IP);
// Send `answer` ({ type, sdp }) back to the browser.

server.on("message", ({ peer, label, data }) => handle(peer, label, data));

function tick() {
    server.send(peer, "reliable", events); // reliable: always queued
    for (const snap of snapshots) {
        if (!server.send(peer, "state", snap)) break; // unreliable: false = dropped, keep it owed
    } // next tick sends fresher absolute state
}

// Latency probes bypass the gate and the pacer:
server.send(peer, "state", pong, { exempt: true });
```

Browser side:

```js
const state = pc.createDataChannel("state", { ordered: false, maxRetransmits: 0 });
const reliable = pc.createDataChannel("reliable");
// Wait for ICE gathering to complete, then send pc.localDescription to your server.
```

Unreliable messages should carry absolute state (full position, not deltas) so any one can be dropped.

## `new Server(options?)`

| option | default | meaning |
|---|---|---|
| `unreliableLimit` | `65536` | Drop an unreliable send when bytes SCTP hasn't released yet (pending + in flight) plus its length would exceed this. `0` disables the gate. |
| `rack` | `true` | RACK-TLP loss detection on the SCTP association |
| `pacer` | `true` | Per-peer send pacer: unreliable sends need budget tokens |
| `rtoMin` | `1000` | SCTP RTO lower bound in ms (RFC 9260) |
| `bindAddress` | `"0.0.0.0"` | Local address peer sockets bind to |
| `drainIntervalMs` | `1` | How often received messages are pulled from the native side, in ms |

64 KiB is a good default. Smaller caps throughput for no gain: 64 KiB still carries about 120 KB/s at 300 ms RTT.

## `accept(offer, advertiseIp, advertisePort = 0)` → `{ peer, answer, localPort }`

Accepts a complete, non-trickle offer, either as a `{ type, sdp }` object or its JSON string. `answer` is a `{ type, sdp }` object. It carries one host candidate `advertiseIp:advertisePort`, so a NAT or test shaper can sit in front. `advertisePort` `0` means the port the peer's socket bound to, which is also returned as `localPort`.

## `send(peer, label, data, { exempt }?)` → `boolean`

Sends a `Buffer` or `Uint8Array` on the channel named `label`.

- Returns `false` only for unreliable channels, when the gate or the pacer budget refused it. Keep that state owed and send fresher state next tick.
- Reliable sends always go, and spend budget first.
- `{ exempt: true }` bypasses the gate and the pacer. Use it for ping/pong so ping measures the real queue.
- Throws if the channel isn't open.

## `on("message", ({ peer, label, data }) => …)`

Fires for every message received from any peer. `Server` is an `EventEmitter`.

## `budget(peer)` → `number`

Bytes the pacer allows right now. It can go negative after a burst of reliable sends.

## `stats(peer)` → `PeerStats | null`

`null` for an unknown peer.

| field | meaning |
|---|---|
| `connected`, `closed` | connection state |
| `stackOutstanding` | bytes SCTP hasn't released yet: pending + in flight |
| `pending`, `inflight` | queued but unsent, sent but unacked |
| `pendingCmd` | sends waiting for the peer's worker thread |
| `cwnd` | congestion window |
| `srttMs`, `minRttMs` | SCTP smoothed RTT and min RTT |
| `rate`, `budget`, `draining` | pacer delivery rate, current tokens, drain mode |
| `dropped`, `pacedDrops`, `sentUnreliable` | unreliable sends dropped by the gate / pacer, and sent |
| `rackLost`, `rackProbes`, `t3Timeouts` | RACK losses, tail loss probes, RTO timeouts |

## `closePeer(peer)`, `close()`

Close one peer, or close every peer and stop emitting messages.

## `native`

The raw Rust binding, if you need it.
