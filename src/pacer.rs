//! Per-peer send pacer: delivery-rate + delay based budget (token bucket).
//!
//! Every `INTERVAL` the controller samples the delivery rate (bytes the peer
//! acked) and keeps a 1 s windowed max. If the sender-side queue or the network
//! queue (srtt - min_rtt) grows past a small target, the rate drops below the
//! delivery rate so the queue drains; otherwise it sits a bit above it to probe.
//! Tokens refill at that rate; reliable sends always go (and may push tokens
//! negative), unreliable sends need tokens. So reliable data gets its share
//! first and unreliable fills what the link can actually carry this tick.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// how often the worker feeds signals
pub const INTERVAL: Duration = Duration::from_millis(50);
/// delivery samples span at least one RTT (SACKs arrive in bursts) and this floor
const MIN_SAMPLE: Duration = Duration::from_millis(100);
const WINDOW: usize = 5; // ~5 RTTs max filter for probing
const START_RATE: f64 = 64.0 * 1024.0;
const FLOOR_RATE: f64 = 8.0 * 1024.0;
const DRAIN_GAIN: f64 = 0.75;
const PROBE_GAIN: f64 = 1.15;
/// extra sender queue allowed on top of one min-RTT worth of delivery
const QUEUE_MARGIN: Duration = Duration::from_millis(40);
/// network queue (srtt - min_rtt) that triggers draining
const NET_QUEUE_LIMIT: Duration = Duration::from_millis(60);
/// token bucket depth, in seconds of rate
const BURST: f64 = 0.1;

#[derive(Debug, Clone, Copy, Default)]
pub struct Signals {
    pub acked_total: u64,
    /// bytes accepted by send() and not yet released by SCTP
    pub outstanding: usize,
    pub srtt: Duration,
    pub min_rtt: Option<Duration>,
    pub mtu: u32,
}

#[derive(Debug)]
pub struct Pacer {
    pub rate: f64,
    pub draining: bool,
    samples: VecDeque<f64>,
    last_acked: Option<u64>,
    win_start: Instant,
    last_refill: Instant,
    own_min_rtt: Option<Duration>,
}

impl Pacer {
    pub fn new(now: Instant) -> Self {
        Pacer {
            rate: START_RATE,
            draining: false,
            samples: VecDeque::new(),
            last_acked: None,
            win_start: now,
            last_refill: now,
            own_min_rtt: None,
        }
    }

    pub fn max_delivery(&self) -> f64 {
        self.samples.iter().copied().fold(0.0, f64::max)
    }

    pub fn burst_cap(&self, mtu: u32) -> f64 {
        (self.rate * BURST).max(4.0 * mtu.max(1200) as f64)
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
        if dt < MIN_SAMPLE.max(s.srtt) {
            return false;
        }
        self.win_start = now;
        let acked = match self.last_acked.replace(s.acked_total) {
            Some(prev) => s.acked_total.saturating_sub(prev),
            None => return true,
        };
        self.samples.push_back(acked as f64 / dt.as_secs_f64());
        while self.samples.len() > WINDOW {
            self.samples.pop_front();
        }
        if s.srtt > Duration::ZERO {
            self.own_min_rtt = Some(self.own_min_rtt.map_or(s.srtt, |m| m.min(s.srtt)));
        }
        let min_rtt = s.min_rtt.or(self.own_min_rtt).unwrap_or(s.srtt);
        let max_d = self.max_delivery();
        let last_d = self.samples.back().copied().unwrap_or(0.0);
        if max_d <= 0.0 {
            // nothing delivered yet (idle or start): keep the start rate
            return true;
        }
        let target = max_d * (min_rtt + QUEUE_MARGIN).as_secs_f64() + 2.0 * s.mtu.max(1200) as f64;
        let net_queue = s.srtt.saturating_sub(min_rtt);
        self.draining = (s.outstanding as f64) > target || net_queue > NET_QUEUE_LIMIT;
        // drain below what the link just delivered; probe above the recent best
        self.rate = if self.draining { last_d * DRAIN_GAIN } else { max_d * PROBE_GAIN }.max(FLOOR_RATE);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fluid link: fixed capacity, the queue builds at the sender (bottleneck = our send path).
    fn sim(capacity: f64, base_rtt_ms: u64, secs: u64) -> (f64, f64) {
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        let mut tokens = 0.0f64;
        let mut queue = 0.0f64;
        let mut acked = 0.0f64;
        let step = Duration::from_millis(1);
        let mut now = t0;
        let mut delays = vec![];
        let mut rates = vec![];
        for i in 0..secs * 1000 {
            now += step;
            tokens = (tokens + p.refill(now)).min(p.burst_cap(1200));
            // greedy app: every 40 ms tick spend all tokens
            if i % 40 == 0 && tokens > 0.0 {
                queue += tokens;
                tokens = 0.0;
            }
            let drained = (capacity * 0.001).min(queue);
            queue -= drained;
            acked += drained;
            let qdelay = Duration::from_secs_f64(queue / capacity);
            let s = Signals {
                acked_total: acked as u64,
                outstanding: queue as usize + (p.rate.min(capacity) * base_rtt_ms as f64 / 1000.0) as usize,
                // SCTP srtt does not see the sender-side queue, only the path
                srtt: Duration::from_millis(base_rtt_ms),
                min_rtt: Some(Duration::from_millis(base_rtt_ms)),
                mtu: 1200,
            };
            p.update(now, &s);
            if i > 3000 {
                delays.push(qdelay.as_secs_f64());
                rates.push(drained * 1000.0);
            }
        }
        let avg_delay = delays.iter().sum::<f64>() / delays.len() as f64;
        let goodput = rates.iter().sum::<f64>() / rates.len() as f64;
        (avg_delay, goodput)
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
}
