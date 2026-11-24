//! Send admission: the unreliable gate, the pacer token bucket, the reliable byte cap,
//! and the counters that tell JS what happened to its sends.
//!
//! Everything here is lock-free on the shared per-peer atomics. Each check-and-reserve
//! is one `fetch_update` (a CAS loop), so a reservation can't be lost to a concurrent
//! refill or another send. The gate compares against `stack_outstanding`, which is a
//! snapshot the worker refreshes every loop: a send never sees bytes SCTP released
//! after that snapshot, so the gate errs on the side of dropping.

use crate::PeerShared;
use crate::pacer::Pacer;
use std::fmt::Display;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed};
use std::time::{Duration, Instant};

/// Admission limits for one server (copied into each send call).
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    /// unreliable gate, bytes not yet released by SCTP; 0 = off
    pub unreliable: usize,
    /// reliable cap, bytes queued for the peer on all channels; 0 = unbounded
    pub reliable: usize,
    pub pacer: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// bytes reserved in `pending_cmd`; hand the message to the worker
    Queued,
    /// unreliable send refused by the gate or the pacer
    Dropped,
    /// reliable send would push the peer's queue past `Limits::reliable`
    Overflow,
}

/// Reserve `fits(pending)` bytes in `pending_cmd`, atomically.
fn reserve(s: &PeerShared, len: usize, fits: impl Fn(usize) -> bool) -> bool {
    s.pending_cmd
        .fetch_update(AcqRel, Acquire, |p| fits(p).then_some(p + len))
        .is_ok()
}

/// Decide whether a send goes. On `Queued` the caller must hand the message to the
/// worker, which releases `len` from `pending_cmd` once SCTP has it.
pub(crate) fn admit(s: &PeerShared, lim: Limits, unreliable: bool, exempt: bool, len: usize) -> Verdict {
    let out = s.stack_outstanding.load(Acquire);
    if unreliable && !exempt {
        if !reserve(s, len, |p| lim.unreliable == 0 || out + p + len <= lim.unreliable) {
            s.dropped.fetch_add(1, Relaxed);
            return Verdict::Dropped;
        }
        if lim.pacer && s.tokens.fetch_update(AcqRel, Acquire, |t| (t >= len as i64).then(|| t - len as i64)).is_err() {
            s.pending_cmd.fetch_sub(len, AcqRel);
            s.paced_drops.fetch_add(1, Relaxed);
            s.dropped.fetch_add(1, Relaxed);
            return Verdict::Dropped;
        }
        s.sent_unreliable.fetch_add(1, Relaxed);
        return Verdict::Queued;
    }
    if !unreliable {
        if !reserve(s, len, |p| lim.reliable == 0 || out + p + len <= lim.reliable) {
            s.reliable_overflows.fetch_add(1, Relaxed);
            return Verdict::Overflow;
        }
    } else {
        // exempt unreliable (latency probes)
        s.pending_cmd.fetch_add(len, AcqRel);
        s.sent_unreliable.fetch_add(1, Relaxed);
    }
    if lim.pacer {
        // reliable and exempt sends always go, and may push the budget negative
        s.tokens.fetch_sub(len as i64, AcqRel);
    }
    Verdict::Queued
}

/// Add the tokens earned since the last call, capped at the burst size, without
/// overwriting concurrent subtractions by `admit`.
pub(crate) fn refill(s: &PeerShared, pacer: &mut Pacer, now: Instant) {
    let add = pacer.refill(now) as i64;
    let cap = pacer.burst_cap(1200) as i64;
    // one CAS: a lowered cap (rate dropped) also trims an over-full bucket
    let _ = s.tokens.fetch_update(AcqRel, Acquire, |t| Some(t.saturating_add(add).min(cap)));
}

/// Record the outcome of handing a message to rtc. Returns the error text on failure
/// so the caller can surface it as an event.
pub(crate) fn record_send<E: Display>(s: &PeerShared, label: &str, r: Result<(), E>) -> Option<String> {
    match r {
        Ok(()) => None,
        Err(e) => {
            s.send_errors.fetch_add(1, Relaxed);
            let msg = format!("send on '{label}' failed: {e}");
            *s.last_send_error.lock().unwrap() = Some(msg.clone());
            log::debug!("{msg}");
            Some(msg)
        }
    }
}

