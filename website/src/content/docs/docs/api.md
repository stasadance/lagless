---
title: API
description: "The lagless server API: send, budget, drain and stats."
---

:::caution[Spike API]
This is the API of the measured spike. v1 will keep the same ideas (bool `send`, `budget`, batched `drain`, true-queue `stats`) but names may change.
:::

## `new Server(options)`

```js
const { Server } = require("lagless");

const server = new Server({
    unreliableLimit: 64 * 1024, // drop unreliable when outstanding + len > limit; 0 = never drop
    rack: true,                 // RACK-TLP loss detection
    rtoMin: undefined,          // SCTP RTO lower bound in ms (default 1000)
    pacer: true,                // per-peer send pacer
});
```

The default send buffer is about 64 KiB. Smaller caps throughput for no gain: 64 KiB still carries about 120 KB/s at 300 ms RTT.

## `server.acceptOffer(offerJson, advertiseIp, advertisePort)`

Takes the browser's offer (JSON `{ type, sdp }`) and returns `{ peer, answer, localPort }`. lagless is signaling-agnostic: you carry the offer and answer over your own WebSocket or HTTP.

## `server.send(peer, label, data, exempt)` → `boolean`

Sends `data` (a `Buffer`) on the channel named `label`.

- Returns `false` when an unreliable message was **not queued** (queue over the limit, or no pacer budget). Keep that state owed and send fresher state next tick. Never re-queue stale state.
- Reliable sends always go.
- `exempt = true` skips the drop gate and the pacer. Use it for ping/pong so ping measures the real queue.

## `server.budget(peer)` → `number`

Bytes the pacer allows right now. Fill it with whatever matters most this tick.

## `server.drain()` → `{ peer, label, data }[]`

Every received message since the last call, in one N-API call. Call it once per tick.

## `server.stats(peer)`

| field | meaning |
|---|---|
| `connected`, `closed` | connection state |
| `stackOutstanding` | true SCTP queue: pending + in flight, bytes |
| `pending`, `inflight` | queued but unsent, sent but unacked |
| `cwnd` | congestion window |
| `srttMs`, `minRttMs` | SCTP smoothed RTT and RACK min RTT |
| `rate`, `budget`, `draining` | pacer delivery rate, tokens, drain mode |
| `dropped`, `pacedDrops`, `sentUnreliable` | unreliable sends dropped by the gate / pacer, and sent |
| `rackLost`, `rackProbes`, `t3Timeouts` | RACK losses, tail loss probes, RTO timeouts |

## `server.close(peer)`, `server.closeAll()`

Close one peer or all of them.
