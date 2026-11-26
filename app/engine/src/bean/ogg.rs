//! 把 D3200 解密后的裸 Opus 帧封装成可播放 / 可被 ffmpeg 解码的 Ogg Opus。
//! 移植自 Soundcore `lib/audio/ogg_opus.dart`（MIT）。

pub const FRAME_SIZE: usize = 160;
pub const SAMPLE_RATE: u32 = 48_000;
pub const SAMPLES_PER_FRAME: u64 = 960; // 20 ms @ 48 kHz
const SERIAL: u32 = 0x4152_4B52; // 'ARKR'

pub fn is_ogg(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[..4] == b"OggS"
}

pub fn is_wav(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

fn trim_trailing_zeros(f: &[u8]) -> &[u8] {
    let mut n = f.len();
    while n > 1 && f[n - 1] == 0 {
        n -= 1;
    }
    &f[..n]
}

fn opus_head(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut b = b"OpusHead".to_vec();
    b.push(1); // version
    b.push(channels);
    b.extend_from_slice(&3840u16.to_le_bytes()); // pre-skip
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // output gain
    b.push(0); // channel mapping family
    b
}

fn opus_tags() -> Vec<u8> {
    let vendor = b"TingoRecorder";
    let mut b = b"OpusTags".to_vec();
    b.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    b.extend_from_slice(vendor);
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

/// Ogg CRC-32：多项式 0x04C11DB7，非反射，初值 0。
fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for byte in data {
        crc ^= (*byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn page(
    packets: &[&[u8]],
    serial: u32,
    sequence: u32,
    granule: u64,
    bos: bool,
    eos: bool,
) -> Vec<u8> {
    let mut segs = Vec::new();
    let mut body = Vec::new();
    for pkt in packets {
        body.extend_from_slice(pkt);
        let mut n = pkt.len();
        while n >= 255 {
            segs.push(255);
            n -= 255;
        }
        segs.push(n as u8);
    }
    let mut header_type = 0u8;
    if bos {
        header_type |= 0x02;
    }
    if eos {
        header_type |= 0x04;
    }

    let mut out = Vec::with_capacity(27 + segs.len() + body.len());
    out.extend_from_slice(b"OggS");
    out.push(0); // version
    out.push(header_type);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // CRC 占位
    out.push(segs.len() as u8);
    out.extend_from_slice(&segs);
    out.extend_from_slice(&body);

    let crc = ogg_crc(&out);
    out[22..26].copy_from_slice(&crc.to_le_bytes());
    out
}

/// 裸 160B 帧序列 → Ogg Opus 字节流。
pub fn mux_raw_frames(raw: &[u8]) -> Result<Vec<u8>, String> {
    if raw.is_empty() {
        return Err("无音频数据".into());
    }
    let mut frames: Vec<&[u8]> = Vec::new();
    if raw.len() >= FRAME_SIZE {
        let mut o = 0;
        while o + FRAME_SIZE <= raw.len() {
            frames.push(&raw[o..o + FRAME_SIZE]);
            o += FRAME_SIZE;
        }
        if o < raw.len() {
            frames.push(&raw[o..]);
        }
    } else {
        frames.push(raw);
    }

    let mut out = Vec::with_capacity(raw.len() + 512);
    let mut seq = 0u32;
    let head = opus_head(1, SAMPLE_RATE);
    let tags = opus_tags();
    out.extend_from_slice(&page(&[&head], SERIAL, seq, 0, true, false));
    seq += 1;
    out.extend_from_slice(&page(&[&tags], SERIAL, seq, 0, false, false));
    seq += 1;

    let per_page = 50;
    let mut granule = 0u64;
    let mut i = 0;
    while i < frames.len() {
        let end = (i + per_page).min(frames.len());
        let packets: Vec<&[u8]> = frames[i..end].iter().map(|f| trim_trailing_zeros(f)).collect();
        granule += SAMPLES_PER_FRAME * packets.len() as u64;
        out.extend_from_slice(&page(&packets, SERIAL, seq, granule, false, end == frames.len()));
        seq += 1;
        i = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_raw_frames_produces_ogg_pages() {
        let mut raw = vec![0u8; 10 * FRAME_SIZE];
        for i in 0..10 {
            let o = i * FRAME_SIZE;
            raw[o] = 0xBC;
            for j in 1..FRAME_SIZE {
                raw[o + j] = (j * 17 + i) as u8;
            }
        }
        let ogg = mux_raw_frames(&raw).unwrap();
        assert!(is_ogg(&ogg));
        assert!(ogg.len() > 200);
        // 首页 CRC 自校验：只取首页字节（27 字节固定头 + 1 个分段 + OpusHead）
        let first_len = 27 + 1 + opus_head(1, SAMPLE_RATE).len();
        let mut probe = ogg[..first_len].to_vec();
        let crc = u32::from_le_bytes(probe[22..26].try_into().unwrap());
        probe[22..26].fill(0);
        assert_eq!(ogg_crc(&probe), crc);
    }

    #[test]
    fn empty_input_rejected() {
        assert!(mux_raw_frames(&[]).is_err());
    }
}
