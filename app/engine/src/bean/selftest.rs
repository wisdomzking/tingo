//! 无真机离线自检：用合成的 ECDH 会话 + 加密分片走完整协议链，
//! 验证帧、握手、文件头、分片解密、完整性校验和 Ogg 封装都正确。

use serde_json::{json, Value};

use super::crypto::{aes_ctr, DeviceCrypto, FILE_KEY_MAGIC};
use super::frame::{checksum, DecodedPacket};
use super::ogg;
use super::pull::{PullSession, PullStatus};
use super::{PAYLOAD_BYTES_PER_FRAME, SLICE_UNIT_SIZE};

fn rx_frame(cmd_type: u8, cmd_id: u8, payload: &[u8]) -> DecodedPacket {
    let mut raw = vec![0x09, 0xFF, 0x00, 0x00, 0x01, cmd_type, cmd_id];
    raw.extend_from_slice(&((10 + payload.len()) as u16).to_le_bytes());
    raw.extend_from_slice(payload);
    raw.push(checksum(&raw));
    DecodedPacket::from_bytes(&raw)
}

pub fn run_selftest() -> Result<Value, String> {
    let frames = 12u32;
    let file_id = 1_760_000_000u32;

    // 1) 双向 ECDH + HKDF
    let mut app = DeviceCrypto::new();
    let mut device = DeviceCrypto::new();
    let app_pub = app.ensure_key_pair();
    let dev_pub = device.ensure_key_pair();
    let dev_shared = device.debug_shared_for(&app_pub)?;
    app = app
        .complete_handshake(&dev_pub, &dev_shared)
        .map_err(|e| format!("握手失败：{e}"))?;
    let session_key = app.session_key.ok_or("没有会话密钥")?;

    // 2) 合成加密文件头
    let session_nonce: Vec<u8> = (3..19).map(|i| i as u8).collect();
    let nonce: Vec<u8> = (20..36).map(|i| i as u8).collect();
    let file_key: Vec<u8> = (50..82).map(|i| i as u8).collect();
    let mut plain_key = FILE_KEY_MAGIC.to_vec();
    plain_key.extend_from_slice(&file_key);
    let enc_key = aes_ctr(&plain_key, &session_key, &session_nonce)?;

    let head_size = frames * SLICE_UNIT_SIZE as u32;
    let mut head_payload = vec![0u8; 88];
    head_payload[0..4].copy_from_slice(&file_id.to_le_bytes());
    head_payload[4..8].copy_from_slice(&head_size.to_le_bytes());
    head_payload[8..24].copy_from_slice(&nonce);
    head_payload[24..70].copy_from_slice(&enc_key);
    head_payload[70..86].copy_from_slice(&session_nonce);
    let head = rx_frame(0x1A, 0x07, &head_payload);

    // 3) 合成加密分片（结构与设备一致）
    let expected_payload: Vec<Vec<u8>> = (0..frames)
        .map(|i| {
            (0..PAYLOAD_BYTES_PER_FRAME)
                .map(|j| ((j as u32 * 7 + i * 13) & 0xFF) as u8)
                .collect()
        })
        .collect();
    let mut slices = Vec::new();
    for (seq, plain) in expected_payload.iter().enumerate() {
        let iv = {
            let mut iv = [0u8; 16];
            iv[..12].copy_from_slice(&nonce[..12]);
            iv[12..].copy_from_slice(&((seq as u32) * 10).to_be_bytes());
            iv
        };
        let cipher = aes_ctr(plain, &file_key, &iv)?;
        let mut payload = Vec::with_capacity(SLICE_UNIT_SIZE);
        payload.extend_from_slice(&(seq as u32).to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&cipher);
        payload.push(0);
        slices.push(rx_frame(0x1A, 0x08, &payload));
    }
    let done = rx_frame(0x1A, 0x0A, &[]);

    // 4) 走完整拉取状态机
    let mut session = PullSession::new(file_id, frames * PAYLOAD_BYTES_PER_FRAME as u32);
    session.set_crypto(app);
    session.feed(&head)?;
    for pkt in &slices {
        session.feed(pkt)?;
    }
    session.feed(&done)?;
    if session.status() != PullStatus::Complete {
        return Err(format!("拉取状态未Complete：{:?}", session.status()));
    }
    let result = session.finalize()?;
    let mut restored = Vec::new();
    for p in &expected_payload {
        restored.extend_from_slice(p);
    }
    if result.opus != restored {
        return Err("解密后的 Opus 数据与原始数据不一致".into());
    }

    // 5) Ogg 封装
    let ogg = ogg::mux_raw_frames(&result.opus)?;
    if !ogg::is_ogg(&ogg) {
        return Err("Ogg 封装失败".into());
    }

    Ok(json!({
        "ok": true,
        "frames": result.frames,
        "payloadBytes": result.received_bytes,
        "oggBytes": ogg.len(),
        "ecdh": "P-256 OK",
        "hkdf": "SHA-256 OK",
        "aesCtr": "128-bit BE counter OK",
        "note": "协议链离线自检通过；真机 BLE 连通性需连接录音豆验证"
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn offline_protocol_chain() {
        let report = super::run_selftest().unwrap();
        assert_eq!(report["ok"], true);
        assert_eq!(report["frames"], 12);
    }
}
