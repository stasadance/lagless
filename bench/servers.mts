// Server adapters with the same interface: one BenchServer, many peers.
import { Server } from '../dist/index.js';
import type { PeerStats } from '../dist/index.js';
import type ndcType from 'node-datachannel';

const LIMIT = 64 * 1024;

export interface BenchPeer {
  answer: string;
  localPort: number;
  /** false = dropped by the gate / pacer */
  send(label: string, buf: Buffer, exempt: boolean): boolean;
  onMessage(fn: (label: string, buf: Buffer) => void): void;
  stats(): PeerStats | NdcStats | null;
}

export interface BenchServer {
  name: string;
  accept(offerJson: string, shaperPort: number): Promise<BenchPeer>;
  close(): void;
}

export interface NdcStats { bufferedAmount: Record<string, number>; dropped: number; sentUnreliable: number }

export interface RtcServerOptions { gate: boolean; rack?: boolean; rtoMin?: number; pace?: boolean }

export function rtcServer({ gate, rack = false, rtoMin, pace = false }: RtcServerOptions): BenchServer {
  const srv = new Server({ unreliableLimit: gate ? LIMIT : 0, rack, rtoMin, pacer: pace });
  const handlers = new Map<number, (label: string, buf: Buffer) => void>();
  srv.on('message', (m) => handlers.get(m.peer)?.(m.label, m.data));
  return {
    name: `rtc-${gate ? 'gate' : 'naive'}${rack ? '-rack' : ''}${pace ? '-pace' : ''}${rtoMin ? '-rto' + rtoMin : ''}`,
    async accept(offerJson, shaperPort) {
      // await: works whether accept() is sync or returns a Promise
      const r = await srv.accept(offerJson, '127.0.0.1', shaperPort);
      const peer = r.peer;
      return {
        answer: JSON.stringify(r.answer),
        localPort: srv.localPort, // the shared server port (r.localPort is the advertised one)
        send(label, buf, exempt) {
          try { return srv.send(peer, label, buf, { exempt }); } catch { return false; }
        },
        onMessage(fn) { handlers.set(peer, fn); },
        stats() { return srv.stats(peer); },
      };
    },
    close() { srv.close(); },
  };
}

/**
 * node-datachannel baseline. `NDC_MODULE` points at another build (e.g. a from-source one) to load
 * instead of the installed prebuilt. With `gate`, the SCTP send buffer is 64 KiB (sendBufferSize +
 * maxMessageSize, verified: bench/README.md) and unreliable sends drop while anything is buffered.
 */
export async function ndcServer({ gate }: { gate: boolean }): Promise<BenchServer> {
  const mod = (await import(process.env.NDC_MODULE ?? 'node-datachannel')) as { default?: typeof ndcType } & typeof ndcType;
  const ndc = mod.default ?? mod;
  if (gate) ndc.setSctpSettings({ sendBufferSize: LIMIT });
  let nextPort = 47000 + Math.floor(Math.random() * 1000);
  const pcs: ndcType.PeerConnection[] = [];
  return {
    name: `ndc-${gate ? 'gate' : 'naive'}`,
    accept(offerJson, shaperPort) {
      const port = nextPort++;
      const pc = new ndc.PeerConnection(`srv${port}`, {
        iceServers: [], bindAddress: '127.0.0.1', portRangeBegin: port, portRangeEnd: port,
        enableIceUdpMux: false,
        // libdatachannel raises each SCTP send buffer to at least maxMessageSize (default 256 KiB),
        // so sendBufferSize alone gives 256 KiB. Cap both for a 64 KiB buffer like lagless's gate.
        ...(gate ? { maxMessageSize: LIMIT } : {}),
      });
      pcs.push(pc);
      const chans: Record<string, ndcType.DataChannel> = {};
      let handler: (label: string, buf: Buffer) => void = () => {};
      let dropped = 0, sentU = 0;
      pc.onDataChannel((dc) => {
        chans[dc.getLabel()] = dc;
        dc.onMessage((m) => handler(dc.getLabel(), Buffer.isBuffer(m) ? m : Buffer.from(m as string)));
      });
      const offer = JSON.parse(offerJson) as { sdp: string };
      return new Promise((resolve) => {
        pc.onGatheringStateChange((s) => {
          if (s !== 'complete') return;
          const d = pc.localDescription();
          if (!d) return;
          // keep a single host candidate pointing at the shaper
          let sdp = d.sdp.split(/\r?\n/).filter((l) => !l.startsWith('a=candidate')).join('\r\n');
          sdp = sdp.replace(/(a=mid:[^\r\n]*)/, `$1\r\na=candidate:1 1 UDP 2122317823 127.0.0.1 ${shaperPort} typ host`);
          resolve({
            answer: JSON.stringify({ type: 'answer', sdp }),
            localPort: port,
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
          });
        });
        pc.setRemoteDescription(offer.sdp, 'offer');
      });
    },
    close() { for (const pc of pcs) { try { pc.close(); } catch {} } },
  };
}
