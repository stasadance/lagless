# 02 — RACK-TLP in rtc-sctp (2026-11-22)

**Result: RACK cuts the ping tail by about 30–40%, and at 5% loss it carries about 25% more unreliable state. At 2% loss it carries less, though, because it now reacts to every lost unreliable chunk by halving cwnd.**

## What was built
These are patches on webrtc-rs/rtc @ 0fbcaa7, in [vendor/patches/](../../vendor/patches/):
- RFC 8985 time-based loss marking on each SACK, plus a reordering-window timer. A chunk counts as lost once it was sent more than reo_wnd before the newest delivered chunk, so it no longer needs 3 dup SACKs.
- Tail loss probe: PTO = 2·SRTT (+200 ms when one chunk is in flight), never later than T3. It resends the highest outstanding chunk, or sends FORWARD-TSN when everything outstanding was abandoned.
- `last_sent` per chunk, so a lost retransmission gets detected too. One cwnd cut per recovery episode.
- Off by default: `TransportConfig::with_rack`, `SettingEngine::with_sctp_rack`. Also `with_rto_min` (rtc's default RTO min is **1000 ms**, libdatachannel's is 200).
- Tests: all 212 rtc-sctp tests pass, including 3 new deterministic loss sims (100 ms RTT):
  - tail loss: 605 ms with RACK vs 1.205 s classic
  - loss without 3 dup SACKs: 375 ms vs 1.25 s
  - lost retransmission: 375 ms vs 1.25 s

## Chrome matrix (median of 3 seeds, gate on, 200 ms RTT, same harness as 01-spike.md)
| config | loss | ping p50 | ping p95 | state age p50 | state age p95 | reliable age p50 | reliable age p95 | goodput u/r KB/s | unrel dropped | seeds |
|---|---|---|---|---|---|---|---|---|---|---|
| rtc-gate | 0% | 206 | 209 | 103 | 106 | 103 | 106 | 27.3/12 | 0% | 3 |
| rtc-gate-rack | 0% | 208 | 210 | 103 | 107 | 103 | 107 | 27.3/12 | 0% | 3 |
| rtc-gate-rack-rto200 | 0% | 208 | 209 | 103 | 107 | 103 | 107 | 27.3/12 | 0% | 3 |
| rtc-gate | 2% | 234 | 698 | 212 | 633 | 2032 | 4462 | 26/11.8 | 4% | 3 |
| rtc-gate-rack | 2% | 255 | 419 | 215 | 306 | 4700 | 4970 | 18.2/11 | 30% | 3 |
| rtc-gate-rack-rto200 | 2% | 257 | 417 | 222 | 320 | 4667 | 4993 | 18.4/10.9 | 33% | 3 |
| rtc-gate | 5% | 230 | 563 | 243 | 475 | 4866 | 7254 | 12.1/11.3 | 51% | 3 |
| rtc-gate-rack | 5% | 252 | 406 | 234 | 487 | 4796 | 5043 | 15.1/11.4 | 40% | 3 |
| rtc-gate-rack-rto200 | 5% | 250 | 445 | 234 | 427 | 4855 | 5254 | 12.9/11.2 | 49% | 3 |

p99 ping at 5% loss, per seed (ms): gate 761 / 1815 / 900, gate+RACK 656 / 487 / 549.

## Read
- **p95 ping:** 698 → 419 ms at 2% loss, 563 → 406 ms at 5%. The p99 tail is gone (1.8 s worst case → 0.66 s). That's the RTO-spike fix working.
- **Goodput at 5%:** unreliable 12.1 → 15.1 KB/s.
- **Goodput at 2% is worse:** unreliable 26 → 18 KB/s, and 30% of unreliable sends get dropped. Classic SCTP mostly never noticed lost maxRetransmits=0 chunks, which got abandoned before they counted 3 misses, so cwnd stayed big. RACK sees every one of those losses and halves cwnd. That's the textbook behavior, but it costs throughput when the loss is random.
- **RTO min 200:** no consistent gain once RACK is on (TLP fires first). Leave the default.
- **Reliable stream** is still about 5 s old under overload. That's the pacer (phase 2).


## v2: cut cwnd only on reliable loss
Patch 0003: a lost chunk that won't be resent (maxRetransmits exhausted or lifetime expired) is abandoned via FORWARD-TSN and no longer enters fast recovery. Reliable loss, or loss with retries left, still halves cwnd. Two new tests cover it (214/214 pass).

| config | loss | ping p50 | ping p95 | state age p50 | state age p95 | reliable age p50 | reliable age p95 | goodput u/r KB/s | unrel dropped | seeds |
|---|---|---|---|---|---|---|---|---|---|---|
| rtc-gate | 0% | 206 | 209 | 103 | 106 | 103 | 106 | 27.3/12 | 0% | 3 |
| rtc-gate-rack | 0% | 208 | 210 | 103 | 107 | 103 | 107 | 27.3/12 | 0% | 3 |
| rtc-gate-rack-v2 | 0% | 207 | 210 | 103 | 106 | 103 | 106 | 27.3/12 | 0% | 3 |
| rtc-gate | 2% | 234 | 698 | 212 | 633 | 2032 | 4462 | 26/11.8 | 4% | 3 |
| rtc-gate-rack | 2% | 255 | 419 | 215 | 306 | 4700 | 4970 | 18.2/11 | 30% | 3 |
| rtc-gate-rack-v2 | 2% | 262 | 355 | 202 | 294 | 4574 | 4952 | 19.7/10.9 | 25% | 3 |
| rtc-gate | 5% | 230 | 563 | 243 | 475 | 4866 | 7254 | 12.1/11.3 | 51% | 3 |
| rtc-gate-rack | 5% | 252 | 406 | 234 | 487 | 4796 | 5043 | 15.1/11.4 | 40% | 3 |
| rtc-gate-rack-v2 | 5% | 237 | 390 | 226 | 300 | 4812 | 5061 | 15/11.3 | 40% | 3 |

p99 ping, v2 per seed: 2% loss 414 / 403 / 410 ms, 5% loss 449 / 414 / 405 ms. That's the flattest tail so far.

Read:
- **Tails improved again.** p95 ping is 355 ms at 2% and 390 ms at 5%. State age p95 is about 300 ms at both. Every seed's p99 is now under 450 ms, vs up to 1.8 s on the baseline.
- **Goodput at 2% came back only a little** (18.2 → 19.7 KB/s, baseline 26). So the unreliable-loss cuts were a small part of that drop. The rest most likely comes from RACK catching *reliable* losses that classic SCTP missed or caught late. I haven't verified that yet, because there's no cut-cause counter. Under overload, the gate trades that capacity for drops (25% at 2%).
- The reliable stream is still about 5 s old under overload. Same story: that's the pacer.
