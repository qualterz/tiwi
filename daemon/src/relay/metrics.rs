use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Serialize;
use tracing::{debug, info, warn};

const MIN_RTT_WINDOW: Duration = Duration::from_secs(30);
const CONGESTION_RTT_INCREASE: Duration = Duration::from_millis(40);
const CONGESTION_LOSS_THRESHOLD: f32 = 0.02;
const CONGESTION_RECOVERY_HOLD: Duration = Duration::from_secs(5);
const MAX_METRICS_FILES: usize = 20;

/// Derivative metrics snapshot for a single reporting window (1s for JSONL, 5s rollup for stdout).
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub ts_ms: u64,
    pub codec: String,
    pub viewers: usize,
    pub in_kbps: f64,
    pub out_kbps: f64,
    pub in_pps: usize,
    pub out_pps: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_delta_ms: Option<u64>,
    pub loss_pct: f32,
    pub pli: u64,
    pub udp_drops: u64,
    pub offers_rejected: u64,
    pub congested: bool,
}

/// Folds `N` 1-second snapshots into one multi-second summary for stdout:
/// rates are averaged, latency/loss take max, event deltas are summed.
pub struct MetricsRollup {
    target: usize,
    count: usize,
    in_kbps_sum: f64,
    out_kbps_sum: f64,
    in_pps_sum: usize,
    out_pps_sum: usize,
    max_rtt_ms: Option<u64>,
    max_rtt_delta_ms: Option<u64>,
    max_loss_pct: f32,
    pli_sum: u64,
    udp_drops_sum: u64,
    offers_rejected_sum: u64,
    congested: bool,
}

impl MetricsRollup {
    pub fn new(target: usize) -> Self {
        Self {
            target: target.max(1),
            count: 0,
            in_kbps_sum: 0.0,
            out_kbps_sum: 0.0,
            in_pps_sum: 0,
            out_pps_sum: 0,
            max_rtt_ms: None,
            max_rtt_delta_ms: None,
            max_loss_pct: 0.0,
            pli_sum: 0,
            udp_drops_sum: 0,
            offers_rejected_sum: 0,
            congested: false,
        }
    }

    pub fn push(&mut self, snap: &MetricsSnapshot) -> Option<MetricsSnapshot> {
        self.count += 1;
        self.in_kbps_sum += snap.in_kbps;
        self.out_kbps_sum += snap.out_kbps;
        self.in_pps_sum += snap.in_pps;
        self.out_pps_sum += snap.out_pps;
        self.max_rtt_ms = match (self.max_rtt_ms, snap.rtt_ms) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.max_rtt_delta_ms = match (self.max_rtt_delta_ms, snap.rtt_delta_ms) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.max_loss_pct = self.max_loss_pct.max(snap.loss_pct);
        self.pli_sum += snap.pli;
        self.udp_drops_sum += snap.udp_drops;
        self.offers_rejected_sum += snap.offers_rejected;
        self.congested |= snap.congested;

        if self.count < self.target {
            return None;
        }

        let n = self.count as f64;
        let out = MetricsSnapshot {
            ts_ms: snap.ts_ms,
            codec: snap.codec.clone(),
            viewers: snap.viewers,
            in_kbps: (self.in_kbps_sum / n * 10.0).round() / 10.0,
            out_kbps: (self.out_kbps_sum / n * 10.0).round() / 10.0,
            in_pps: self.in_pps_sum / self.count,
            out_pps: self.out_pps_sum / self.count,
            rtt_ms: self.max_rtt_ms,
            rtt_delta_ms: self.max_rtt_delta_ms,
            loss_pct: self.max_loss_pct,
            pli: self.pli_sum,
            udp_drops: self.udp_drops_sum,
            offers_rejected: self.offers_rejected_sum,
            congested: self.congested,
        };
        self.reset();
        Some(out)
    }

    fn reset(&mut self) {
        self.count = 0;
        self.in_kbps_sum = 0.0;
        self.out_kbps_sum = 0.0;
        self.in_pps_sum = 0;
        self.out_pps_sum = 0;
        self.max_rtt_ms = None;
        self.max_rtt_delta_ms = None;
        self.max_loss_pct = 0.0;
        self.pli_sum = 0;
        self.udp_drops_sum = 0;
        self.offers_rejected_sum = 0;
        self.congested = false;
    }
}

