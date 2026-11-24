//! lagless: low-latency WebRTC data channel server for Node.js.
//!
//! Built on the sans-IO webrtc-rs `rtc` stack (our fork, with RACK-TLP and
//! transport-stats patches). One event-loop thread and one UDP port for all
//! peers (see event_loop.rs).
//! Unreliable sends are gated on the true SCTP queue and on a per-peer pacer
//! budget, so stale game state is dropped instead of queued.

mod event_loop;
mod pacer;
mod stun;

use event_loop::{Cmd, Ctl, NewPeer};
use napi::Either;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use rtc::peer_connection::RTCPeerConnectionBuilder;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::configuration::setting_engine::SettingEngineBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::{CandidateConfig, CandidateHostConfig, RTCIceCandidate};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// live peers, shared by the JS thread (send/stats) and the loop (removes dead ones)
pub(crate) type Registry = Arc<Mutex<HashMap<u32, Arc<PeerShared>>>>;

#[derive(Default)]
pub(crate) struct PeerShared {
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
    /// Local address the server's UDP socket binds to. Default "0.0.0.0".
    /// "::" binds dual-stack (IPv4 + IPv6) where the OS allows it.
    pub bind_address: Option<String>,
    /// UDP port every peer shares. Default 0 (any free port, see `localPort`).
    pub port: Option<u32>,
}

const DEFAULT_UNRELIABLE_LIMIT: u32 = 64 * 1024;

#[napi(object)]
pub struct Accepted {
    pub peer: u32,
    pub answer: String,
    pub local_port: u32,
}

/// One item of a batch pushed to JS. `kind` is message | open | channelopen | close | error.
#[napi(object)]
pub struct Event {
    pub kind: String,
    pub peer: u32,
    pub label: Option<String>,
    pub data: Option<Buffer>,
    /// close: local | closed | failed. error: what failed.
    pub reason: Option<String>,
}

