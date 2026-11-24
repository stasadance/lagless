//! Per-peer send pacer: delivery-rate + delay based budget (token bucket).
//!
//! Every `INTERVAL` the controller samples the delivery rate (bytes the peer
//! acked) and keeps a 1 s windowed max. If the sender-side queue or the network
//! queue (srtt - min_rtt) grows past a small target, the rate drops below the
//! delivery rate so the queue drains; otherwise it sits a bit above it to probe.
//! Tokens refill at that rate; reliable sends always go (and may push tokens
//! negative), unreliable sends need tokens. So reliable data gets its share
//! first and unreliable fills what the link can actually carry this tick.
//!
//! App-limited samples (BBR-style): when the app sent less than the budget
//! allowed and no queue built, the delivery sample measures the app, not the
//! link (BBR also requires nothing waiting on cwnd). Such a sample only enters the max filter if it raises it, so a quiet
//! period does not ratchet the rate down.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// how often the worker feeds signals
pub const INTERVAL: Duration = Duration::from_millis(50);
/// delivery samples span at least one RTT (SACKs arrive in bursts) and this floor
const MIN_SAMPLE: Duration = Duration::from_millis(100);
/// ...but no longer than this, or a fast-growing queue (srtt rising faster than
/// wall time) would keep the controller from ever running
const MAX_SAMPLE: Duration = Duration::from_millis(500);
const WINDOW: usize = 5; // ~5 RTTs max filter for probing
pub const DEFAULT_START_RATE: f64 = 64.0 * 1024.0;
pub const DEFAULT_FLOOR_RATE: f64 = 8.0 * 1024.0;
/// token bucket depth, in seconds of rate
pub const DEFAULT_BURST: f64 = 0.1;
const DRAIN_GAIN: f64 = 0.75;
const PROBE_GAIN: f64 = 1.15;
/// extra sender queue allowed on top of one min-RTT worth of delivery
const QUEUE_MARGIN: Duration = Duration::from_millis(40);
/// network queue (srtt - min_rtt) that triggers draining
const NET_QUEUE_LIMIT: Duration = Duration::from_millis(60);
/// drain samples after which an RTT that did not come down counts as a path change
const DRAIN_PROBE: u32 = 2;
/// how much srtt must fall during those samples for the queue to count as ours
const DRAIN_EFFECT: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// bytes/s before the first delivery sample
    pub start_rate: f64,
    /// rate never goes below this, bytes/s
    pub floor_rate: f64,
    /// token bucket depth, seconds of rate
    pub burst: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config { start_rate: DEFAULT_START_RATE, floor_rate: DEFAULT_FLOOR_RATE, burst: DEFAULT_BURST }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Signals {
    pub acked_total: u64,
    /// bytes accepted by send() and not yet released by SCTP
    pub outstanding: usize,
    pub srtt: Duration,
    pub min_rtt: Option<Duration>,
    pub mtu: u32,
    /// the budget ran out (or a send was dropped for lack of it) since the
    /// last update: the app wanted more than the pacer allowed
    pub budget_limited: bool,
    /// SCTP had data waiting on cwnd (pending > 0 or inflight near cwnd) at
    /// some point since the last update: loss recovery, not the app, set the rate
    pub cwnd_limited: bool,
}

#[derive(Debug)]
pub struct Pacer {
    pub rate: f64,
    pub draining: bool,
    /// the last delivery sample was app-limited
    pub app_limited: bool,
    cfg: Config,
    samples: VecDeque<f64>,
    last_acked: Option<u64>,
    win_start: Instant,
    last_refill: Instant,
    own_min_rtt: Option<Duration>,
    /// raised base RTT after a path change (see `update`); follows srtt down
    rtt_floor: Option<Duration>,
    /// srtt when the current drain began, and drain samples since
    drain_from: Option<(Duration, u32)>,
}

