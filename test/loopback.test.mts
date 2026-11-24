// End-to-end over loopback: node-datachannel (libdatachannel) client <-> lagless server.
import { test, after } from 'node:test';
import assert from 'node:assert';
import ndc from 'node-datachannel';
import { spawn, spawnSync } from 'node:child_process';
import type { ChildProcess } from 'node:child_process';
import { Server } from '../dist/index.js';
import type { Message } from '../dist/index.js';
import dgram from 'node:dgram';

after(() => ndc.cleanup());

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
    turn.kill();
  }
});

async function offerOf(pc: ndc.PeerConnection): Promise<string> {
  let gathered = false;
  pc.onGatheringStateChange((s) => { if (s === 'complete') gathered = true; });
  pc.setLocalDescription();
  await waitFor('gathering', () => gathered || pc.gatheringState() === 'complete');
  const offer = pc.localDescription();
  assert.ok(offer);
  return offer.sdp;
}

async function exchange(server: Server, pc: ndc.PeerConnection, checkOffer?: (sdp: string) => void, ip = '127.0.0.1'): Promise<number> {
  const u = pc.createDataChannel('u', { unordered: true, maxRetransmits: 0 });
  const r = pc.createDataChannel('r');
  const got: Record<'u' | 'r', Buffer[]> = { u: [], r: [] };
  u.onMessage((m) => got.u.push(Buffer.from(m as Buffer)));
  r.onMessage((m) => got.r.push(Buffer.from(m as Buffer)));
  const sdp = await offerOf(pc);
  checkOffer?.(sdp);

  const inbox: Message[] = [];
  const opened = new Set<string>();
  let open = false;
  const acc = await server.accept({ type: 'offer', sdp }, ip);
  server.on('message', (m) => { if (m.peer === acc.peer) inbox.push(m); });
  server.on('open', (e) => { if (e.peer === acc.peer) open = true; });
  server.on('channelopen', (e) => { if (e.peer === acc.peer) opened.add(e.label); });
  assert.ok(acc.peer > 0);
  assert.strictEqual(acc.localPort, server.localPort);
  assert.strictEqual(acc.answer.type, 'answer');
  pc.setRemoteDescription(acc.answer.sdp, 'answer');

  let uOpen = false, rOpen = false;
  u.onOpen(() => { uOpen = true; });
  r.onOpen(() => { rOpen = true; });
  await waitFor('channels open', () => (uOpen || u.isOpen()) && (rOpen || r.isOpen()));
  await waitFor('server connected', () => server.stats(acc.peer)?.connected === true);
  await waitFor('open event', () => open);

  // client -> server
  u.sendMessageBinary(Buffer.from('ping-u'));
  r.sendMessageBinary(Buffer.from('ping-r'));
  await waitFor('server inbox', () => inbox.length >= 2);
  const labels = inbox.map((m) => `${m.label}:${m.data.toString()}`).sort();
  assert.deepStrictEqual(labels, ['r:ping-r', 'u:ping-u']);

  // server -> client (channels open on the server once DCEP completes)
  await waitFor('channelopen events', () => opened.has('u') && opened.has('r'));
  assert.strictEqual(server.send(acc.peer, 'u', Buffer.from('pong-u'), { exempt: true }), true);
  assert.strictEqual(server.send(acc.peer, 'r', new TextEncoder().encode('pong-r')), true);
  await waitFor('client inbox', () => got.u.length >= 1 && got.r.length >= 1);
  assert.strictEqual(got.u[0].toString(), 'pong-u');
  assert.strictEqual(got.r[0].toString(), 'pong-r');

  const st = server.stats(acc.peer);
  assert.ok(st);
  assert.strictEqual(st.closed, false);
  assert.ok(st.rate > 0, 'pacer rate');
  assert.strictEqual(typeof server.budget(acc.peer), 'number');
  return acc.peer;
}

test('many peers share one port; closePeer and remote close emit close and drop the peer', { timeout: 60000 }, async () => {
  const server = new Server();
  const pcs = [0, 1, 2].map((i) => new ndc.PeerConnection(`c${i}`, { iceServers: [] }));
  const closes = new Map<number, string>();
  server.on('close', (e) => closes.set(e.peer, e.reason));
  try {
    const peers = await Promise.all(pcs.map((pc) => exchange(server, pc)));
    assert.strictEqual(new Set(peers).size, 3);

    // local close: client sees the channel go away quickly (ABORT / close_notify, no timeout)
    let clientClosed = false;
    pcs[0].onStateChange((s) => { if (s === 'closed' || s === 'failed' || s === 'disconnected') clientClosed = true; });
    server.closePeer(peers[0]);
    await waitFor('local close event', () => closes.get(peers[0]) === 'local');
    assert.strictEqual(server.stats(peers[0]), null);
    assert.throws(() => server.send(peers[0], 'r', Buffer.from('x')), /no such peer/);
    await waitFor('client notices close', () => clientClosed, 5000);

    // remote close: the peer is removed without the app polling
    pcs[1].close();
    await waitFor('remote close event', () => closes.has(peers[1]), 15000);
    assert.strictEqual(server.stats(peers[1]), null);

    // the third peer is untouched
    assert.strictEqual(server.stats(peers[2])?.connected, true);
  } finally {
    for (const pc of pcs) pc.close();
    server.close();
  }
});

test('IPv6 advertise address over a dual-stack socket', { timeout: 30000 }, async (t) => {
  const ok = await new Promise<boolean>((res) => {
    const s = dgram.createSocket('udp6');
    s.once('error', () => { s.close(); res(false); });
    s.bind(0, '::1', () => { s.close(); res(true); });
  });
  if (!ok) { t.skip('no IPv6 loopback'); return; }
  const server = new Server({ bindAddress: '::' });
  const pc = new ndc.PeerConnection('v6', { iceServers: [] });
  try {
    await exchange(server, pc, undefined, '::1');
  } finally {
    pc.close();
    server.close();
  }
});

test('unknown channel and unknown peer are errors', () => {
  const server = new Server({ pacer: false, rack: false, unreliableLimit: 0 });
  assert.throws(() => server.send(42, 'u', Buffer.from('x')), /no such peer/);
  assert.strictEqual(server.stats(42), null);
  assert.strictEqual(server.budget(42), 0);
  server.close();
});
