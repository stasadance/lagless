// Two server adapters with the same interface:
//   accept(offerSdpJson, shaperPort) -> { answer (json), localPort }
//   send(label, buf, exempt) -> bool
//   onMessage(fn(label, buf))
//   stats() -> {}
//   close()
const LIMIT = 64 * 1024;

function rtcServer({ gate, rack = false, rtoMin, pace = false }) {
  const native = require('..');
  const srv = new native.Server({ unreliableLimit: gate ? LIMIT : 0, rack, rtoMin, pacer: pace });
  let peer = 0;
  let handler = () => {};
  const timer = setInterval(() => {
    for (const m of srv.drain()) handler(m.label, m.data);
  }, 1);
  return {
    name: `rtc-${gate ? 'gate' : 'naive'}${rack ? '-rack' : ''}${pace ? '-pace' : ''}${rtoMin ? '-rto' + rtoMin : ''}`,
    async accept(offerJson, shaperPort) {
      const r = srv.acceptOffer(offerJson, '127.0.0.1', shaperPort);
      peer = r.peer;
      return { answer: r.answer, localPort: r.localPort };
    },
    send(label, buf, exempt) {
      try { return srv.send(peer, label, buf, !!exempt); } catch { return false; }
    },
    onMessage(fn) { handler = fn; },
    stats() { return srv.stats(peer); },
    close() { clearInterval(timer); srv.closeAll(); },
  };
}

let ndcPort = 47000;
function ndcServer({ gate }) {
  const ndc = require('node-datachannel');
  if (gate) ndc.setSctpSettings({ sendBufferSize: LIMIT });
  const port = ndcPort++;
  const pc = new ndc.PeerConnection('srv', {
    iceServers: [], bindAddress: '127.0.0.1', portRangeBegin: port, portRangeEnd: port,
    enableIceUdpMux: false,
  });
  const chans = {};
  let handler = () => {};
  let dropped = 0, sentU = 0;
  pc.onDataChannel((dc) => {
    chans[dc.getLabel()] = dc;
    dc.onMessage((m) => handler(dc.getLabel(), Buffer.isBuffer(m) ? m : Buffer.from(m)));
  });
  return {
    name: `ndc-${gate ? 'gate' : 'naive'}`,
    accept(offerJson, shaperPort) {
      const offer = JSON.parse(offerJson);
      return new Promise((resolve) => {
        pc.onGatheringStateChange((s) => {
          if (s !== 'complete') return;
          const d = pc.localDescription();
          // keep a single host candidate pointing at the shaper
          let sdp = d.sdp.split(/\r?\n/).filter((l) => !l.startsWith('a=candidate')).join('\r\n');
          sdp = sdp.replace(/(a=mid:[^\r\n]*)/, `$1\r\na=candidate:1 1 UDP 2122317823 127.0.0.1 ${shaperPort} typ host`);
          resolve({ answer: JSON.stringify({ type: 'answer', sdp }), localPort: port });
        });
        pc.setRemoteDescription(offer.sdp, 'offer');
      });
    },
    send(label, buf, exempt) {
      const dc = chans[label];
      if (!dc || !dc.isOpen()) return false;
      const unreliable = label === 'u';
      if (unreliable && gate && !exempt) {
        for (const c of Object.values(chans)) {
          if (c.bufferedAmount() > 0) { dropped++; return false; }
        }
      }
      if (unreliable) sentU++;
      try { return dc.sendMessageBinary(buf); } catch { return false; }
    },
    onMessage(fn) { handler = fn; },
    stats() {
      const ba = {};
      for (const [k, c] of Object.entries(chans)) ba[k] = c.bufferedAmount();
      return { bufferedAmount: ba, dropped, sentUnreliable: sentU };
    },
    close() { try { pc.close(); } catch {} },
  };
}

module.exports = { rtcServer, ndcServer };
