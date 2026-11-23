// End-to-end over loopback: node-datachannel (libdatachannel) client <-> lagless server.
const { test } = require('node:test');
const assert = require('node:assert');
const ndc = require('node-datachannel');
const { Server } = require('..');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(what, fn, ms = 10000) {
  const t0 = Date.now();
  while (!fn()) {
    if (Date.now() - t0 > ms) throw new Error(`timeout waiting for ${what}`);
    await sleep(10);
  }
}

test('client connects, both channels carry data both ways', async () => {
  const server = new Server();
  const pc = new ndc.PeerConnection('client', { iceServers: [] });
  const u = pc.createDataChannel('u', { unordered: true, maxRetransmits: 0 });
  const r = pc.createDataChannel('r');
  const got = { u: [], r: [] };
  u.onMessage((m) => got.u.push(Buffer.from(m)));
  r.onMessage((m) => got.r.push(Buffer.from(m)));
  let gathered = false;
  pc.onGatheringStateChange((s) => { if (s === 'complete') gathered = true; });
  pc.setLocalDescription();
  await waitFor('gathering', () => gathered || pc.gatheringState() === 'complete');
  const offer = pc.localDescription();

  const acc = server.acceptOffer(JSON.stringify({ type: 'offer', sdp: offer.sdp }), '127.0.0.1', 0);
  assert.ok(acc.peer > 0);
  assert.ok(acc.localPort > 0);
  pc.setRemoteDescription(JSON.parse(acc.answer).sdp, 'answer');

  let uOpen = false, rOpen = false;
  u.onOpen(() => { uOpen = true; });
  r.onOpen(() => { rOpen = true; });
  await waitFor('channels open', () => (uOpen || u.isOpen()) && (rOpen || r.isOpen()));
  await waitFor('server connected', () => server.stats(acc.peer).connected);

  // client -> server
  u.sendMessageBinary(Buffer.from('ping-u'));
  r.sendMessageBinary(Buffer.from('ping-r'));
  const inbox = [];
  await waitFor('server inbox', () => { inbox.push(...server.drain()); return inbox.length >= 2; });
  const labels = inbox.map((m) => `${m.label}:${m.data.toString()}`).sort();
  assert.deepStrictEqual(labels, ['r:ping-r', 'u:ping-u']);

  // server -> client (channels open on the server once DCEP completes)
  await waitFor('server channel open', () => {
    try { return server.send(acc.peer, 'u', Buffer.from('pong-u'), true); } catch { return false; }
  });
  assert.strictEqual(server.send(acc.peer, 'r', Buffer.from('pong-r'), false), true);
  await waitFor('client inbox', () => got.u.length >= 1 && got.r.length >= 1);
  assert.strictEqual(got.u[0].toString(), 'pong-u');
  assert.strictEqual(got.r[0].toString(), 'pong-r');

  const st = server.stats(acc.peer);
  assert.strictEqual(st.closed, false);
  assert.ok(st.rate > 0, 'pacer rate');
  assert.strictEqual(typeof server.budget(acc.peer), 'number');

  pc.close();
  server.closeAll();
  ndc.cleanup();
});

test('unknown channel and unknown peer are errors', () => {
  const server = new Server({ pacer: false, rack: false, unreliableLimit: 0 });
  assert.throws(() => server.send(42, 'u', Buffer.from('x'), false), /no such peer/);
  assert.strictEqual(server.stats(42), null);
  assert.strictEqual(server.budget(42), 0);
});
