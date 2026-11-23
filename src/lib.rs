//! lagless: low-latency WebRTC data channel server for Node.js.
//!
//! Built on the sans-IO webrtc-rs `rtc` stack (vendored with RACK-TLP and
//! transport-stats patches). Per peer: one worker thread and one UDP socket.
//! Unreliable sends are gated on the true SCTP queue and on a per-peer pacer
//! budget, so stale game state is dropped instead of queued.

mod pacer;

use bytes::BytesMut;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::setting_engine::SettingEngineBuilder;
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::{RTCMessage, TaggedRTCMessage};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::{CandidateConfig, CandidateHostConfig, RTCIceCandidate};
use rtc::peer_connection::{RTCPeerConnection, RTCPeerConnectionBuilder};
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Inbound {
    peer: u32,
    label: String,
    data: Vec<u8>,
}

enum Cmd {
    Send { label: String, data: Vec<u8> },
    Close,
}

#[derive(Default)]
struct PeerShared {
    /// bytes SCTP has not released yet (sum of outstanding_bytes over channels)
    stack_outstanding: AtomicUsize,
    /// bytes accepted by send() but not yet handed to the worker loop
    pending_cmd: AtomicUsize,
    dropped: AtomicU64,
    sent_unreliable: AtomicU64,
    connected: AtomicBool,
    closed: AtomicBool,
    /// label -> unreliable?
    channels: Mutex<HashMap<String, bool>>,
    // pacer
    tokens: AtomicI64,
    rate: AtomicU64,
    draining: AtomicBool,
    paced_drops: AtomicU64,
    // SCTP transport state
    srtt_ms: AtomicU64,
    min_rtt_ms: AtomicU64,
    cwnd: AtomicU64,
    inflight: AtomicU64,
    pending: AtomicU64,
    rack_lost: AtomicU64,
    rack_probes: AtomicU64,
    t3_timeouts: AtomicU64,
}

struct Peer {
    shared: Arc<PeerShared>,
    tx: Sender<Cmd>,
}

#[napi(object)]
pub struct ServerOptions {
    /// Drop an unreliable send when bytes not yet released by SCTP + its length
    /// would exceed this. 0 disables the gate. Default 65536.
    pub unreliable_limit: Option<u32>,
    /// RACK-TLP loss detection on the SCTP association. Default true.
    pub rack: Option<bool>,
    /// SCTP RTO lower bound in ms. Default 1000 (RFC 9260).
    pub rto_min: Option<u32>,
    /// Per-peer send pacer: unreliable sends need budget tokens. Default true.
    pub pacer: Option<bool>,
    /// Local address peer sockets bind to. Default "0.0.0.0".
    pub bind_address: Option<String>,
}

const DEFAULT_UNRELIABLE_LIMIT: u32 = 64 * 1024;

#[napi(object)]
pub struct Accepted {
    pub peer: u32,
    pub answer: String,
    pub local_port: u32,
}

#[napi(object)]
pub struct Message {
    pub peer: u32,
    pub label: String,
    pub data: Buffer,
}

#[napi(object)]
pub struct PeerStats {
    pub connected: bool,
    pub closed: bool,
    pub stack_outstanding: u32,
    pub pending_cmd: u32,
    pub dropped: f64,
    pub sent_unreliable: f64,
    pub paced_drops: f64,
    pub budget: f64,
    pub rate: f64,
    pub draining: bool,
    pub srtt_ms: f64,
    pub min_rtt_ms: f64,
    pub cwnd: f64,
    pub inflight: f64,
    pub pending: f64,
    pub rack_lost: f64,
    pub rack_probes: f64,
    pub t3_timeouts: f64,
}

#[napi]
pub struct Server {
    limit: usize,
    rack: bool,
    rto_min: Option<u32>,
    pacer: bool,
    bind_address: String,
    next_id: u32,
    peers: HashMap<u32, Peer>,
    inbox: Arc<Mutex<Vec<Inbound>>>,
}