impl Event {
    pub(crate) fn simple(kind: &str, peer: u32) -> Self {
        Event {
            kind: kind.into(),
            peer,
            label: None,
            data: None,
            reason: None,
        }
    }
    pub(crate) fn channel(kind: &str, peer: u32, label: String) -> Self {
        Event {
            label: Some(label),
            ..Self::simple(kind, peer)
        }
    }
    pub(crate) fn message(peer: u32, label: String, data: Vec<u8>) -> Self {
        Event {
            data: Some(data.into()),
            ..Self::channel("message", peer, label)
        }
    }
    pub(crate) fn close(peer: u32, reason: &str) -> Self {
        Event {
            reason: Some(reason.into()),
            ..Self::simple("close", peer)
        }
    }
    pub(crate) fn error(peer: u32, reason: String) -> Self {
        Event {
            reason: Some(reason),
            ..Self::simple("error", peer)
        }
    }
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
    local_addr: SocketAddr,
    next_id: std::sync::atomic::AtomicU32,
    registry: Registry,
    ctl: Ctl,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[napi]
impl Server {
    /// Binds the shared UDP socket and starts the event-loop thread (idle until peers exist).
    #[napi(constructor)]
    pub fn new(opts: Option<ServerOptions>) -> Result<Self> {
        let _ = env_logger::try_init();
        let opts = opts.unwrap_or(ServerOptions {
            unreliable_limit: None,
            rack: None,
            rto_min: None,
            pacer: None,
            bind_address: None,
            port: None,
        });
        let bind = opts.bind_address.as_deref().unwrap_or("0.0.0.0");
        let ip: IpAddr = bind
            .parse()
            .map_err(|e| Error::from_reason(format!("bindAddress {bind}: {e}")))?;
        let addr = SocketAddr::new(ip, opts.port.unwrap_or(0) as u16);
        let sock =
            bind_socket(addr).map_err(|e| Error::from_reason(format!("bind {addr}: {e}")))?;
        let local_addr = sock
            .local_addr()
            .map_err(|e| Error::from_reason(e.to_string()))?;
        let pacer = opts.pacer.unwrap_or(true);
        let registry: Registry = Arc::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let (lp, ctl) = event_loop::new(sock, registry.clone(), pacer, tx, rx)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        let thread = std::thread::Builder::new()
            .name("lagless".into())
            .spawn(move || lp.run())
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Server {
            limit: opts.unreliable_limit.unwrap_or(DEFAULT_UNRELIABLE_LIMIT) as usize,
            rack: opts.rack.unwrap_or(true),
            rto_min: opts.rto_min,
            pacer,
            local_addr,
            next_id: 1.into(),
            registry,
            ctl,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// UDP port every peer shares.
    #[napi(getter)]
    pub fn local_port(&self) -> u32 {
        self.local_addr.port() as u32
    }

    /// Set the callback that receives event batches (`Event[]`). Holds the process
    /// open until `shutdown()`. Only the first call counts.
    #[napi]
    pub fn start(&self, cb: Function<Vec<Event>, ()>) -> Result<()> {
        let tsfn = cb
            .build_threadsafe_function::<Vec<Event>>()
            .callee_handled::<false>()
            .build()?;
        self.ctl.push(Cmd::Start(tsfn));
        Ok(())
    }

    /// Accept a full (non-trickle) offer JSON (`{type, sdp}`). The answer carries one
    /// host candidate per advertise IP at `advertisePort` (0 = the server's port), so a
    /// NAT or test shaper can sit in front. Resolves with the peer id, answer JSON and port.
    /// Building the peer (certificate, DTLS/SCTP setup) runs off the JS thread.
    #[napi(ts_return_type = "Promise<Accepted>")]
    pub fn accept_offer(
        &self,
        offer_json: String,
        advertise_ip: Either<String, Vec<String>>,
        advertise_port: u32,
    ) -> Result<AsyncTask<AcceptTask>> {
        let ips = match advertise_ip {
            Either::A(s) => vec![s],
            Either::B(v) => v,
        };
        if ips.is_empty() {
            return Err(Error::from_reason("need at least one advertise IP"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let port = if advertise_port == 0 {
            self.local_addr.port()
        } else {
            advertise_port as u16
        };
        Ok(AsyncTask::new(AcceptTask {
            id,
            offer_json,
            ips,
            port,
            rack: self.rack,
            rto_min: self.rto_min,
            registry: self.registry.clone(),
            ctl: self.ctl.clone(),
        }))
    }

    /// Returns false when an unreliable send was dropped by the backpressure gate
    /// (caller keeps the state "owed"). `exempt` bypasses the gate (latency probes).
    #[napi]
    pub fn send(&self, peer: u32, label: String, data: Buffer, exempt: bool) -> Result<bool> {
        let s = self
            .registry
            .lock()
            .unwrap()
            .get(&peer)
            .cloned()
            .ok_or_else(|| Error::from_reason("no such peer"))?;
        let unreliable = match s.channels.lock().unwrap().get(&label) {
            Some(u) => *u,
            None => return Err(Error::from_reason(format!("channel {label} not open"))),
        };
        let len = data.len();
        if unreliable {
            if !exempt && self.limit > 0 {
                let out = s.stack_outstanding.load(Ordering::Relaxed)
                    + s.pending_cmd.load(Ordering::Relaxed);
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
        if !self.ctl.push(Cmd::Send {
            peer,
            label,
            data: data.to_vec(),
        }) {
            s.pending_cmd.fetch_sub(len, Ordering::Relaxed);
            return Err(Error::from_reason("server closed"));
        }
        Ok(true)
    }

    /// Bytes the pacer allows right now (can be negative after reliable sends).
    #[napi]
    pub fn budget(&self, peer: u32) -> f64 {
        self.registry
            .lock()
            .unwrap()
            .get(&peer)
            .map_or(0.0, |s| s.tokens.load(Ordering::Relaxed) as f64)
    }

    /// Per-peer queue, pacer and SCTP state. `null` for an unknown or closed peer.
    #[napi]
    pub fn stats(&self, peer: u32) -> Option<PeerStats> {
        let s = self.registry.lock().unwrap().get(&peer).cloned()?;
        Some(PeerStats {
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
        })
    }

    /// Close one peer: SCTP ABORT + DTLS close_notify go out, then a `close` event.
    #[napi]
    pub fn close(&self, peer: u32) {
        self.ctl.push(Cmd::Close(peer));
    }

    /// Close every peer (the server keeps running).
    #[napi]
    pub fn close_all(&self) {
        self.ctl.push(Cmd::CloseAll);
    }

    /// Close every peer, stop the loop thread and release the callback.
    #[napi]
    pub fn shutdown(&self) {
        self.ctl.push(Cmd::Shutdown);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

/// Big socket buffers: one socket carries every peer.
fn bind_socket(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() && addr.ip().is_unspecified() {
        let _ = s.set_only_v6(false);
    }
    let _ = s.set_recv_buffer_size(4 << 20);
    let _ = s.set_send_buffer_size(4 << 20);
    s.bind(&addr.into())?;
    Ok(s.into())
}

pub struct AcceptTask {
    id: u32,
    offer_json: String,
    ips: Vec<String>,
    port: u16,
    rack: bool,
    rto_min: Option<u32>,
    registry: Registry,
    ctl: Ctl,
}

impl Task for AcceptTask {
    type Output = String;
    type JsValue = Accepted;

    fn compute(&mut self) -> Result<String> {
        self.build().map_err(|e| Error::from_reason(e.to_string()))
    }

    fn resolve(&mut self, _env: Env, answer: String) -> Result<Accepted> {
        Ok(Accepted {
            peer: self.id,
            answer,
            local_port: self.port as u32,
        })
    }
}

impl AcceptTask {
    fn build(&mut self) -> anyhow::Result<String> {
        let offer: RTCSessionDescription = serde_json::from_str(&self.offer_json)?;
        let mut seb = SettingEngineBuilder::new()
            .with_lite(true)
            .with_sctp_rack(self.rack);
        if let Some(ms) = self.rto_min {
            seb = seb.with_sctp_rto_min(ms as u64);
        }
        let mut pc = RTCPeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_setting_engine(seb.build())
            .build(Instant::now())?;
        pc.set_remote_description(Instant::now(), offer)?;
        // addresses the peer sees (a shaper/NAT may sit in front); ICE matches inbound by these
        let mut local_addrs = Vec::new();
        for ip in &self.ips {
            let parsed: IpAddr = ip
                .parse()
                .map_err(|e| anyhow::anyhow!("advertise IP {ip}: {e}"))?;
            local_addrs.push(SocketAddr::new(parsed, self.port));
            let cand = CandidateHostConfig {
                base_config: CandidateConfig {
                    network: "udp".to_owned(),
                    address: parsed.to_string(),
                    port: self.port,
                    component: 1,
                    ..Default::default()
                },
                ..Default::default()
            }
            .new_candidate_host()?;
            pc.add_local_candidate(RTCIceCandidate::from(&cand).to_json()?)?;
        }
        let answer = pc.create_answer(None)?;
        pc.set_local_description(Instant::now(), answer.clone())?;
        let answer = pc.local_description().unwrap_or(answer);
        let ufrag = stun::sdp_ufrag(&answer.sdp)
            .ok_or_else(|| anyhow::anyhow!("answer has no ice-ufrag"))?;
        let shared = Arc::new(PeerShared::default());
        self.registry
            .lock()
            .unwrap()
            .insert(self.id, shared.clone());
        let np = NewPeer {
            id: self.id,
            pc,
            ufrag,
            local_addrs,
            shared,
        };
        if !self.ctl.push(Cmd::Add(Box::new(np))) {
            self.registry.lock().unwrap().remove(&self.id);
            anyhow::bail!("server closed");
        }
        Ok(serde_json::to_string(&answer)?)
    }
}
