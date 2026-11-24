//! lagless: low-latency WebRTC data channel server for Node.js.
//!
//! Built on the sans-IO webrtc-rs `rtc` stack (our fork, with RACK-TLP and
//! transport-stats patches). One event-loop thread and one UDP port for all
//! peers (see event_loop.rs).
//! Unreliable sends are gated on the true SCTP queue and on a per-peer pacer
//! budget, so stale game state is dropped instead of queued.

mod event_loop;
mod gate;
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
    /// bytes SCTP holds but has not sent yet (cwnd full), refreshed every loop pass
    stack_unsent: AtomicUsize,
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
    app_limited: AtomicBool,
    paced_drops: AtomicU64,
    egress_drops: AtomicU64,
    /// a send hit the gate or the pacer budget since the last pacer update
    /// (the link, not the app, limited sending)
    budget_limited: AtomicBool,
    // SCTP transport state
    srtt_ms: AtomicU64,
    min_rtt_ms: AtomicU64,
    cwnd: AtomicU64,
    inflight: AtomicU64,
    pending: AtomicU64,
    rack_lost: AtomicU64,
    rack_probes: AtomicU64,
    t3_timeouts: AtomicU64,
    // send outcomes (see gate.rs)
    reliable_overflows: AtomicU64,
    send_errors: AtomicU64,
    overloaded: AtomicBool,
    overloads: AtomicU64,
    /// negotiated SCTP max message size, 0 until known
    max_message_size: AtomicUsize,
}

#[napi(object)]
pub struct ServerOptions {
    /// Drop an unreliable send when bytes not yet released by SCTP + its length
    /// would exceed this. 0 disables the gate. Default 65536. The gate also drops
    /// unreliable while SCTP holds more than 2400 B it cannot send yet (cwnd full),
    /// so unreliable never sits in SCTP's queue ahead of reliable.
    ///
    /// A channel counts as unreliable when it was opened with `maxRetransmits` or
    /// `maxPacketLifeTime` set (any value, so `maxRetransmits: 3` is gated too).
    /// Only those channels are gated and paced; every other channel is reliable.
    pub unreliable_limit: Option<u32>,
    /// Reliable byte cap: when a reliable send would push the bytes queued for a peer
    /// (all channels, not yet released by SCTP) past this, the peer is closed with
    /// reason "reliable-overflow" and send() returns false. 0 = unbounded. Default 4 MiB.
    pub reliable_limit: Option<u32>,
    /// Reliable-overload signal: an `overload` event (and `stats().overloaded`) once the
    /// pacer budget has stayed below zero for this many ms (reliable load alone fills the
    /// link and unreliable gets nothing). 0 disables. Default 1000.
    pub overload_ms: Option<u32>,
    /// SCTP send weight per channel label, in data channel priority units
    /// (128 very-low, 256 low = default, 512 medium, 1024 high). Setting any switches the
    /// association to weighted fair queueing: each channel gets a share of the bytes SCTP
    /// sends in proportion to its weight, so a 1024 pong channel jumps a 256 state queue.
    pub channel_priority: Option<HashMap<String, u32>>,
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
    /// Pacer rate before the first delivery sample, bytes/s. Default 65536.
    pub pacer_start_rate: Option<u32>,
    /// Pacer rate floor, bytes/s. Default 8192.
    pub pacer_min_rate: Option<u32>,
    /// Token bucket depth in ms of rate (pacer and egress cap). Default 100.
    pub pacer_burst_ms: Option<u32>,
    /// Server-wide egress cap over all peers, bytes/s. Unreliable sends need
    /// budget from it too; reliable and exempt sends always go and may push it negative.
    /// Not fair across peers: whoever sends first in a tick gets it. 0 or unset = off.
    pub egress_rate: Option<u32>,
}

/// Shared token bucket for the server-wide egress cap. Refilled lazily on send().
struct Egress {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl Egress {
    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + self.rate * dt).min(self.rate * self.burst);
    }
}

const DEFAULT_UNRELIABLE_LIMIT: u32 = 64 * 1024;
const DEFAULT_RELIABLE_LIMIT: u32 = 4 * 1024 * 1024;
const DEFAULT_OVERLOAD_MS: u32 = 1000;

#[napi(object)]
pub struct Accepted {
    pub peer: u32,
    pub answer: String,
    pub local_port: u32,
}

/// One item of a batch pushed to JS. `kind` is message | open | channelopen | close | error | overload.
#[napi(object)]
pub struct Event {
    pub kind: String,
    pub peer: u32,
    pub label: Option<String>,
    pub data: Option<Buffer>,
    /// close: local | closed | failed | reliable-overflow. error: what failed.
    /// overload: start | end.
    pub reason: Option<String>,
    /// message: sent as a string (PPID 51/56) rather than binary
    pub is_string: Option<bool>,
}

