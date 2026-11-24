// End-to-end over loopback: node-datachannel (libdatachannel) client <-> lagless server.
import { test } from 'node:test';
import assert from 'node:assert';
import ndc from 'node-datachannel';
import { spawn, spawnSync } from 'node:child_process';
import type { ChildProcess } from 'node:child_process';
import { Server } from '../dist/index.js';
import type { Message } from '../dist/index.js';

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function waitFor(what: string, fn: () => boolean, ms = 10000): Promise<void> {
  const t0 = Date.now();
  while (!fn()) {
    if (Date.now() - t0 > ms) throw new Error(`timeout waiting for ${what}`);
    await sleep(10);
  }
}

test('client connects, both channels carry data both ways', { timeout: 30000 }, async () => {
  const server = new Server();
  const pc = new ndc.PeerConnection('client', { iceServers: [] });
  try {
    await exchange(server, pc);
  } finally {
    pc.close();
    server.close();
    ndc.cleanup();
  }
});

// Relay-only client (TURN on the client side) against the ICE-lite server: the
// server needs no TURN of its own, the TURN server relays UDP to its host candidate.
const hasCoturn = spawnSync('turnserver', ['--version']).error === undefined;
test('relay-only client connects through TURN', { timeout: 30000, skip: !hasCoturn && 'coturn (turnserver) not installed' }, async () => {
  const port = 20000 + Math.floor(Math.random() * 20000);
  const turn: ChildProcess = spawn('turnserver', [
    '-n', '--listening-ip=127.0.0.1', '--relay-ip=127.0.0.1', `--listening-port=${port}`,
    '--lt-cred-mech', '--user=u:p', '--realm=lagless', '--allow-loopback-peers',
    '--no-cli', '--no-tls', '--no-dtls', '--log-file=stdout',
  ], { stdio: 'ignore' });
  await sleep(500);
  const server = new Server();
  const pc = new ndc.PeerConnection('client', {
    iceServers: [`turn:u:p@127.0.0.1:${port}?transport=udp`],
    iceTransportPolicy: 'relay',
  });
  try {
    await exchange(server, pc, (sdp) => {
      const cands = sdp.split(/\r?\n/).filter((l) => l.startsWith('a=candidate:'));
      assert.ok(cands.length > 0, 'client gathered a relay candidate');
      for (const c of cands) assert.match(c, / typ relay /);
    });
  } finally {
    pc.close();
    server.close();
    ndc.cleanup();
    turn.kill();
  }
});

async function exchange(server: Server, pc: ndc.PeerConnection, checkOffer?: (sdp: string) => void): Promise<void> {
  const u = pc.createDataChannel('u', { unordered: true, maxRetransmits: 0 });
  const r = pc.createDataChannel('r');
  const got: Record<'u' | 'r', Buffer[]> = { u: [], r: [] };
  u.onMessage((m) => got.u.push(Buffer.from(m as Buffer)));
  r.onMessage((m) => got.r.push(Buffer.from(m as Buffer)));
  let gathered = false;
  pc.onGatheringStateChange((s) => { if (s === 'complete') gathered = true; });
  pc.setLocalDescription();
  await waitFor('gathering', () => gathered || pc.gatheringState() === 'complete');
  const offer = pc.localDescription();
  assert.ok(offer);
  checkOffer?.(offer.sdp);

  const inbox: Message[] = [];
  server.on('message', (m) => inbox.push(m));

  const acc = server.accept({ type: 'offer', sdp: offer.sdp }, '127.0.0.1');
  assert.ok(acc.peer > 0);
  assert.ok(acc.localPort > 0);
  assert.strictEqual(acc.answer.type, 'answer');
  pc.setRemoteDescription(acc.answer.sdp, 'answer');

  let uOpen = false, rOpen = false;
  u.onOpen(() => { uOpen = true; });
  r.onOpen(() => { rOpen = true; });
  await waitFor('channels open', () => (uOpen || u.isOpen()) && (rOpen || r.isOpen()));
  await waitFor('server connected', () => server.stats(acc.peer)?.connected === true);

  // client -> server
  u.sendMessageBinary(Buffer.from('ping-u'));
  r.sendMessageBinary(Buffer.from('ping-r'));
  await waitFor('server inbox', () => inbox.length >= 2);
  const labels = inbox.map((m) => `${m.label}:${m.data.toString()}`).sort();
  assert.deepStrictEqual(labels, ['r:ping-r', 'u:ping-u']);

  // server -> client (channels open on the server once DCEP completes)
  await waitFor('server channel open', () => {
    try { return server.send(acc.peer, 'u', Buffer.from('pong-u'), { exempt: true }); } catch { return false; }
  });
  assert.strictEqual(server.send(acc.peer, 'r', new TextEncoder().encode('pong-r')), true);
  await waitFor('client inbox', () => got.u.length >= 1 && got.r.length >= 1);
  assert.strictEqual(got.u[0].toString(), 'pong-u');
  assert.strictEqual(got.r[0].toString(), 'pong-r');

  const st = server.stats(acc.peer);
  assert.ok(st);
  assert.strictEqual(st.closed, false);
  assert.ok(st.rate > 0, 'pacer rate');
  assert.strictEqual(typeof server.budget(acc.peer), 'number');
}

