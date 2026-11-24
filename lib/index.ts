import { EventEmitter } from 'node:events';
import { Server as NativeServer } from '../binding.js';
import type { AcceptedAudio, AudioOptions, AudioSend, AudioSlot, Event as NativeEvent, PeerStats, ServerOptions } from '../binding.js';

export type { AcceptedAudio, AudioOptions, AudioSend, AudioSlot, PeerStats, ServerOptions };

export interface Message {
  peer: number;
  label: string;
  data: Buffer;
  /** The sender sent a string (decode `data` as UTF-8), not binary. */
  isString: boolean;
}

export interface SessionDescription {
  type: 'offer' | 'answer';
  sdp: string;
}

export interface Accepted {
  peer: number;
  answer: SessionDescription;
  /** UDP port the answer advertises (the server's shared port unless overridden). */
  localPort: number;
  /** Negotiated audio: set when `audio` was passed and the offer has Opus audio m-lines. */
  audio?: AcceptedAudio;
}

export interface AcceptOptions {
  /**
   * Answer the offer's Opus audio m-lines (same peer, port and DTLS session as the data
   * channels). Browser sendonly/sendrecv m-lines become server receive m-lines (`rtp` events);
   * each `send` entry takes a browser recvonly m-line (`mid`, or the next free one in offer
   * order) with its SSRC in the answer. Without it, audio m-lines are rejected.
   */
  audio?: AudioOptions;
}

export interface RtpPacket {
  peer: number;
  mid: string;
  /** Decrypted RTP packet, header + payload. */
  data: Buffer;
}

export interface SendOptions {
  /** Skip the unreliable backpressure gate and the pacer (latency probes). */
  exempt?: boolean;
}

/**
 * Why a peer closed: `local` (closePeer/close), `closed` (remote closed), `failed` (ICE/DTLS failed),
 * `reliable-overflow` (a reliable send would have pushed its queue past `reliableLimit`).
 */
export type CloseReason = 'local' | 'closed' | 'failed' | 'reliable-overflow' | (string & {});

export class PeerError extends Error {
  readonly peer: number;
  constructor(peer: number, message: string) {
    super(message);
    this.peer = peer;
    this.name = 'PeerError';
  }
}

interface Events {
  message: [Message];
  /** ICE + DTLS connected. */
  open: [{ peer: number }];
  /** A data channel is open and can be sent on. */
  channelopen: [{ peer: number; label: string }];
  /** The peer is gone and has been removed (send() now throws "no such peer"). */
  close: [{ peer: number; reason: CloseReason }];
  /** Something failed for one peer (e.g. rtc refused a send). Only emitted if a listener exists. */
  error: [PeerError];
  /**
   * `overloaded: true` once the pacer budget stayed negative for `overloadMs` (reliable load alone
   * fills the link, unreliable gets nothing): shed reliable load or kick. `false` when it recovers.
   */
  overload: [{ peer: number; overloaded: boolean }];
  /** An RTP packet arrived on an audio m-line the server receives on. RTCP never shows up here. */
  rtp: [RtpPacket];
  /** DTLS-SRTP is up for this audio m-line: sendRtp can go. Closes with the peer. */
  rtpopen: [{ peer: number; mid: string }];
}

/**
 * WebRTC data channel server. One SCTP association per peer, all peers on one UDP
 * port and one native thread. Received messages and lifecycle events are pushed in
 * batches from that thread (no polling).
 */
export class Server extends EventEmitter<Events> {
  readonly native: NativeServer;
  private started = false;

  constructor(opts: ServerOptions = {}) {
    super();
    this.native = new NativeServer(opts);
  }

  /** The UDP port every peer shares. */
  get localPort(): number {
    return this.native.localPort;
  }

  /**
   * The server's public address as a STUN server sees it, asked from the shared UDP port,
   * so a NAT's mapping for that port is what comes back. Pass it to `accept()`:
   * `accept(offer, a.ip, a.port)`. `stunServer` is `host[:port]` (default port 3478).
   * Retransmits until `timeoutMs`, then rejects.
   */
  async publicAddress(stunServer: string, timeoutMs = 3000): Promise<{ ip: string; port: number }> {
    return this.native.publicAddress(stunServer, timeoutMs);
  }

