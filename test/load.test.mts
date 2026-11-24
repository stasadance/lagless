// Many peers on one Server: every peer connects, both channels carry data both ways,
// and closing everything gives the OS threads back. LOAD_PEERS (default 50) sets the count.
import { test } from 'node:test';
import assert from 'node:assert';
import ndc from 'node-datachannel';
import { Server } from '../dist/index.js';
import { connect, sleep, threadCount, waitFor } from './helpers.mts';
import type { Client } from './helpers.mts';

const PEERS = Number(process.env.LOAD_PEERS ?? 50);

test(`${PEERS} peers at once`, { timeout: 120000 }, async () => {
  const threads0 = threadCount();
  const server = new Server();
  const inbox = new Map<number, number>();
  server.on('message', (m) => inbox.set(m.peer, (inbox.get(m.peer) ?? 0) + 1));
  const clients: Client[] = [];
  try {
    // connect in batches so gathering/handshakes overlap without one giant burst
    for (let i = 0; i < PEERS; i += 10) {
      clients.push(...await Promise.all(Array.from({ length: Math.min(10, PEERS - i) }, (_, j) => connect(server, `c${i + j}`, 30000))));
    }
    assert.strictEqual(new Set(clients.map((c) => c.peer)).size, PEERS, 'distinct peer ids');

    // 2 s of 25 Hz traffic both ways on every peer
    const t0 = Date.now();
    let ticks = 0;
    while (Date.now() - t0 < 2000) {
      for (const c of clients) {
        server.send(c.peer, 'r', Buffer.alloc(200, 4));
        server.send(c.peer, 'u', Buffer.alloc(500, 1));
        c.r.sendMessageBinary(Buffer.alloc(100, 5));
        c.u.sendMessageBinary(Buffer.alloc(100, 6));
      }
      ticks++;
      await sleep(40);
    }
    await waitFor('every client got all reliable', () => clients.every((c) => c.got.r >= ticks + 1), 20000);
    await waitFor('server got reliable from every peer', () => clients.every((c) => (inbox.get(c.peer) ?? 0) >= ticks), 20000);
    for (const c of clients) {
      assert.ok(c.got.u > 0, `peer ${c.peer} got unreliable`);
      const st = server.stats(c.peer);
      assert.ok(st && st.connected && !st.closed, `peer ${c.peer} connected`);
    }
  } finally {
    for (const c of clients) c.close();
    server.close();
    ndc.cleanup();
  }
  // native threads must not outlive the peers (Linux only: counts /proc/self/task)
  if (threads0 != null) {
    await waitFor('threads released', () => (threadCount() ?? 0) <= threads0 + 8, 15000)
      .catch(() => assert.fail(`threads: ${threads0} before, ${threadCount()} after close`));
  }
});
