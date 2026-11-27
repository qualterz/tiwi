use std::sync::Arc;
use std::time::Instant;

use str0m::format::{Codec, FormatParams};

use super::SharedRtp;

/// Safe RTP payload cap well below 1500-byte Ethernet MTU (leaving room for IP/UDP/SRTP/RTP headers).
const MAX_RTP_PAYLOAD: usize = 1200;
const VIDEO_CLOCK_HZ: u32 = 90_000;
const OPUS_CLOCK_HZ: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

#[derive(Debug, Clone)]
pub struct DirectVideoConfig {
    pub codec: VideoCodec,
    /// Optional codec extra-data from the encoder (e.g. Annex-B SPS/PPS for H.264, VPS/SPS/PPS for H.265).
    pub extra_data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub is_video: bool,
    pub is_keyframe: bool,
    pub pts: i64,
    pub timebase_num: u32,
    pub timebase_den: u32,
    pub data: Vec<u8>,
}

pub(crate) struct FramePacketizer {
    codec: VideoCodec,
    h264_sps: Option<Vec<u8>>,
    h264_pps: Option<Vec<u8>>,
    h265_vps: Option<Vec<u8>>,
    h265_sps: Option<Vec<u8>>,
    h265_pps: Option<Vec<u8>>,
}

impl FramePacketizer {
    pub fn new(cfg: &DirectVideoConfig) -> Self {
        let mut p = Self {
            codec: cfg.codec,
            h264_sps: None,
            h264_pps: None,
            h265_vps: None,
            h265_sps: None,
            h265_pps: None,
        };
        if !cfg.extra_data.is_empty() {
            match cfg.codec {
                VideoCodec::H264 => p.scan_h264_params(&cfg.extra_data),
                VideoCodec::H265 => p.scan_h265_params(&cfg.extra_data),
                VideoCodec::Av1 => {}
            }
        }
        p
    }

    pub fn video_spec(&self) -> (Codec, FormatParams) {
        // Use FormatParams::default() so egress enables all standard WebRTC profiles
        // (e.g. H.264 Baseline/Main/High at level 3.1 with level-asymmetry-allowed),
        // letting any browser negotiate cleanly while in-band SPS/PPS conveys the actual profile/level.
        match self.codec {
            VideoCodec::H264 => (Codec::H264, FormatParams::default()),
            VideoCodec::H265 => (Codec::H265, FormatParams::default()),
            VideoCodec::Av1 => (Codec::Av1, FormatParams::default()),
        }
    }

    pub fn packetize(
        &mut self,
        frame: &EncodedFrame,
        wall_time: Instant,
        out: &mut Vec<SharedRtp>,
    ) {
        out.clear();
        if frame.data.is_empty() {
            return;
        }

        if !frame.is_video {
            let ts = pts_to_rtp_timestamp(
                frame.pts,
                frame.timebase_num,
                frame.timebase_den,
                OPUS_CLOCK_HZ,
            );
            out.push(SharedRtp {
                is_video: false,
                timestamp: ts,
                wall_time,
                marker: false,
                payload: Arc::from(frame.data.as_slice()),
            });
            return;
        }

        let ts = pts_to_rtp_timestamp(
            frame.pts,
            frame.timebase_num,
            frame.timebase_den,
            VIDEO_CLOCK_HZ,
        );
        match self.codec {
            VideoCodec::H264 => {
                self.packetize_h264(&frame.data, frame.is_keyframe, ts, wall_time, out)
            }
            VideoCodec::H265 => {
                self.packetize_h265(&frame.data, frame.is_keyframe, ts, wall_time, out)
            }
            VideoCodec::Av1 => packetize_av1(&frame.data, frame.is_keyframe, ts, wall_time, out),
        }

        if let Some(last) = out.last_mut() {
            last.marker = true;
        }
    }

    fn scan_h264_params(&mut self, data: &[u8]) {
        for nal in split_annex_b(data) {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1F {
                7 => self.h264_sps = Some(nal.to_vec()),
                8 => self.h264_pps = Some(nal.to_vec()),
                _ => {}
            }
        }
    }

