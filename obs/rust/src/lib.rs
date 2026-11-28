#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString, c_char, c_int};
use std::fs::{self, File};
use std::io;
use std::net::IpAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::{Mutex, Once, RwLock};
use std::time::SystemTime;

use tiwi_daemon::{
    DirectEngine, DirectVideoConfig, EncodedFrame, RelayConfig, VideoCodec, default_logs_dir,
    format_utc_filename_ts, generate_token, prune_old_files,
};
use tracing::{Level, Metadata};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const MAX_LOG_FILES: usize = 20;

pub type TiwiLogCallback = Option<unsafe extern "C" fn(level: c_int, message: *const c_char)>;

static LOG_CALLBACK: RwLock<TiwiLogCallback> = RwLock::new(None);
static INIT_TRACING: Once = Once::new();

struct ObsLogMakeWriter;

struct ObsLogWriter {
    level: c_int,
    buf: Vec<u8>,
}

impl io::Write for ObsLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ObsLogWriter {
    fn drop(&mut self) {
        let trimmed = String::from_utf8_lossy(&self.buf);
        let msg = trimmed.trim();
        if msg.is_empty() {
            return;
        }
        let cb = LOG_CALLBACK.read().ok().and_then(|guard| *guard);
        if let Some(cb) = cb
            && let Ok(cstr) = CString::new(msg.replace('\0', " "))
        {
            unsafe { cb(self.level, cstr.as_ptr()) };
        }
    }
}

impl<'a> MakeWriter<'a> for ObsLogMakeWriter {
    type Writer = ObsLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ObsLogWriter {
            level: 2,
            buf: Vec::with_capacity(128),
        }
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        let level = match *meta.level() {
            Level::ERROR => 0,
            Level::WARN => 1,
            Level::INFO => 2,
            Level::DEBUG | Level::TRACE => 3,
        };
        ObsLogWriter {
            level,
            buf: Vec::with_capacity(128),
        }
    }
}

fn open_log_file(dir: &Path, ts: &str) -> io::Result<(PathBuf, File)> {
    fs::create_dir_all(dir)?;
    let raw_path = dir.join(format!("{ts}.log"));
    let path = std::path::absolute(&raw_path).unwrap_or(raw_path);
    let file = File::create(&path)?;
    prune_old_files(dir, MAX_LOG_FILES, |n| n.ends_with(".log"));
    Ok((path, file))
}

fn init_tracing() {
    INIT_TRACING.call_once(|| {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("tiwi_daemon=info,tiwi_obs=info"));

        let obs_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .without_time()
            .with_level(false)
            .with_target(false)
            .with_writer(ObsLogMakeWriter);

        let mut log_file_path = None;
        let mut log_file_err = None;
        let file_layer = if cfg!(test) {
            None
        } else {
            default_logs_dir().and_then(|dir| {
                let ts = format_utc_filename_ts(SystemTime::now());
                match open_log_file(&dir, &ts) {
                    Ok((path, file)) => {
                        log_file_path = Some(path);
                        Some(
                            tracing_subscriber::fmt::layer()
                                .with_ansi(false)
                                .with_target(false)
                                .with_writer(Mutex::new(file)),
                        )
                    }
                    Err(e) => {
                        log_file_err = Some((dir, e));
                        None
                    }
                }
            })
        };

        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(obs_layer)
            .with(file_layer)
            .try_init();

        if let Some((dir, err)) = log_file_err {
            tracing::warn!(dir = %dir.display(), err = %err, "Could not open log file");
        }
        if let Some(path) = log_file_path {
            tracing::info!(path = %path.display(), "Log file");
        }
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tiwi_set_log_callback(cb: TiwiLogCallback) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Ok(mut guard) = LOG_CALLBACK.write() {
            *guard = cb;
        }
        init_tracing();
    }));
}

pub struct TiwiEngine {
    inner: DirectEngine,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiwiVideoCodec {
    H264 = 0,
    H265 = 1,
    Av1 = 2,
}

impl From<TiwiVideoCodec> for VideoCodec {
    fn from(c: TiwiVideoCodec) -> Self {
        match c {
            TiwiVideoCodec::H264 => VideoCodec::H264,
            TiwiVideoCodec::H265 => VideoCodec::H265,
            TiwiVideoCodec::Av1 => VideoCodec::Av1,
        }
    }
}

#[repr(C)]
pub struct TiwiConfig {
    pub token: *const c_char,
    pub viewer_url: *const c_char,
    pub mqtt_url: *const c_char,
    pub stun_server: *const c_char,
    pub public_ip: *const c_char,
    pub logs_dir: *const c_char,
    pub media_port: u16,
    pub max_peers: u32,
    pub max_uplink_mbps: f64,
}

#[repr(C)]
pub struct TiwiMetrics {
    pub live: bool,
    pub congested: bool,
    pub viewers: u32,
    pub in_kbps: f64,
    pub out_kbps: f64,
    pub rtt_ms: i64,
    pub loss_pct: f32,
    pub codec: [c_char; 16],
}

unsafe fn opt_cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(ptr) }.to_str().ok()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

