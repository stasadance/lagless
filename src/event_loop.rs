//! One thread, one UDP socket for every peer.
//!
//! The thread blocks in mio until the socket is readable, a command arrives from
//! JS (waker), or the earliest rtc timer / pacer tick is due. Datagrams are demuxed
//! to peers by remote address; a new address is bound to a peer by the local ICE
//! ufrag in its STUN Binding request. Inbound messages and lifecycle events are
//! batched and pushed to JS through one ThreadsafeFunction call per wakeup.

use crate::gate;
use crate::pacer;
use crate::{Event, PeerShared, Registry};
use bytes::BytesMut;
use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token, Waker};
use napi::Status;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use rtc::data_channel::RTCDataChannelId;
use rtc::peer_connection::RTCPeerConnection;
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::{RTCMessage, TaggedRTCMessage};
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::peer_connection::transport::{RTCDtlsTransportState, RTCSctpTransportState};
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

pub type Tsfn = ThreadsafeFunction<Vec<Event>, (), Vec<Event>, Status, false>;

pub const SOCK: Token = Token(0);
pub const WAKE: Token = Token(1);

/// Pacer refill granularity. Only runs while there are peers.
const TICK_PACED: Duration = Duration::from_millis(10);

pub struct NewPeer {
    pub id: u32,
    pub pc: RTCPeerConnection,
    pub ufrag: String,
    /// advertised host candidates; inbound packets are tagged with the one of the same family
    pub local_addrs: Vec<SocketAddr>,
    pub shared: Arc<PeerShared>,
}

pub enum Cmd {
    Start(Tsfn),
    Add(Box<NewPeer>),
    Send {
        peer: u32,
        label: String,
        data: Vec<u8>,
        is_string: bool,
    },
    Close(u32),
    /// close a peer gracefully with this reason (e.g. reliable-overflow)
    Kick(u32, &'static str),
    CloseAll,
    /// send a STUN Binding request from the shared socket; the mapped address goes to `reply`
    Stun {
        to: SocketAddr,
        txid: [u8; 12],
        reply: Sender<SocketAddr>,
    },
    Shutdown,
}

/// JS-thread handle to the loop: queue a command and wake it (once per batch).
#[derive(Clone)]
pub struct Ctl {
    tx: Sender<Cmd>,
    waker: Arc<Waker>,
    pending: Arc<AtomicBool>,
}

impl Ctl {
    pub fn push(&self, cmd: Cmd) -> bool {
        if self.tx.send(cmd).is_err() {
            return false;
        }
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.waker.wake();
        }
        true
    }
}

struct PeerConn {
    pc: RTCPeerConnection,
    shared: Arc<PeerShared>,
    ufrag: String,
    local_addrs: Vec<SocketAddr>,
    remotes: Vec<SocketAddr>,
    labels: HashMap<RTCDataChannelId, String>,
    ids: HashMap<String, RTCDataChannelId>,
    pacer: pacer::Pacer,
    overload: gate::Overload,
    deadline: Option<Instant>,
    opened: bool,
    sctp_up: bool,
    /// SCTP had data waiting on cwnd at a stats snapshot since the last pacer update
    cwnd_limited: bool,
}

impl PeerConn {
    fn local_for(&self, from: SocketAddr) -> SocketAddr {
        self.local_addrs
            .iter()
            .copied()
            .find(|a| a.is_ipv4() == from.is_ipv4())
            .unwrap_or(self.local_addrs[0])
    }

    /// Register a channel by id; returns its label. Also covers a message that
    /// shows up for a channel we have not seen OnOpen for yet.
    fn learn(&mut self, cid: RTCDataChannelId, priorities: &HashMap<String, u16>) -> Option<String> {
        if let Some(l) = self.labels.get(&cid) {
            return Some(l.clone());
        }
        let mut dc = self.pc.data_channel(cid)?;
        let label = dc.label().to_string();
        if let Some(&w) = priorities.get(&label)
            && let Err(e) = dc.set_priority(w)
        {
            log::debug!("set_priority {label}: {e}");
        }
        let unreliable = dc.max_retransmits().is_some() || dc.max_packet_life_time().is_some();
        self.labels.insert(cid, label.clone());
        self.ids.insert(label.clone(), cid);
        self.shared
            .channels
            .lock()
            .unwrap()
            .insert(label.clone(), unreliable);
        if let Some(max) = self.pc.sctp().and_then(|t| t.max_message_size()) {
            self.shared
                .max_message_size
                .store(max as usize, Ordering::Relaxed);
        }
        Some(label)
    }
}

