// Shared test helpers: node-datachannel clients connected to a lagless Server over loopback.
import fs from 'node:fs';
import ndc from 'node-datachannel';
import type { Server } from '../dist/index.js';

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function waitFor(what: string, fn: () => boolean, ms = 10000): Promise<void> {
  const t0 = Date.now();
  while (!fn()) {
    if (Date.now() - t0 > ms) throw new Error(`timeout waiting for ${what}`);
    await sleep(10);
  }
}

export interface Client {
  peer: number;
  pc: ndc.PeerConnection;
  u: ndc.DataChannel;
  r: ndc.DataChannel;
  got: { u: number; r: number; uBytes: number; rBytes: number };
  close(): void;
}

/** One ndc client: offer, accept on the server, wait until both channels are open both ways. */
export async function connect(server: Server, name: string, timeoutMs = 15000): Promise<Client> {
  const pc = new ndc.PeerConnection(name, { iceServers: [] });
  const u = pc.createDataChannel('u', { unordered: true, maxRetransmits: 0 });
  const r = pc.createDataChannel('r');
  const got = { u: 0, r: 0, uBytes: 0, rBytes: 0 };
  u.onMessage((m) => { got.u++; got.uBytes += (m as Buffer).length; });
  r.onMessage((m) => { got.r++; got.rBytes += (m as Buffer).length; });
  pc.setLocalDescription();
  await waitFor('gathering', () => pc.gatheringState() === 'complete', timeoutMs);
  const offer = pc.localDescription();
  if (!offer) throw new Error('no offer');
  // await: works whether accept() is sync or returns a Promise
  const acc = await server.accept({ type: 'offer', sdp: offer.sdp }, '127.0.0.1');
  pc.setRemoteDescription(acc.answer.sdp, 'answer');
  await waitFor('client channels open', () => u.isOpen() && r.isOpen(), timeoutMs);
  await waitFor('server channel open', () => {
    try { return server.send(acc.peer, 'r', Buffer.from('hello')); } catch { return false; }
  }, timeoutMs);
  return {
    peer: acc.peer, pc, u, r, got,
    close() { try { pc.close(); } catch {} },
  };
}

/** OS threads in this process, or null where we can't count them (non-Linux). */
export function threadCount(): number | null {
  try { return fs.readdirSync('/proc/self/task').length; } catch { return null; }
}

export const rssMB = () => Math.round(process.memoryUsage().rss / 1024 / 1024);
