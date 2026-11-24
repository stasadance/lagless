// One run: headless Chromium client(s) <-> UDP shaper <-> server. Prints one JSON result line.
// usage: node bench/run.mts <kind> <gate 0|1> <lossPct> <seed> <durSec> [rttMs] [uKBps] [rKBps] [flags]
//   kind: ndc (node-datachannel baseline) | rtc[+rack][+pace][+rto<ms>] (lagless with features on)
// flags (all optional, see bench/README.md):
//   --peers N            N clients at once (one RTCPeerConnection each)
//   --rate KBps          server->client bottleneck, shared by all peers; --up-rate for client->server
//   --queue ms           bottleneck FIFO depth (default 200 ms of --rate): bufferbloat
//   --ge p,r,bad[,good]  Gilbert-Elliott bursty loss instead of random lossPct (both ways)
//   --jitter ms          uniform extra delay per packet (reorders), both ways
//   --swing ms,periodS   one-way delay swings by up to +ms over periodS seconds (mobile-like RTT)
//   --label name         scenario name written to the result
// needs playwright + chromium: pnpm exec playwright install chromium (or CHROMIUM_PATH=...)
import { parseArgs } from 'node:util';
import { chromium } from 'playwright';
import { createShaper, Link } from './shaper.mts';
import type { DirectionOptions, GilbertElliott } from './shaper.mts';
import { rtcServer, ndcServer } from './servers.mts';
import type { BenchPeer } from './servers.mts';

const { positionals, values: f } = parseArgs({
  allowPositionals: true,
  options: {
    peers: { type: 'string', default: '1' },
    rate: { type: 'string', default: '0' },
    'up-rate': { type: 'string', default: '0' },
    queue: { type: 'string', default: '200' },
    ge: { type: 'string' },
    jitter: { type: 'string', default: '0' },
    swing: { type: 'string' },
    label: { type: 'string' },
  },
});
const [kind, gateS, lossS, seedS, durS, rttS = '200', uS = '28', rS = '12'] = positionals;
const gate = gateS === '1';
const loss = Number(lossS) / 100;
const seed = Number(seedS);
const dur = Number(durS) * 1000;
const rtt = Number(rttS);
const peers = Number(f.peers);
const TICK = 40;
const uPerTick = Math.round((Number(uS) * 1024 * TICK) / 1000); // unreliable bytes/tick
const rPerTick = Math.round((Number(rS) * 1024 * TICK) / 1000); // reliable bytes/tick
const U_MSG = 560;

const ge: GilbertElliott | undefined = f.ge
  ? (([p, r, lossBad, lossGood = 0]) => ({ p, r, lossBad, lossGood }))(f.ge.split(',').map(Number))
  : undefined;
const swing = f.swing
  ? (([ms, s]) => ({ swingMs: ms, periodMs: s * 1000 }))(f.swing.split(',').map(Number))
  : undefined;
const queueMs = Number(f.queue);
const downLink = Number(f.rate) ? new Link({ rateKBps: Number(f.rate), queueMs }) : undefined;
const upLink = Number(f['up-rate']) ? new Link({ rateKBps: Number(f['up-rate']), queueMs }) : undefined;
const dir = (link?: Link): DirectionOptions => ({ loss, ge, jitterMs: Number(f.jitter), link });

function pct(a: number[], p: number): number | null {
  if (!a.length) return null;
  const s = [...a].sort((x, y) => x - y);
  return Math.round(s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))]);
}

interface ClientResult { rtts: number[]; uAge: number[]; rAge: number[]; pingsSent: number; pongs: number; uMsgs: number; rMsgs: number; uKBps: number; rKBps: number }

