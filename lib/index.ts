import { EventEmitter } from 'node:events';
import { Server as NativeServer } from '../binding.js';
import type { Message, PeerStats, ServerOptions as NativeServerOptions } from '../binding.js';

export type { Message, PeerStats };

export interface ServerOptions extends NativeServerOptions {
  /** How often received messages are pulled from the native side, in ms. Default 1. */
  drainIntervalMs?: number;
}

export interface SessionDescription {
  type: 'offer' | 'answer';
  sdp: string;
}

export interface Accepted {
  peer: number;
  answer: SessionDescription;
  /** UDP port the peer's socket is bound to. */
  localPort: number;
}

export interface SendOptions {
  /** Skip the unreliable backpressure gate and the pacer (latency probes). */
  exempt?: boolean;
}

interface Events {
  message: [Message];
}

/**
 * WebRTC data channel server. One SCTP association per peer.
 * Emits `message` for every message received from any peer.
 */
export class Server extends EventEmitter<Events> {
  readonly native: NativeServer;
  private readonly drainIntervalMs: number;
  private timer: NodeJS.Timeout | undefined;

  constructor(opts: ServerOptions = {}) {
    super();
    const { drainIntervalMs = 1, ...nativeOpts } = opts;
    this.native = new NativeServer(nativeOpts);
    this.drainIntervalMs = drainIntervalMs;
  }

  /**
   * Accept a complete (non-trickle) offer. The answer carries one host candidate
   * `advertiseIp:advertisePort`; port 0 means the port the peer socket bound to.
   */
  accept(offer: SessionDescription | string, advertiseIp: string, advertisePort = 0): Accepted {
    const offerJson = typeof offer === 'string' ? offer : JSON.stringify(offer);
    const r = this.native.acceptOffer(offerJson, advertiseIp, advertisePort);
    this.startDrain();
    return { peer: r.peer, answer: JSON.parse(r.answer) as SessionDescription, localPort: r.localPort };
  }

  /**
   * Send on a channel by label. Returns false when an unreliable send was dropped by
   * the backpressure gate or the pacer: the state is still owed, send fresh state next tick.
   */
  send(peer: number, label: string, data: Buffer | Uint8Array, opts: SendOptions = {}): boolean {
    const buf = Buffer.isBuffer(data) ? data : Buffer.from(data.buffer, data.byteOffset, data.byteLength);
    return this.native.send(peer, label, buf, opts.exempt ?? false);
  }

  /** Bytes the pacer allows right now (can be negative after reliable sends). */
  budget(peer: number): number {
    return this.native.budget(peer);
  }

  /** Per-peer queue, pacer and SCTP state. `null` for an unknown peer. */
  stats(peer: number): PeerStats | null {
    return this.native.stats(peer);
  }

  /** Close one peer. */
  closePeer(peer: number): void {
    this.native.close(peer);
  }

  /** Close every peer and stop emitting messages. */
  close(): void {
    this.drain();
    if (this.timer) clearInterval(this.timer);
    this.timer = undefined;
    this.native.closeAll();
  }

  private startDrain(): void {
    if (this.timer) return;
    this.timer = setInterval(() => this.drain(), this.drainIntervalMs);
  }

  private drain(): void {
    for (const m of this.native.drain()) this.emit('message', m);
  }
}
