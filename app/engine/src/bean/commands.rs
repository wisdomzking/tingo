//! D3200 命令字构造器（PROTOCOL.md §5）。
//! 移植自 Soundcore `lib/protocol/commands.dart`（MIT）。

use super::frame::{encode, u16_le, u32_le};

/// 已知 RX type/id 常量。
pub mod rx {
    pub const DEVICE_INFO_TYPE: u8 = 0x01;
    pub const DEVICE_INFO_ID: u8 = 0x01;
    pub const BATTERY_ID: u8 = 0x03;
    pub const CHARGING_ID: u8 = 0x04;
    pub const SYNC_TIME_ID: u8 = 0xA6;
    pub const RESET_ID: u8 = 0xB8;

    pub const AUDIO_TYPE: u8 = 0x18;
    pub const AUDIO_CONTROL_ID: u8 = 0x82;

    pub const TRANSPORT_TYPE: u8 = 0x1A;
    pub const TRANSPORT_TYPE_ALT: u8 = 0x1B;
    pub const FILE_HEAD_ID: u8 = 0x07;
    pub const FILE_SLICE_ID: u8 = 0x08;
    pub const FILE_SLICE_MARK_ID: u8 = 0x12;
    pub const FILE_DONE_ID: u8 = 0x0A;
    pub const FILE_LIST_ID: u8 = 0x0E;
    pub const DELETE_ID: u8 = 0x10;
    pub const BT_STATUS_ID: u8 = 0x0F;
    pub const WIFI_CONFIG_ID: u8 = 0x05;
    pub const WIFI_CLOSE_ID: u8 = 0x02;
    pub const RECORD_STATUS_ID: u8 = 0x06;

    pub const BIND_TYPE: u8 = 0x0B;
    pub const BIND_RESULT_ID: u8 = 0x87;

    pub const ENCRYPT_TYPE: u8 = 0x2E;
    pub const ENCRYPT_HANDSHAKE_ID: u8 = 0x01;

    pub fn is_transport(cmd_type: u8) -> bool {
        cmd_type == TRANSPORT_TYPE || cmd_type == TRANSPORT_TYPE_ALT
    }
}

pub struct DeviceCommands;

impl DeviceCommands {
    pub fn get_device_info() -> Vec<u8> {
        encode(0x01, 0x01, &[])
    }

    pub fn reset_device() -> Vec<u8> {
        encode(0x01, 0xB8, &[])
    }

    /// UTC unix 秒 + 有符号时区小时。
    pub fn sync_time(unix_seconds: u32, timezone_hours: i8) -> Vec<u8> {
        let mut payload = u32_le(unix_seconds);
        payload.push(timezone_hours as u8);
        encode(0x01, 0xA6, &payload)
    }

    pub fn start_record() -> Vec<u8> {
        encode(0x18, 0x82, &[0x01])
    }

    pub fn pause_record() -> Vec<u8> {
        encode(0x18, 0x82, &[0x02])
    }

    pub fn list_files(page: u16) -> Vec<u8> {
        encode(0x1A, 0x0E, &u16_le(page))
    }

    /// 推荐：带结束时间的离线文件列表（Windows 实测用的就是这一路）。
    pub fn list_files_with_end_time(page: u16) -> Vec<u8> {
        encode(0x1B, 0x0E, &u16_le(page))
    }

    pub fn delete_file(file_id: u32) -> Vec<u8> {
        encode(0x1A, 0x10, &u32_le(file_id))
    }

    pub fn start_bt_transport(is_ios: bool) -> Vec<u8> {
        encode(0x1A, 0x0F, &[if is_ios { 1 } else { 0 }])
    }

    pub fn stop_bt_transport(is_ios: bool) -> Vec<u8> {
        encode(0x1A, 0x11, &[if is_ios { 1 } else { 0 }])
    }

    /// 拉取一个离线文件：offset(4) + fileId(4) + realtimeFlag(1)。
    /// 离线导出固定 `already_transferred=0, realtime_flag=0`。
    pub fn start_transport_audio_file(
        file_id: u32,
        already_transferred: u32,
        realtime_flag: u8,
    ) -> Vec<u8> {
        let mut payload = u32_le(already_transferred);
        payload.extend_from_slice(&u32_le(file_id));
        payload.push(realtime_flag);
        encode(0x1A, 0x07, &payload)
    }

    pub fn send_wifi_config(ssid: &str, password: &str) -> Vec<u8> {
        let ssid = ssid.as_bytes();
        let pwd = password.as_bytes();
        let mut payload = Vec::with_capacity(ssid.len() + pwd.len() + 2);
        payload.push((ssid.len() & 0xFF) as u8);
        payload.extend_from_slice(ssid);
        payload.push((pwd.len() & 0xFF) as u8);
        payload.extend_from_slice(pwd);
        encode(0x1A, 0x05, &payload)
    }

    pub fn close_wifi_mode() -> Vec<u8> {
        encode(0x1A, 0x02, &[])
    }

    pub fn notify_encrypt_public_key(public_key: &[u8]) -> Vec<u8> {
        encode(0x2E, 0x01, public_key)
    }

    pub fn bind() -> Vec<u8> {
        encode(0x0B, 0x87, &[0x01])
    }

    pub fn unbind() -> Vec<u8> {
        encode(0x0B, 0x87, &[0x00])
    }

    pub fn set_find_my(enable: bool) -> Vec<u8> {
        encode(0x10, 0xA2, &[if enable { 1 } else { 0 }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bean::frame::{read_u16_le, verify_checksum};

    #[test]
    fn wifi_config_length_prefixed_payload() {
        let f = DeviceCommands::send_wifi_config("WiFi-1", "123");
        assert_eq!(f[5], 0x1A);
        assert_eq!(f[6], 0x05);
        assert!(verify_checksum(&f));
        assert_eq!(f[9], 6);
        assert_eq!(&f[10..16], b"WiFi-1");
        assert_eq!(f[16], 3);
        assert_eq!(&f[17..20], b"123");
    }

    #[test]
    fn list_files_with_end_time_page() {
        let f = DeviceCommands::list_files_with_end_time(2);
        assert_eq!(f[5], 0x1B);
        assert_eq!(f[6], 0x0E);
        assert_eq!(&f[9..11], &[2, 0]);
    }

    #[test]
    fn bind_unbind_stock_payload() {
        let bind = DeviceCommands::bind();
        let unbind = DeviceCommands::unbind();
        assert_eq!(bind[5], 0x0B);
        assert_eq!(bind[6], 0x87);
        assert_eq!(bind[9], 0x01);
        assert_eq!(unbind[9], 0x00);
        assert_eq!(unbind.len(), 11);
    }

    #[test]
    fn transport_request_layout() {
        let f = DeviceCommands::start_transport_audio_file(0x1234_5678, 0, 0);
        assert_eq!(f[5], 0x1A);
        assert_eq!(f[6], 0x07);
        assert_eq!(read_u16_le(&f, 7) as usize, f.len());
        assert_eq!(&f[9..13], &[0, 0, 0, 0]);
        assert_eq!(&f[13..17], &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(f[17], 0);
    }
}
