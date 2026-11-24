// Audio RTP on the same peer as the data channels, against headless browsers:
// mic (sendonly) + 4 playback slots (recvonly) + 2 data channels in one offer.
// A browser Playwright doesn't have installed is skipped (CHROMIUM_PATH picks a Chromium
// binary), unless LAGLESS_REQUIRE_BROWSERS=1, as in CI's test job, which installs all three.
import test from 'node:test';
import assert from 'node:assert/strict';
import { chromium, firefox, webkit, type Browser, type Page } from 'playwright';
import os from 'node:os';
import { Server } from '../dist/index.js';
import { waitFor } from './helpers.mts';

const SLOTS = 4;

/** Firefox gathers no loopback candidates, so it needs a LAN address to pair with. */
function advertiseFor(name: string): string {
  if (name !== 'firefox') return '127.0.0.1';
  const lan = Object.values(os.networkInterfaces()).flat()
    .find((i) => i && i.family === 'IPv4' && !i.internal);
  return lan?.address ?? '127.0.0.1';
}

type Name = 'chromium' | 'firefox' | 'webkit';

async function launch(name: Name): Promise<Browser> {
  if (name === 'firefox') {
    return firefox.launch({
      firefoxUserPrefs: {
        'media.peerconnection.ice.loopback': true,
        'media.peerconnection.ice.obfuscate_host_addresses': false,
        'media.autoplay.default': 0,
      },
    });
  }
  if (name === 'webkit') return webkit.launch();
  return chromium.launch({
    executablePath: process.env.CHROMIUM_PATH,
    args: [
      '--allow-loopback-in-peer-connection',
      '--disable-features=WebRtcHideLocalIpsWithMdns',
      '--autoplay-policy=no-user-gesture-required',
    ],
  });
}

/** Launch errors (browser not installed) skip that browser's tests. */
async function available(name: Name): Promise<string | false> {
  if (process.env.LAGLESS_REQUIRE_BROWSERS === '1') return false;
  try {
    const b = await launch(name);
    const ok = await (await b.newPage()).evaluate(() => typeof RTCPeerConnection === 'function');
    await b.close();
    return ok ? false : `${name} has no WebRTC`;
  } catch (e) {
    return `${name} not installed (${String(e).split('\n')[0]})`;
  }
}

/** Browser side: build the voice-client offer, return it. */
async function offer(page: Page, withMic: boolean): Promise<string> {
  return page.evaluate(async ({ slots, withMic }) => {
    const pc = new RTCPeerConnection();
    (window as any).pc = pc;
    const u = pc.createDataChannel('u', { ordered: false, maxRetransmits: 0 });
    const r = pc.createDataChannel('r');
    (window as any).dcs = { u, r, got: [] as string[] };
    r.onmessage = (e) => (window as any).dcs.got.push(String(e.data));
    if (withMic) {
      const ctx = new AudioContext();
      const osc = ctx.createOscillator();
      const dst = ctx.createMediaStreamDestination();
      osc.connect(dst);
      osc.start();
      pc.addTransceiver(dst.stream.getAudioTracks()[0], { direction: 'sendonly' });
    } else {
      pc.addTransceiver('audio', { direction: 'sendonly' });
    }
    for (let i = 0; i < slots; i++) pc.addTransceiver('audio', { direction: 'recvonly' });
    const o = await pc.createOffer();
    await pc.setLocalDescription(o);
    return pc.localDescription!.sdp;
  }, { slots: SLOTS, withMic });
}

async function answer(page: Page, sdp: string): Promise<void> {
  await page.evaluate(async (sdp) => {
    await (window as any).pc.setRemoteDescription({ type: 'answer', sdp });
  }, sdp);
}

/** inbound-rtp packetsReceived per mid. */
async function inbound(page: Page): Promise<Record<string, number>> {
  return page.evaluate(async () => {
    const pc: RTCPeerConnection = (window as any).pc;
    const out: Record<string, number> = {};
    for (const t of pc.getTransceivers()) {
      if (!t.mid) continue;
      (await t.receiver.getStats()).forEach((s: any) => {
        if (s.type === 'inbound-rtp') out[t.mid!] = s.packetsReceived;
      });
    }
    return out;
  });
}

