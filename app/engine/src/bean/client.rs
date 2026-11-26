//! 录音豆高层会话：设备信息 / 文件清单 / 离线导出。
//!
//! 借鉴 Soundcore 的会话流程（PROTOCOL.md §9），同时保留其防御性判断：
//! 文件头 fileId 校验、序号连续性、显式完成包、95% 完整性阈值。

use std::path::Path;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::Serialize;

use super::ble::{self, BeanConnection, NotificationStream};
use super::commands::{rx, DeviceCommands};
use super::crypto::DeviceCrypto;
use super::frame::{process_buffer, DecodedPacket};
use super::models::{DeviceInfo, OfflineFileEntry, OfflineFileList};
use super::pull::{PullEvent, PullSession, PullStatus};
use super::raw_opus_to_wav;

const HEAD_TIMEOUT: Duration = Duration::from_secs(15);
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullReport {
    pub file_id: u32,
    pub wav_path: String,
    pub frames: u64,
    pub received_bytes: u32,
    pub expected_bytes: u32,
    pub duration_ms: u64,
    pub deleted_from_device: bool,
}

pub struct BeanClient {
    conn: BeanConnection,
    stream: NotificationStream,
    buffer: Vec<u8>,
    crypto: Option<DeviceCrypto>,
}

impl BeanClient {
    pub async fn connect(target: &str) -> Result<Self, String> {
        let (conn, stream) = ble::connect(target).await?;
        Ok(Self {
            conn,
            stream,
            buffer: Vec::with_capacity(4096),
            crypto: None,
        })
    }

    /// 严格连接：用于绑定，找不到指定设备就失败，不会连到旁边另一台。
    pub async fn connect_strict(target: &str) -> Result<Self, String> {
        let (conn, stream) = ble::connect_strict(target).await?;
        Ok(Self {
            conn,
            stream,
            buffer: Vec::with_capacity(4096),
            crypto: None,
        })
    }

    /// 消费式关闭：BLE 通知流是 `Send + !Sync`，若用 `&self` 跨 await，
    /// 会让 axum handler 的 future 不是 `Send`。取而代之由调用方交出所有权。
    pub async fn close(self) -> Result<(), String> {
        let result = self.conn.disconnect().await;
        // WinRT 释放 GATT 连接是异步的；立刻重连会报 busy 或直接找不到设备。
        tokio::time::sleep(Duration::from_millis(800)).await;
        result
    }

    /// 从 notify 流读一个数据块；超时只是"这一轮没数据"，由调用方决定下一步。
    async fn pump(&mut self, timeout: Duration) -> Result<(), String> {
        let next = tokio::select! {
            n = self.stream.next() => n,
            _ = tokio::time::sleep(timeout) => return Ok(()),
        };
        match next {
            Some(bytes) => self.buffer.extend_from_slice(&bytes),
            None => return Err("BLE 通知流已断开".into()),
        }
        Ok(())
    }

    fn drain(&mut self) -> Vec<DecodedPacket> {
        process_buffer(&mut self.buffer)
    }

