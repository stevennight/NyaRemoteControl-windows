//! Adaptive bitrate, driven by what the network actually delivers.
//!
//! The primary signal is **backlog**: encoded video that QUIC has accepted but
//! not yet put on the wire. A growing backlog means we produce more than the
//! path carries, and the measured send rate then tells how much it does carry,
//! so the new target is set from that rate instead of blind multiplicative
//! cuts. RTT growth and loss are noisy on relayed / proxied paths (reordering
//! looks like loss), so only the more aggressive policies use them.
//!
//! Policies (chosen by the client, see `BitratePolicy`):
//!
//! | policy   | reacts to                         | sustained | floor |
//! |----------|-----------------------------------|-----------|-------|
//! | quality  | backlog > 1 s                     | 2 s       | 60 %  |
//! | balanced | backlog > 400 ms, queueing delay  | 0.75 s    | 35 %  |
//! | smooth   | backlog > 200 ms, delay, loss     | 0.5 s     | 15 %  |
//! | fixed    | nothing                           | –         | 100 % |
//!
//! Every change costs a keyframe (FFmpeg's NVENC restarts with an IDR frame),
//! so changes are few and large: a cut is at least 10 %, recovery goes up
//! ×1.4 at most every 2 s and straight to the maximum once within 10 % of it.

use std::time::{Duration, Instant};

use nya_proto::pb::{BitratePolicy, StreamMode};

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub now: Instant,
    pub rtt: Duration,
    pub lost_packets: u64,
    pub sent_packets: u64,
    /// Total bytes QUIC has put on the wire (for the send rate).
    pub sent_bytes: u64,
    pub backlog_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct Params {
    backlog_ms: u64,
    /// Queueing-delay trigger: max(fixed, factor × min RTT); None = ignored.
    queue: Option<(Duration, f64)>,
    /// Loss-rate trigger; None = ignored.
    loss: Option<f64>,
    sustained: u32,
    floor: f64,
}

pub fn resolve(policy: i32, mode: i32) -> BitratePolicy {
    match BitratePolicy::try_from(policy).unwrap_or(BitratePolicy::Unspecified) {
        BitratePolicy::Unspecified if mode == StreamMode::Game as i32 => BitratePolicy::Balanced,
        BitratePolicy::Unspecified => BitratePolicy::Quality,
        p => p,
    }
}

pub fn policy_name(p: BitratePolicy) -> &'static str {
    match p {
        BitratePolicy::Quality => "清晰优先",
        BitratePolicy::Balanced => "均衡",
        BitratePolicy::Smooth => "流畅优先",
        BitratePolicy::Fixed => "固定码率",
        BitratePolicy::Unspecified => "自动",
    }
}

fn params(p: BitratePolicy) -> Params {
    match p {
        BitratePolicy::Smooth => Params {
            backlog_ms: 200,
            queue: Some((Duration::from_millis(80), 1.0)),
            loss: Some(0.05),
            sustained: 2,
            floor: 0.15,
        },
        BitratePolicy::Balanced => Params {
            backlog_ms: 400,
            queue: Some((Duration::from_millis(150), 1.5)),
            loss: None,
            sustained: 3,
            floor: 0.35,
        },
        _ => Params { backlog_ms: 1000, queue: None, loss: None, sustained: 8, floor: 0.6 },
    }
}

#[derive(Debug)]
pub struct Abr {
    policy: BitratePolicy,
    p: Params,
    max: u32,
    min: u32,
    target: u32,
    reported: u32,
    min_rtt: Option<Duration>,
    min_rtt_at: Instant,
    prev: Option<Sample>,
    /// Smoothed send rate (kbit/s) while video was queued, i.e. what the path
    /// carries; a still picture (little to send) says nothing about that.
    rate_kbps: f64,
    rate_at: Instant,
    bad_streak: u32,
    last_bad: Instant,
    last_decrease: Instant,
    last_increase: Instant,
    /// The last adjustment and why (also logged).
    pub note: String,
    note_at: Option<Instant>,
    /// Backlog of the latest sample, in ms at the current target.
    backlog_now_ms: u64,
}

