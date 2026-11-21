"""Windows 平台的音频采集：用 ffmpeg 的 dshow 录麦克风和系统声音（WASAPI loopback）。

设计原则：对上层 record.py 保持与 macOS ``recorder.swift`` 一致的契约——
    * 向输出目录写 ``system.wav`` / ``mic.wav``（48 kHz 单声道 16 位 PCM）
    * stdout 先打印一行 ``READY``，结束时打印 ``ERROR: …``（若有错）
    * 收到 SIGINT（本模块转成 stdin 的 ``q``）后正常收尾、写好 wav 文件头

这样 mixdown / duck_mix / GrowingWav 全部零改动，跨平台只在这一层分流。

系统声音在 Windows 上靠 dshow 的「立体声混音 / Stereo Mix」或虚拟线缆
（VB-CABLE、Voicemeeter）回环采集，本质就是 WASAPI 的 loopback 设备。设备名
因机器而异，按关键词自动挑选，也可用环境变量 TINGO_SYSTEM_DEVICE /
TINGO_MIC_DEVICE 精确指定；enumerate_devices() 可列出全部候选。
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from dataclasses import dataclass

# ffmpeg 设备枚举行里的名字："麦克风 (xxx)" (audio) —— 取第一个引号里的内容
_DEVICE_RE = re.compile(r'"([^"]+)"\s*\((audio|video)\)')

# 系统声音（WASAPI loopback / 虚拟线缆）设备名关键词：命中任意一个即认为是回环设备
SYSTEM_KEYWORDS = (
    "立体声混音",
    "stereo mix",
    "mixage stéréo",
    "what u hear",
    "wave out mix",
    "cable output",  # VB-AUDIO Virtual Cable 的回环输出
    "voicemeeter",
    "virtual cable",
    "loopback",
    "环回",
    "回环",
)
# 这些是"虚拟线缆的输入端/其它"，不是麦克风，录制时要排除
MIC_EXCLUDE_KEYWORDS = (
    "cable input",
    "voicemeeter input",
    "stereo mix",
    "立体声混音",
    "loopback",
    "环回",
    "回环",
)
MIC_KEYWORDS = ("麦克风", "microphone", "mic ", "mikrofon", "micrófono")


@dataclass(frozen=True)
class AudioDevice:
    name: str
    kind: str  # audio | video

    @property
    def is_audio(self) -> bool:
        return self.kind == "audio"


def _denoise(s: str) -> str:
    """统一大小写与空白，方便关键词匹配。"""
    return " ".join(s.lower().split())


def enumerate_devices(ffmpeg: str = "ffmpeg", timeout: float = 10) -> list[AudioDevice]:
    """调用 ffmpeg -list_devices 列出 dshow 设备。解析失败返回空列表（不抛异常）。"""
    try:
        proc = subprocess.run(
            [ffmpeg, "-hide_banner", "-f", "dshow", "-list_devices", "true", "-i", "dummy"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    text = (proc.stderr or "") + "\n" + (proc.stdout or "")
    found: list[AudioDevice] = []
    for name, kind in _DEVICE_RE.findall(text):
        dev = AudioDevice(name=name, kind=kind)
        if dev not in found:
            found.append(dev)
    return found


def _match(name: str, keywords: tuple[str, ...]) -> bool:
    low = _denoise(name)
    return any(_denoise(k) in low for k in keywords)


def is_system_device(name: str) -> bool:
    return _match(name, SYSTEM_KEYWORDS)


def pick_system_device(devices: list[AudioDevice] | None = None) -> str | None:
    """挑一个系统声音回环设备；优先用户用 TINGO_SYSTEM_DEVICE 指定的。"""
    override = os.environ.get("TINGO_SYSTEM_DEVICE", "").strip()
    if override:
        return override
    devices = enumerate_devices() if devices is None else devices
    loopbacks = [d.name for d in devices if d.is_audio and is_system_device(d.name)]
    # VB-CABLE / Stereo Mix 都行；优先 Stereo Mix（不依赖额外装驱动）
    loopbacks.sort(key=lambda n: 0 if _match(n, ("立体声混音", "stereo mix", "what u hear")) else 1)
    return loopbacks[0] if loopbacks else None


def pick_mic_device(devices: list[AudioDevice] | None = None) -> str | None:
    """挑麦克风；排除虚拟线缆输入端和回环设备。"""
    override = os.environ.get("TINGO_MIC_DEVICE", "").strip()
    if override:
        return override
    devices = enumerate_devices() if devices is None else devices
    mics = [
        d.name
        for d in devices
        if d.is_audio and not is_system_device(d.name) and not _match(d.name, MIC_EXCLUDE_KEYWORDS)
    ]
    named = [m for m in mics if _match(m, MIC_KEYWORDS)]
    candidates = named or mics
    return candidates[0] if candidates else None


def system_audio_supported(ffmpeg: str = "ffmpeg") -> bool:
    """Windows 上是否有可用的系统声音回环设备。（mic 是否可用由 ffmpeg/设备决定，这里只看系统声音）"""
    if sys.platform != "win32":
        return False
    return pick_system_device() is not None


def build_dshow_cmd(device: str, out_wav: str, ffmpeg: str = "ffmpeg") -> list[str]:
    """构造一路 dshow 采集命令：48 kHz 单声道 16 位 PCM wav，与 macOS 录音组件同格式。"""
    return [
        ffmpeg,
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "dshow",
        "-i",
        f"audio={device}",
        "-ac",
        "1",
        "-ar",
        "48000",
        "-c:a",
        "pcm_s16le",
        out_wav,
    ]