    async fn wait_for(
        &mut self,
        cmd_type: u8,
        cmd_id: u8,
        timeout: Duration,
    ) -> Result<DecodedPacket, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pkt) = self
                .drain()
                .into_iter()
                .find(|p| p.cmd_type == cmd_type && p.cmd_id == cmd_id)
            {
                return Ok(pkt);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "等待 0x{cmd_type:02X}/0x{cmd_id:02X} 响应超时"
                ));
            }
            self.pump(remaining).await?;
        }
    }

    async fn request(
        &mut self,
        frame: &[u8],
        cmd_type: u8,
        cmd_id: u8,
        timeout: Duration,
    ) -> Result<DecodedPacket, String> {
        self.conn.write_frame(frame).await?;
        self.wait_for(cmd_type, cmd_id, timeout).await
    }

    /// 读设备信息（01/01）。
    pub async fn device_info(&mut self) -> Result<DeviceInfo, String> {
        let pkt = self
            .request(
                &DeviceCommands::get_device_info(),
                rx::DEVICE_INFO_TYPE,
                rx::DEVICE_INFO_ID,
                Duration::from_secs(10),
            )
            .await?;
        DeviceInfo::parse_frame(&pkt.raw)
            .ok_or_else(|| format!("设备信息帧太短：{}B", pkt.raw.len()))
    }

    /// 同步时间（可选，误差不影响导出；失败可忽略）。
    pub async fn sync_time(&mut self) -> Result<(), String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        let tz = chrono::Local::now().offset().local_minus_utc() / 3600;
        self.request(
            &DeviceCommands::sync_time(now, tz as i8),
            rx::DEVICE_INFO_TYPE,
            rx::SYNC_TIME_ID,
            Duration::from_secs(5),
        )
        .await?;
        Ok(())
    }

    /// 离线文件清单：优先 1B/0E（带结束时间），老固件回退 1A/0E。
    pub async fn list_files(&mut self) -> Result<OfflineFileList, String> {
        match self
            .request(
                &DeviceCommands::list_files_with_end_time(0),
                rx::TRANSPORT_TYPE_ALT,
                rx::FILE_LIST_ID,
                Duration::from_secs(10),
            )
            .await
        {
            Ok(pkt) => Ok(OfflineFileList::parse(&pkt.payload, true)),
            Err(_) => {
                let pkt = self
                    .request(
                        &DeviceCommands::list_files(0),
                        rx::TRANSPORT_TYPE,
                        rx::FILE_LIST_ID,
                        Duration::from_secs(10),
                    )
                    .await?;
                Ok(OfflineFileList::parse(&pkt.payload, false))
            }
        }
    }

    /// ECDH 握手（2E/01），建立会话密钥。导出前自动调用，可重复调用。
    pub async fn handshake(&mut self) -> Result<(), String> {
        let mut crypto = DeviceCrypto::new();
        let app_pub = crypto.ensure_key_pair();
        let pkt = self
            .request(
                &DeviceCommands::notify_encrypt_public_key(&app_pub),
                rx::ENCRYPT_TYPE,
                rx::ENCRYPT_HANDSHAKE_ID,
                Duration::from_secs(15),
            )
            .await?;
        if pkt.raw.len() < 106 {
            return Err(format!("加密握手响应太短：{}B", pkt.raw.len()));
        }
        let dev_pub = &pkt.raw[9..74];
        let dev_shared = &pkt.raw[74..106];
        let crypto = match crypto.complete_handshake(dev_pub, dev_shared) {
            Ok(c) => c,
            Err(e) => {
                let raw_hex: String = pkt.raw.iter().map(|b| format!("{b:02X}")).collect();
                return Err(format!(
                    "加密握手失败：{e}；原始帧({}B)={raw_hex}",
                    pkt.raw.len()
                ));
            }
        };
        self.crypto = Some(crypto);
        Ok(())
    }

    /// 拉取一个离线文件并转成 16 kHz mono wav。
    ///
    /// `delete_after_import`: 仅在 wav 落盘并校验后才发送 1A/10；默认 false。
    pub async fn pull_file(
        &mut self,
        file: &OfflineFileEntry,
        out_wav: &Path,
        delete_after_import: bool,
    ) -> Result<PullReport, String> {
        if self.crypto.is_none() {
            self.handshake().await?;
        }
        let mut session = PullSession::new(file.file_id, file.payload_bytes);
        if let Some(crypto) = self.crypto.take() {
            session.set_crypto(crypto);
        }
        self.conn
            .write_frame(&DeviceCommands::start_transport_audio_file(
                file.file_id,
                0,
                0,
            ))
            .await?;

        let started = Instant::now();
        let mut last_packet = Instant::now();
        let head_deadline = started + HEAD_TIMEOUT;
        loop {
            for pkt in self.drain() {
                last_packet = Instant::now();
                match session.feed(&pkt) {
                    Ok(events) => {
                        for ev in events {
                            if let PullEvent::MismatchedHead { expected, got } = ev {
                                return Err(format!(
                                    "设备返回了其他文件（{got}）的数据，而不是 {expected}。请在录音结束后重试"
                                ));
                            }
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            match session.status() {
                PullStatus::Complete => break,
                PullStatus::MismatchedHead { expected, got } => {
                    return Err(format!("文件头不匹配：期望 {expected}，收到 {got}"));
                }
                PullStatus::Error(e) => return Err(e),
                PullStatus::Pending => {}
            }
            if Instant::now() > head_deadline && session.received_bytes() == 0 {
                return Err("15 秒内没有收到文件头，请确认录音已结束".into());
            }
            if last_packet.elapsed() > INACTIVITY_TIMEOUT {
                return Err(format!(
                    "传输停滞：{} 秒未收到数据，已取消",
                    INACTIVITY_TIMEOUT.as_secs()
                ));
            }
            let remaining = INACTIVITY_TIMEOUT
                .saturating_sub(last_packet.elapsed())
                .min(Duration::from_millis(100));
            self.pump(remaining).await?;
        }

        let result = session.finalize()?;
        let started_at = Instant::now();
        raw_opus_to_wav(&result.opus, out_wav)?;
        let duration_ms = started_at.elapsed().as_millis() as u64;

        let mut deleted = false;
        if delete_after_import {
            // 完整性校验 + wav 落盘通过后才允许删除，且删除后复查清单。
            self.request(
                &DeviceCommands::delete_file(file.file_id),
                rx::TRANSPORT_TYPE,
                rx::DELETE_ID,
                Duration::from_secs(10),
            )
            .await?;
            let after = self.list_files().await?;
            if after.files.iter().any(|f| f.file_id == file.file_id) {
                return Err(format!("文件 {} 删除后仍在清单中", file.file_id));
            }
            deleted = true;
        }

        Ok(PullReport {
            file_id: file.file_id,
            wav_path: out_wav.to_string_lossy().to_string(),
            frames: result.frames,
            received_bytes: result.received_bytes,
            expected_bytes: result.expected_bytes,
            duration_ms,
            deleted_from_device: deleted,
        })
    }
}