unsafe fn write_cstr(src: &str, out_buf: *mut c_char, out_len: usize) -> bool {
    if out_buf.is_null() || out_len == 0 {
        return false;
    }
    let bytes = src.as_bytes();
    if bytes.len() + 1 > out_len {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf as *mut u8, bytes.len());
        *out_buf.add(bytes.len()) = 0;
    }
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tiwi_generate_token(out_buf: *mut c_char, out_len: usize) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        let token = generate_token();
        unsafe { write_cstr(&token, out_buf, out_len) }
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tiwi_engine_create(cfg: *const TiwiConfig) -> *mut TiwiEngine {
    catch_unwind(AssertUnwindSafe(|| {
        init_tracing();

        let mut relay_cfg = RelayConfig {
            logs_dir: default_logs_dir(),
            ..RelayConfig::default()
        };
        if !cfg.is_null() {
            let c = unsafe { &*cfg };
            if let Some(token) = unsafe { opt_cstr(c.token) } {
                relay_cfg.token = token;
            }
            if let Some(url) = unsafe { opt_cstr(c.viewer_url) } {
                relay_cfg.viewer_url = url;
            }
            if let Some(url) = unsafe { opt_cstr(c.mqtt_url) } {
                relay_cfg.mqtt_url = url;
            }
            if let Some(stun) = unsafe { opt_cstr(c.stun_server) } {
                relay_cfg.stun_server = stun;
            }
            if let Some(ip_str) = unsafe { opt_cstr(c.public_ip) } {
                relay_cfg.public_ip = ip_str.parse::<IpAddr>().ok();
            }
            if let Some(logs) = unsafe { opt_cstr(c.logs_dir) } {
                relay_cfg.logs_dir = Some(PathBuf::from(logs));
            }
            relay_cfg.media_port = c.media_port;
            relay_cfg.max_peers = c.max_peers as usize;
            relay_cfg.max_uplink_mbps = c.max_uplink_mbps.max(0.0);
        }

        match DirectEngine::start(relay_cfg) {
            Ok(inner) => Box::into_raw(Box::new(TiwiEngine { inner })),
            Err(e) => {
                tracing::error!(err = %e, "Failed to start Tiwi direct engine");
                std::ptr::null_mut()
            }
        }
    }))
    .unwrap_or(std::ptr::null_mut())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tiwi_engine_destroy(engine: *mut TiwiEngine) {
    if engine.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let boxed = unsafe { Box::from_raw(engine) };
        boxed.inner.shutdown();
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tiwi_engine_watch_url(
    engine: *const TiwiEngine,
    out_buf: *mut c_char,
    out_len: usize,
) -> bool {
    if engine.is_null() {
        return false;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let eng = unsafe { &*engine };
        unsafe { write_cstr(eng.inner.watch_url(), out_buf, out_len) }
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tiwi_engine_start_stream(
    engine: *mut TiwiEngine,
    codec: TiwiVideoCodec,
    extra_data: *const u8,
    extra_len: usize,
) -> bool {
    if engine.is_null() {
        return false;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let eng = unsafe { &*engine };
        let extra = if !extra_data.is_null() && extra_len > 0 {
            unsafe { slice::from_raw_parts(extra_data, extra_len) }.to_vec()
        } else {
            Vec::new()
        };
        eng.inner.start_stream(DirectVideoConfig {
            codec: codec.into(),
            extra_data: extra,
        })
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tiwi_engine_push_frame(
    engine: *mut TiwiEngine,
    is_video: bool,
    is_keyframe: bool,
    pts: i64,
    timebase_num: u32,
    timebase_den: u32,
    data: *const u8,
    len: usize,
) -> bool {
    if engine.is_null() || data.is_null() || len == 0 {
        return false;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let eng = unsafe { &*engine };
        let bytes = unsafe { slice::from_raw_parts(data, len) }.to_vec();
        eng.inner.push_frame(EncodedFrame {
            is_video,
            is_keyframe,
            pts,
            timebase_num,
            timebase_den,
            data: bytes,
        })
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tiwi_engine_stop_stream(engine: *mut TiwiEngine) -> bool {
    if engine.is_null() {
        return false;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let eng = unsafe { &*engine };
        eng.inner.stop_stream()
    }))
    .unwrap_or(false)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tiwi_engine_get_metrics(
    engine: *const TiwiEngine,
    out: *mut TiwiMetrics,
) -> bool {
    if engine.is_null() || out.is_null() {
        return false;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let eng = unsafe { &*engine };
        let dst = unsafe { &mut *out };
        *dst = TiwiMetrics {
            live: false,
            congested: false,
            viewers: 0,
            in_kbps: 0.0,
            out_kbps: 0.0,
            rtt_ms: -1,
            loss_pct: 0.0,
            codec: [0; 16],
        };
        if let Some(snap) = eng.inner.metrics() {
            dst.live = true;
            dst.congested = snap.congested;
            dst.viewers = snap.viewers.min(u32::MAX as usize) as u32;
            dst.in_kbps = snap.in_kbps;
            dst.out_kbps = snap.out_kbps;
            dst.rtt_ms = snap.rtt_ms.map(|v| v as i64).unwrap_or(-1);
            dst.loss_pct = snap.loss_pct;
            let max_len = dst.codec.len() - 1;
            for (slot, &byte) in dst
                .codec
                .iter_mut()
                .zip(snap.codec.as_bytes())
                .take(max_len)
            {
                *slot = byte as c_char;
            }
        }
        true
    }))
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn generate_token_writes_valid_cstr() {
        let mut buf = [0 as c_char; 128];
        assert!(unsafe { tiwi_generate_token(buf.as_mut_ptr(), buf.len()) });
        let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        assert!(s.len() >= 32);
    }

    #[test]
    fn open_log_file_creates_and_prunes() {
        let dir = std::env::temp_dir().join(format!("tiwi-obs-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let (path, _file) = open_log_file(&dir, "2026-01-01T00-00-00Z").unwrap();
        assert!(path.exists());
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("2026-01-01T00-00-00Z.log")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ffi_engine_lifecycle_start_push_stop() {
        let logs_dir =
            std::env::temp_dir().join(format!("tiwi-obs-metrics-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&logs_dir);
        let logs_cstr = CString::new(logs_dir.to_str().unwrap()).unwrap();

        let token = CString::new("test-token-123").unwrap();
        let mqtt = CString::new("ws://127.0.0.1:1/mqtt").unwrap();
        let stun = CString::new("").unwrap();
        let cfg = TiwiConfig {
            token: token.as_ptr(),
            viewer_url: std::ptr::null(),
            mqtt_url: mqtt.as_ptr(),
            stun_server: stun.as_ptr(),
            public_ip: std::ptr::null(),
            logs_dir: logs_cstr.as_ptr(),
            media_port: 0,
            max_peers: 2,
            max_uplink_mbps: 0.0,
        };

        let engine = unsafe { tiwi_engine_create(&cfg) };
        assert!(!engine.is_null());

        let mut url_buf = [0 as c_char; 256];
        assert!(unsafe { tiwi_engine_watch_url(engine, url_buf.as_mut_ptr(), url_buf.len()) });
        let url = unsafe { CStr::from_ptr(url_buf.as_ptr()) }
            .to_str()
            .unwrap();
        assert!(url.ends_with("#test-token-123"));

        assert!(unsafe {
            tiwi_engine_start_stream(engine, TiwiVideoCodec::H264, std::ptr::null(), 0)
        });
        let frame = [0u8, 0, 0, 1, 0x65, 0x88, 0x84];
        assert!(unsafe {
            tiwi_engine_push_frame(engine, true, true, 0, 1, 60, frame.as_ptr(), frame.len())
        });

        let mut metrics = TiwiMetrics {
            live: false,
            congested: false,
            viewers: 0,
            in_kbps: 0.0,
            out_kbps: 0.0,
            rtt_ms: -1,
            loss_pct: 0.0,
            codec: [0; 16],
        };
        assert!(unsafe { tiwi_engine_get_metrics(engine, &mut metrics) });
        assert!(unsafe { tiwi_engine_stop_stream(engine) });
        unsafe { tiwi_engine_destroy(engine) };
        assert!(logs_dir.exists());
        let _ = fs::remove_dir_all(&logs_dir);
    }
}