    fn scan_h265_params(&mut self, data: &[u8]) {
        for nal in split_annex_b(data) {
            if nal.len() < 2 {
                continue;
            }
            match (nal[0] >> 1) & 0x3F {
                32 => self.h265_vps = Some(nal.to_vec()),
                33 => self.h265_sps = Some(nal.to_vec()),
                34 => self.h265_pps = Some(nal.to_vec()),
                _ => {}
            }
        }
    }

    fn packetize_h264(
        &mut self,
        data: &[u8],
        is_keyframe: bool,
        ts: u32,
        wall_time: Instant,
        out: &mut Vec<SharedRtp>,
    ) {
        let mut has_sps = false;
        let mut has_pps = false;
        let mut nals: Vec<&[u8]> = Vec::new();

        for nal in split_annex_b(data) {
            if nal.is_empty() {
                continue;
            }
            let nal_type = nal[0] & 0x1F;
            if nal_type == 9 {
                // Skip Access Unit Delimiter (AUD)
                continue;
            }
            if nal_type == 7 {
                has_sps = true;
                self.h264_sps = Some(nal.to_vec());
            } else if nal_type == 8 {
                has_pps = true;
                self.h264_pps = Some(nal.to_vec());
            }
            nals.push(nal);
        }

        if is_keyframe {
            if !has_sps && let Some(sps) = self.h264_sps.as_deref() {
                emit_h264_nal(sps, ts, wall_time, out);
            }
            if !has_pps && let Some(pps) = self.h264_pps.as_deref() {
                emit_h264_nal(pps, ts, wall_time, out);
            }
        }

        for nal in nals {
            emit_h264_nal(nal, ts, wall_time, out);
        }
    }

    fn packetize_h265(
        &mut self,
        data: &[u8],
        is_keyframe: bool,
        ts: u32,
        wall_time: Instant,
        out: &mut Vec<SharedRtp>,
    ) {
        let mut has_vps = false;
        let mut has_sps = false;
        let mut has_pps = false;
        let mut nals: Vec<&[u8]> = Vec::new();

        for nal in split_annex_b(data) {
            if nal.len() < 2 {
                continue;
            }
            let nal_type = (nal[0] >> 1) & 0x3F;
            if nal_type == 35 {
                // Skip Access Unit Delimiter (AUD)
                continue;
            }
            match nal_type {
                32 => {
                    has_vps = true;
                    self.h265_vps = Some(nal.to_vec());
                }
                33 => {
                    has_sps = true;
                    self.h265_sps = Some(nal.to_vec());
                }
                34 => {
                    has_pps = true;
                    self.h265_pps = Some(nal.to_vec());
                }
                _ => {}
            }
            nals.push(nal);
        }

        if is_keyframe {
            if !has_vps && let Some(vps) = self.h265_vps.as_deref() {
                emit_h265_nal(vps, ts, wall_time, out);
            }
            if !has_sps && let Some(sps) = self.h265_sps.as_deref() {
                emit_h265_nal(sps, ts, wall_time, out);
            }
            if !has_pps && let Some(pps) = self.h265_pps.as_deref() {
                emit_h265_nal(pps, ts, wall_time, out);
            }
        }

        for nal in nals {
            emit_h265_nal(nal, ts, wall_time, out);
        }
    }
}

pub(crate) fn pts_to_rtp_timestamp(pts: i64, num: u32, den: u32, clock_hz: u32) -> u32 {
    if num == 0 || den == 0 {
        return pts as u32;
    }
    let scaled = (pts as i128 * num as i128 * clock_hz as i128) / den as i128;
    scaled as u32
}

/// Splits an Annex-B byte stream (`00 00 01` or `00 00 00 01` start codes) into raw NAL unit slices.
/// If no start code is found, yields the entire slice as a single NAL unit.
fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push((i, i + 3));
                i += 3;
                continue;
            } else if i + 3 < data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                starts.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    if starts.is_empty() {
        return if data.is_empty() {
            Vec::new()
        } else {
            vec![data]
        };
    }

    let mut nals = Vec::with_capacity(starts.len());
    for idx in 0..starts.len() {
        let start = starts[idx].1;
        let end = if idx + 1 < starts.len() {
            starts[idx + 1].0
        } else {
            data.len()
        };
        if start < end {
            nals.push(&data[start..end]);
        }
    }
    nals
}

