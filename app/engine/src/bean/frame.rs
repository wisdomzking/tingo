//! D3200 二进制帧（PROTOCOL.md §4）。
//!
//! 1:1 移植自 tacshi/Soundcore `lib/protocol/frame.dart`（MIT），
//! 保持 TX/RX magic、累加校验和、粘包重组与状态字节判定完全一致。

pub const TX_MAGIC: [u8; 5] = [0x08, 0xEE, 0x00, 0x00, 0x00];
pub const RX_MAGIC: [u8; 4] = [0x09, 0xFF, 0x00, 0x00];

/// 累加校验和：所有字节之和取低 8 位。
pub fn checksum(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |sum, b| sum.wrapping_add(*b))
}

pub fn verify_checksum(packet: &[u8]) -> bool {
    if packet.len() < 2 {
        return false;
    }
    checksum(&packet[..packet.len() - 1]) == packet[packet.len() - 1]
}

/// 构造手机 → 设备帧。
/// `08 EE 00 00 00 | type | id | len_lo | len_hi | payload | checksum`
pub fn encode(cmd_type: u8, cmd_id: u8, payload: &[u8]) -> Vec<u8> {
    // magic(5) + type(1) + id(1) + len(2) + payload + csum(1)
    let body_len = 5 + 2 + 2 + payload.len() + 1;
    let mut out = Vec::with_capacity(body_len);
    out.extend_from_slice(&TX_MAGIC);
    out.push(cmd_type);
    out.push(cmd_id);
    out.push((body_len & 0xFF) as u8);
    out.push(((body_len >> 8) & 0xFF) as u8);
    out.extend_from_slice(payload);
    let csum = checksum(&out);
    out.push(csum);
    out
}

pub fn is_rx_header(data: &[u8], offset: usize) -> bool {
    if data.len() < offset + 4 {
        return false;
    }
    data[offset..offset + 4] == RX_MAGIC
}

/// 从流式缓冲区取出全部完整 RX 包，并压缩掉已消费的字节。
pub fn process_buffer(buffer: &mut Vec<u8>) -> Vec<DecodedPacket> {
    let mut results = Vec::new();
    let mut i = 0usize;
    while i + 10 <= buffer.len() {
        let mut found = None;
        for j in i..=buffer.len() - 10 {
            if is_rx_header(buffer, j) {
                found = Some(j);
                break;
            }
        }
        let Some(start) = found else { break };
        i = start;
        if i + 9 > buffer.len() {
            break;
        }
        let total_len = read_u16_le(buffer, i + 7) as usize;
        if total_len < 10 {
            i += 1;
            continue;
        }
        if i + total_len > buffer.len() {
            break; // 半包，等下次 notify
        }
        let slice = buffer[i..i + total_len].to_vec();
        if verify_checksum(&slice) {
            results.push(DecodedPacket::from_bytes(&slice));
        }
        i += total_len;
    }
    if i > 0 && i <= buffer.len() {
        buffer.drain(..i);
    }
    results
}

#[derive(Debug, Clone)]
pub struct DecodedPacket {
    pub raw: Vec<u8>,
    pub status_byte: u8,
    pub cmd_type: u8,
    pub cmd_id: u8,
    pub payload: Vec<u8>,
}

impl DecodedPacket {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let payload = if bytes.len() > 10 {
            bytes[9..bytes.len() - 1].to_vec()
        } else {
            Vec::new()
        };
        Self {
            raw: bytes.to_vec(),
            status_byte: bytes[4],
            cmd_type: bytes[5],
            cmd_id: bytes[6],
            payload,
        }
    }

    pub fn success_flag(&self) -> u8 {
        self.status_byte & 0x0F
    }

    pub fn high_nibble(&self) -> u8 {
        (self.status_byte & 0xF0) >> 4
    }

    /// 固件 03.56 会回单独 FF，但状态字节仍显示成功——必须靠 payload 判拒绝。
    pub fn is_rejected(&self) -> bool {
        self.payload.len() == 1 && self.payload[0] == 0xFF
    }

    pub fn is_success(&self) -> bool {
        !self.is_rejected() && (self.success_flag() == 1 || self.status_byte == 1)
    }
}

pub fn read_u16_le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

pub fn read_u32_le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn u16_le(v: u16) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

pub fn u32_le(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bean::commands::DeviceCommands;

    #[test]
    fn get_device_info_has_magic_checksum_and_length() {
        let f = DeviceCommands::get_device_info();
        assert_eq!(&f[..5], &TX_MAGIC);
        assert_eq!(f[5], 0x01);
        assert_eq!(f[6], 0x01);
        assert!(verify_checksum(&f));
        assert_eq!(read_u16_le(&f, 7) as usize, f.len());
    }

    #[test]
    fn start_record_payload() {
        let f = DeviceCommands::start_record();
        assert_eq!(f[5], 0x18);
        assert_eq!(f[6], 0x82);
        assert_eq!(f[9], 0x01);
        assert!(verify_checksum(&f));
    }

    #[test]
    fn process_buffer_parses_rx_packet() {
        let mut body = vec![0x09, 0xFF, 0x00, 0x00, 0x01, 0x01, 0x01, 0x0A, 0x00];
        let csum = checksum(&body);
        body.push(csum);
        let mut buf = body.clone();
        let out = process_buffer(&mut buf);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cmd_type, 0x01);
        assert_eq!(out[0].cmd_id, 0x01);
        assert!(out[0].is_success());
        assert!(buf.is_empty());
    }

    #[test]
    fn process_buffer_handles_sticky_and_partial_packets() {
        let mut one = vec![0x09, 0xFF, 0x00, 0x00, 0x01, 0x1A, 0x08, 0x0B, 0x00, 0x42];
        one.push(checksum(&one));
        let mut two = vec![0x09, 0xFF, 0x00, 0x00, 0x01, 0x1A, 0x0A, 0x0A, 0x00];
        two.push(checksum(&two));
        let mut buf = one.clone();
        buf.extend_from_slice(&two[..6]); // 第二帧只到一半
        let out = process_buffer(&mut buf);
        assert_eq!(out.len(), 1);
        assert_eq!(buf.len(), 6);
        buf.extend_from_slice(&two[6..]);
        let out = process_buffer(&mut buf);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].cmd_id, 0x0A);
    }

    #[test]
    fn rejected_payload_ff_is_not_success() {
        let mut body = vec![0x09, 0xFF, 0x00, 0x00, 0x01, 0x1A, 0x07, 0x0B, 0x00, 0xFF];
        body.push(checksum(&body));
        let pkt = DecodedPacket::from_bytes(&body);
        assert!(pkt.is_success() == false);
        assert!(pkt.is_rejected());
    }
}