pub struct Loop {
    poll: Poll,
    sock: UdpSocket,
    v6: bool,
    rx: Receiver<Cmd>,
    pending: Arc<AtomicBool>,
    registry: Registry,
    pacer_on: bool,
    pacer_cfg: pacer::Config,
    overload_ms: u32,
    priorities: HashMap<String, u16>,
    tsfn: Option<Tsfn>,
    peers: HashMap<u32, PeerConn>,
    by_addr: HashMap<SocketAddr, u32>,
    by_ufrag: HashMap<String, u32>,
    timers: BTreeSet<(Instant, u32)>,
    dirty: HashSet<u32>,
    out: Vec<Event>,
    next_tick: Option<Instant>,
    last_stats: Instant,
    buf: Vec<u8>,
    /// open STUN transactions: txid -> (server, started, reply)
    stun: HashMap<[u8; 12], (SocketAddr, Instant, Sender<SocketAddr>)>,
}

/// Bind the shared socket and build the loop; the caller runs it on its own thread.
pub fn new(
    sock: std::net::UdpSocket,
    registry: Registry,
    pacer_on: bool,
    pacer_cfg: pacer::Config,
    overload_ms: u32,
    priorities: HashMap<String, u16>,
    tx: Sender<Cmd>,
    rx: Receiver<Cmd>,
) -> std::io::Result<(Loop, Ctl)> {
    sock.set_nonblocking(true)?;
    let v6 = sock.local_addr()?.is_ipv6();
    let mut sock = UdpSocket::from_std(sock);
    let poll = Poll::new()?;
    poll.registry()
        .register(&mut sock, SOCK, Interest::READABLE)?;
    let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
    let pending = Arc::new(AtomicBool::new(false));
    let ctl = Ctl {
        tx,
        waker,
        pending: pending.clone(),
    };
    Ok((
        Loop {
            poll,
            sock,
            v6,
            rx,
            pending,
            registry,
            pacer_on,
            pacer_cfg,
            overload_ms,
            priorities,
            tsfn: None,
            peers: HashMap::new(),
            by_addr: HashMap::new(),
            by_ufrag: HashMap::new(),
            timers: BTreeSet::new(),
            dirty: HashSet::new(),
            out: Vec::new(),
            next_tick: None,
            last_stats: Instant::now(),
            buf: vec![0u8; 65536],
            stun: HashMap::new(),
        },
        ctl,
    ))
}

/// v4-mapped v6 (from a dual-stack socket) -> plain v4, so it matches v4 candidates.
fn canonical(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}

impl Loop {
    pub fn run(mut self) {
        let mut events = Events::with_capacity(64);
        loop {
            let now = Instant::now();
            let due = self
                .timers
                .first()
                .map(|t| t.0)
                .into_iter()
                .chain(self.next_tick)
                .min();
            let timeout = due.map(|d| d.saturating_duration_since(now));
            if let Err(e) = self.poll.poll(&mut events, timeout)
                && e.kind() != std::io::ErrorKind::Interrupted
            {
                log::error!("lagless: poll failed: {e}");
                break;
            }
            let readable = events.iter().any(|e| e.token() == SOCK);
            if !self.commands() {
                break;
            }
            if readable {
                self.read_socket();
            }
            let now = Instant::now();
            while let Some(&(t, id)) = self.timers.first() {
                if t > now {
                    break;
                }
                self.timers.pop_first();
                if let Some(p) = self.peers.get_mut(&id) {
                    p.deadline = None;
                    if let Err(e) = p.pc.handle_timeout(now) {
                        log::debug!("peer {id}: handle_timeout: {e}");
                    }
                    self.dirty.insert(id);
                }
            }
            if self.next_tick.is_some_and(|t| t <= now) {
                self.tick(now);
            }
            self.process_dirty();
            self.flush_events();
        }
        // shutdown: say goodbye to everyone, deliver the last events, release the tsfn
        let ids: Vec<u32> = self.peers.keys().copied().collect();
        for id in ids {
            self.remove(id, "local", true);
        }
        self.flush_events();
    }