/// RFC 6184 H.264 Single NAL Unit or FU-A fragmentation.
fn emit_h264_nal(nal: &[u8], ts: u32, wall_time: Instant, out: &mut Vec<SharedRtp>) {
    if nal.len() <= MAX_RTP_PAYLOAD {
        out.push(SharedRtp {
            is_video: true,
            timestamp: ts,
            wall_time,
            marker: false,
            payload: Arc::from(nal),
        });
        return;
    }

    let fu_indicator = (nal[0] & 0x60) | 28;
    let nal_type = nal[0] & 0x1F;
    let body = &nal[1..];
    let max_frag = MAX_RTP_PAYLOAD - 2;
    let total_chunks = body.len().div_ceil(max_frag);

    for (idx, chunk) in body.chunks(max_frag).enumerate() {
        let mut fu_header = nal_type;
        if idx == 0 {
            fu_header |= 0x80; // Start bit
        }
        if idx + 1 == total_chunks {
            fu_header |= 0x40; // End bit
        }
        let mut buf = Vec::with_capacity(2 + chunk.len());
        buf.push(fu_indicator);
        buf.push(fu_header);
        buf.extend_from_slice(chunk);
        out.push(SharedRtp {
            is_video: true,
            timestamp: ts,
            wall_time,
            marker: false,
            payload: Arc::from(buf),
        });
    }
}

/// RFC 7798 H.265/HEVC Single NAL Unit or FU (type 49) fragmentation.
fn emit_h265_nal(nal: &[u8], ts: u32, wall_time: Instant, out: &mut Vec<SharedRtp>) {
    if nal.len() <= MAX_RTP_PAYLOAD {
        out.push(SharedRtp {
            is_video: true,
            timestamp: ts,
            wall_time,
            marker: false,
            payload: Arc::from(nal),
        });
        return;
    }

    let nal_type = (nal[0] >> 1) & 0x3F;
    let hdr0 = (nal[0] & 0x81) | (49 << 1);
    let hdr1 = nal[1];
    let body = &nal[2..];
    let max_frag = MAX_RTP_PAYLOAD - 3;
    let total_chunks = body.len().div_ceil(max_frag);

    for (idx, chunk) in body.chunks(max_frag).enumerate() {
        let mut fu_header = nal_type;
        if idx == 0 {
            fu_header |= 0x80; // Start bit
        }
        if idx + 1 == total_chunks {
            fu_header |= 0x40; // End bit
        }
        let mut buf = Vec::with_capacity(3 + chunk.len());
        buf.push(hdr0);
        buf.push(hdr1);
        buf.push(fu_header);
        buf.extend_from_slice(chunk);
        out.push(SharedRtp {
            is_video: true,
            timestamp: ts,
            wall_time,
            marker: false,
            payload: Arc::from(buf),
        });
    }
}

/// RTP Payload Format for AV1 (OBU stream -> AV1 Aggregation Header + OBU elements).
fn packetize_av1(
    data: &[u8],
    is_keyframe: bool,
    ts: u32,
    wall_time: Instant,
    out: &mut Vec<SharedRtp>,
) {
    let obus = parse_av1_obus(data);
    let mut is_first_packet = true;

    for obu in obus {
        // Skip OBU_TEMPORAL_DELIMITER (type 2) per AV1 RTP specification.
        let obu_type = (obu[0] >> 3) & 0x0F;
        if obu_type == 2 {
            continue;
        }

        // Strip obu_has_size_field bit and embedded LEB128 size so the OBU element matches AV1 RTP framing.
        let stripped = strip_obu_size_field(obu);
        let max_frag = MAX_RTP_PAYLOAD - 1;
        let total_chunks = stripped.len().div_ceil(max_frag).max(1);

        for (idx, chunk) in stripped.chunks(max_frag).enumerate() {
            let z = u8::from(idx > 0);
            let y = u8::from(idx + 1 < total_chunks);
            let w = 1u8; // Single OBU element per packet: no LEB128 size prefix required.
            let n = u8::from(is_first_packet && is_keyframe);
            is_first_packet = false;

            let agg_hdr = (z << 7) | (y << 6) | (w << 4) | (n << 3);
            let mut buf = Vec::with_capacity(1 + chunk.len());
            buf.push(agg_hdr);
            buf.extend_from_slice(chunk);
            out.push(SharedRtp {
                is_video: true,
                timestamp: ts,
                wall_time,
                marker: false,
                payload: Arc::from(buf),
            });
        }
    }
}

