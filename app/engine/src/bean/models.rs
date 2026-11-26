//! 设备信息 / 文件清单 / 扫描结果数据模型（PROTOCOL.md §6）。
//! 移植自 Soundcore `lib/protocol/models.dart`（MIT），字段与解析偏移一致。

use serde::Serialize;

use super::frame::{read_u16_le, read_u32_le};

pub const FRAME_DURATION_MS: u32 = 20;
pub const LIST_UNITS_PER_FRAME: u32 = 20;
pub const TRANSPORT_BYTES_PER_FRAME: u32 = 166;
pub const PAYLOAD_BYTES_PER_FRAME: u32 = 160;

pub fn payload_bytes_for_head_size(head_size: u32) -> u32 {
    head_size / TRANSPORT_BYTES_PER_FRAME * PAYLOAD_BYTES_PER_FRAME
}

/// 电池档位 0..9 → 10%..100%；大于 9 的值按字面百分比（兼容新固件）。
pub fn battery_remaining(raw: u8) -> u8 {
    let v = if raw > 100 { raw & 0x7F } else { raw };
    if v <= 9 {
        (v + 1) * 10
    } else {
        v.min(100)
    }
}

fn ascii(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn format_mac(b: &[u8]) -> String {
    if b.len() < 6 {
        return String::new();
    }
    if b[..6].iter().all(|x| *x == b[0]) {
        return String::new();
    }
    b[..6]
        .iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// 从广播厂商数据里读录音豆的真实 MAC（PROTOCOL.md §2）。
///
/// 厂商数据 = company id (u16) + payload，payload 前 6 字节是真实 MAC，
/// 第 7 字节 bit7 表示是否已绑定账号。Windows/macOS 给 BLE 设备分配的
/// 系统地址是随机私有地址（RPA），会轮换；真实 MAC 才是稳定标识。
/// 返回 (MAC 字符串, 是否已绑定)；数据不足或全零时返回 (None, false)。
pub fn mac_from_manufacturer(data: &std::collections::HashMap<u16, Vec<u8>>) -> (Option<String>, bool) {
    for payload in data.values() {
        if payload.len() >= 7 {
            let mac = format_mac(payload);
            if !mac.is_empty() {
                return (Some(mac), (payload[6] & 0x80) != 0);
            }
        }
    }
    (None, false)
}

fn format_storage_kb(v: u32) -> String {
    if v == 0 {
        return "0 MB".into();
    }
    if v >= 1024 * 1024 {
        compact(v as f64 / (1024.0 * 1024.0)) + " GB"
    } else if v >= 1024 {
        compact(v as f64 / 1024.0) + " MB"
    } else {
        format!("{v} KB")
    }
}

fn compact(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").unwrap_or(&s).to_string()
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub connect_status: Option<u8>,
    /// 麦克风豆电量百分比
    pub battery: u8,
    pub charging: bool,
    pub firmware_version: String,
    pub serial_number: String,
    pub total_memory_kb: u32,
    pub free_memory_kb: u32,
    pub box_battery: Option<u8>,
    pub box_charging: bool,
    pub box_mac: String,
    pub box_firmware: String,
    pub wifi_firmware: String,
    pub recording: bool,
    pub storage_label: String,
}

impl DeviceInfo {
    /// 从完整 RX 帧解析（offset 9 起，对齐 DeviceInfoDispatch.onParseDeviceInfo）。
    pub fn parse_frame(raw: &[u8]) -> Option<Self> {
        if raw.len() < 20 {
            return None;
        }
        let mut o = 9usize;
        let u8_at = |o: &mut usize| -> Option<u8> {
            if *o >= raw.len() {
                return None;
            }
            let v = raw[*o];
            *o += 1;
            Some(v)
        };
        let take = |o: &mut usize, n: usize| -> &[u8] {
            let end = (*o + n).min(raw.len());
            let s = &raw[*o..end];
            *o = end;
            s
        };

        let connect_status = u8_at(&mut o)?;
        let battery_raw = u8_at(&mut o)?;
        let charging_status = u8_at(&mut o)?;
        let fw = ascii(take(&mut o, 5));
        let sn = ascii(take(&mut o, 16)).to_lowercase();
        if o + 8 > raw.len() {
            return None;
        }
        let total_mem = read_u32_le(raw, o);
        o += 4;
        let free_mem = read_u32_le(raw, o);
        o += 4;
        let box_charging_flag = u8_at(&mut o).unwrap_or(0) == 1;
        let box_fw = ascii(take(&mut o, 5));
        let box_battery_raw = u8_at(&mut o);
        let box_mac = format_mac(take(&mut o, 6));

        let mut recording = false;
        let mut wifi_fw = String::new();
        if o + 6 <= raw.len() {
            let _color = u8_at(&mut o);
            let _auto_off = u8_at(&mut o);
            let _auto_off_index = u8_at(&mut o);
            let _pickup_light = u8_at(&mut o);
            let _box_light = u8_at(&mut o);
            if o < raw.len() {
                recording = u8_at(&mut o) == Some(1);
            }
            if o + 5 <= raw.len() {
                wifi_fw = ascii(take(&mut o, 5));
            }
        }

        let used = total_mem.saturating_sub(free_mem.min(total_mem));
        Some(Self {
            connect_status: Some(connect_status),
            battery: battery_remaining(battery_raw),
            charging: charging_status == 1,
            firmware_version: fw,
            serial_number: sn,
            total_memory_kb: total_mem,
            free_memory_kb: free_mem,
            box_battery: box_battery_raw.map(battery_remaining),
            box_charging: box_charging_flag,
            box_mac,
            box_firmware: box_fw,
            wifi_firmware: wifi_fw,
            recording,
            storage_label: format!(
                "{} / {}",
                format_storage_kb(used),
                format_storage_kb(total_mem)
            ),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineFileEntry {
    pub file_id: u32,
    /// 设备列表里的 size 是 20 单位/帧（≈ms），不是字节数。
    pub size_units: u32,
    pub end_time: Option<u32>,
    pub frame_count: u32,
    pub duration_ms: u32,
    pub payload_bytes: u32,
    pub title: String,
}

impl OfflineFileEntry {
    pub fn new(file_id: u32, size_units: u32, end_time: Option<u32>) -> Self {
        let frame_count = if size_units > 0 {
            size_units / LIST_UNITS_PER_FRAME
        } else {
            0
        };
        Self {
            file_id,
            size_units,
            end_time,
            frame_count,
            duration_ms: frame_count * FRAME_DURATION_MS,
            payload_bytes: frame_count * PAYLOAD_BYTES_PER_FRAME,
            title: title_from_file_id(file_id),
        }
    }
}

fn title_from_file_id(file_id: u32) -> String {
    // fileId 通常是 UNIX 秒；用 UTC 日期做标题，避免引入 chrono 之外的依赖。
    let secs = file_id as i64;
    if !(1_000_000_000..20_000_000_000).contains(&secs) {
        return format!("文件 {file_id}");
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Howard Hinnant 的 civil_from_days 算法（UTC）。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    if m <= 2 {
        y += 1;
    }
    (y, m, d)
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineFileList {
    pub files: Vec<OfflineFileEntry>,
    pub file_count: u16,
    pub current_transport_timestamp: Option<u32>,
    pub current_transport_duration: Option<u32>,
}

impl OfflineFileList {
    /// payload 从帧 offset 9 开始；`1B/0E` 每条 12B，`1A/0E` 每条 8B。
    pub fn parse(payload: &[u8], with_end_time: bool) -> Self {
        if payload.len() < 2 {
            return Self::default();
        }
        let file_count = read_u16_le(payload, 0);
        let entry_size = if with_end_time { 12 } else { 8 };
        let mut o = 2usize;
        let mut files = Vec::new();
        for _ in 0..file_count {
            if o + entry_size > payload.len() {
                break;
            }
            let file_id = read_u32_le(payload, o);
            o += 4;
            let end_time = if with_end_time {
                let t = read_u32_le(payload, o);
                o += 4;
                Some(t)
            } else {
                None
            };
            let size = read_u32_le(payload, o);
            o += 4;
            if size > 0 {
                files.push(OfflineFileEntry::new(file_id, size, end_time));
            }
        }
        let current_transport_timestamp = if o + 8 <= payload.len() {
            Some(read_u32_le(payload, o))
        } else {
            None
        };
        let current_transport_duration = if o + 8 <= payload.len() {
            Some(read_u32_le(payload, o + 4))
        } else {
            None
        };
        Self {
            files,
            file_count,
            current_transport_timestamp,
            current_transport_duration,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedDevice {
    pub id: String,
    pub name: String,
    pub rssi: i16,
    pub adv_name: String,
    pub product_code: String,
    pub service_uuid: String,
    pub mac_address: String,
    pub is_bound_advertised: bool,
    pub is_d3200: bool,
}

impl ScannedDevice {
    pub fn display_name(&self) -> &str {
        if !self.name.is_empty() {
            &self.name
        } else if !self.adv_name.is_empty() {
            &self.adv_name
        } else {
            &self.id
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_bucket_encoding() {
        assert_eq!(battery_remaining(0), 10);
        assert_eq!(battery_remaining(2), 30);
        assert_eq!(battery_remaining(3), 40);
        assert_eq!(battery_remaining(9), 100);
        assert_eq!(battery_remaining(100), 100);
        assert_eq!(battery_remaining(50), 50);
    }

    #[test]
    fn file_list_with_end_time() {
        let payload = vec![
            1, 0, // count
            100, 0, 0, 0, // fileId
            200, 0, 0, 0, // endTime
            44, 1, 0, 0, // size 300
            144, 1, 0, 0, // transport ts 400
            244, 1, 0, 0, // transport duration 500
        ];
        let list = OfflineFileList::parse(&payload, true);
        assert_eq!(list.file_count, 1);
        assert_eq!(list.files[0].file_id, 100);
        assert_eq!(list.files[0].end_time, Some(200));
        assert_eq!(list.files[0].size_units, 300);
        assert_eq!(list.files[0].frame_count, 15);
        assert_eq!(list.files[0].payload_bytes, 2400);
        assert_eq!(list.current_transport_timestamp, Some(400));
        assert_eq!(list.current_transport_duration, Some(500));
    }

    #[test]
    fn head_size_converts_to_payload_bytes() {
        assert_eq!(payload_bytes_for_head_size(99_102), 95_520);
    }
}