impl Pacer {
    pub fn new(now: Instant, cfg: Config) -> Self {
        Pacer {
            rate: cfg.start_rate,
            draining: false,
            app_limited: false,
            cfg,
            samples: VecDeque::new(),
            last_acked: None,
            win_start: now,
            last_refill: now,
            own_min_rtt: None,
            rtt_floor: None,
            drain_from: None,
        }
    }

    pub fn max_delivery(&self) -> f64 {
        self.samples.iter().copied().fold(0.0, f64::max)
    }

    pub fn burst_cap(&self, mtu: u32) -> f64 {
        (self.rate * self.cfg.burst).max(4.0 * mtu.max(1200) as f64)
    }

    /// Tokens to add since the last call.
    pub fn refill(&mut self, now: Instant) -> f64 {
        let dt = now.saturating_duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.rate * dt
    }

    /// Run the controller if an interval elapsed. Returns true when it ran.
    pub fn update(&mut self, now: Instant, s: &Signals) -> bool {
        let dt = now.saturating_duration_since(self.win_start);
        if dt < MIN_SAMPLE.max(s.srtt.min(MAX_SAMPLE)) {
            return false;
        }
        self.win_start = now;
        let acked = match self.last_acked.replace(s.acked_total) {
            Some(prev) => s.acked_total.saturating_sub(prev),
            None => return true,
        };
        let sample = acked as f64 / dt.as_secs_f64();
        if s.srtt > Duration::ZERO {
            self.own_min_rtt = Some(self.own_min_rtt.map_or(s.srtt, |m| m.min(s.srtt)));
        }
        if let Some(f) = self.rtt_floor {
            if s.srtt > Duration::ZERO && s.srtt < f {
                self.rtt_floor = Some(s.srtt);
            }
        }
        let base_rtt = s.min_rtt.or(self.own_min_rtt).unwrap_or(s.srtt);
        let min_rtt = self.rtt_floor.map_or(base_rtt, |f| f.max(base_rtt));
        let mtu = s.mtu.max(1200) as f64;
        let target_for = |rate: f64| rate * (min_rtt + QUEUE_MARGIN).as_secs_f64() + 2.0 * mtu;
        let net_queue = s.srtt.saturating_sub(min_rtt);
        // the link, not the app, set this sample if the budget bound or a queue built
        let queued = (s.outstanding as f64) > target_for(self.max_delivery().max(sample));
        self.app_limited = !s.budget_limited && !s.cwnd_limited && !queued && net_queue <= NET_QUEUE_LIMIT;
        // app-limited samples may only raise the max filter (BBR)
        if !self.app_limited || sample >= self.max_delivery() {
            self.samples.push_back(sample);
            while self.samples.len() > WINDOW {
                self.samples.pop_front();
            }
        }
        let max_d = self.max_delivery();
        if max_d <= 0.0 {
            // nothing delivered yet (idle or start): keep the start rate
            return true;
        }
        self.draining = (s.outstanding as f64) > target_for(max_d) || net_queue > NET_QUEUE_LIMIT;
        // A queue we built drains when we send below the delivery rate, so srtt falls.
        // If srtt did not fall after a few drain samples, the path RTT went up (mobile,
        // route change): take the current srtt as the new base instead of starving.
        if self.draining {
            let (from, n) = self.drain_from.get_or_insert((s.srtt, 0));
            *n += 1;
            if *n > DRAIN_PROBE {
                if s.srtt + DRAIN_EFFECT >= *from {
                    self.rtt_floor = Some(s.srtt);
                    self.drain_from = None;
                    // re-check against the new base; a sender-side queue still drains
                    let min_rtt = s.srtt.max(base_rtt);
                    let target = max_d * (min_rtt + QUEUE_MARGIN).as_secs_f64() + 2.0 * mtu;
                    self.draining = (s.outstanding as f64) > target;
                } else {
                    // it is ours and still draining: measure the next stretch
                    self.drain_from = Some((s.srtt, 1));
                }
            }
        } else {
            self.drain_from = None;
        }
        // drain below what the link just delivered; probe above the recent best
        self.rate = if self.draining { sample * DRAIN_GAIN } else { max_d * PROBE_GAIN }.max(self.cfg.floor_rate);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fluid link: fixed capacity, the queue builds at the sender (bottleneck = our send path).
    /// `demand(ms)` is how many bytes the app wants to send per 40 ms tick (None = all tokens).
    /// Returns (avg queue delay, avg goodput) after the first 3 s, and the pacer.
    fn sim_with(capacity: f64, base_rtt_ms: u64, secs: u64, demand: impl Fn(u64) -> Option<f64>) -> (f64, f64, Pacer, Vec<f64>) {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0, Config::default());
        let mut tokens = 0.0f64;
        let mut queue = 0.0f64;
        let mut acked = 0.0f64;
        let mut limited = false;
        let step = Duration::from_millis(1);
        let mut now = t0;
        let mut delays = vec![];
        let mut rates = vec![];
        let mut trace = vec![];
        for i in 0..secs * 1000 {
            now += step;
            tokens = (tokens + p.refill(now)).min(p.burst_cap(1200));
            if i % 40 == 0 && tokens > 0.0 {
                let want = demand(i).unwrap_or(f64::INFINITY);
                let spend = want.min(tokens);
                if want > tokens {
                    limited = true;
                }
                queue += spend;
                tokens -= spend;
            }
            let drained = (capacity * 0.001).min(queue);
            queue -= drained;
            acked += drained;
            trace.push(drained);
            let qdelay = Duration::from_secs_f64(queue / capacity);
            let s = Signals {
                acked_total: acked as u64,
                outstanding: queue as usize + (p.rate.min(capacity) * base_rtt_ms as f64 / 1000.0) as usize,
                // SCTP srtt does not see the sender-side queue, only the path
                srtt: Duration::from_millis(base_rtt_ms),
                min_rtt: Some(Duration::from_millis(base_rtt_ms)),
                mtu: 1200,
                budget_limited: limited,
                cwnd_limited: false,
            };
            if p.update(now, &s) {
                limited = false;
            }
            if i > 3000 {
                delays.push(qdelay.as_secs_f64());
                rates.push(drained * 1000.0);
            }
        }
        let avg_delay = delays.iter().sum::<f64>() / delays.len() as f64;
        let goodput = rates.iter().sum::<f64>() / rates.len() as f64;
        (avg_delay, goodput, p, trace)
    }

