---
title: API
description: "The lagless server API: acceptOffer, send, budget, drain and stats."
---

## Quick start

```js
const { Server } = require("lagless");

const server = new Server(); // unreliableLimit 65536, rack true, pacer true, bindAddress "0.0.0.0"

// Signaling is yours: get the browser's offer (gathering complete, non-trickle) over HTTP or WebSocket.
const { peer, answer } = server.acceptOffer(offerJson, PUBLIC_IP, 0);
// Send `answer` (JSON { type, sdp }) back to the browser.

setInterval(() => {
    for (const m of server.drain()) handle(m.peer, m.label, m.data); // all inbound, one call
}, 1);

function tick() {
    server.send(peer, "reliable", events, false); // reliable: always queued
    for (const snap of snapshots) {
        if (!server.send(peer, "state", snap, false)) {
            break; // unreliable: false = dropped, keep it owed and send fresher state next tick
        }
    }
}

// Latency probes bypass the gate and the pacer:
server.send(peer, "state", pong, true);
```

Browser side:

```js
const state = pc.createDataChannel("state", { ordered: false, maxRetransmits: 0 });
const reliable = pc.createDataChannel("reliable");
// Wait for ICE gathering to complete, then send pc.localDescription to your server.
```

## `new Server(options?)`

| option | default | meaning |
|---|---|---|
| `unreliableLimit` | `65536` | Drop an unreliable send when bytes SCTP hasn't released yet (pending + in flight) plus its length would exceed this. `0` disables the gate. |
| `rack` | `true` | RACK-TLP loss detection on the SCTP association |
| `pacer` | `true` | Per-peer send pacer: unreliable sends need budget tokens |
| `rtoMin` | `1000` | SCTP RTO lower bound in ms (RFC 9260) |
| `bindAddress` | `"0.0.0.0"` | Local address peer sockets bind to |

64 KiB is a good default. Smaller caps throughput for no gain: 64 KiB still carries about 120 KB/s at 300 ms RTT.

## `acceptOffer(offerJson, advertiseIp, advertisePort)` → `{ peer, answer, localPort }`

Accepts a full, non-trickle offer (`{ type, sdp }` as JSON). The answer carries one host candidate `advertiseIp:advertisePort`, so a NAT or test shaper can sit in front. `advertisePort` `0` means the bound port.

## `send(peer, label, data, exempt)` → `boolean`

Sends a `Buffer` on the channel named `label`.

- Returns `false` only for unreliable channels, when the gate or the pacer budget refused it. Keep that state owed and send fresher state next tick.
- Reliable sends always go, and spend budget first.
- `exempt = true` bypasses the gate and the pacer. Use it for ping/pong so ping measures the real queue.
- Throws if the channel isn't open.

## `budget(peer)` → `number`

Bytes the pacer allows right now. It can go negative after a burst of reliable sends.

## `drain()` → `{ peer, label, data }[]`

Every message received since the last call, from all peers, in one N-API call.

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

## `close(peer)`, `closeAll()`

Close one peer and stop its worker, or close every peer.