fn parse_av1_obus(data: &[u8]) -> Vec<&[u8]> {
    let mut obus = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let start = pos;
        let header = data[pos];
        let has_extension = (header & 0x04) != 0;
        let has_size = (header & 0x02) != 0;
        pos += 1 + usize::from(has_extension);
        if pos > data.len() {
            break;
        }
        if has_size {
            let Some((size, leb_len)) = read_leb128(&data[pos..]) else {
                break;
            };
            pos += leb_len;
            let end = pos.saturating_add(size).min(data.len());
            obus.push(&data[start..end]);
            pos = end;
        } else {
            obus.push(&data[start..]);
            break;
        }
    }
    obus
}

fn strip_obu_size_field(obu: &[u8]) -> Vec<u8> {
    if obu.is_empty() {
        return Vec::new();
    }
    let has_size = (obu[0] & 0x02) != 0;
    if !has_size {
        return obu.to_vec();
    }
    let has_extension = (obu[0] & 0x04) != 0;
    let hdr_len = 1 + usize::from(has_extension);
    if obu.len() <= hdr_len {
        return obu.to_vec();
    }
    let Some((_, leb_len)) = read_leb128(&obu[hdr_len..]) else {
        return obu.to_vec();
    };
    let mut out = Vec::with_capacity(obu.len() - leb_len);
    out.push(obu[0] & !0x02);
    if has_extension {
        out.push(obu[1]);
    }
    out.extend_from_slice(&obu[hdr_len + leb_len..]);
    out
}