#[napi]
impl Server {
    #[napi(constructor)]
    pub fn new(opts: Option<ServerOptions>) -> Self {
        let _ = env_logger::try_init();
        let opts = opts.unwrap_or(ServerOptions {
            unreliable_limit: None,
            rack: None,
            rto_min: None,
            pacer: None,
            bind_address: None,
        });
        Server {
            limit: opts.unreliable_limit.unwrap_or(DEFAULT_UNRELIABLE_LIMIT) as usize,
            rack: opts.rack.unwrap_or(true),
            rto_min: opts.rto_min,
            pacer: opts.pacer.unwrap_or(true),
            bind_address: opts.bind_address.unwrap_or_else(|| "0.0.0.0".to_string()),
            next_id: 1,
            peers: HashMap::new(),
            inbox: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Accept a full (non-trickle) offer JSON (`{type, sdp}`). The answer carries one
    /// host candidate `advertiseIp:advertisePort` (port 0 = the bound port), so a NAT
    /// or test shaper can sit in front. Returns the peer id, answer JSON and bound port.
    #[napi]
    pub fn accept_offer(&mut self, offer_json: String, advertise_ip: String, advertise_port: u32) -> Result<Accepted> {
        let id = self.next_id;
        self.next_id += 1;
        let shared = Arc::new(PeerShared::default());
        let (tx, rx) = channel::<Cmd>();
        let (ans_tx, ans_rx) = channel::<std::result::Result<(String, u16), String>>();
        let sh = shared.clone();
        let inbox = self.inbox.clone();
        let (rack, rto_min, pacer) = (self.rack, self.rto_min, self.pacer);
        let bind = self.bind_address.clone();
        std::thread::Builder::new()
            .name(format!("peer-{id}"))
            .spawn(move || {
                if let Err(e) = worker(id, offer_json, bind, advertise_ip, advertise_port as u16, rack, rto_min, pacer, sh.clone(), inbox, rx, &ans_tx) {
                    let _ = ans_tx.send(Err(e.to_string()));
                }
                sh.closed.store(true, Ordering::Relaxed);
            })
            .map_err(|e| Error::from_reason(e.to_string()))?;
        let (answer, port) = ans_rx
            .recv()
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(Error::from_reason)?;
        self.peers.insert(id, Peer { shared, tx });
        Ok(Accepted { peer: id, answer, local_port: port as u32 })
    }

    /// Returns false when an unreliable send was dropped by the backpressure gate
    /// (caller keeps the state "owed"). `exempt` bypasses the gate (latency probes).
    #[napi]
    pub fn send(&self, peer: u32, label: String, data: Buffer, exempt: bool) -> Result<bool> {
        let p = self.peers.get(&peer).ok_or_else(|| Error::from_reason("no such peer"))?;
        let s = &p.shared;
        let unreliable = match s.channels.lock().unwrap().get(&label) {
            Some(u) => *u,
            None => return Err(Error::from_reason(format!("channel {label} not open"))),
        };
        let len = data.len();
        if unreliable {
            if !exempt && self.limit > 0 {
                let out = s.stack_outstanding.load(Ordering::Relaxed) + s.pending_cmd.load(Ordering::Relaxed);
                if out + len > self.limit {
                    s.dropped.fetch_add(1, Ordering::Relaxed);
                    return Ok(false);
                }
            }
            if !exempt && self.pacer && s.tokens.load(Ordering::Relaxed) < len as i64 {
                s.paced_drops.fetch_add(1, Ordering::Relaxed);
                s.dropped.fetch_add(1, Ordering::Relaxed);
                return Ok(false);
            }
            s.sent_unreliable.fetch_add(1, Ordering::Relaxed);
        }
        if self.pacer {
            // reliable always goes, and may push the budget negative
            s.tokens.fetch_sub(len as i64, Ordering::Relaxed);
        }
        s.pending_cmd.fetch_add(len, Ordering::Relaxed);
        p.tx
            .send(Cmd::Send { label, data: data.to_vec() })
            .map_err(|_| Error::from_reason("peer closed"))?;
        Ok(true)
    }

    /// Bytes the pacer allows right now (can be negative after reliable sends).
    #[napi]
    pub fn budget(&self, peer: u32) -> f64 {
        self.peers
            .get(&peer)
            .map_or(0.0, |p| p.shared.tokens.load(Ordering::Relaxed) as f64)
    }

    /// Take every message received since the last call (all peers, one N-API call).
    #[napi]
    pub fn drain(&self) -> Vec<Message> {
        let v = std::mem::take(&mut *self.inbox.lock().unwrap());
        v.into_iter()
            .map(|m| Message { peer: m.peer, label: m.label, data: Buffer::from(m.data) })
            .collect()
    }

    /// Per-peer queue, pacer and SCTP state. `null` for an unknown peer.
    #[napi]
    pub fn stats(&self, peer: u32) -> Option<PeerStats> {
        self.peers.get(&peer).map(|p| {
            let s = &p.shared;
            PeerStats {
                connected: s.connected.load(Ordering::Relaxed),
                closed: s.closed.load(Ordering::Relaxed),
                stack_outstanding: s.stack_outstanding.load(Ordering::Relaxed) as u32,
                pending_cmd: s.pending_cmd.load(Ordering::Relaxed) as u32,
                dropped: s.dropped.load(Ordering::Relaxed) as f64,
                sent_unreliable: s.sent_unreliable.load(Ordering::Relaxed) as f64,
                paced_drops: s.paced_drops.load(Ordering::Relaxed) as f64,
                budget: s.tokens.load(Ordering::Relaxed) as f64,
                rate: s.rate.load(Ordering::Relaxed) as f64,
                draining: s.draining.load(Ordering::Relaxed),
                srtt_ms: s.srtt_ms.load(Ordering::Relaxed) as f64,
                min_rtt_ms: s.min_rtt_ms.load(Ordering::Relaxed) as f64,
                cwnd: s.cwnd.load(Ordering::Relaxed) as f64,
                inflight: s.inflight.load(Ordering::Relaxed) as f64,
                pending: s.pending.load(Ordering::Relaxed) as f64,
                rack_lost: s.rack_lost.load(Ordering::Relaxed) as f64,
                rack_probes: s.rack_probes.load(Ordering::Relaxed) as f64,
                t3_timeouts: s.t3_timeouts.load(Ordering::Relaxed) as f64,
            }
        })
    }

    /// Close one peer and stop its worker.
    #[napi]
    pub fn close(&mut self, peer: u32) {
        if let Some(p) = self.peers.remove(&peer) {
            let _ = p.tx.send(Cmd::Close);
        }
    }

    /// Close every peer.
    #[napi]
    pub fn close_all(&mut self) {
        for (_, p) in self.peers.drain() {
            let _ = p.tx.send(Cmd::Close);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn worker(
    id: u32,
    offer_json: String,
    bind: String,
    advertise_ip: String,
    advertise_port: u16,
    rack: bool,
    rto_min: Option<u32>,
    pacer_on: bool,
    sh: Arc<PeerShared>,
    inbox: Arc<Mutex<Vec<Inbound>>>,
    rx: Receiver<Cmd>,
    ans_tx: &Sender<std::result::Result<(String, u16), String>>,
) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(format!("{bind}:0"))?;
    let local_addr = socket.local_addr()?;

    let offer: RTCSessionDescription = serde_json::from_str(&offer_json)?;
    let mut seb = SettingEngineBuilder::new().with_lite(true).with_sctp_rack(rack);
    if let Some(ms) = rto_min {
        seb = seb.with_sctp_rto_min(ms as u64);
    }
    let setting_engine = seb.build();
    let mut pc: RTCPeerConnection = RTCPeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_setting_engine(setting_engine)
        .build(Instant::now())?;
    pc.set_remote_description(Instant::now(), offer)?;
    // address the peer sees (a shaper/NAT may sit in front); ICE matches inbound by this
    let adv_port = if advertise_port == 0 { local_addr.port() } else { advertise_port };
    let adv_addr: SocketAddr = format!("{advertise_ip}:{adv_port}").parse()?;
    let cand = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: advertise_ip,
            port: adv_port,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    pc.add_local_candidate(RTCIceCandidate::from(&cand).to_json()?)?;
    let answer = pc.create_answer(None)?;
    pc.set_local_description(Instant::now(), answer.clone())?;
    let answer = pc.local_description().unwrap_or(answer);
    ans_tx
        .send(Ok((serde_json::to_string(&answer)?, local_addr.port())))
        .ok();

    let mut buf = vec![0u8; 2048];
    let mut labels: HashMap<_, String> = HashMap::new();
    let mut ids: HashMap<String, _> = HashMap::new();
    let mut deadline: Option<Instant> = None;
    let mut pacer = pacer::Pacer::new(Instant::now());
    if pacer_on {
        sh.rate.store(pacer.rate as u64, Ordering::Relaxed);
    }
    let mut last_stats = Instant::now();

    loop {
        // commands from JS
        loop {
            match rx.try_recv() {
                Ok(Cmd::Send { label, data }) => {
                    let len = data.len();
                    if let Some(cid) = ids.get(&label).copied() {
                        if let Some(mut dc) = pc.data_channel(cid) {
                            let _ = dc.send(Instant::now(), BytesMut::from(&data[..]));
                        }
                    }
                    sh.pending_cmd.fetch_sub(len, Ordering::Relaxed);
                }
                Ok(Cmd::Close) => return Ok(()),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        if let Some(d) = deadline {
            if Instant::now() >= d {
                let _ = pc.handle_timeout(Instant::now());
            }
        }

        while let Some(msg) = pc.poll_write() {
            let addr = msg.transport.peer_addr;
            if let Err(e) = socket.send_to(&msg.message, addr) {
                log::debug!("peer {id}: send_to {addr} failed: {e}");
            }
        }
        while let Some(ev) = pc.poll_event() {
            match ev {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(st) => {
                    let s = st.to_string();
                    if s == "connected" {
                        sh.connected.store(true, Ordering::Relaxed);
                    }
                    if s == "failed" || s == "closed" {
                        return Ok(());
                    }
                }
                RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(cid)) => {
                    if let Some(dc) = pc.data_channel(cid) {
                        let label = dc.label().to_string();
                        let unreliable = dc.max_retransmits().is_some() || dc.max_packet_life_time().is_some();
                        labels.insert(cid, label.clone());
                        ids.insert(label.clone(), cid);
                        sh.channels.lock().unwrap().insert(label, unreliable);
                    }
                }
                _ => {}
            }
        }
        {
            let mut got = Vec::new();
            while let Some(TaggedRTCMessage { message, .. }) = pc.poll_read() {
                if let RTCMessage::DataChannelMessage(cid, m) = message {
                    if let Some(l) = labels.get(&cid) {
                        got.push(Inbound { peer: id, label: l.clone(), data: m.data.to_vec() });
                    }
                }
            }
            if !got.is_empty() {
                inbox.lock().unwrap().extend(got);
            }
        }
        // true stack queue
        let mut out = 0usize;
        for cid in ids.values() {
            if let Some(dc) = pc.data_channel(*cid) {
                out += dc.outstanding_bytes();
            }
        }
        sh.stack_outstanding.store(out, Ordering::Relaxed);

        // pacer + transport stats
        let now = Instant::now();
        if pacer_on {
            let add = pacer.refill(now);
            let cap = pacer.burst_cap(1200) as i64;
            let t = sh.tokens.fetch_add(add as i64, Ordering::Relaxed) + add as i64;
            if t > cap {
                sh.tokens.store(cap, Ordering::Relaxed);
            }
        }
        if now.saturating_duration_since(last_stats) >= pacer::INTERVAL {
            last_stats = now;
            if let Some(ts) = pc.sctp_transport_stats() {
                sh.srtt_ms.store(ts.srtt_ms, Ordering::Relaxed);
                sh.min_rtt_ms.store(ts.min_rtt_ms.unwrap_or(0), Ordering::Relaxed);
                sh.cwnd.store(ts.cwnd as u64, Ordering::Relaxed);
                sh.inflight.store(ts.inflight_bytes as u64, Ordering::Relaxed);
                sh.pending.store(ts.pending_bytes as u64, Ordering::Relaxed);
                sh.rack_lost.store(ts.rack_lost, Ordering::Relaxed);
                sh.rack_probes.store(ts.rack_probes, Ordering::Relaxed);
                sh.t3_timeouts.store(ts.t3_timeouts, Ordering::Relaxed);
                if pacer_on {
                    let sig = pacer::Signals {
                        acked_total: ts.acked_bytes_total,
                        outstanding: out + sh.pending_cmd.load(Ordering::Relaxed),
                        srtt: Duration::from_millis(ts.srtt_ms),
                        min_rtt: ts.min_rtt_ms.map(Duration::from_millis),
                        mtu: ts.mtu,
                    };
                    // force the interval check inside update to pass
                    pacer.update(now, &sig);
                    sh.rate.store(pacer.rate as u64, Ordering::Relaxed);
                    sh.draining.store(pacer.draining, Ordering::Relaxed);
                }
            }
        }

        deadline = pc.poll_timeout();
        let now = Instant::now();
        let mut wait = Duration::from_millis(1);
        if let Some(d) = deadline {
            wait = wait.min(d.saturating_duration_since(now));
        }
        if wait.is_zero() {
            wait = Duration::from_micros(50);
        }
        socket.set_read_timeout(Some(wait))?;
        match socket.recv_from(&mut buf) {
            Ok((n, peer_addr)) => {
                let r = pc.handle_read(TaggedBytesMut {
                    now: Instant::now(),
                    transport: TransportContext {
                        local_addr: adv_addr,
                        peer_addr,
                        ecn: None,
                        transport_protocol: TransportProtocol::UDP,
                    },
                    message: BytesMut::from(&buf[..n]),
                });
                if let Err(e) = r {
                    log::debug!("peer {id}: handle_read: {e}");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
    }
}