/// Appends 1-second JSONL metrics for the daemon run, then compresses to `.jsonl.gz`
/// on shutdown and prunes old metrics files.
pub struct MetricsWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    written: u64,
    failed: bool,
}

impl MetricsWriter {
    pub fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let ts = format_utc_filename_ts(SystemTime::now());
        let raw_path = dir.join(format!("{ts}.metrics.jsonl"));
        let path = std::path::absolute(&raw_path).unwrap_or(raw_path);
        let file = File::create(&path)?;
        prune_old_metrics(dir, MAX_METRICS_FILES);
        info!(path = %path.display(), "Metrics file");
        Ok(Self {
            path,
            writer: BufWriter::new(file),
            written: 0,
            failed: false,
        })
    }

    pub fn write(&mut self, snap: &MetricsSnapshot) {
        if self.failed {
            return;
        }
        let res = serde_json::to_writer(&mut self.writer, snap)
            .map_err(io::Error::other)
            .and_then(|()| self.writer.write_all(b"\n"))
            .and_then(|()| self.writer.flush());
        match res {
            Ok(()) => self.written += 1,
            Err(e) => {
                self.failed = true;
                warn!(path = %self.path.display(), err = %e, "Failed to write metrics snapshot");
            }
        }
    }

    /// Flushes and gzips the `.jsonl` file into `.jsonl.gz` (or removes it if empty).
    pub fn finish(mut self) {
        let _ = self.writer.flush();
        drop(self.writer);

        if self.written == 0 {
            let _ = fs::remove_file(&self.path);
            return;
        }

        let mut gz_os = self.path.as_os_str().to_os_string();
        gz_os.push(".gz");
        let dst = PathBuf::from(gz_os);

        match gzip_file(&self.path, &dst) {
            Ok(()) => {
                let _ = fs::remove_file(&self.path);
                if let Some(dir) = dst.parent() {
                    prune_old_metrics(dir, MAX_METRICS_FILES);
                }
                debug!(path = %dst.display(), "Metrics file compressed");
            }
            Err(e) => {
                warn!(path = %self.path.display(), err = %e, "Failed to compress metrics file");
            }
        }
    }
}

fn gzip_file(src: &Path, dst: &Path) -> io::Result<()> {
    let mut input = File::open(src)?;
    let output = File::create(dst)?;
    let mut encoder = GzEncoder::new(output, Compression::default());
    io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

fn prune_old_metrics(dir: &Path, max_files: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".metrics.jsonl") || n.ends_with(".metrics.jsonl.gz"))
        })
        .collect();
    if files.len() <= max_files {
        return;
    }
    // Filenames start with ISO-8601 UTC timestamps, so lexical order == chronological order.
    files.sort();
    let to_remove = files.len() - max_files;
    for old in files.into_iter().take(to_remove) {
        let _ = fs::remove_file(old);
    }
}

fn unix_ms(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Formats a UTC `SystemTime` as `YYYY-MM-DDTHH-MM-SSZ` (safe on Windows and Unix filenames).
fn format_utc_filename_ts(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let min = (rem % 3_600) / 60;
    let sec = rem % 60;

    // Howard Hinnant's civil-from-days algorithm
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{min:02}-{sec:02}Z")
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
    pub last_tick: Instant,

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
            last_tick: now,
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

    /// Produces a 1-second derivative snapshot and resets per-interval delta counters.
    pub fn take_snapshot(
        &mut self,
        now: Instant,
        codec: String,
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

        let in_kbps = ((self.in_bytes_sum as f64 * 8.0) / 100.0).round() / 10.0;
        let out_kbps = ((self.out_bytes_sum as f64 * 8.0) / 100.0).round() / 10.0;
        let loss_pct = (max_loss.unwrap_or(0.0) * 10000.0).round() / 100.0;

        let snap = MetricsSnapshot {
            ts_ms: unix_ms(SystemTime::now()),
            codec,
            viewers,
            in_kbps,
            out_kbps,
            in_pps: self.in_pkts_window.len(),
            out_pps: self.out_pkts_window.len(),
            rtt_ms,
            rtt_delta_ms,
            loss_pct,
            pli: self.pli_sent,
            udp_drops: self.udp_drops,
            offers_rejected: self.offers_rejected,
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