  /**
   * Accept an offer. It may be sent before ICE gathering finishes (trickle): the server is
   * ICE-lite and learns the client's address from its checks, so the client's later
   * candidates aren't needed. The answer carries one host candidate per
   * advertise IP (IPv4 and/or IPv6) at `advertisePort`; 0 means the server's port.
   */
  async accept(
    offer: SessionDescription | string,
    advertiseIp: string | string[],
    advertisePort = 0,
    opts: AcceptOptions = {},
  ): Promise<Accepted> {
    const offerJson = typeof offer === 'string' ? offer : JSON.stringify(offer);
    this.start();
    const r = await this.native.acceptOffer(offerJson, advertiseIp, advertisePort, opts.audio);
    const acc: Accepted = { peer: r.peer, answer: JSON.parse(r.answer) as SessionDescription, localPort: r.localPort };
    if (r.audio) acc.audio = r.audio;
    return acc;
  }

  /**
   * Send a finished RTP packet (header + payload) on an audio m-line from `accepted.audio.send`.
   * Its SSRC and payload type must be the negotiated ones. It is SRTP-encrypted and sent at once:
   * no backpressure gate, pacer or egress cap. Returns false until `rtpopen`. Throws if the peer
   * is unknown or closed, the server doesn't send on `mid`, or the packet is under 12 bytes.
   */
  sendRtp(peer: number, mid: string, data: Buffer | Uint8Array): boolean {
    const buf = Buffer.isBuffer(data) ? data : Buffer.from(data.buffer, data.byteOffset, data.byteLength);
    return this.native.sendRtp(peer, mid, buf);
  }

  /**
   * Send on a channel by label. A string is sent as a WebRTC string message (the browser
   * gets a string), bytes as binary. Returns false when an unreliable send was dropped by
   * the backpressure gate, the pacer or the egress cap (the state is still owed, send fresh state next
   * tick), or when a reliable send hit `reliableLimit` (the peer is then closed, `close`
   * reason `reliable-overflow`). Throws if the peer is unknown or closed, the channel isn't
   * open, or the message is larger than the negotiated max message size.
   */
  send(peer: number, label: string, data: string | Buffer | Uint8Array, opts: SendOptions = {}): boolean {
    const isString = typeof data === 'string';
    const buf = isString
      ? Buffer.from(data, 'utf8')
      : Buffer.isBuffer(data) ? data : Buffer.from(data.buffer, data.byteOffset, data.byteLength);
    return this.native.send(peer, label, buf, opts.exempt ?? false, isString);
  }

  /** Bytes the pacer allows right now (can be negative after reliable sends). */
  budget(peer: number): number {
    return this.native.budget(peer);
  }

  /** Per-peer queue, pacer and SCTP state. `null` for an unknown or closed peer. */
  stats(peer: number): PeerStats | null {
    return this.native.stats(peer);
  }

  /** Close one peer gracefully (SCTP ABORT + DTLS close_notify). Emits `close`. */
  closePeer(peer: number): void {
    this.native.close(peer);
  }

  /** Close every peer, stop the native thread. `close` events for open peers still arrive. */
  close(): void {
    this.native.shutdown();
  }

  private start(): void {
    if (this.started) return;
    this.started = true;
    this.native.start((batch) => {
      for (const e of batch) this.dispatch(e);
    });
  }

  private dispatch(e: NativeEvent): void {
    switch (e.kind) {
      case 'message':
        this.emit('message', { peer: e.peer, label: e.label!, data: e.data!, isString: e.isString ?? false });
        break;
      case 'open':
        this.emit('open', { peer: e.peer });
        break;
      case 'channelopen':
        this.emit('channelopen', { peer: e.peer, label: e.label! });
        break;
      case 'close':
        this.emit('close', { peer: e.peer, reason: e.reason ?? 'closed' });
        break;
      case 'overload':
        this.emit('overload', { peer: e.peer, overloaded: e.reason === 'start' });
        break;
      case 'rtp':
        this.emit('rtp', { peer: e.peer, mid: e.mid!, data: e.data! });
        break;
      case 'rtpopen':
        this.emit('rtpopen', { peer: e.peer, mid: e.mid! });
        break;
      case 'error':
        if (this.listenerCount('error') > 0) this.emit('error', new PeerError(e.peer, e.reason ?? 'error'));
        break;
    }
  }
}