const DECREASE_GAP: Duration = Duration::from_secs(1);
const CLEAR_BEFORE_INCREASE: Duration = Duration::from_millis(1500);
const INCREASE_GAP: Duration = Duration::from_secs(2);
const INCREASE: f64 = 1.4;
/// Smallest cut, and how close to the maximum an increase goes all the way.
const MIN_STEP: f64 = 0.1;
const MIN_RTT_WINDOW: Duration = Duration::from_secs(30);
/// A send-rate measurement older than this is not used.
const RATE_FRESH: Duration = Duration::from_secs(2);

impl Abr {
    /// `None` for the fixed policy.
    pub fn new(max_kbps: u32, policy: BitratePolicy, now: Instant) -> Option<Self> {
        if policy == BitratePolicy::Fixed {
            return None;
        }
        let p = params(policy);
        let max = max_kbps.max(500);
        Some(Self {
            policy,
            p,
            max,
            min: ((max as f64 * p.floor) as u32).max(500).min(max),
            target: max,
            reported: max,
            min_rtt: None,
            min_rtt_at: now,
            prev: None,
            rate_kbps: 0.0,
            rate_at: now,
            bad_streak: 0,
            last_bad: now - Duration::from_secs(60),
            last_decrease: now - Duration::from_secs(60),
            last_increase: now,
            note: String::new(),
            note_at: None,
            backlog_now_ms: 0,
        })
    }

    #[cfg(test)]
    pub fn target(&self) -> u32 {
        self.target
    }

    fn congestion(&mut self, s: &Sample) -> Option<String> {
        if self.min_rtt.is_none_or(|m| s.rtt < m) || s.now - self.min_rtt_at > MIN_RTT_WINDOW {
            self.min_rtt = Some(s.rtt);
            self.min_rtt_at = s.now;
        }
        let min_rtt = self.min_rtt.unwrap();
        let prev = self.prev.replace(*s);
        if let Some(prev) = prev {
            let dt = (s.now - prev.now).as_secs_f64();
            if dt > 0.0 && (prev.backlog_bytes > 0 || s.backlog_bytes > 0) {
                let kbps = s.sent_bytes.saturating_sub(prev.sent_bytes) as f64 * 8.0 / 1000.0 / dt;
                let fresh = self.rate_kbps > 0.0 && s.now - self.rate_at < RATE_FRESH;
                self.rate_kbps = if fresh { self.rate_kbps * 0.7 + kbps * 0.3 } else { kbps };
                self.rate_at = s.now;
            }
        }
        let backlog_ms = s.backlog_bytes * 8 / self.target.max(1) as u64;
        self.backlog_now_ms = backlog_ms;
        if backlog_ms > self.p.backlog_ms {
            return Some(format!("发送积压 {backlog_ms} ms"));
        }
        if let Some((fixed, factor)) = self.p.queue {
            let queue = s.rtt.saturating_sub(min_rtt);
            if queue > fixed.max(min_rtt.mul_f64(factor)) {
                return Some(format!("排队延迟 {} ms", queue.as_millis()));
            }
        }
        if let (Some(limit), Some(prev)) = (self.p.loss, prev) {
            let lost = s.lost_packets.saturating_sub(prev.lost_packets);
            let sent = s.sent_packets.saturating_sub(prev.sent_packets).max(1);
            let rate = lost as f64 / sent as f64;
            if lost >= 5 && rate > limit {
                return Some(format!("丢包 {:.0}%", rate * 100.0));
            }
        }
        None
    }

    /// For the statistics panel: live state, then the last adjustment and
    /// how long ago it was (the reason it gives is from that moment).
    pub fn summary(&self, now: Instant) -> String {
        let mut out = format!("{}，目标 {:.1} Mbps，当前积压 {} ms", policy_name(self.policy), self.target as f64 / 1000.0, self.backlog_now_ms);
        if let Some(at) = self.note_at {
            out += &format!("；{} 秒前：{}", (now - at).as_secs(), self.note);
        }
        out
    }

