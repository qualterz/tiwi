use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MIN_RTT_WINDOW: Duration = Duration::from_secs(30);
const CONGESTION_RTT_INCREASE: Duration = Duration::from_millis(40);
const CONGESTION_LOSS_THRESHOLD: f32 = 0.02;
const CONGESTION_RECOVERY_HOLD: Duration = Duration::from_secs(5);

/// Real-time rolling 1-second bitrate tracker + windowed min-RTT congestion detector.
pub struct RelayMetrics {
    in_bytes_window: VecDeque<(Instant, usize)>,
    in_bytes_sum: usize,
    in_pkts_window: VecDeque<Instant>,
    out_bytes_window: VecDeque<(Instant, usize)>,
    out_bytes_sum: usize,

    // Windowed min-RTT samples (fixes monotonic min_rtt never decaying)
    rtt_samples: VecDeque<(Instant, Duration)>,
    pub congested: bool,
    clean_since: Option<Instant>,
    pub last_log: Instant,
}

impl RelayMetrics {
    pub fn new(now: Instant) -> Self {
        Self {
            in_bytes_window: VecDeque::new(),
            in_bytes_sum: 0,
            in_pkts_window: VecDeque::new(),
            out_bytes_window: VecDeque::new(),
            out_bytes_sum: 0,
            rtt_samples: VecDeque::new(),
            congested: false,
            clean_since: None,
            last_log: now,
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
    }

    pub fn in_kbps(&mut self, now: Instant) -> f64 {
        self.prune(now);
        (self.in_bytes_sum as f64 * 8.0) / 1000.0
    }

    pub fn in_pps(&mut self, now: Instant) -> usize {
        self.prune(now);
        self.in_pkts_window.len()
    }

    pub fn out_kbps(&mut self, now: Instant) -> f64 {
        self.prune(now);
        (self.out_bytes_sum as f64 * 8.0) / 1000.0
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

    pub fn reset_ingest(&mut self) {
        self.in_bytes_window.clear();
        self.in_bytes_sum = 0;
        self.in_pkts_window.clear();
    }
}