for (const name of ['chromium', 'firefox', 'webkit'] as const) {
  const skip = await available(name);

  test(`${name}: mic RTP in, slot RTP out, data channels on the same peer`, { timeout: 60000, skip }, async () => {
    const browser = await launch(name);
    const server = new Server();
    try {
      const page = await browser.newPage();
      const sdp = await offer(page, true);
      const acc = await server.accept({ type: 'offer', sdp }, advertiseFor(name), 0, {
        audio: {
          fmtp: 'minptime=10;useinbandfec=1;stereo=0;maxaveragebitrate=32000',
          send: Array.from({ length: SLOTS }, (_, i) => ({ ssrc: 1000 + i, streamId: 'germs', trackId: `voice${i}` })),
        },
      });
      const audio = acc.audio!;
      assert.ok(audio, 'audio negotiated');
      assert.equal(audio.recv.length, 1);
      assert.deepEqual(audio.send.map((s) => s.ssrc), [1000, 1001, 1002, 1003]);
      const mic = audio.recv[0];
      const slot0 = audio.send[0].mid;
      assert.ok(!audio.send.some((s) => s.mid === mic));

      const a = acc.answer.sdp;
      assert.match(a, /a=ssrc:1000 cname:germs/);
      assert.match(a, /a=msid:germs voice0/);
      assert.match(a, new RegExp(`a=fmtp:${audio.pt} minptime=10;useinbandfec=1;stereo=0;maxaveragebitrate=32000`));
      assert.match(a, new RegExp(`a=rtpmap:${audio.pt} opus/48000/2`, 'i'));
      assert.doesNotMatch(a, /m=audio 0 /, 'no audio m-line rejected');

      // not open before DTLS-SRTP is up
      assert.equal(server.sendRtp(acc.peer, slot0, Buffer.alloc(20)), false);
      assert.throws(() => server.sendRtp(acc.peer, mic, Buffer.alloc(20)), /no audio sender/);
      assert.throws(() => server.sendRtp(acc.peer, slot0, Buffer.alloc(4)), /12 bytes/);

      const opened = new Set<string>();
      server.on('rtpopen', (e) => { if (e.peer === acc.peer) opened.add(e.mid); });
      const errors: string[] = [];
      server.on('error', (e) => errors.push(e.message));
      let inPkts = 0;
      // SFU echo: mic packet -> slot 0 with our SSRC and the negotiated PT
      server.on('rtp', ({ peer, mid, data }) => {
        if (peer !== acc.peer) return;
        assert.equal(mid, mic);
        assert.ok(data.length >= 12 && data[0] >> 6 === 2, 'RTP v2');
        inPkts++;
        const out = Buffer.from(data);
        out[1] = (out[1] & 0x80) | audio.pt;
        out.writeUInt32BE(audio.send[0].ssrc, 8);
        try { server.sendRtp(peer, slot0, out); } catch { /* peer closed at teardown */ }
      });

      await answer(page, a);
      await waitFor('rtpopen on all audio mids', () => opened.size === SLOTS + 1, 15000);
      await waitFor('mic RTP at the server', () => inPkts >= 25, 15000);
      let got: Record<string, number> = {};
      const t0 = Date.now();
      while (Date.now() - t0 < 15000) {
        got = await inbound(page);
        if ((got[slot0] ?? 0) >= 20) break;
        await new Promise((r) => setTimeout(r, 200));
      }
      assert.ok((got[slot0] ?? 0) >= 20, `slot 0 packetsReceived ${JSON.stringify(got)}`);
      for (const s of audio.send.slice(1)) assert.ok(!got[s.mid], `slot ${s.mid} got nothing`);
      assert.deepEqual(errors, []);

      // data channels on the same peer still work
      await waitFor('reliable send', () => {
        try { return server.send(acc.peer, 'r', 'hi'); } catch { return false; }
      }, 10000);
      const t1 = Date.now();
      let dc: string[] = [];
      while (Date.now() - t1 < 5000 && dc.length === 0) {
        dc = await page.evaluate(() => (window as any).dcs.got);
        await new Promise((r) => setTimeout(r, 50));
      }
      assert.ok(dc.includes('hi'));

      const st = server.stats(acc.peer)!;
      assert.ok(st.audioPacketsIn >= 25 && st.audioBytesIn > 0);
      assert.ok(st.audioPacketsOut >= 20 && st.audioBytesOut > 0);
      assert.equal(st.sentUnreliable, 0, 'RTP is not counted as data channel traffic');
    } finally {
      server.close();
      await browser.close();
    }
  });

  test(`${name}: audio m-lines without the audio option are rejected, data channels work`, { timeout: 30000, skip }, async () => {
    const browser = await launch(name);
    const server = new Server();
    try {
      const page = await browser.newPage();
      const sdp = await offer(page, false);
      const acc = await server.accept({ type: 'offer', sdp }, advertiseFor(name));
      assert.equal(acc.audio, undefined);
      assert.equal((acc.answer.sdp.match(/m=audio 0 /g) ?? []).length, SLOTS + 1);
      await answer(page, acc.answer.sdp);
      await waitFor('reliable send', () => {
        try { return server.send(acc.peer, 'r', 'hi'); } catch { return false; }
      }, 10000);
    } finally {
      server.close();
      await browser.close();
    }
  });
}
