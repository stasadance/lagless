// Two server adapters with the same interface (one peer each).
import ndc from 'node-datachannel';
import { Server } from '../dist/index.js';
import type { PeerStats } from '../dist/index.js';

const LIMIT = 64 * 1024;

export interface BenchServer {
  name: string;
  accept(offerJson: string, shaperPort: number): Promise<{ answer: string; localPort: number }>;
  /** false = dropped by the gate / pacer */
  send(label: string, buf: Buffer, exempt: boolean): boolean;
  onMessage(fn: (label: string, buf: Buffer) => void): void;
  stats(): PeerStats | NdcStats | null;
  close(): void;
}

export interface NdcStats { bufferedAmount: Record<string, number>; dropped: number; sentUnreliable: number }

export interface RtcServerOptions { gate: boolean; rack?: boolean; rtoMin?: number; pace?: boolean }

export function rtcServer({ gate, rack = false, rtoMin, pace = false }: RtcServerOptions): BenchServer {
  const srv = new Server({ unreliableLimit: gate ? LIMIT : 0, rack, rtoMin, pacer: pace });
  let peer = 0;
  return {
    name: `rtc-${gate ? 'gate' : 'naive'}${rack ? '-rack' : ''}${pace ? '-pace' : ''}${rtoMin ? '-rto' + rtoMin : ''}`,
    async accept(offerJson, shaperPort) {
      const r = srv.accept(offerJson, '127.0.0.1', shaperPort);
      peer = r.peer;
      return { answer: JSON.stringify(r.answer), localPort: r.localPort };
    },
    send(label, buf, exempt) {
      try { return srv.send(peer, label, buf, { exempt }); } catch { return false; }
    },
    onMessage(fn) { srv.on('message', (m) => fn(m.label, m.data)); },
    stats() { return srv.stats(peer); },
    close() { srv.close(); },
  };
}

let ndcPort = 47000;
export function ndcServer({ gate }: { gate: boolean }): BenchServer {
  if (gate) ndc.setSctpSettings({ sendBufferSize: LIMIT });
  const port = ndcPort++;
  const pc = new ndc.PeerConnection('srv', {
    iceServers: [], bindAddress: '127.0.0.1', portRangeBegin: port, portRangeEnd: port,
    enableIceUdpMux: false,
  });
  const chans: Record<string, ndc.DataChannel> = {};
  let handler: (label: string, buf: Buffer) => void = () => {};
  let dropped = 0, sentU = 0;
  pc.onDataChannel((dc) => {
    chans[dc.getLabel()] = dc;
    dc.onMessage((m) => handler(dc.getLabel(), Buffer.isBuffer(m) ? m : Buffer.from(m as string)));
  });
  return {
    name: `ndc-${gate ? 'gate' : 'naive'}`,
    accept(offerJson, shaperPort) {
      const offer = JSON.parse(offerJson) as { sdp: string };
      return new Promise((resolve) => {
        pc.onGatheringStateChange((s) => {
          if (s !== 'complete') return;
          const d = pc.localDescription();
          if (!d) return;
          // keep a single host candidate pointing at the shaper
          let sdp = d.sdp.split(/\r?\n/).filter((l) => !l.startsWith('a=candidate')).join('\r\n');
          sdp = sdp.replace(/(a=mid:[^\r\n]*)/, `$1\r\na=candidate:1 1 UDP 2122317823 127.0.0.1 ${shaperPort} typ host`);
          resolve({ answer: JSON.stringify({ type: 'answer', sdp }), localPort: port });
        });
        pc.setRemoteDescription(offer.sdp, 'offer');
      });
    },
    send(label, buf, exempt) {
      const dc = chans[label];
      if (!dc || !dc.isOpen()) return false;
      const unreliable = label === 'u';
      if (unreliable && gate && !exempt) {
        for (const c of Object.values(chans)) {
          if (c.bufferedAmount() > 0) { dropped++; return false; }
        }
      }
      if (unreliable) sentU++;
      try { return dc.sendMessageBinary(buf); } catch { return false; }
    },
    onMessage(fn) { handler = fn; },
    stats() {
      const ba: Record<string, number> = {};
      for (const [k, c] of Object.entries(chans)) ba[k] = c.bufferedAmount();
      return { bufferedAmount: ba, dropped, sentUnreliable: sentU };
    },
    close() { try { pc.close(); } catch {} },
  };
}