    /// Feed one sample; returns the new target when it moved by 5 % or more.
    pub fn update(&mut self, s: Sample) -> Option<u32> {
        match self.congestion(&s) {
            Some(why) => {
                self.bad_streak += 1;
                self.last_bad = s.now;
                if self.bad_streak >= self.p.sustained && s.now - self.last_decrease >= DECREASE_GAP {
                    // What the path actually carried, a little under; a 10–30 % step.
                    let measured = if self.rate_kbps > 0.0 && s.now - self.rate_at < RATE_FRESH {
                        self.rate_kbps * 0.9
                    } else {
                        self.target as f64 * 0.85
                    };
                    let next = measured.min(self.target as f64 * (1.0 - MIN_STEP)).max(self.target as f64 * 0.7);
                    let next = (next as u32).clamp(self.min, self.max);
                    // Already at the floor: no change, no keyframe.
                    if next < self.target {
                        self.target = next;
                        self.note = format!("{why}，实际 {:.1} Mbps → 降到 {:.1} Mbps", self.rate_kbps / 1000.0, self.target as f64 / 1000.0);
                        self.note_at = Some(s.now);
                    }
                    self.last_decrease = s.now;
                    self.bad_streak = 0;
                }
            }
            None => {
                self.bad_streak = 0;
                if self.target < self.max
                    && s.now - self.last_bad >= CLEAR_BEFORE_INCREASE
                    && s.now - self.last_increase >= INCREASE_GAP
                {
                    let next = self.target as f64 * INCREASE;
                    self.target = if next >= self.max as f64 * (1.0 - MIN_STEP) { self.max } else { next as u32 };
                    self.last_increase = s.now;
                    self.note = format!("网络恢复 → 升到 {:.1} Mbps", self.target as f64 / 1000.0);
                    self.note_at = Some(s.now);
                }
            }
        }
        let diff = (self.target as i64 - self.reported as i64).unsigned_abs() as f64;
        if diff >= self.reported as f64 * 0.05 || (self.target == self.max && self.reported != self.max) {
            self.reported = self.target;
            Some(self.target)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sim {
        t0: Instant,
        ms: u64,
        sent_bytes: u64,
        sent_pk: u64,
        lost: u64,
    }

    impl Sim {
        fn new() -> Self {
            Self { t0: Instant::now(), ms: 0, sent_bytes: 0, sent_pk: 0, lost: 0 }
        }
        /// One 250 ms step at `kbps` actually sent.
        fn step(&mut self, a: &mut Abr, kbps: u64, rtt: u64, lost: u64, backlog: u64) -> Option<u32> {
            self.ms += 250;
            self.sent_bytes += kbps * 1000 / 8 / 4;
            self.sent_pk += 100;
            self.lost += lost;
            a.update(Sample {
                now: self.t0 + Duration::from_millis(self.ms),
                rtt: Duration::from_millis(rtt),
                lost_packets: self.lost,
                sent_packets: self.sent_pk,
                sent_bytes: self.sent_bytes,
                backlog_bytes: backlog,
            })
        }
    }

    #[test]
    fn fixed_policy_has_no_controller() {
        assert!(Abr::new(10_000, BitratePolicy::Fixed, Instant::now()).is_none());
    }

    #[test]
    fn automatic_policy_follows_mode() {
        assert_eq!(resolve(0, StreamMode::Office as i32), BitratePolicy::Quality);
        assert_eq!(resolve(0, StreamMode::Game as i32), BitratePolicy::Balanced);
        assert_eq!(resolve(BitratePolicy::Smooth as i32, StreamMode::Office as i32), BitratePolicy::Smooth);
    }

    #[test]
    fn loss_and_jitter_do_not_touch_quality_or_balanced() {
        for policy in [BitratePolicy::Quality, BitratePolicy::Balanced] {
            let mut sim = Sim::new();
            let mut a = Abr::new(10_000, policy, sim.t0).unwrap();
            for i in 0..80 {
                // 20 % "loss" (reordering on a proxy) and RTT spikes, no backlog.
                let rtt = if i % 4 == 0 { 120 } else { 40 };
                assert!(sim.step(&mut a, 8_000, rtt, 20, 0).is_none(), "{policy:?} step {i}");
            }
            assert_eq!(a.target(), 10_000);
        }
    }

    #[test]
    fn quality_needs_long_heavy_backlog_and_keeps_60_percent() {
        let mut sim = Sim::new();
        let mut a = Abr::new(10_000, BitratePolicy::Quality, sim.t0).unwrap();
        // 800 ms backlog is tolerated.
        for _ in 0..20 {
            assert!(sim.step(&mut a, 9_000, 30, 0, 1_000_000).is_none());
        }
        // 2 s of >1 s backlog while only 3 Mbit/s get through.
        let mut last = None;
        for _ in 0..60 {
            if let Some(v) = sim.step(&mut a, 3_000, 30, 0, 2_000_000) {
                last = Some(v);
            }
        }
        assert_eq!(last, Some(6_000), "floor 60 %");
        assert!(a.note.contains("积压"));
    }

    #[test]
    fn balanced_sets_target_from_measured_rate_and_recovers() {
        let mut sim = Sim::new();
        let mut a = Abr::new(10_000, BitratePolicy::Balanced, sim.t0).unwrap();
        for _ in 0..4 {
            sim.step(&mut a, 7_500, 30, 0, 0);
        }
        // Path carries ~7.5 Mbit/s, backlog of 500 ms builds up.
        let mut v = None;
        for _ in 0..3 {
            v = v.or(sim.step(&mut a, 7_500, 30, 0, 700_000));
        }
        let v = v.expect("reacts after 0.75 s");
        assert!((6_500..=7_100).contains(&v), "about 0.9 × measured, got {v}");
        // Clear again: back to 10 Mbit/s within a few seconds.
        for _ in 0..24 {
            sim.step(&mut a, 7_000, 30, 0, 0);
        }
        assert_eq!(a.target(), 10_000);
    }

    #[test]
    fn still_picture_does_not_lower_the_measured_rate() {
        let mut sim = Sim::new();
        let mut a = Abr::new(10_000, BitratePolicy::Balanced, sim.t0).unwrap();
        // 10 s of a still picture: almost nothing to send, nothing queued.
        for _ in 0..40 {
            sim.step(&mut a, 200, 30, 0, 0);
        }
        // Then more than the path carries (9 Mbit/s): set from that, not from the idle rate.
        let mut v = None;
        for _ in 0..3 {
            v = v.or(sim.step(&mut a, 9_000, 30, 0, 700_000));
        }
        let v = v.expect("reacts after 0.75 s");
        assert!((8_000..=8_200).contains(&v), "about 0.9 × 9 Mbit/s, got {v}");
    }

    #[test]
    fn few_large_changes_because_each_costs_a_keyframe() {
        let mut sim = Sim::new();
        let mut a = Abr::new(10_000, BitratePolicy::Smooth, sim.t0).unwrap();
        let mut changes = Vec::new();
        // 8 s of a path carrying 1 Mbit/s: down to the 15 % floor (30 % steps, 1 s apart).
        for _ in 0..32 {
            changes.extend(sim.step(&mut a, 1_000, 30, 0, 400_000));
        }
        assert_eq!(a.target(), 1_500);
        let cuts = changes.len();
        assert!(changes.windows(2).all(|w| w[1] as f64 <= w[0] as f64 * 0.9 || w[1] == 1_500), "{changes:?}");
        // Clear: back to the maximum in a few steps (1.5 → 2.1 → 2.94 → 4.1 → 5.8 → 8.1 → 10).
        for _ in 0..80 {
            changes.extend(sim.step(&mut a, 1_500, 30, 0, 0));
        }
        assert_eq!(a.target(), 10_000);
        assert!(changes.len() - cuts <= 6, "{changes:?}");
        assert!(changes[cuts..].windows(2).all(|w| w[1] as f64 >= w[0] as f64 * 1.1), "{changes:?}");
    }

    #[test]
    fn one_dip_costs_two_changes() {
        let mut sim = Sim::new();
        let mut a = Abr::new(10_000, BitratePolicy::Balanced, sim.t0).unwrap();
        let mut changes = Vec::new();
        for _ in 0..3 {
            changes.extend(sim.step(&mut a, 8_000, 30, 0, 700_000));
        }
        for _ in 0..40 {
            changes.extend(sim.step(&mut a, 7_000, 30, 0, 0));
        }
        assert_eq!(changes, vec![7_200, 10_000]);
    }

    #[test]
    fn smooth_reacts_to_sustained_loss() {
        let mut sim = Sim::new();
        let mut a = Abr::new(10_000, BitratePolicy::Smooth, sim.t0).unwrap();
        sim.step(&mut a, 9_000, 30, 0, 0);
        let mut r = None;
        for _ in 0..2 {
            r = r.or(sim.step(&mut a, 9_000, 30, 20, 0));
        }
        assert!(r.is_some());
        assert!(a.note.contains("丢包"));
    }
}