(async () => {
  const shapers = await Promise.all(Array.from({ length: peers }, (_, i) =>
    createShaper({ delayMs: rtt / 2, seed: seed * 1000 + i, up: dir(upLink), down: dir(downLink), swing })));
  // kind: ndc | rtc | rtc+rack | rtc+rack+rto200
  const parts = kind.split('+');
  const rtoP = parts.find((p) => p.startsWith('rto'));
  const server = parts[0] === 'rtc'
    ? rtcServer({ gate, rack: parts.includes('rack'), pace: parts.includes('pace'), rtoMin: rtoP ? Number(rtoP.slice(3)) : undefined })
    : await ndcServer({ gate });
  const sp: BenchPeer[] = [];

  const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH, args: ['--allow-loopback-in-peer-connection', '--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const page = await browser.newPage();
  await page.goto('about:blank');
  await page.exposeFunction('signal', async (i: number, offerJson: string) => {
    const p = await server.accept(offerJson, shapers[i].port);
    shapers[i].setTarget(p.localPort);
    p.onMessage((_label, buf) => {
      if (buf[0] === 2) { // ping -> pong, exempt from gate
        const out = Buffer.from(buf); out[0] = 3;
        p.send('u', out, true);
      }
    });
    sp[i] = p;
    return p.answer;
  });

  const clientResult = page.evaluate(async ({ dur, peers }) => {
    const w = window as unknown as { signal(i: number, o: string): Promise<string>; __open: number };
    w.__open = 0;
    async function client(i: number) {
      const pc = new RTCPeerConnection();
      const u = pc.createDataChannel('u', { ordered: false, maxRetransmits: 0 });
      const r = pc.createDataChannel('r', { ordered: true });
      u.binaryType = 'arraybuffer'; r.binaryType = 'arraybuffer';
      const rtts: number[] = [], uAge: number[] = [], rAge: number[] = [];
      let pingsSent = 0, pongs = 0, uMsgs = 0, rMsgs = 0, uBytes = 0, rBytes = 0;
      let start = 0;
      const onMsg = (ev: MessageEvent<ArrayBuffer>) => {
        const dv = new DataView(ev.data);
        const t = dv.getUint8(0);
        const now = Date.now();
        if (t === 3) { if (start) { pongs++; rtts.push(performance.now() - dv.getFloat64(5)); } return; }
        const age = now - dv.getFloat64(1);
        if (t === 1 && start) { uMsgs++; uBytes += ev.data.byteLength; uAge.push(age); }
        if (t === 4 && start) { rMsgs++; rBytes += ev.data.byteLength; rAge.push(age); }
      };
      u.onmessage = onMsg; r.onmessage = onMsg;
      await pc.setLocalDescription(await pc.createOffer());
      await new Promise<void>((res) => {
        if (pc.iceGatheringState === 'complete') return res();
        pc.onicegatheringstatechange = () => pc.iceGatheringState === 'complete' && res();
        setTimeout(res, 3000);
      });
      const answer = JSON.parse(await w.signal(i, JSON.stringify(pc.localDescription)));
      await pc.setRemoteDescription(answer);
      await new Promise<void>((res, rej) => {
        let n = 0; const chk = () => { if (u.readyState === 'open' && r.readyState === 'open') res(); else if (++n > 400) rej(new Error('open timeout ' + pc.connectionState)); else setTimeout(chk, 50); };
        chk();
      });
      w.__open++;
      while (w.__open < peers) await new Promise((res) => setTimeout(res, 20)); // all peers up
      await new Promise((res) => setTimeout(res, 3000)); // warmup
      start = performance.now();
      let id = 0;
      const pinger = setInterval(() => {
        const b = new ArrayBuffer(13); const dv = new DataView(b);
        dv.setUint8(0, 2); dv.setUint32(1, id++); dv.setFloat64(5, performance.now());
        try { u.send(b); pingsSent++; } catch {}
      }, 200);
      await new Promise((res) => setTimeout(res, dur));
      clearInterval(pinger);
      const secs = (performance.now() - start) / 1000;
      pc.close();
      return { rtts, uAge, rAge, pingsSent, pongs, uMsgs, rMsgs, uKBps: uBytes / 1024 / secs, rKBps: rBytes / 1024 / secs };
    }
    return Promise.all(Array.from({ length: peers }, (_, i) => client(i)));
  }, { dur, peers });

  // wait for channels open, then load
  const t0 = Date.now();
  while (((await page.evaluate(() => (window as unknown as { __open?: number }).__open).catch(() => 0)) ?? 0) < peers) {
    if (Date.now() - t0 > 20000 + 2000 * peers) { console.log(JSON.stringify({ error: 'connect timeout', label: f.label })); process.exit(1); }
    await new Promise((r) => setTimeout(r, 50));
  }
  let uOffered = 0, uAccepted = 0;
  const tick = setInterval(() => {
    const now = Date.now();
    for (const p of sp) {
      // reliable first (it always goes), then unreliable fills what is left
      const rb = Buffer.alloc(rPerTick); rb[0] = 4; rb.writeDoubleBE(now, 1);
      p.send('r', rb, false);
      for (let i = 0; i < Math.max(1, Math.round(uPerTick / U_MSG)); i++) {
        const b = Buffer.alloc(U_MSG); b[0] = 1; b.writeDoubleBE(now, 1);
        uOffered++;
        if (p.send('u', b, false)) uAccepted++; // dropped = owed; next tick carries fresh absolute state
      }
    }
  }, TICK);
  let maxQ = 0;
  const qs = setInterval(() => {
    for (const p of sp) {
      const s = p.stats();
      const q = s && 'stackOutstanding' in s ? s.stackOutstanding + s.pendingCmd : 0;
      if (q > maxQ) maxQ = q;
    }
  }, 100);

  let res: ClientResult[];
  try { res = await clientResult; } catch (e) { console.log(JSON.stringify({ error: String(e), label: f.label })); process.exit(1); }
  clearInterval(tick); clearInterval(qs);
  const all = <K extends 'rtts' | 'uAge' | 'rAge'>(k: K) => res.flatMap((r) => r[k]);
  const sum = (k: 'uKBps' | 'rKBps' | 'pingsSent') => res.reduce((a, r) => a + r[k], 0);
  const rtts = all('rtts'), uAge = all('uAge'), rAge = all('rAge');
  const out = {
    server: server.name, label: f.label, lossPct: Number(lossS), seed, rtt, peers,
    shape: { rate: Number(f.rate) || undefined, upRate: Number(f['up-rate']) || undefined, queueMs: downLink || upLink ? queueMs : undefined, ge, jitter: Number(f.jitter) || undefined, swing },
    ping: { p50: pct(rtts, 50), p95: pct(rtts, 95), p99: pct(rtts, 99), n: rtts.length, sent: sum('pingsSent') },
    uAge: { p50: pct(uAge, 50), p95: pct(uAge, 95) },
    rAge: { p50: pct(rAge, 50), p95: pct(rAge, 95) },
    // per peer: goodput is the mean per peer, worst = the worst peer's p95
    goodput: { uKBps: +(sum('uKBps') / peers).toFixed(1), rKBps: +(sum('rKBps') / peers).toFixed(1) },
    worstPeer: peers > 1 ? {
      pingP95: Math.max(...res.map((r) => pct(r.rtts, 95) ?? 0)),
      rAgeP95: Math.max(...res.map((r) => pct(r.rAge, 95) ?? 0)),
      minUKBps: +Math.min(...res.map((r) => r.uKBps)).toFixed(1),
    } : undefined,
    unreliable: { offered: uOffered, accepted: uAccepted },
    maxStackQueue: maxQ || undefined,
    serverStats: sp[0]?.stats(),
    shaper: shapers[0].stats(),
    bottleneck: downLink || upLink ? { down: downLink?.st, up: upLink?.st } : undefined,
  };
  console.log(JSON.stringify(out));
  server.close(); for (const s of shapers) s.close();
  await browser.close();
  process.exit(0);
})().catch((e) => { console.log(JSON.stringify({ error: String(e.stack || e), label: f.label })); process.exit(1); });