fn read_leb128(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut value: usize = 0;
    for (i, &b) in bytes.iter().take(8).enumerate() {
        value |= ((b & 0x7F) as usize) << (i * 7);
        if (b & 0x80) == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pts_scales_to_rtp_clocks() {
        // 1 second at 60fps (pts=60, tb=1/60) -> 90_000
        assert_eq!(pts_to_rtp_timestamp(60, 1, 60, VIDEO_CLOCK_HZ), 90_000);
        // 20ms audio frame at 48kHz (pts=960, tb=1/48000) -> 960
        assert_eq!(pts_to_rtp_timestamp(960, 1, 48_000, OPUS_CLOCK_HZ), 960);
    }

    #[test]
    fn opus_packetizes_as_single_packet() {
        let mut p = FramePacketizer::new(&DirectVideoConfig {
            codec: VideoCodec::H264,
            extra_data: Vec::new(),
        });
        let frame = EncodedFrame {
            is_video: false,
            is_keyframe: true,
            pts: 960,
            timebase_num: 1,
            timebase_den: 48_000,
            data: vec![0xFC, 0x01, 0x02, 0x03],
        };
        let mut out = Vec::new();
        p.packetize(&frame, Instant::now(), &mut out);
        assert_eq!(out.len(), 1);
        assert!(!out[0].is_video);
        assert!(!out[0].marker);
        assert_eq!(out[0].timestamp, 960);
        assert_eq!(&*out[0].payload, &[0xFC, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn h264_injects_sps_pps_on_keyframe() {
        // High profile 3.1 (64 00 1f) SPS + PPS in extra_data
        let extra = vec![
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1F, 0xAC, // SPS
            0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80, // PPS
        ];
        let mut p = FramePacketizer::new(&DirectVideoConfig {
            codec: VideoCodec::H264,
            extra_data: extra,
        });
        let (codec, fmt) = p.video_spec();
        assert_eq!(codec, Codec::H264);
        assert_eq!(fmt, FormatParams::default());

        // Keyframe containing only IDR slice (no inline SPS/PPS)
        let frame = EncodedFrame {
            is_video: true,
            is_keyframe: true,
            pts: 1,
            timebase_num: 1,
            timebase_den: 30,
            data: vec![0, 0, 0, 1, 0x65, 0x88, 0x84],
        };
        let mut out = Vec::new();
        p.packetize(&frame, Instant::now(), &mut out);
        assert_eq!(out.len(), 3); // SPS, PPS, IDR
        assert_eq!(out[0].payload[0] & 0x1F, 7);
        assert_eq!(out[1].payload[0] & 0x1F, 8);
        assert_eq!(out[2].payload[0] & 0x1F, 5);
        assert!(!out[0].marker);
        assert!(!out[1].marker);
        assert!(out[2].marker);
        assert_eq!(out[2].timestamp, 3000);
    }

    #[test]
    fn h264_fragments_large_nal_with_fu_a() {
        let mut p = FramePacketizer::new(&DirectVideoConfig {
            codec: VideoCodec::H264,
            extra_data: Vec::new(),
        });
        let mut data = vec![0, 0, 0, 1, 0x65]; // IDR header
        data.extend(vec![0xAA; 2600]);
        let frame = EncodedFrame {
            is_video: true,
            is_keyframe: true,
            pts: 0,
            timebase_num: 1,
            timebase_den: 60,
            data,
        };
        let mut out = Vec::new();
        p.packetize(&frame, Instant::now(), &mut out);
        assert_eq!(out.len(), 3);
        // FU-A indicator = 0x60 | 28 = 0x7C
        assert_eq!(out[0].payload[0], 0x7C);
        assert_eq!(out[0].payload[1], 0x80 | 5); // Start + IDR
        assert!(!out[0].marker);
        assert_eq!(out[1].payload[1], 5); // Middle
        assert!(!out[1].marker);
        assert_eq!(out[2].payload[1], 0x40 | 5); // End + IDR
        assert!(out[2].marker);
    }

    #[test]
    fn h265_fragments_large_nal_with_fu() {
        let mut p = FramePacketizer::new(&DirectVideoConfig {
            codec: VideoCodec::H265,
            extra_data: Vec::new(),
        });
        // HEVC IDR_W_RADL (type 19 -> hdr0 = 19 << 1 = 0x26, hdr1 = 0x01)
        let mut data = vec![0, 0, 0, 1, 0x26, 0x01];
        data.extend(vec![0xBB; 2500]);
        let frame = EncodedFrame {
            is_video: true,
            is_keyframe: true,
            pts: 0,
            timebase_num: 1,
            timebase_den: 60,
            data,
        };
        let mut out = Vec::new();
        p.packetize(&frame, Instant::now(), &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].payload[0] >> 1) & 0x3F, 49);
        assert_eq!(out[0].payload[2], 0x80 | 19);
        assert_eq!(out[2].payload[2], 0x40 | 19);
        assert!(out[2].marker);
    }

    #[test]
    fn av1_strips_temporal_delimiter_and_sets_new_coded_video_sequence() {
        let mut p = FramePacketizer::new(&DirectVideoConfig {
            codec: VideoCodec::Av1,
            extra_data: Vec::new(),
        });
        // OBU_TEMPORAL_DELIMITER (type 2, has_size=1, size=0) + OBU_SEQUENCE_HEADER (type 1, has_size=1, size=2)
        let data = vec![
            0x12, 0x00, // TD
            0x0A, 0x02, 0x11, 0x22, // Seq header
        ];
        let frame = EncodedFrame {
            is_video: true,
            is_keyframe: true,
            pts: 0,
            timebase_num: 1,
            timebase_den: 60,
            data,
        };
        let mut out = Vec::new();
        p.packetize(&frame, Instant::now(), &mut out);
        assert_eq!(out.len(), 1);
        // Z=0, Y=0, W=1 (0x10), N=1 (0x08) -> 0x18
        assert_eq!(out[0].payload[0], 0x18);
        // obu_has_size_field cleared: 0x0A & !0x02 = 0x08
        assert_eq!(&out[0].payload[1..], &[0x08, 0x11, 0x22]);
        assert!(out[0].marker);
    }
}