    /// Drain the command queue. false = shut down.
    fn commands(&mut self) -> bool {
        self.pending.store(false, Ordering::Release);
        loop {
            match self.rx.try_recv() {
                Ok(Cmd::Start(t)) => {
                    if self.tsfn.is_none() {
                        self.tsfn = Some(t);
                    }
                }
                Ok(Cmd::Add(np)) => self.add(*np),
                Ok(Cmd::Send {
                    peer,
                    label,
                    data,
                    is_string,
                }) => self.send_on(peer, &label, data, is_string),
                Ok(Cmd::Close(id)) => self.remove(id, "local", true),
                Ok(Cmd::Kick(id, reason)) => self.remove(id, reason, true),
                Ok(Cmd::CloseAll) => {
                    let ids: Vec<u32> = self.peers.keys().copied().collect();
                    for id in ids {
                        self.remove(id, "local", true);
                    }
                }
                Ok(Cmd::Stun { to, txid, reply }) => self.stun_send(to, txid, reply),
                Ok(Cmd::Shutdown) | Err(TryRecvError::Disconnected) => return false,
                Err(TryRecvError::Empty) => return true,
            }
        }
    }

    fn add(&mut self, np: NewPeer) {
        let now = Instant::now();
        let pacer = pacer::Pacer::new(now, self.pacer_cfg);
        if self.pacer_on {
            np.shared.rate.store(pacer.rate as u64, Ordering::Relaxed);
        }
        self.by_ufrag.insert(np.ufrag.clone(), np.id);
        self.peers.insert(
            np.id,
            PeerConn {
                pc: np.pc,
                shared: np.shared,
                ufrag: np.ufrag,
                local_addrs: np.local_addrs,
                remotes: Vec::new(),
                labels: HashMap::new(),
                ids: HashMap::new(),
                pacer,
                overload: gate::Overload::new(self.overload_ms),
                cwnd_limited: false,
                deadline: None,
                opened: false,
                sctp_up: false,
            },
        );
        if self.next_tick.is_none() {
            self.next_tick = Some(now + self.tick_every());
        }
        self.dirty.insert(np.id);
    }

    fn stun_send(&mut self, to: SocketAddr, txid: [u8; 12], reply: Sender<SocketAddr>) {
        let now = Instant::now();
        // the caller retransmits with the same txid and gives up after its timeout
        self.stun
            .retain(|_, (_, t, _)| now.saturating_duration_since(*t) < Duration::from_secs(30));
        self.stun.entry(txid).or_insert((to, now, reply));
        let dst = match to {
            SocketAddr::V4(a) if self.v6 => {
                SocketAddr::new(a.ip().to_ipv6_mapped().into(), a.port())
            }
            a => a,
        };
        if let Err(e) = self.sock.send_to(&crate::stun::binding_request(&txid), dst) {
            log::debug!("lagless: STUN send_to {dst}: {e}");
        }
    }

    fn tick_every(&self) -> Duration {
        if self.pacer_on {
            TICK_PACED
        } else {
            pacer::INTERVAL
        }
    }

    fn send_on(&mut self, id: u32, label: &str, data: Vec<u8>, is_string: bool) {
        let Some(p) = self.peers.get_mut(&id) else {
            return;
        };
        let len = data.len();
        let res = match p
            .ids
            .get(label)
            .copied()
            .and_then(|cid| p.pc.data_channel(cid))
        {
            // validated as UTF-8 in Server::send
            Some(mut dc) if is_string => dc
                .send_text(Instant::now(), String::from_utf8(data).unwrap_or_default())
                .map_err(|e| e.to_string()),
            Some(mut dc) => dc
                .send(Instant::now(), BytesMut::from(&data[..]))
                .map_err(|e| e.to_string()),
            None => Err("channel not open".to_string()),
        };
        p.shared.pending_cmd.fetch_sub(len, Ordering::AcqRel);
        if let Some(msg) = gate::record_send(&p.shared, label, res) {
            self.out.push(Event::error(id, msg));
        }
        self.dirty.insert(id);
    }