test('unknown channel and unknown peer are errors', () => {
  const server = new Server({ pacer: false, rack: false, unreliableLimit: 0 });
  assert.throws(() => server.send(42, 'u', Buffer.from('x')), /no such peer/);
  assert.strictEqual(server.stats(42), null);
  assert.strictEqual(server.budget(42), 0);
  server.close();
});

async function connect(server: Server, pc: ndc.PeerConnection) {
  const r = pc.createDataChannel('r');
  const got: (string | Buffer)[] = [];
  r.onMessage((m) => got.push(typeof m === 'string' ? m : Buffer.from(m as Buffer)));
  let gathered = false;
  pc.onGatheringStateChange((s) => { if (s === 'complete') gathered = true; });
  pc.setLocalDescription();
  await waitFor('gathering', () => gathered || pc.gatheringState() === 'complete');
  const offer = pc.localDescription();
  assert.ok(offer);
  const inbox: Message[] = [];
  server.on('message', (m) => inbox.push(m));
  const acc = server.accept({ type: 'offer', sdp: offer.sdp }, '127.0.0.1');
  pc.setRemoteDescription(acc.answer.sdp, 'answer');
  await waitFor('channel open', () => r.isOpen());
  await waitFor('server channel open', () => {
    try { return server.send(acc.peer, 'r', Buffer.alloc(0)); } catch { return false; }
  });
  return { peer: acc.peer, r, got, inbox };
}

test('string messages keep their type both ways', { timeout: 30000 }, async () => {
  const server = new Server();
  const pc = new ndc.PeerConnection('client', { iceServers: [] });
  try {
    const { peer, r, got, inbox } = await connect(server, pc);
    r.sendMessage('hi-text');
    r.sendMessageBinary(Buffer.from('hi-bin'));
    await waitFor('server inbox', () => inbox.length >= 2);
    assert.deepStrictEqual(inbox.map((m) => [m.data.toString(), m.isString]), [['hi-text', true], ['hi-bin', false]]);
    got.length = 0;
    assert.strictEqual(server.send(peer, 'r', 'yo-text'), true);
    assert.strictEqual(server.send(peer, 'r', Buffer.from('yo-bin')), true);
    await waitFor('client inbox', () => got.length >= 2);
    assert.strictEqual(got[0], 'yo-text');
    assert.ok(Buffer.isBuffer(got[1]) && got[1].toString() === 'yo-bin');
    assert.strictEqual(server.stats(peer)?.sendErrors, 0);
  } finally {
    pc.close();
    server.close();
  }
});

test('reliable byte cap kicks the peer', { timeout: 30000 }, async () => {
  const server = new Server({ reliableLimit: 64 * 1024 });
  const pc = new ndc.PeerConnection('client', { iceServers: [] });
  try {
    const { peer } = await connect(server, pc);
    const chunk = Buffer.alloc(16 * 1024);
    let ok = 0;
    while (server.send(peer, 'r', chunk)) ok++;
    assert.ok(ok >= 1 && ok < 64, `sent ${ok} chunks before overflow`);
    const st = server.stats(peer);
    assert.ok(st);
    assert.strictEqual(st.closeReason, 'reliable-overflow');
    assert.strictEqual(st.reliableOverflows, 1);
    assert.strictEqual(st.closed, true);
    assert.throws(() => server.send(peer, 'r', chunk), /peer closed/);
  } finally {
    pc.close();
    server.close();
    ndc.cleanup();
  }
});
