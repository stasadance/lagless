// One run: headless Chromium client <-> UDP shaper <-> server. Prints one JSON result line.
// usage: node bench/run.js <kind> <gate 0|1> <lossPct> <seed> <durSec> [rttMs] [uKBps] [rKBps]
//   kind: ndc (node-datachannel baseline) | rtc[+rack][+pace][+rto<ms>] (lagless with features on)
// needs playwright + chromium: pnpm add -D playwright && pnpm exec playwright install chromium
import { chromium } from 'playwright';
import { createShaper } from './shaper.mts';
import { rtcServer, ndcServer } from './servers.mts';

const [kind, gateS, lossS, seedS, durS, rttS = '200', uS = '28', rS = '12'] = process.argv.slice(2);
const gate = gateS === '1';
const loss = Number(lossS) / 100;
const seed = Number(seedS);
const dur = Number(durS) * 1000;
const rtt = Number(rttS);
const TICK = 40;
const uPerTick = Math.round((Number(uS) * 1024 * TICK) / 1000); // unreliable bytes/tick
const rPerTick = Math.round((Number(rS) * 1024 * TICK) / 1000); // reliable bytes/tick
const U_MSG = 560;

function pct(a: number[], p: number): number | null {
  if (!a.length) return null;
  const s = [...a].sort((x, y) => x - y);
  return Math.round(s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))]);
}

(async () => {
  const shaper = await createShaper({ delayMs: rtt / 2, loss, seed });
  // kind: ndc | rtc | rtc+rack | rtc+rack+rto200
  const parts = kind.split('+');
  const rtoP = parts.find((p) => p.startsWith('rto'));
  const server = parts[0] === 'rtc'
    ? rtcServer({ gate, rack: parts.includes('rack'), pace: parts.includes('pace'), rtoMin: rtoP ? Number(rtoP.slice(3)) : undefined })
    : ndcServer({ gate });

  server.onMessage((_label, buf) => {
    if (buf[0] === 2) { // ping -> pong, exempt from gate
      const out = Buffer.from(buf); out[0] = 3;
      server.send('u', out, true);
    }
  });

  const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_PATH, args: ['--allow-loopback-in-peer-connection', '--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const page = await browser.newPage();
  await page.goto('about:blank');
  await page.exposeFunction('signal', async (offerJson: string) => {
    const r = await server.accept(offerJson, shaper.port);
    shaper.setTarget(r.localPort);
    return r.answer;
  });

  const clientResult = page.evaluate(async ({ dur }) => {
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
      if (t === 3) { pongs++; rtts.push(performance.now() - dv.getFloat64(5)); return; }
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
    const answer = JSON.parse(await (window as unknown as { signal(o: string): Promise<string> }).signal(JSON.stringify(pc.localDescription)));
    await pc.setRemoteDescription(answer);
    await new Promise<void>((res, rej) => {
      let n = 0; const chk = () => { if (u.readyState === 'open' && r.readyState === 'open') res(); else if (++n > 300) rej(new Error('open timeout ' + pc.connectionState)); else setTimeout(chk, 50); };
      chk();
    });
    (window as unknown as { __open: boolean }).__open = true;
    await new Promise((res) => setTimeout(res, 3000)); // warmup
    start = performance.now();
    rtts.length = 0;
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
  }, { dur });

  // wait for channels open, then load
  const t0 = Date.now();
  while (!(await page.evaluate(() => (window as unknown as { __open?: boolean }).__open).catch(() => false))) {
    if (Date.now() - t0 > 20000) { console.log(JSON.stringify({ error: 'connect timeout' })); process.exit(1); }
    await new Promise((r) => setTimeout(r, 50));
  }
  let uOffered = 0, uAccepted = 0;
  const tick = setInterval(() => {
    const now = Date.now();
    // reliable first (it always goes), then unreliable fills what is left
    const rb = Buffer.alloc(rPerTick); rb[0] = 4; rb.writeDoubleBE(now, 1);
    server.send('r', rb, false);
    for (let i = 0; i < Math.max(1, Math.round(uPerTick / U_MSG)); i++) {
      const b = Buffer.alloc(U_MSG); b[0] = 1; b.writeDoubleBE(now, 1);
      uOffered++;
      if (server.send('u', b, false)) uAccepted++; // dropped = owed; next tick carries fresh absolute state
    }
  }, TICK);
  let maxQ = 0;
  const qs = setInterval(() => {
    const s = server.stats();
    const q = s && 'stackOutstanding' in s ? s.stackOutstanding + s.pendingCmd : 0;
    if (q > maxQ) maxQ = q;
  }, 100);

  let res: Awaited<typeof clientResult>;
  try { res = await clientResult; } catch (e) { console.log(JSON.stringify({ error: String(e) })); process.exit(1); }
  clearInterval(tick); clearInterval(qs);
  const out = {
    server: server.name, lossPct: Number(lossS), seed, rtt,
    ping: { p50: pct(res.rtts, 50), p95: pct(res.rtts, 95), p99: pct(res.rtts, 99), n: res.rtts.length, sent: res.pingsSent },
    uAge: { p50: pct(res.uAge, 50), p95: pct(res.uAge, 95) },
    rAge: { p50: pct(res.rAge, 50), p95: pct(res.rAge, 95) },
    goodput: { uKBps: +res.uKBps.toFixed(1), rKBps: +res.rKBps.toFixed(1) },
    unreliable: { offered: uOffered, accepted: uAccepted },
    maxStackQueue: maxQ || undefined,
    serverStats: server.stats(),
    shaper: shaper.stats(),
  };
  console.log(JSON.stringify(out));
  server.close(); shaper.close();
  await browser.close();
  process.exit(0);
})().catch((e) => { console.log(JSON.stringify({ error: String(e.stack || e) })); process.exit(1); });