    fn sim(capacity: f64, base_rtt_ms: u64, secs: u64) -> (f64, f64) {
        let (d, g, _, _) = sim_with(capacity, base_rtt_ms, secs, |_| None);
        (d, g)
    }

    #[test]
    fn converges_to_capacity_with_small_queue() {
        for cap in [20.0 * 1024.0, 60.0 * 1024.0, 300.0 * 1024.0] {
            let (delay, goodput) = sim(cap, 200, 10);
            println!("cap={cap} delay={delay:.3}s goodput={goodput:.0}");
            // standing queue: under 100 ms, or a handful of packets on slow links
            let queued = delay * cap;
            assert!(delay < 0.1 || queued < 5000.0, "queue delay {delay}s ({queued:.0} B)");
            assert!(goodput > 0.8 * cap, "goodput {goodput} of {cap}");
        }
    }

    #[test]
    fn app_limited_period_keeps_rate() {
        // greedy 4 s, then 6 s at 5% of capacity: the rate must stay near capacity
        let cap = 300.0 * 1024.0;
        let (_, _, p, _) = sim_with(cap, 100, 10, |ms| if ms < 4000 { None } else { Some(cap * 0.05 * 0.04) });
        println!("rate after quiet period {:.0} (cap {cap})", p.rate);
        assert!(p.app_limited);
        assert!(p.rate > 0.8 * cap, "rate ratcheted down to {}", p.rate);
    }

