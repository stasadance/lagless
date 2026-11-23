---
title: Why lagless
description: "What goes wrong with WebRTC data channels in io games under packet loss, and how lagless fixes it."
---

Browser io games send most of their state over unreliable WebRTC data channels. On a bad connection (200 ms RTT, a few percent loss) the usual Node.js stack falls apart: ping climbs to several seconds and stays there.

## Where the lag comes from

These were measured while hunting lag in a real game on node-datachannel (libdatachannel + usrsctp):

| Problem in node-datachannel / usrsctp | lagless |
|---|---|
| Hidden 1 MiB usrsctp send buffer, so `bufferedAmount` lies | Pure Rust SCTP. The whole queue (pending + in flight) is ours and exposed |
| cwnd and stack queue need a C++ patch to read | cwnd, ssthresh, srtt exposed per peer |
| Some options silently ignored | Unknown options throw |
| No RACK/TLP, so loss waits for the RTO and p95 spikes | RACK-TLP (RFC 8985), ported from Pion's approach |
| No real priority between reliable and unreliable | Per-peer pacer: reliable first, unreliable fills the rest |

## The numbers

At 200 ms RTT and 5% loss, with a real headless Chrome client:

| setup | ping p50 | ping p95 |
|---|---|---|
| node-datachannel, naive | 13.6 s | 26.3 s |
| lagless spike (drop gate + RACK + pacer) | **209 ms** | **354 ms** |

Full tables are on the [Benchmarks](/lagless/docs/benchmarks/) page.

## Why not just use WebTransport?

WebTransport (QUIC datagrams) has good loss recovery built in, and it now ships in Chrome, Edge, Firefox and Safari 26.4+. It's a real alternative. lagless is for games that already run on WebRTC data channels, or that need them for peer reachability.
