// Userspace UDP shaper: one-way delay + random loss each way (seeded).
import dgram from 'node:dgram';
import type { RemoteInfo } from 'node:dgram';
import type { AddressInfo } from 'node:net';

export interface ShaperOptions { delayMs: number; loss: number; seed: number }
export type Shaper = Awaited<ReturnType<typeof createShaper>>;

function mulberry32(a: number): () => number {
  return function () {
    a |= 0; a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export async function createShaper({ delayMs, loss, seed }: ShaperOptions) {
  const front = dgram.createSocket('udp4'); // client side
  const back = dgram.createSocket('udp4');  // server side
  const rndUp = mulberry32(seed * 2 + 1);
  const rndDown = mulberry32(seed * 2 + 2);
  let client: RemoteInfo | null = null;
  let target: { address: string; port: number } | null = null;
  const st = { up: 0, upDrop: 0, down: 0, downDrop: 0 };
  front.on('message', (msg, rinfo) => {
    client = rinfo;
    if (!target) return;
    st.up++;
    if (rndUp() < loss) { st.upDrop++; return; }
    const t = target;
    setTimeout(() => { try { back.send(msg, t.port, t.address); } catch {} }, delayMs);
  });
  back.on('message', (msg) => {
    if (!client) return;
    st.down++;
    if (rndDown() < loss) { st.downDrop++; return; }
    const c: RemoteInfo = client;
    setTimeout(() => { try { front.send(msg, c.port, c.address); } catch {} }, delayMs);
  });
  await new Promise<void>((r) => front.bind(0, '127.0.0.1', r));
  await new Promise<void>((r) => back.bind(0, '127.0.0.1', r));
  return {
    port: (front.address() as AddressInfo).port,
    setTarget(port: number) { target = { address: '127.0.0.1', port }; },
    stats: () => ({ ...st }),
    close() { front.close(); back.close(); },
  };
}
