// Userspace UDP shaper, one per peer. Each way: loss (random or Gilbert-Elliott bursts),
// one-way delay with optional jitter (which reorders) and a slow RTT swing, and an optional
// bandwidth cap with a FIFO queue (bufferbloat). A `Link` can be shared by several shapers
// so peers contend for one bottleneck.
import dgram from 'node:dgram';
import type { RemoteInfo } from 'node:dgram';
import type { AddressInfo } from 'node:net';

export function mulberry32(a: number): () => number {
  return function () {
    a |= 0; a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * Gilbert-Elliott loss: two states. good -> bad with probability p per packet, bad -> good with r.
 * Loss probability is `lossGood` in good and `lossBad` in bad. Mean burst length 1/r packets.
 */
export interface GilbertElliott { p: number; r: number; lossBad: number; lossGood?: number }

export interface LinkOptions {
  /** Bottleneck rate in KB/s (1 KB = 1024 B). 0 = no cap. */
  rateKBps: number;
  /** Bottleneck queue in ms at `rateKBps`; packets that would wait longer are tail-dropped. */
  queueMs: number;
}

/** A rate-limited FIFO bottleneck. Shared between shapers = shared uplink. */
export class Link {
  private readonly bytesPerMs: number;
  private readonly maxQueueBytes: number;
  private busyUntil = 0; // ms timestamp the queue drains at
  readonly st = { queued: 0, tailDrops: 0, maxQueueMs: 0 };
  constructor({ rateKBps, queueMs }: LinkOptions) {
    this.bytesPerMs = (rateKBps * 1024) / 1000;
    this.maxQueueBytes = this.bytesPerMs * queueMs;
  }
  /** Returns the ms to wait before the packet leaves the bottleneck, or -1 to drop it. */
  admit(bytes: number, now: number): number {
    if (!this.bytesPerMs) return 0;
    const start = Math.max(now, this.busyUntil);
    const backlogBytes = (start - now) * this.bytesPerMs;
    if (backlogBytes + bytes > this.maxQueueBytes) { this.st.tailDrops++; return -1; }
    this.busyUntil = start + bytes / this.bytesPerMs;
    const wait = this.busyUntil - now;
    this.st.queued++;
    if (wait > this.st.maxQueueMs) this.st.maxQueueMs = Math.round(wait);
    return wait;
  }
}

export interface DirectionOptions {
  /** Random loss probability (ignored when `ge` is set). */
  loss: number;
  ge?: GilbertElliott;
  /** Uniform extra delay in [0, jitterMs) per packet. Packets can overtake each other. */
  jitterMs?: number;
  /** Bottleneck for this direction. */
  link?: Link;
}

export interface ShaperOptions {
  /** Base one-way delay, each way. */
  delayMs: number;
  seed: number;
  up: DirectionOptions;   // client -> server
  down: DirectionOptions; // server -> client
  /** RTT swing: one-way delay += swingMs * (0.5 - 0.5 cos(2 pi t / periodMs)), both ways. */
  swing?: { swingMs: number; periodMs: number };
}

export type Shaper = Awaited<ReturnType<typeof createShaper>>;

function direction(o: DirectionOptions, rnd: () => number) {
  let bad = false;
  const st = { pkts: 0, drops: 0 };
  return {
    st,
    /** -1 = drop, else extra ms on top of base delay */
    decide(bytes: number, now: number): number {
      st.pkts++;
      let lost: boolean;
      if (o.ge) {
        bad = bad ? rnd() >= o.ge.r : rnd() < o.ge.p;
        lost = rnd() < (bad ? o.ge.lossBad : (o.ge.lossGood ?? 0));
      } else lost = rnd() < o.loss;
      if (lost) { st.drops++; return -1; }
      const q = o.link ? o.link.admit(bytes, now) : 0;
      if (q < 0) { st.drops++; return -1; }
      return q + (o.jitterMs ? rnd() * o.jitterMs : 0);
    },
  };
}

export async function createShaper({ delayMs, seed, up, down, swing }: ShaperOptions) {
  const front = dgram.createSocket('udp4'); // client side
  const back = dgram.createSocket('udp4');  // server side
  const dUp = direction(up, mulberry32(seed * 2 + 1));
  const dDown = direction(down, mulberry32(seed * 2 + 2));
  const t0 = performance.now();
  const base = (now: number) => swing
    ? delayMs + swing.swingMs * (0.5 - 0.5 * Math.cos((2 * Math.PI * (now - t0)) / swing.periodMs))
    : delayMs;
  let client: RemoteInfo | null = null;
  let target: { address: string; port: number } | null = null;
  front.on('message', (msg, rinfo) => {
    client = rinfo;
    if (!target) return;
    const now = performance.now();
    const extra = dUp.decide(msg.length, now);
    if (extra < 0) return;
    const t = target;
    setTimeout(() => { try { back.send(msg, t.port, t.address); } catch {} }, base(now) + extra);
  });
  back.on('message', (msg) => {
    if (!client) return;
    const now = performance.now();
    const extra = dDown.decide(msg.length, now);
    if (extra < 0) return;
    const c: RemoteInfo = client;
    setTimeout(() => { try { front.send(msg, c.port, c.address); } catch {} }, base(now) + extra);
  });
  await new Promise<void>((r) => front.bind(0, '127.0.0.1', r));
  await new Promise<void>((r) => back.bind(0, '127.0.0.1', r));
  return {
    port: (front.address() as AddressInfo).port,
    setTarget(port: number) { target = { address: '127.0.0.1', port }; },
    stats: () => ({ up: dUp.st.pkts, upDrop: dUp.st.drops, down: dDown.st.pkts, downDrop: dDown.st.drops }),
    close() { front.close(); back.close(); },
  };
}
