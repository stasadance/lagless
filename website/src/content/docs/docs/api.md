---
title: API
description: "The lagless server API: accept, send, on('message'), budget and stats."
---

## Quick start

```ts
import { Server } from "lagless";

const server = new Server({ port: 7000 }); // one UDP port for every peer; unreliableLimit 65536, rack true, pacer true

// Signaling is yours: get the browser's offer over HTTP or WebSocket (no need to wait for ICE gathering).
const { peer, answer } = await server.accept(offer, PUBLIC_IP); // or [PUBLIC_IPV4, PUBLIC_IPV6]
// Send `answer` ({ type, sdp }) back to the browser.

server.on("message", ({ peer, label, data }) => handle(peer, label, data));
server.on("close", ({ peer, reason }) => forget(peer)); // peer is already removed

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
| `unreliableLimit` | `65536` | Drop an unreliable send when bytes SCTP hasn't released yet (pending + in flight) plus its length would exceed this. It also drops unreliable while SCTP holds more than 2400 B it can't send yet (cwnd full), so unreliable never queues ahead of reliable. `0` disables the gate. |
| `rack` | `true` | RACK-TLP loss detection on the SCTP association |
| `pacer` | `true` | Per-peer send pacer: unreliable sends need budget tokens |
| `rtoMin` | `1000` | SCTP RTO lower bound in ms (RFC 9260) |
| `bindAddress` | `"0.0.0.0"` | Local address the server's UDP socket binds to. `"::"` binds dual-stack (IPv4 + IPv6) |
| `port` | `0` | UDP port every peer shares. `0` picks a free one (see `localPort`) |
| `pacerStartRate` | `65536` | Pacer rate before the first delivery sample, bytes/s |
| `pacerMinRate` | `8192` | Pacer rate floor, bytes/s |
| `pacerBurstMs` | `100` | Token bucket depth in ms of rate (pacer and egress cap) |
| `egressRate` | off | Server-wide egress cap over all peers, bytes/s. Unreliable sends need budget from it too; reliable always goes. Not fair across peers: first sender in a tick wins |

All peers share one UDP socket and one native thread. Inbound datagrams are matched to peers by remote address, and a new address is matched by the ICE username in its STUN check.

64 KiB is a good default. Smaller caps throughput for no gain: 64 KiB still carries about 120 KB/s at 300 ms RTT.

## `localPort`

The UDP port every peer shares.

## `publicAddress(stunServer, timeoutMs = 3000)` → `Promise<{ ip, port }>`

Sends a STUN Binding request from the shared UDP port and resolves with the address the STUN server saw, which is the NAT's mapping for that port. Pass it to `accept(offer, ip, port)`. `stunServer` is `host[:port]` (port 3478 by default, a `stun:` prefix is fine). Retransmits until `timeoutMs`, then rejects. The NAT must let inbound UDP reach that mapping (1:1 NAT or a port forward).

## `accept(offer, advertiseIp, advertisePort = 0)` → `Promise<{ peer, answer, localPort }>`

Accepts an offer, either as a `{ type, sdp }` object or its JSON string. The offer can be sent before ICE gathering finishes (trickle): the server is ICE-lite and learns the client's address from its connectivity checks, so later client candidates aren't needed. Building the peer runs off the JS thread. `answer` is a `{ type, sdp }` object. It carries one host candidate per advertise IP (`advertiseIp` is a string or an array, IPv4 and/or IPv6) at `advertisePort`, so a NAT or test shaper can sit in front. `advertisePort` `0` means `localPort`, which is also returned. With several IPs of the same family, inbound packets are tagged with the first one; use them for addresses that reach the same host.

## `send(peer, label, data, { exempt }?)` → `boolean`

Sends a `Buffer` or `Uint8Array` on the channel named `label`.

- Returns `false` only for unreliable channels, when the gate, the pacer budget or the egress cap refused it. Keep that state owed and send fresher state next tick.
- Reliable sends always go, and spend budget first.
- `{ exempt: true }` bypasses the gate and the pacer. Use it for ping/pong so ping measures the real queue.
- Throws if the channel isn't open.

## `on("message", ({ peer, label, data }) => …)`

Fires for every message received from any peer. `Server` is an `EventEmitter`. Messages and events are pushed in batches from the native thread; nothing polls.

## Lifecycle events

| event | payload | when |
|---|---|---|
| `open` | `{ peer }` | ICE and DTLS connected |
| `channelopen` | `{ peer, label }` | a data channel is open; `send` on it works from now |
| `close` | `{ peer, reason }` | the peer is gone and already removed (`stats` is `null`, `send` throws). `reason`: `local` (`closePeer`/`close`), `closed` (the browser closed), `failed` (ICE/DTLS failed) |
| `error` | `PeerError` with `.peer` | something failed for one peer, e.g. rtc refused a send. Only emitted when an `error` listener exists, so it never throws |

## `budget(peer)` → `number`

Bytes the pacer allows right now. It can go negative after a burst of reliable sends.

## `stats(peer)` → `PeerStats | null`

`null` for an unknown or closed peer.

Not every field is live. `budget`, `stackOutstanding`, `pendingCmd` and the drop counters are. The SCTP fields (`srttMs`, `minRttMs`, `cwnd`, `inflight`, `pending`, RACK/T3 counters) are snapshots taken every 50 ms. `rate`, `draining` and `appLimited` change once per pacer sample, every max(100 ms, srtt).

| field | meaning |
|---|---|
| `connected`, `closed` | connection state |
| `stackOutstanding` | bytes SCTP hasn't released yet: pending + in flight |
| `pending`, `inflight` | queued but unsent, sent but unacked |
| `pendingCmd` | sends waiting for the native event loop |
| `cwnd` | congestion window |
| `srttMs`, `minRttMs` | SCTP smoothed RTT and min RTT |
| `rate`, `budget`, `draining` | pacer delivery rate, current tokens, drain mode |
| `appLimited` | the last pacer sample was app-limited (app sent less than the link carries) |
| `dropped`, `pacedDrops`, `egressDrops`, `sentUnreliable` | unreliable sends dropped (all causes) / by the pacer / by the egress cap, and sent |
| `rackLost`, `rackProbes`, `t3Timeouts` | RACK losses, tail loss probes, RTO timeouts |

## `closePeer(peer)`, `close()`

Close one peer, or close every peer and stop the native thread. Both are graceful: SCTP ABORT and DTLS close_notify go out, so the browser notices at once, and a `close` event follows for each peer.

## `native`

The raw Rust binding, if you need it.