/// Reliable-overload detector: the pacer budget stayed below zero for `hold`.
/// That means reliable load alone is at or above what the link carries and
/// unreliable sends get no budget at all.
pub(crate) struct Overload {
    hold: Duration,
    negative_since: Option<Instant>,
}

impl Overload {
    pub(crate) fn new(hold_ms: u32) -> Self {
        Overload { hold: Duration::from_millis(hold_ms as u64), negative_since: None }
    }

    /// Call after each refill. Returns `Some(true)` when overload starts and
    /// `Some(false)` when it ends.
    pub(crate) fn update(&mut self, s: &PeerShared, now: Instant) -> Option<bool> {
        if self.hold.is_zero() {
            return None;
        }
        let was = s.overloaded.load(Relaxed);
        if s.tokens.load(Acquire) < 0 {
            let since = *self.negative_since.get_or_insert(now);
            if !was && now.saturating_duration_since(since) >= self.hold {
                s.overloaded.store(true, Relaxed);
                s.overloads.fetch_add(1, Relaxed);
                return Some(true);
            }
        } else {
            self.negative_since = None;
            if was {
                s.overloaded.store(false, Relaxed);
                return Some(false);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn lim() -> Limits {
        Limits { unreliable: 1000, reliable: 4000, pacer: true }
    }

    #[test]
    fn unreliable_gate_and_budget() {
        let s = PeerShared::default();
        s.tokens.store(500, Relaxed);
        assert_eq!(admit(&s, lim(), true, false, 400), Verdict::Queued);
        assert_eq!(admit(&s, lim(), true, false, 400), Verdict::Dropped); // budget
        assert_eq!(s.paced_drops.load(Relaxed), 1);
        assert_eq!(s.pending_cmd.load(Relaxed), 400, "failed budget releases its reservation");
        s.tokens.store(10_000, Relaxed);
        assert_eq!(admit(&s, lim(), true, false, 700), Verdict::Dropped); // gate: 400 + 700 > 1000
        assert_eq!(admit(&s, lim(), true, true, 700), Verdict::Queued); // exempt
        assert_eq!(s.dropped.load(Relaxed), 2);
    }

    #[test]
    fn reliable_cap_overflows() {
        let s = PeerShared::default();
        assert_eq!(admit(&s, lim(), false, false, 3000), Verdict::Queued);
        s.stack_outstanding.store(900, Relaxed);
        assert_eq!(admit(&s, lim(), false, false, 200), Verdict::Overflow);
        assert_eq!(s.reliable_overflows.load(Relaxed), 1);
        assert_eq!(s.tokens.load(Relaxed), -3000, "reliable spends budget");
        let unbounded = Limits { reliable: 0, ..lim() };
        assert_eq!(admit(&s, unbounded, false, false, 1 << 30), Verdict::Queued);
    }

    #[test]
    fn refill_keeps_concurrent_spends() {
        let s = Arc::new(PeerShared::default());
        let t0 = Instant::now();
        let mut p = Pacer::new(t0);
        let spender = {
            let s = s.clone();
            std::thread::spawn(move || {
                for _ in 0..100_000 {
                    s.tokens.fetch_sub(1, AcqRel);
                }
            })
        };
        let mut t = t0;
        for _ in 0..100_000 {
            t += Duration::from_nanos(1);
            refill(&s, &mut p, t); // adds 0 each call, so the result is exact
        }
        spender.join().unwrap();
        assert_eq!(s.tokens.load(Relaxed), -100_000, "lost update");
    }

    #[test]
    fn overload_after_hold() {
        let s = PeerShared::default();
        let mut o = Overload::new(100);
        let t0 = Instant::now();
        s.tokens.store(-1, Relaxed);
        assert_eq!(o.update(&s, t0), None);
        assert_eq!(o.update(&s, t0 + Duration::from_millis(99)), None);
        assert_eq!(o.update(&s, t0 + Duration::from_millis(100)), Some(true));
        assert!(s.overloaded.load(Relaxed));
        assert_eq!(o.update(&s, t0 + Duration::from_millis(200)), None);
        s.tokens.store(0, Relaxed);
        assert_eq!(o.update(&s, t0 + Duration::from_millis(210)), Some(false));
        assert_eq!(s.overloads.load(Relaxed), 1);
    }
}
