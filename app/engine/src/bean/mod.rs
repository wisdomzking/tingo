//! 录音豆（Anker soundcore Work / D3200）接入层。
//!
//! 协议、加密、文件拉取、Opus 封装都在这里完成，输出 16 kHz mono wav 后
//! 直接复用 tingo 现有的 ASR → 转写 → 纪要链路。
//!
//! 协议来自 tacshi/Soundcore（MIT，仅用于自有硬件互操作）。

pub mod ble;
pub mod client;
pub mod commands;
pub mod crypto;
pub mod frame;
pub mod models;
pub mod ogg;
pub mod pull;
pub mod selftest;

/// 1A/08 传输单元：seq(4) + flags(1) + opus(160) + pad(1)
pub const SLICE_UNIT_SIZE: usize = 166;
pub const PAYLOAD_BYTES_PER_FRAME: usize = 160;

use std::path::Path;
use std::process::Stdio;

/// 把解密后的裸 Opus 帧写成 tingo 期望的 16 kHz mono 16bit wav。
/// 先封装成 Ogg Opus，再调用项目已有的 ffmpeg（无黑框）。
pub fn raw_opus_to_wav(raw_opus: &[u8], wav_path: &Path) -> Result<(), String> {
    let ogg = ogg::mux_raw_frames(raw_opus)?;
    let parent = wav_path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("创建输出目录失败：{e}"))?;
    let stem = wav_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "bean".into());
    let ogg_path = parent.join(format!("{stem}.opus.ogg"));
    std::fs::write(&ogg_path, &ogg)
        .map_err(|e| format!("写入 Ogg 失败：{e}"))?;

    let ff = crate::platform::ffmpeg();
    let out = crate::platform::command(&ff)
        .args([
            "-hide_banner", "-loglevel", "error", "-y",
            "-i", &ogg_path.to_string_lossy(),
            "-vn", "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le",
        ])
        .arg(wav_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("启动 ffmpeg 失败：{e}"))?;
    if out.status.success() && wav_path.is_file() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let detail = err.lines().last().unwrap_or("解码失败").trim();
    Err(format!("Opus 解码失败：{detail}"))
}
