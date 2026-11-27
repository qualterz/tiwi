use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveToColumn;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{execute, queue};
use tokio::sync::watch;

use tiwi_daemon::MetricsSnapshot;

const RTT_SPIKE_THRESHOLD_MS: u64 = 40;

pub struct TuiHeader {
    pub version: &'static str,
    pub watch_url: String,
    pub whip_url: String,
    pub whip_auth: bool,
    pub config_path: Option<PathBuf>,
    pub log_path: Option<PathBuf>,
    pub metrics_path: Option<PathBuf>,
    pub warning: Option<String>,
}

pub struct TuiLimits {
    pub max_peers: usize,
    pub max_uplink_mbps: f64,
}

pub async fn run(
    header: TuiHeader,
    limits: TuiLimits,
    mut status_rx: watch::Receiver<Option<MetricsSnapshot>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let started_at = Instant::now();
    let _ = print_header(&header);
    let _ = render_status_line(
        status_rx.borrow().as_ref(),
        &limits,
        started_at.elapsed().as_secs(),
    );

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let _ = render_status_line(
                    status_rx.borrow().as_ref(),
                    &limits,
                    started_at.elapsed().as_secs(),
                );
            }
            changed = status_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let _ = render_status_line(
                    status_rx.borrow().as_ref(),
                    &limits,
                    started_at.elapsed().as_secs(),
                );
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    let mut stdout = io::stdout();
    let _ = execute!(stdout, Print("\n"));
}

fn print_header(header: &TuiHeader) -> io::Result<()> {
    let mut stdout = io::stdout();
    queue!(
        stdout,
        Print("\n"),
        SetAttribute(Attribute::Bold),
        Print(format!("tiwi {}\n\n", header.version)),
        SetAttribute(Attribute::Reset),
    )?;

    write_field(&mut stdout, "Watch", &header.watch_url, None)?;
    let whip_suffix = if header.whip_auth {
        Some(" (auth)")
    } else {
        None
    };
    write_field(&mut stdout, "WHIP", &header.whip_url, whip_suffix)?;

    if let Some(path) = &header.config_path {
        write_field(&mut stdout, "Config", &path.display().to_string(), None)?;
    }
    if let Some(path) = &header.log_path {
        write_field(&mut stdout, "Logs", &path.display().to_string(), None)?;
    }
    if let Some(path) = &header.metrics_path {
        write_field(&mut stdout, "Metrics", &path.display().to_string(), None)?;
    }
    if let Some(warn_msg) = &header.warning {
        queue!(
            stdout,
            Print("\n"),
            SetForegroundColor(Color::Yellow),
            Print(format!("  Warning: {warn_msg}\n")),
            ResetColor,
        )?;
    }

    queue!(stdout, Print("\n"))?;
    stdout.flush()
}

fn write_field(
    stdout: &mut io::Stdout,
    label: &str,
    value: &str,
    dim_suffix: Option<&str>,
) -> io::Result<()> {
    queue!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print(format!("  {label:<8}")),
        ResetColor,
        Print(value),
    )?;
    if let Some(suffix) = dim_suffix {
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print(suffix),
            ResetColor,
        )?;
    }
    queue!(stdout, Print("\n"))
}

fn render_status_line(
    snap: Option<&MetricsSnapshot>,
    limits: &TuiLimits,
    uptime_secs: u64,
) -> io::Result<()> {
    let viewers = snap.map_or(0, |s| s.viewers);
    let in_kbps = snap.map_or(0.0, |s| s.in_kbps);
    let out_kbps = snap.map_or(0.0, |s| s.out_kbps);
    let rtt_ms = snap.and_then(|s| s.rtt_ms).unwrap_or(0);
    let rtt_spiked = snap
        .and_then(|s| s.rtt_delta_ms)
        .is_some_and(|d| d > RTT_SPIKE_THRESHOLD_MS);
    let loss_pct = snap.map_or(0.0, |s| s.loss_pct);
    let congested = snap.is_some_and(|s| s.congested);

    let max_uplink_kbps = limits.max_uplink_mbps * 1000.0;

    let peers_color = if limits.max_peers > 0 && viewers >= limits.max_peers {
        Some(Color::Yellow)
    } else {
        None
    };
    let out_color = if congested {
        Some(Color::Red)
    } else if max_uplink_kbps > 0.0 && out_kbps >= max_uplink_kbps * 0.85 {
        Some(Color::Yellow)
    } else {
        None
    };
    let rtt_color = if rtt_spiked {
        Some(if congested { Color::Red } else { Color::Yellow })
    } else {
        None
    };
    let loss_color = if loss_pct >= 5.0 {
        Some(Color::Red)
    } else if loss_pct >= 1.0 {
        Some(Color::Yellow)
    } else {
        None
    };

    let segments: [(String, Option<Color>); 6] = [
        (format!("{viewers} peers"), peers_color),
        (format!("in {}", format_bitrate(in_kbps)), None),
        (format!("out {}", format_bitrate(out_kbps)), out_color),
        (format!("rtt {rtt_ms}ms"), rtt_color),
        (format!("loss {loss_pct:.1}%"), loss_color),
        (format!("up {}", format_uptime(uptime_secs)), None),
    ];

    let max_cols = terminal::size()
        .map(|(cols, _)| usize::from(cols).saturating_sub(1))
        .unwrap_or(usize::MAX);

    let mut stdout = io::stdout();
    queue!(stdout, MoveToColumn(0), Clear(ClearType::CurrentLine))?;

    let mut used = 0usize;
    for (i, (text, color)) in segments.iter().enumerate() {
        if i > 0 {
            let sep = " · ";
            let sep_len = sep.chars().count();
            if used + sep_len > max_cols {
                break;
            }
            queue!(
                stdout,
                SetForegroundColor(Color::DarkGrey),
                Print(sep),
                ResetColor
            )?;
            used += sep_len;
        }

        let remaining = max_cols.saturating_sub(used);
        if remaining == 0 {
            break;
        }

        let clipped: String = text.chars().take(remaining).collect();
        used += clipped.chars().count();

        if let Some(c) = color {
            queue!(stdout, SetForegroundColor(*c), Print(&clipped), ResetColor)?;
        } else {
            queue!(stdout, Print(&clipped))?;
        }
    }

    stdout.flush()
}

fn format_bitrate(kbps: f64) -> String {
    if kbps >= 1000.0 {
        format!("{:.1} Mbps", kbps / 1000.0)
    } else {
        format!("{kbps:.0} kbps")
    }
}

fn format_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}