impl Event {
    pub(crate) fn simple(kind: &str, peer: u32) -> Self {
        Event {
            kind: kind.into(),
            peer,
            label: None,
            data: None,
            reason: None,
            is_string: None,
        }
    }
    pub(crate) fn channel(kind: &str, peer: u32, label: String) -> Self {
        Event {
            label: Some(label),
            ..Self::simple(kind, peer)
        }
    }
    pub(crate) fn message(peer: u32, label: String, data: Vec<u8>, is_string: bool) -> Self {
        Event {
            data: Some(data.into()),
            is_string: Some(is_string),
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
    pub(crate) fn overload(peer: u32, on: bool) -> Self {
        Event {
            reason: Some(if on { "start" } else { "end" }.into()),
            ..Self::simple("overload", peer)
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
    /// unreliable sends dropped by the server-wide egress cap
    pub egress_drops: f64,
    pub budget: f64,
    /// pacer rate, bytes/s; updated about every max(100 ms, srtt)
    pub rate: f64,
    pub draining: bool,
    /// the last pacer delivery sample was app-limited (app sent less than the link carries)
    pub app_limited: bool,
    pub srtt_ms: f64,
    pub min_rtt_ms: f64,
    pub cwnd: f64,
    pub inflight: f64,
    pub pending: f64,
    pub rack_lost: f64,
    pub rack_probes: f64,
    pub t3_timeouts: f64,
    /// messages rtc refused after send() returned true (each also emits `error`)
    pub send_errors: f64,
    /// pacer budget has been negative for `overloadMs`: shed reliable load or kick
    pub overloaded: bool,
    /// times `overloaded` turned true
    pub overloads: f64,
}

#[napi]
pub struct Server {
    limits: gate::Limits,
    egress: Option<Mutex<Egress>>,
    wfq: bool,
    rack: bool,
    rto_min: Option<u32>,
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
            reliable_limit: None,
            overload_ms: None,
            channel_priority: None,
            rack: None,
            rto_min: None,
            pacer: None,
            bind_address: None,
            port: None,
            pacer_start_rate: None,
            pacer_min_rate: None,
            pacer_burst_ms: None,
            egress_rate: None,
        });
        let d = pacer::Config::default();
        let pacer_cfg = pacer::Config {
            start_rate: opts.pacer_start_rate.map_or(d.start_rate, |v| v.max(1) as f64),
            floor_rate: opts.pacer_min_rate.map_or(d.floor_rate, |v| v.max(1) as f64),
            burst: opts.pacer_burst_ms.map_or(d.burst, |v| v.max(1) as f64 / 1000.0),
        };
        let egress = opts.egress_rate.filter(|r| *r > 0).map(|r| {
            let rate = r as f64;
            Mutex::new(Egress {
                rate,
                burst: pacer_cfg.burst,
                tokens: rate * pacer_cfg.burst,
                last: Instant::now(),
            })
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
        let priorities: HashMap<String, u16> = opts
            .channel_priority
            .unwrap_or_default()
            .into_iter()
            .map(|(l, p)| (l, p.clamp(1, u16::MAX as u32) as u16))
            .collect();
        let wfq = !priorities.is_empty();
        let overload_ms = if pacer { opts.overload_ms.unwrap_or(DEFAULT_OVERLOAD_MS) } else { 0 };
        let (lp, ctl) = event_loop::new(sock, registry.clone(), pacer, pacer_cfg, overload_ms, priorities, tx, rx)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        let thread = std::thread::Builder::new()
            .name("lagless".into())
            .spawn(move || lp.run())
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Server {
            limits: gate::Limits {
                unreliable: opts.unreliable_limit.unwrap_or(DEFAULT_UNRELIABLE_LIMIT) as usize,
                reliable: opts.reliable_limit.unwrap_or(DEFAULT_RELIABLE_LIMIT) as usize,
                pacer,
            },
            egress,
            wfq,
            rack: opts.rack.unwrap_or(true),
            rto_min: opts.rto_min,
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
            bound_port: self.local_addr.port(),
            rack: self.rack,
            rto_min: self.rto_min,
            wfq: self.wfq,
            registry: self.registry.clone(),
            ctl: self.ctl.clone(),
        }))
    }

    /// Returns false when an unreliable send was dropped by the backpressure gate
    /// (caller keeps the state "owed"), or when a reliable send hit the reliable byte
    /// cap (the peer is then closed: `close` event, reason "reliable-overflow").
    /// `exempt` bypasses the gate and pacer (latency probes). `is_string` sends `data`
    /// (UTF-8) as a string message.
    ///
    /// Each check-and-reserve is atomic, but the gate reads a snapshot of the SCTP queue
    /// taken by the loop thread, so limits are approximate by up to one loop pass.
    /// Call send() for a given Server from one JS thread (it is not shareable anyway).
    #[napi]
    pub fn send(
        &self,
        peer: u32,
        label: String,
        data: Buffer,
        exempt: bool,
        is_string: Option<bool>,
    ) -> Result<bool> {
        let s = self
            .registry
            .lock()
            .unwrap()
            .get(&peer)
            .cloned()
            .ok_or_else(|| Error::from_reason("no such peer"))?;
        if s.closed.load(Ordering::Acquire) {
            return Err(Error::from_reason("peer closed"));
        }
        let unreliable = match s.channels.lock().unwrap().get(&label) {
            Some(u) => *u,
            None => return Err(Error::from_reason(format!("channel {label} not open"))),
        };
        let len = data.len();
        let max = s.max_message_size.load(Ordering::Relaxed);
        if max > 0 && len > max {
            return Err(Error::from_reason(format!(
                "message of {len} bytes exceeds max message size {max}"
            )));
        }
        let is_string = is_string.unwrap_or(false);
        if is_string && std::str::from_utf8(&data).is_err() {
            return Err(Error::from_reason("string message is not valid UTF-8"));
        }
        // server-wide egress cap: held across admit so check + spend is one step
        let mut egress = self.egress.as_ref().map(|e| e.lock().unwrap());
        if let Some(eg) = egress.as_deref_mut() {
            eg.refill(Instant::now());
            if unreliable && !exempt && eg.tokens < len as f64 {
                s.egress_drops.fetch_add(1, Ordering::Relaxed);
                s.dropped.fetch_add(1, Ordering::Relaxed);
                return Ok(false);
            }
        }
        match gate::admit(&s, self.limits, unreliable, exempt, len) {
            gate::Verdict::Queued => {
                if let Some(eg) = egress.as_deref_mut() {
                    eg.tokens -= len as f64;
                }
                drop(egress);
            }
            gate::Verdict::Dropped => return Ok(false),
            gate::Verdict::Overflow => {
                s.closed.store(true, Ordering::Release);
                self.ctl.push(Cmd::Kick(peer, "reliable-overflow"));
                return Ok(false);
            }
        }
        if !self.ctl.push(Cmd::Send {
            peer,
            label,
            data: data.to_vec(),
            is_string,
        }) {
            s.pending_cmd.fetch_sub(len, Ordering::AcqRel);
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
    /// SCTP fields (srtt, cwnd, inflight, ...) are snapshots taken every 50 ms;
    /// `rate`, `draining` and `appLimited` change once per pacer sample
    /// (every max(100 ms, srtt)). `budget`, `stackOutstanding` and the drop
    /// counters are live.
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
            egress_drops: s.egress_drops.load(Ordering::Relaxed) as f64,
            budget: s.tokens.load(Ordering::Relaxed) as f64,
            rate: s.rate.load(Ordering::Relaxed) as f64,
            draining: s.draining.load(Ordering::Relaxed),
            app_limited: s.app_limited.load(Ordering::Relaxed),
            srtt_ms: s.srtt_ms.load(Ordering::Relaxed) as f64,
            min_rtt_ms: s.min_rtt_ms.load(Ordering::Relaxed) as f64,
            cwnd: s.cwnd.load(Ordering::Relaxed) as f64,
            inflight: s.inflight.load(Ordering::Relaxed) as f64,
            pending: s.pending.load(Ordering::Relaxed) as f64,
            rack_lost: s.rack_lost.load(Ordering::Relaxed) as f64,
            rack_probes: s.rack_probes.load(Ordering::Relaxed) as f64,
            t3_timeouts: s.t3_timeouts.load(Ordering::Relaxed) as f64,
            send_errors: s.send_errors.load(Ordering::Relaxed) as f64,
            overloaded: s.overloaded.load(Ordering::Relaxed),
            overloads: s.overloads.load(Ordering::Relaxed) as f64,
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
    /// the shared socket's bound port (what `localPort` reports)
    bound_port: u16,
    rack: bool,
    rto_min: Option<u32>,
    wfq: bool,
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
            local_port: self.bound_port as u32,
        })
    }
}

impl AcceptTask {
    fn build(&mut self) -> anyhow::Result<String> {
        let offer: RTCSessionDescription = serde_json::from_str(&self.offer_json)?;
        let mut seb = SettingEngineBuilder::new()
            .with_lite(true)
            .with_sctp_rack(self.rack)
            .with_sctp_weighted_fair_queueing(self.wfq);
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