    #[test]
    fn recovers_fast_after_quiet_period() {
        // greedy 4 s, 5 s at 5% of capacity, greedy again: the first 500 ms back are near capacity
        let cap = 300.0 * 1024.0;
        let (_, _, _, trace) = sim_with(cap, 100, 12, |ms| if (4000..9000).contains(&ms) { Some(cap * 0.05 * 0.04) } else { None });
        let back = trace[9000..9500].iter().sum::<f64>() / 0.5;
        println!("goodput 0-500 ms after resume {back:.0} (cap {cap})");
        assert!(back > 0.8 * cap, "slow recovery: {back}");
    }

    /// Fluid link with the queue in the network (bufferbloat): srtt sees it. `extra_ms(t)`
    /// adds path delay that has nothing to do with our sending (mobile RTT swing).
    /// Greedy app. Returns (avg self-induced queue delay, goodput, fraction of samples draining).
    fn sim_net(capacity: f64, base_rtt_ms: f64, secs: u64, extra_ms: impl Fn(f64) -> f64) -> (f64, f64, f64) {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0, Config::default());
        let (mut tokens, mut queue, mut acked) = (0.0f64, 0.0f64, 0.0f64);
        let mut now = t0;
        let (mut delays, mut rates) = (vec![], vec![]);
        let (mut drains, mut updates) = (0u32, 0u32);
        let mut limited = false;
        for i in 0..secs * 1000 {
            now += Duration::from_millis(1);
            tokens = (tokens + p.refill(now)).min(p.burst_cap(1200));
            if i % 40 == 0 && tokens > 0.0 {
                queue += tokens;
                tokens = 0.0;
                limited = true;
            }
            let drained = (capacity * 0.001).min(queue);
            queue -= drained;
            acked += drained;
            let path = base_rtt_ms + extra_ms(i as f64 / 1000.0);
            let qms = queue / capacity * 1000.0;
            let s = Signals {
                acked_total: acked as u64,
                outstanding: queue as usize + (p.rate.min(capacity) * path / 1000.0) as usize,
                srtt: Duration::from_secs_f64((path + qms) / 1000.0),
                min_rtt: Some(Duration::from_millis(base_rtt_ms as u64)),
                mtu: 1200,
                budget_limited: limited,
                cwnd_limited: false,
            };
            if p.update(now, &s) {
                limited = false;
                if i > 3000 {
                    updates += 1;
                    drains += p.draining as u32;
                }
            }
            if i > 3000 {
                delays.push(qms / 1000.0);
                rates.push(drained * 1000.0);
            }
        }
        let n = delays.len() as f64;
        (delays.iter().sum::<f64>() / n, rates.iter().sum::<f64>() / n, drains as f64 / updates.max(1) as f64)
    }

    #[test]
    fn bufferbloat_queue_drains() {
        let cap = 32.0 * 1024.0;
        let (q, g, d) = sim_net(cap, 200.0, 30, |_| 0.0);
        println!("bloat: queue {q:.3}s goodput {g:.0} drain {d:.2}");
        assert!(q < 0.15, "network queue {q}s");
        assert!(g > 0.8 * cap, "goodput {g}");
    }

    #[test]
    fn rtt_swing_is_not_read_as_queue() {
        // one-way +200 ms over 20 s periods (RTT 200 -> 600 ms), plenty of capacity
        let cap = 64.0 * 1024.0;
        let swing = |t: f64| 400.0 * (0.5 - 0.5 * (2.0 * std::f64::consts::PI * t / 20.0).cos());
        let (q, g, d) = sim_net(cap, 200.0, 60, swing);
        println!("swing: queue {q:.3}s goodput {g:.0} drain {d:.2}");
        assert!(q < 0.15, "network queue {q}s");
        // without the path-change check this starves at the floor (~15 KB/s)
        assert!(g > 0.8 * cap, "goodput {g}");
        assert!(d < 0.6, "draining {d:.2} of samples");
    }
}
