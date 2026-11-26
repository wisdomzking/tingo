//! 离线文件拉取状态机（PROTOCOL.md §5.3 / §9）。
//!
//! 借鉴 Soundcore `lib/ble/ble_file_pull.dart`（MIT）的健壮性细节：
//!   - 严格校验 1A/07 文件头 fileId，拒绝"别的文件头"
//!   - 连续序号检查 + 显式 1A/0A 完成通知 + 帧数/字节数匹配
//!   - 分片按 seq*10 AES-CTR 解密，输出裸 Opus 帧
//! 传输层（BLE notify）与本模块解耦：上层只管把收到的包喂给 `feed()`。

use super::crypto::{AudioFileSecretKey, DeviceCrypto};
use super::frame::DecodedPacket;
use super::models::payload_bytes_for_head_size;
use super::{commands::rx, PAYLOAD_BYTES_PER_FRAME, SLICE_UNIT_SIZE};

#[derive(Debug, Clone, PartialEq)]
pub enum PullEvent {
    /// 收到匹配的文件头，携带设备报告大小（transport bytes）
    Head { file_id: u32, head_size: u32 },
    /// 收到数据分片
    Slice { bytes: usize },
    /// 收到完成通知
    Done,
    /// 设备返回了其他文件的头，必须中止（不能删文件）
    MismatchedHead { expected: u32, got: u32 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PullStatus {
    Pending,
    Complete,
    MismatchedHead { expected: u32, got: u32 },
    Error(String),
}

pub struct PullSession {
    file_id: u32,
    expected_bytes: u32,
    received_bytes: u32,
    last_seq: Option<u32>,
    gaps: u32,
    head_seen: bool,
    done_seen: bool,
    mismatched_head: Option<u32>,
    decrypt_on: bool,
    crypto: DeviceCrypto,
    pub opus: Vec<u8>,
}

impl PullSession {
    pub fn new(file_id: u32, expected_bytes_from_list: u32) -> Self {
        Self {
            file_id,
            expected_bytes: expected_bytes_from_list,
            received_bytes: 0,
            last_seq: None,
            gaps: 0,
            head_seen: false,
            done_seen: false,
            mismatched_head: None,
            decrypt_on: false,
            crypto: DeviceCrypto::new(),
            opus: Vec::new(),
        }
    }

    /// 注入会话密钥（来自 2E/01 握手）；必须在喂文件头之前调用。
    pub fn set_crypto(&mut self, crypto: DeviceCrypto) {
        self.crypto = crypto;
    }

    pub fn expected_bytes(&self) -> u32 {
        self.expected_bytes
    }

    pub fn received_bytes(&self) -> u32 {
        self.received_bytes
    }

    pub fn status(&self) -> PullStatus {
        if let Some(got) = self.mismatched_head {
            return PullStatus::MismatchedHead {
                expected: self.file_id,
                got,
            };
        }
        if self.done_seen || (self.expected_bytes > 0 && self.received_bytes >= self.expected_bytes) {
            PullStatus::Complete
        } else {
            PullStatus::Pending
        }
    }

    /// 喂入一个 RX 包，返回本次事件。
    pub fn feed(&mut self, pkt: &DecodedPacket) -> Result<Vec<PullEvent>, String> {
        if !rx::is_transport(pkt.cmd_type) {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        match pkt.cmd_id {
            rx::FILE_HEAD_ID => {
                let Some(secret) = AudioFileSecretKey::parse_frame(&pkt.raw) else {
                    return Ok(events);
                };
                if secret.file_id != self.file_id {
                    self.mismatched_head = Some(secret.file_id);
                    events.push(PullEvent::MismatchedHead {
                        expected: self.file_id,
                        got: secret.file_id,
                    });
                    return Ok(events);
                }
                let file_id_key = self.file_id.to_string();
                // 全零加密材料 = 设备未启用加密，直接按裸 Opus 帧处理。
                if secret.encrypted_file_key.iter().any(|b| *b != 0) {
                    if let Err(e) = self.crypto.init_file_decrypt(
                        &file_id_key,
                        &secret.encrypted_file_key,
                        &secret.session_nonce,
                        &secret.nonce,
                    ) {
                        let hx = |b: &[u8]| -> String { b.iter().map(|v| format!("{v:02X}")).collect() };
                        return Err(format!(
                            "文件头密钥解封失败：{e}；sessionNonce={} nonce={} encKey={}",
                            hx(&secret.session_nonce),
                            hx(&secret.nonce),
                            hx(&secret.encrypted_file_key)
                        ));
                    }
                    self.decrypt_on = true;
                } else {
                    self.decrypt_on = false;
                }
                if secret.file_size > 0 {
                    self.expected_bytes = payload_bytes_for_head_size(secret.file_size);
                }
                self.head_seen = true;
                events.push(PullEvent::Head {
                    file_id: secret.file_id,
                    head_size: secret.file_size,
                });
            }
            rx::FILE_SLICE_ID | rx::FILE_SLICE_MARK_ID => {
                if !self.head_seen {
                    // 还没拿到头不能建立解密器，先缓存也无意义；让上层继续等
                    return Ok(events);
                }
                let (bytes, _) = self.write_slices(&pkt.raw)?;
                self.received_bytes += bytes as u32;
                events.push(PullEvent::Slice { bytes });
            }
            rx::FILE_DONE_ID => {
                self.done_seen = true;
                events.push(PullEvent::Done);
            }
            _ => {}
        }
        Ok(events)
    }

    /// 从 1A/08 帧解析 166B 单元：seq(4) + flags(1) + opus(160) + pad(1)。
    fn write_slices(&mut self, frame: &[u8]) -> Result<(usize, u32), String> {
        let mut i = 9usize;
        let mut written = 0usize;
        while frame.len().saturating_sub(i) >= SLICE_UNIT_SIZE {
            let seq = u32::from_le_bytes([
                frame[i], frame[i + 1], frame[i + 2], frame[i + 3],
            ]);
            i += 4;
            i += 1; // flags
            let data_end = i + PAYLOAD_BYTES_PER_FRAME as usize;
            if data_end > frame.len() {
                break;
            }
            let mut chunk = frame[i..data_end].to_vec();
            i = data_end;
            if i < frame.len() {
                i += 1; // padding
            }
            if let Some(last) = self.last_seq {
                if seq != last.wrapping_add(1) {
                    self.gaps += 1;
                }
            }
            self.last_seq = Some(seq);
            if self.decrypt_on {
                if let Some(plain) = self.crypto.decrypt_chunk(
                    &self.file_id.to_string(),
                    seq,
                    &chunk,
                ) {
                    chunk = plain;
                }
            }
            self.opus.extend_from_slice(&chunk);
            written += chunk.len();
        }
        Ok((written, self.gaps))
    }

    /// 结束并校验完整性；通过则返回裸 Opus 数据与统计。
    pub fn finalize(self) -> Result<PullResult, String> {
        let result = PullResult {
            file_id: self.file_id,
            opus: self.opus,
            expected_bytes: self.expected_bytes,
            received_bytes: self.received_bytes,
            done_seen: self.done_seen,
            gaps: self.gaps,
            frames: self.last_seq.map(|s| s as u64 + 1).unwrap_or(0),
        };
        if result.received_bytes == 0 {
            return Err(format!(
                "文件 {} 没有收到任何音频数据，请保持 BLE 连接后重试",
                self.file_id
            ));
        }
        if result.expected_bytes > 0
            && result.received_bytes * 100 < result.expected_bytes * 95
        {
            return Err(format!(
                "BLE 导出不完整：{}/{} 字节（<95%）",
                result.received_bytes, result.expected_bytes
            ));
        }
        if result.gaps > 0 {
            return Err(format!("分片序号不连续，缺 {} 个包，请重试", result.gaps));
        }
        if !result.done_seen && result.expected_bytes > 0 && result.received_bytes < result.expected_bytes {
            return Err(format!(
                "未收到完成通知且数据不足：{}/{} 字节",
                result.received_bytes, result.expected_bytes
            ));
        }
        Ok(result)
    }
}

#[derive(Debug)]
pub struct PullResult {
    pub file_id: u32,
    pub opus: Vec<u8>,
    pub expected_bytes: u32,
    pub received_bytes: u32,
    pub done_seen: bool,
    pub gaps: u32,
    pub frames: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bean::frame::{checksum, encode};

    fn rx(type_id: u8, id: u8, payload: &[u8]) -> DecodedPacket {
        let mut raw = vec![0x09, 0xFF, 0x00, 0x00, 0x01, type_id, id];
        raw.extend_from_slice(&((10 + payload.len()) as u16).to_le_bytes());
        raw.extend_from_slice(payload);
        raw.push(checksum(&raw));
        DecodedPacket::from_bytes(&raw)
    }

    fn head(file_id: u32, size: u32) -> DecodedPacket {
        let mut payload = vec![0u8; 88];
        payload[0..4].copy_from_slice(&file_id.to_le_bytes());
        payload[4..8].copy_from_slice(&size.to_le_bytes());
        // 裸帧，没有加密材料 → decryptOn=false，直接存原文
        rx(0x1A, 0x07, &payload)
    }

    fn slice(seq: u32, data: &[u8; 160]) -> DecodedPacket {
        let mut payload = Vec::new();
        payload.extend_from_slice(&seq.to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(data);
        payload.push(0);
        rx(0x1A, 0x08, &payload)
    }

    #[test]
    fn pulls_slices_and_finalizes() {
        let mut s = PullSession::new(100, 160 * 2);
        let events = s.feed(&head(100, 2 * SLICE_UNIT_SIZE as u32)).unwrap();
        assert!(matches!(events.first(), Some(PullEvent::Head { .. })));
        let a = [0x11u8; 160];
        let b = [0x22u8; 160];
        s.feed(&slice(0, &a)).unwrap();
        s.feed(&slice(1, &b)).unwrap();
        s.feed(&rx(0x1A, 0x0A, &[])).unwrap();
        let r = s.finalize().unwrap();
        assert_eq!(r.opus.len(), 320);
        assert_eq!(r.frames, 2);
        assert!(r.done_seen);
    }

    #[test]
    fn rejects_mismatched_head() {
        let mut s = PullSession::new(100, 320);
        let events = s.feed(&head(999, 332)).unwrap();
        assert_eq!(
            events.first(),
            Some(&PullEvent::MismatchedHead { expected: 100, got: 999 })
        );
        assert!(s.opus.is_empty());
    }

    #[test]
    fn detects_sequence_gaps() {
        let mut s = PullSession::new(100, 160 * 2);
        s.feed(&head(100, 332)).unwrap();
        let a = [0x11u8; 160];
        s.feed(&slice(0, &a)).unwrap();
        s.feed(&slice(2, &a)).unwrap();
        s.feed(&rx(0x1A, 0x0A, &[])).unwrap();
        assert!(s.finalize().is_err());
    }

    #[test]
    fn build_request_uses_offline_flag() {
        let f = crate::bean::commands::DeviceCommands::start_transport_audio_file(100, 0, 0);
        let mut p = 0u32.to_le_bytes().to_vec();
        p.extend_from_slice(&100u32.to_le_bytes());
        p.push(0);
        assert_eq!(f, encode(0x1A, 0x07, &p));
    }
}
