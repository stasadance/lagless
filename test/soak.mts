// Long-run soak: SOAK_PEERS peers (default 20) with churn (one peer replaced every
// SOAK_CHURN_MS, default 2000) and steady traffic for SOAK_SECONDS (default 60). Samples RSS and
// OS threads; fails if RSS keeps growing after warmup or threads/peers leak after close.
// usage: node test/soak.mts   (pnpm test:soak)
import assert from 'node:assert';
import ndc from 'node-datachannel';
import { Server } from '../dist/index.js';
import { connect, rssMB, sleep, threadCount, waitFor } from './helpers.mts';
import type { Client } from './helpers.mts';

const PEERS = Number(process.env.SOAK_PEERS ?? 20);
const SECONDS = Number(process.env.SOAK_SECONDS ?? 60);
const CHURN_MS = Number(process.env.SOAK_CHURN_MS ?? 2000);
const MAX_GROWTH_MB = Number(process.env.SOAK_MAX_GROWTH_MB ?? 64);

const threads0 = threadCount();
const server = new Server();
let received = 0;
server.on('message', () => { received++; });
let n = 0;
const clients: Client[] = [];
for (let i = 0; i < PEERS; i++) clients.push(await connect(server, `s${n++}`, 30000));

const samples: { t: number; rss: number; threads: number | null; peers: number }[] = [];
const t0 = Date.now();
let lastChurn = t0, lastSample = 0, churned = 0, sent = 0;
while (Date.now() - t0 < SECONDS * 1000) {
  for (const c of clients) {
    server.send(c.peer, 'r', Buffer.alloc(300, 4));
    for (let k = 0; k < 2; k++) server.send(c.peer, 'u', Buffer.alloc(560, 1));
    c.u.sendMessageBinary(Buffer.alloc(64, 2));
    sent++;
  }
  if (Date.now() - lastChurn >= CHURN_MS) {
    lastChurn = Date.now();
    const old = clients.shift()!;
    old.close();
    server.closePeer(old.peer);
    clients.push(await connect(server, `s${n++}`, 30000));
    churned++;
  }
  if (Date.now() - lastSample >= 5000) {
    lastSample = Date.now();
    samples.push({ t: Math.round((Date.now() - t0) / 1000), rss: rssMB(), threads: threadCount(), peers: clients.length });
    console.log(JSON.stringify(samples.at(-1)));
  }
  await sleep(40);
}
for (const c of clients) c.close();
server.close();
ndc.cleanup();

const warm = samples.slice(Math.floor(samples.length / 3));
const growth = warm.length > 1 ? warm.at(-1)!.rss - warm[0].rss : 0;
console.log(JSON.stringify({ seconds: SECONDS, peers: PEERS, churned, sent, received, rssGrowthAfterWarmupMB: growth, threads0, threadsAtEnd: threadCount() }));
assert.ok(received > 0, 'server received messages');
assert.ok(growth <= MAX_GROWTH_MB, `RSS grew ${growth} MB after warmup (limit ${MAX_GROWTH_MB})`);
if (threads0 != null) {
  // peak threads must not scale with churned peers, and must come back after close
  const peak = Math.max(...samples.map((s) => s.threads ?? 0));
  assert.ok(peak <= threads0 + PEERS * 2 + 16, `thread peak ${peak} with ${PEERS} peers (start ${threads0}): leak across churn?`);
  await waitFor('threads released', () => (threadCount() ?? 0) <= threads0 + 8, 15000)
    .catch(() => assert.fail(`threads: ${threads0} before, ${threadCount()} after close`));
}
console.log('soak ok');
process.exit(0);