    /// Read until WouldBlock, demux, feed rtc.
    fn read_socket(&mut self) {
        loop {
            let (n, from) = match self.sock.recv_from(&mut self.buf) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    // ICMP errors (ECONNREFUSED etc) surface here on some OSes; not fatal
                    log::debug!("lagless: recv_from: {e}");
                    continue;
                }
            };
            let from = canonical(from);
            let pkt = &self.buf[..n];
            if let Some((txid, mapped)) = crate::stun::binding_response(pkt)
                && self.stun.get(&txid).is_some_and(|(to, _, _)| *to == from)
            {
                if let Some((_, _, reply)) = self.stun.remove(&txid) {
                    let _ = reply.send(canonical(mapped));
                }
                continue;
            }
            let id = match crate::stun::binding_ufrag(pkt)
                .and_then(|u| self.by_ufrag.get(u))
                .copied()
            {
                Some(id) => {
                    if self.by_addr.insert(from, id) != Some(id)
                        && let Some(p) = self.peers.get_mut(&id)
                    {
                        p.remotes.push(from);
                    }
                    id
                }
                None => match self.by_addr.get(&from) {
                    Some(id) => *id,
                    None => continue,
                },
            };
            let Some(p) = self.peers.get_mut(&id) else {
                continue;
            };
            let local_addr = p.local_for(from);
            let r = p.pc.handle_read(TaggedBytesMut {
                now: Instant::now(),
                transport: TransportContext {
                    local_addr,
                    peer_addr: from,
                    ecn: None,
                    transport_protocol: TransportProtocol::UDP,
                },
                message: BytesMut::from(pkt),
            });
            if let Err(e) = r {
                log::debug!("peer {id}: handle_read: {e}");
            }
            self.dirty.insert(id);
        }
    }

    /// Pacer refill for every peer; SCTP stats + pacer update every pacer::INTERVAL.
    fn tick(&mut self, now: Instant) {
        let stats_due = now.saturating_duration_since(self.last_stats) >= pacer::INTERVAL;
        if stats_due {
            self.last_stats = now;
        }
        for (&id, p) in self.peers.iter_mut() {
            let sh = &p.shared;
            if self.pacer_on {
                gate::refill(sh, &mut p.pacer, now);
                if let Some(on) = p.overload.update(sh, now) {
                    self.out.push(Event::overload(id, on));
                }
            }
            if !stats_due {
                continue;
            }
            if let Some(ts) = p.pc.sctp_transport_stats() {
                sh.srtt_ms.store(ts.srtt_ms, Ordering::Relaxed);
                sh.min_rtt_ms
                    .store(ts.min_rtt_ms.unwrap_or(0), Ordering::Relaxed);
                sh.cwnd.store(ts.cwnd as u64, Ordering::Relaxed);
                sh.inflight
                    .store(ts.inflight_bytes as u64, Ordering::Relaxed);
                sh.pending.store(ts.pending_bytes as u64, Ordering::Relaxed);
                sh.rack_lost.store(ts.rack_lost, Ordering::Relaxed);
                sh.rack_probes.store(ts.rack_probes, Ordering::Relaxed);
                sh.t3_timeouts.store(ts.t3_timeouts, Ordering::Relaxed);
                if self.pacer_on {
                    p.cwnd_limited |= ts.pending_bytes > 0
                        || ts.inflight_bytes + ts.mtu as usize > ts.cwnd as usize;
                    let sig = pacer::Signals {
                        acked_total: ts.acked_bytes_total,
                        outstanding: sh.stack_outstanding.load(Ordering::Relaxed)
                            + sh.pending_cmd.load(Ordering::Relaxed),
                        srtt: Duration::from_millis(ts.srtt_ms),
                        min_rtt: ts.min_rtt_ms.map(Duration::from_millis),
                        mtu: ts.mtu,
                        budget_limited: sh.budget_limited.load(Ordering::Relaxed),
                        cwnd_limited: p.cwnd_limited,
                    };
                    if p.pacer.update(now, &sig) {
                        sh.budget_limited.store(false, Ordering::Relaxed);
                        p.cwnd_limited = false;
                    }
                    sh.rate.store(p.pacer.rate as u64, Ordering::Relaxed);
                    sh.draining.store(p.pacer.draining, Ordering::Relaxed);
                    sh.app_limited.store(p.pacer.app_limited, Ordering::Relaxed);
                }
            }
        }
        self.next_tick = if self.peers.is_empty() {
            None
        } else {
            Some(now + self.tick_every())
        };
    }

    fn process_dirty(&mut self) {
        let dirty: Vec<u32> = self.dirty.drain().collect();
        for id in dirty {
            if let Some(reason) = self.process(id) {
                self.remove(id, reason, false);
            }
        }
    }

    /// Pump one peer's rtc queues. Some(reason) = the peer is dead.
    fn process(&mut self, id: u32) -> Option<&'static str> {
        let p = self.peers.get_mut(&id)?;
        let mut dead = None;
        while let Some(ev) = p.pc.poll_event() {
            match ev {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(st) => match st {
                    RTCPeerConnectionState::Connected => {
                        p.shared.connected.store(true, Ordering::Relaxed);
                        if !p.opened {
                            p.opened = true;
                            self.out.push(Event::simple("open", id));
                        }
                    }
                    RTCPeerConnectionState::Failed => dead = Some("failed"),
                    RTCPeerConnectionState::Closed => dead = Some("closed"),
                    _ => {}
                },
                RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(cid)) => {
                    if let Some(label) = p.learn(cid, &self.priorities) {
                        self.out.push(Event::channel("channelopen", id, label));
                    }
                }
                RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnClose(cid)) => {
                    if let Some(label) = p.labels.remove(&cid) {
                        p.ids.remove(&label);
                        p.shared.channels.lock().unwrap().remove(&label);
                    }
                }
                _ => {}
            }
        }
        // rtc keeps the pc "connected" after the remote's SCTP ABORT/SHUTDOWN or DTLS
        // close_notify (ICE is still up), so watch the transports directly to notice a
        // remote close without waiting ~30 s for ICE to fail
        if dead.is_none()
            && let Some(sctp) = p.pc.sctp()
        {
            let up = sctp.state() == RTCSctpTransportState::Connected;
            let dtls_gone = matches!(
                sctp.transport().state(),
                RTCDtlsTransportState::Closed | RTCDtlsTransportState::Failed
            );
            if (p.sctp_up && !up) || (p.opened && dtls_gone) {
                dead = Some("closed");
            }
            p.sctp_up |= up;
        }
        while let Some(TaggedRTCMessage { message, .. }) = p.pc.poll_read() {
            if let RTCMessage::DataChannelMessage(cid, m) = message
                && let Some(label) = p.learn(cid, &self.priorities)
            {
                self.out
                    .push(Event::message(id, label, m.data.to_vec(), m.is_string));
            }
        }
        let mut out = 0usize;
        for cid in p.ids.values() {
            if let Some(dc) = p.pc.data_channel(*cid) {
                out += dc.outstanding_bytes();
            }
        }
        p.shared.stack_outstanding.store(out, Ordering::Release);
        flush_writes(&self.sock, self.v6, id, &mut p.pc);

        let d = p.pc.poll_timeout();
        if d != p.deadline {
            if let Some(old) = p.deadline {
                self.timers.remove(&(old, id));
            }
            if let Some(new) = d {
                self.timers.insert((new, id));
            }
            p.deadline = d;
        }
        dead
    }

    /// Drop a peer: graceful = send SCTP ABORT + DTLS close_notify first.
    fn remove(&mut self, id: u32, reason: &str, graceful: bool) {
        let Some(mut p) = self.peers.remove(&id) else {
            return;
        };
        if graceful {
            if let Err(e) = p.pc.close() {
                log::debug!("peer {id}: close: {e}");
            }
            flush_writes(&self.sock, self.v6, id, &mut p.pc);
        }
        if let Some(d) = p.deadline {
            self.timers.remove(&(d, id));
        }
        for a in &p.remotes {
            if self.by_addr.get(a) == Some(&id) {
                self.by_addr.remove(a);
            }
        }
        if self.by_ufrag.get(&p.ufrag) == Some(&id) {
            self.by_ufrag.remove(&p.ufrag);
        }
        self.dirty.remove(&id);
        self.registry.lock().unwrap().remove(&id);
        p.shared.connected.store(false, Ordering::Relaxed);
        p.shared.closed.store(true, Ordering::Relaxed);
        self.out.push(Event::close(id, reason));
    }

    fn flush_events(&mut self) {
        if self.out.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.out);
        if let Some(t) = &self.tsfn {
            t.call(batch, ThreadsafeFunctionCallMode::NonBlocking);
        }
    }
}

fn flush_writes(sock: &UdpSocket, v6: bool, id: u32, pc: &mut RTCPeerConnection) {
    while let Some(msg) = pc.poll_write() {
        let mut to = msg.transport.peer_addr;
        if v6 && let SocketAddr::V4(a) = to {
            to = SocketAddr::new(a.ip().to_ipv6_mapped().into(), a.port());
        }
        if let Err(e) = sock.send_to(&msg.message, to) {
            // WouldBlock = socket buffer full: drop like the network would, SCTP recovers
            log::debug!("peer {id}: send_to {to} failed: {e}");
        }
    }
}
