# bench

Real headless Chromium client(s) → userspace UDP shaper → server (lagless or node-datachannel).
25 Hz ticks per peer: reliable first (480 B), then 2 × 560 B unreliable state, ping every 200 ms on the
unreliable channel. Reports ping, one-way state/reliable age (same host clock) and goodput.

Only the **server → client** direction is lagless. Client → server is Chrome's own SCTP (dcSCTP) in
every row, including the ping's outbound leg.

```sh
pnpm build && pnpm exec playwright install chromium   # or CHROMIUM_PATH=/path/to/chrome
node bench/run.mts rtc+rack+pace 1 5 1 45      # kind gate loss% seed seconds [rttMs] [uKBps] [rKBps] [flags]
bench/matrix.sh results.jsonl rtc+rack+pace rtc ndc   # LOSSES, SEEDS, DUR, RTT, GATE env
bench/scenarios.sh scenarios.jsonl                    # kind:gate list, SEEDS, DUR, ONLY env
node bench/summarize.mts results.jsonl
```

Kinds: `ndc` (node-datachannel), `rtc` (lagless, gate only), `+rack`, `+pace`, `+rto<ms>`.

## Shaper flags

| flag | what |
|---|---|
| `--peers N` | N clients at once, one RTCPeerConnection each, each with its own shaper |
| `--rate KBps` | server → client bottleneck shared by all peers (FIFO, tail drop); `--up-rate` for client → server |
| `--queue ms` | bottleneck queue depth at `--rate` (default 200). Big = bufferbloat |
| `--ge p,r,bad[,good]` | Gilbert-Elliott bursty loss: good→bad with p per packet, bad→good with r, loss `bad` / `good` in each state. Replaces random loss |
| `--jitter ms` | uniform extra delay per packet, so packets reorder |
| `--swing ms,periodS` | one-way delay rises by up to `ms` and back over `periodS` (mobile-like RTT swings) |
| `--label name` | scenario name in the output |

`scenarios.sh` runs: `random5` (5% random), `bloat` (32 KB/s cap, 1 s queue), `bloat-loss`
(48 KB/s, 500 ms queue, 2%), `contention8` (8 peers on a 240 KB/s link, 300 ms queue, 1%),
`bursty` (GE 0.01/0.25/0.5, ~2% loss in bursts of ~4), `jitter` (30 ms, 1%), `rtt-swing`
(RTT 200 → 600 ms every 20 s, 1%) and `rtt400` (400 ms RTT, 2%).

## node-datachannel baseline

`ndc` with gate 1 means 64 KiB SCTP send buffer + "drop unreliable while anything is buffered".
`setSctpSettings({ sendBufferSize })` alone is not enough: libdatachannel 0.24.x raises each socket's
send buffer to at least `maxMessageSize` (default 256 KiB), so the adapter also sets
`maxMessageSize: 65536`. Verified by stalling the receiver and counting what usrsctp accepts
(prebuilt and from-source 0.33.6 behave the same: 256 KiB without it, 64 KiB with it).
`NDC_MODULE=/path/to/node-datachannel/dist/esm/lib/index.mjs` loads another build instead of the
installed one.

Write-ups: [results/](results/) (01 spike, 02 RACK-TLP, 03 pacer, 04 scenarios + re-run baseline).
Rows in 01/02 used an older tick order (unreliable first) and the 256 KiB ndc buffer; compare 04 rows
with each other.
