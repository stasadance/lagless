// Userspace UDP shaper: one-way delay + random loss each way (seeded).
const dgram = require('dgram');

function mulberry32(a) {
  return function () {
    a |= 0; a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

async function createShaper({ delayMs, loss, seed }) {
  const front = dgram.createSocket('udp4'); // client side
  const back = dgram.createSocket('udp4');  // server side
  const rndUp = mulberry32(seed * 2 + 1);
  const rndDown = mulberry32(seed * 2 + 2);
  let client = null;
  let target = null;
  const st = { up: 0, upDrop: 0, down: 0, downDrop: 0 };
  front.on('message', (msg, rinfo) => {
    client = rinfo;
    if (!target) return;
    st.up++;
    if (rndUp() < loss) { st.upDrop++; return; }
    setTimeout(() => { try { back.send(msg, target.port, target.address); } catch {} }, delayMs);
  });
  back.on('message', (msg) => {
    if (!client) return;
    st.down++;
    if (rndDown() < loss) { st.downDrop++; return; }
    const c = client;
    setTimeout(() => { try { front.send(msg, c.port, c.address); } catch {} }, delayMs);
  });
  await new Promise((r) => front.bind(0, '127.0.0.1', r));
  await new Promise((r) => back.bind(0, '127.0.0.1', r));
  return {
    port: front.address().port,
    setTarget(port) { target = { address: '127.0.0.1', port }; },
    stats: () => ({ ...st }),
    close() { front.close(); back.close(); },
  };
}

module.exports = { createShaper };
