---
title: Game netcode rules
description: "App-level rules that keep io game state fresh on lossy links."
---

lagless fixes the transport. These rules are for your game code, and they matter just as much.

## Send absolute state on unreliable channels

Each unreliable message should stand on its own: positions, not deltas from a message that might be lost.

## Treat `false` as owed

When `send()` returns `false`, don't queue that message. Mark the state as owed and send the freshest version next tick.

## Keep the reliable stream lean

Reliable bytes block behind each other. Put only events that must arrive (chat, kills, inventory) on the reliable channel, and stream everything else unreliably.

## Check freshness per entity on receive

Stamp state per entity and ignore anything older than what you already applied.

## "Sent" isn't "delivered"

A sent unreliable message can still be lost. Use a sequence number or ack so you know what the client actually has, and resend fresh state for anything missing.

## Show honest ping

On the HUD, show `max(lastRtt, now − oldestUnansweredPing)`. A stall then shows up as it happens instead of after the pong finally arrives.

## Read the stats split

Compare ICE (STUN) RTT, SCTP srtt and app ping:

- STUN low and app ping high: the queue is on your server.
- Both high: the path itself is bufferbloated.
