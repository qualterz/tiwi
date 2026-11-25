use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MIN_RTT_WINDOW: Duration = Duration::from_secs(30);
const CONGESTION_RTT_INCREASE: Duration = Duration::from_millis(40);
const CONGESTION_LOSS_THRESHOLD: f32 = 0.02;
const CONGESTION_RECOVERY_HOLD: Duration = Duration::from_secs(5);

/// Derivative metrics snapshot for a single reporting window.
pub struct MetricsSnapshot {
    pub in_kbps: f64,
    pub out_kbps: f64,
    pub in_pps: usize,
    pub out_pps: usize,
    pub rtt_ms: Option<u64>,
    pub rtt_delta_ms: Option<u64>,
    pub loss_pct: f32,
    pub pli: u64,
    pub udp_drops: u64,
    pub offers_rejected: u64,
    pub viewers: usize,
    pub congested: bool,
}

/// Real-time rolling 1-second rate tracker, windowed min-RTT congestion detector,
/// and per-interval delta counters.
pub struct RelayMetrics {
    in_bytes_window: VecDeque<(Instant, usize)>,
    in_bytes_sum: usize,
    in_pkts_window: VecDeque<Instant>,
    out_bytes_window: VecDeque<(Instant, usize)>,
    out_bytes_sum: usize,
    out_pkts_window: VecDeque<Instant>,

    // Windowed min-RTT samples (fixes monotonic min_rtt never decaying)
    rtt_samples: VecDeque<(Instant, Duration)>,
    pub congested: bool,
    clean_since: Option<Instant>,
    pub last_log: Instant,

    // Per-interval delta counters (reset on each snapshot)
    pli_sent: u64,
    udp_drops: u64,
    offers_rejected: u64,
}

impl RelayMetrics {
    pub fn new(now: Instant) -> Self {
        Self {
            in_bytes_window: VecDeque::new(),
            in_bytes_sum: 0,
            in_pkts_window: VecDeque::new(),
            out_bytes_window: VecDeque::new(),
            out_bytes_sum: 0,
            out_pkts_window: VecDeque::new(),
            rtt_samples: VecDeque::new(),
            congested: false,
            clean_since: None,
            last_log: now,
            pli_sent: 0,
            udp_drops: 0,
            offers_rejected: 0,
        }
    }

    fn prune(&mut self, now: Instant) {
        let cutoff = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
        while let Some(&(ts, bytes)) = self.in_bytes_window.front() {
            if ts < cutoff {
                self.in_bytes_window.pop_front();
                self.in_bytes_sum = self.in_bytes_sum.saturating_sub(bytes);
            } else {
                break;
            }
        }
        while let Some(&ts) = self.in_pkts_window.front() {
            if ts < cutoff {
                self.in_pkts_window.pop_front();
            } else {
                break;
            }
        }
        while let Some(&(ts, bytes)) = self.out_bytes_window.front() {
            if ts < cutoff {
                self.out_bytes_window.pop_front();
                self.out_bytes_sum = self.out_bytes_sum.saturating_sub(bytes);
            } else {
                break;
            }
        }
        while let Some(&ts) = self.out_pkts_window.front() {
            if ts < cutoff {
                self.out_pkts_window.pop_front();
            } else {
                break;
            }
        }
        let rtt_cutoff = now.checked_sub(MIN_RTT_WINDOW).unwrap_or(now);
        while let Some(&(ts, _)) = self.rtt_samples.front() {
            if ts < rtt_cutoff {
                self.rtt_samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn record_in(&mut self, now: Instant, bytes: usize) {
        self.in_bytes_window.push_back((now, bytes));
        self.in_bytes_sum += bytes;
        self.in_pkts_window.push_back(now);
        self.prune(now);
    }

    pub fn record_out(&mut self, now: Instant, bytes: usize) {
        self.out_bytes_window.push_back((now, bytes));
        self.out_bytes_sum += bytes;
        self.out_pkts_window.push_back(now);
    }

    pub fn record_pli(&mut self) {
        self.pli_sent += 1;
    }

    pub fn record_udp_drop(&mut self) {
        self.udp_drops += 1;
    }

    pub fn record_offer_rejected(&mut self) {
        self.offers_rejected += 1;
    }

    pub fn in_kbps(&mut self, now: Instant) -> f64 {
        self.prune(now);
        (self.in_bytes_sum as f64 * 8.0) / 1000.0
    }

    /// Updates windowed min-RTT and wall-time congestion state.
    /// Returns `Some(new_state)` if congestion state transitioned.
    pub fn update_congestion(
        &mut self,
        now: Instant,
        rtt: Option<Duration>,
        loss: Option<f32>,
    ) -> Option<bool> {
        self.prune(now);
        if let Some(r) = rtt {
            self.rtt_samples.push_back((now, r));
        }

        let min_rtt = self.rtt_samples.iter().map(|&(_, r)| r).min();
        let rtt_spiked = match (rtt, min_rtt) {
            (Some(cur), Some(min)) => cur > min + CONGESTION_RTT_INCREASE,
            _ => false,
        };
        let high_loss = loss.is_some_and(|l| l > CONGESTION_LOSS_THRESHOLD);

        if rtt_spiked && high_loss {
            self.clean_since = None;
            if !self.congested {
                self.congested = true;
                return Some(true);
            }
        } else if self.congested {
            let clean_start = *self.clean_since.get_or_insert(now);
            if now.duration_since(clean_start) >= CONGESTION_RECOVERY_HOLD {
                self.congested = false;
                self.clean_since = None;
                return Some(false);
            }
        }
        None
    }

    /// Produces a derivative snapshot and resets per-interval delta counters.
    pub fn take_snapshot(
        &mut self,
        now: Instant,
        viewers: usize,
        max_rtt: Option<Duration>,
        max_loss: Option<f32>,
    ) -> MetricsSnapshot {
        self.prune(now);
        let min_rtt = self.rtt_samples.iter().map(|&(_, r)| r).min();
        let rtt_ms = max_rtt.map(|d| d.as_millis() as u64);
        let rtt_delta_ms = match (max_rtt, min_rtt) {
            (Some(cur), Some(min)) => Some(cur.saturating_sub(min).as_millis() as u64),
            _ => None,
        };

        let snap = MetricsSnapshot {
            in_kbps: (self.in_bytes_sum as f64 * 8.0) / 1000.0,
            out_kbps: (self.out_bytes_sum as f64 * 8.0) / 1000.0,
            in_pps: self.in_pkts_window.len(),
            out_pps: self.out_pkts_window.len(),
            rtt_ms,
            rtt_delta_ms,
            loss_pct: max_loss.unwrap_or(0.0) * 100.0,
            pli: self.pli_sent,
            udp_drops: self.udp_drops,
            offers_rejected: self.offers_rejected,
            viewers,
            congested: self.congested,
        };

        self.pli_sent = 0;
        self.udp_drops = 0;
        self.offers_rejected = 0;
        snap
    }

    pub fn reset_ingest(&mut self) {
        self.in_bytes_window.clear();
        self.in_bytes_sum = 0;
        self.in_pkts_window.clear();
        self.out_bytes_window.clear();
        self.out_bytes_sum = 0;
        self.out_pkts_window.clear();
        self.pli_sent = 0;
        self.udp_drops = 0;
        self.offers_rejected = 0;
    }
}
