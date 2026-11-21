"""Windows dshow 录音后端的单元测试：设备解析 / 挑选 / 命令构造，不依赖真实硬件。"""

import sys

import pytest

from tingo import windows_audio as wa


@pytest.fixture
def devices():
    return [
        wa.AudioDevice("麦克风阵列 (适用于数字麦克风的英特尔® 智音技术)", "audio"),
        wa.AudioDevice("立体声混音 (Realtek(R) Audio)", "audio"),
        wa.AudioDevice("CABLE Output (VB-Audio Virtual Cable)", "audio"),
        wa.AudioDevice("FHD Camera", "video"),
    ]


def test_enumerate_devices_parses_ffmpeg_output(monkeypatch):
    stderr = """\
[in#0 @ 0x1] "FHD Camera" (video)
[in#0 @ 0x1]   Alternative name "@device_pnp_xxx"
[in#0 @ 0x1] "麦克风阵列 (Realtek)" (audio)
[in#0 @ 0x1] "立体声混音 (Realtek(R) Audio)" (audio)
"""
    fake = type("P", (), {"stderr": stderr, "stdout": ""})()
    monkeypatch.setattr(wa.subprocess, "run", lambda *a, **k: fake)
    found = wa.enumerate_devices()
    assert [d.name for d in found] == ["FHD Camera", "麦克风阵列 (Realtek)", "立体声混音 (Realtek(R) Audio)"]
    assert [d.kind for d in found] == ["video", "audio", "audio"]


def test_enumerate_devices_handles_ffmpeg_missing(monkeypatch):
    def boom(*a, **k):
        raise OSError("no ffmpeg")

    monkeypatch.setattr(wa.subprocess, "run", boom)
    assert wa.enumerate_devices() == []


def test_is_system_device():
    assert wa.is_system_device("立体声混音 (Realtek(R) Audio)")
    assert wa.is_system_device("Stereo Mix (Realtek)")
    assert wa.is_system_device("CABLE Output (VB-Audio Virtual Cable)")
    assert wa.is_system_device("Voicemeeter AUX Input")
    assert not wa.is_system_device("麦克风阵列 (Realtek)")


def test_pick_system_device_prefers_stereo_mix(devices, monkeypatch):
    monkeypatch.delenv("TINGO_SYSTEM_DEVICE", raising=False)
    picked = wa.pick_system_device(devices)
    # 优先立体声混音，而不是 VB-CABLE
    assert picked == "立体声混音 (Realtek(R) Audio)"


def test_pick_system_device_env_override(devices, monkeypatch):
    monkeypatch.setenv("TINGO_SYSTEM_DEVICE", "自定义回环")
    assert wa.pick_system_device(devices) == "自定义回环"


def test_pick_mic_device_excludes_loopback(devices, monkeypatch):
    monkeypatch.delenv("TINGO_MIC_DEVICE", raising=False)
    picked = wa.pick_mic_device(devices)
    assert picked == "麦克风阵列 (适用于数字麦克风的英特尔® 智音技术)"
    assert "立体声混音" not in picked and "CABLE" not in picked


def test_pick_mic_device_env_override(devices, monkeypatch):
    monkeypatch.setenv("TINGO_MIC_DEVICE", "我的麦")
    assert wa.pick_mic_device(devices) == "我的麦"


def test_pick_returns_none_when_no_match(monkeypatch):
    monkeypatch.delenv("TINGO_SYSTEM_DEVICE", raising=False)
    monkeypatch.delenv("TINGO_MIC_DEVICE", raising=False)
    assert wa.pick_system_device([]) is None
    assert wa.pick_mic_device([]) is None


def test_system_audio_supported_requires_win32(monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.setattr(wa, "pick_system_device", lambda: "立体声混音")
    assert wa.system_audio_supported() is False
    monkeypatch.setattr(sys, "platform", "win32")
    assert wa.system_audio_supported() is True


def test_build_dshow_cmd():
    cmd = wa.build_dshow_cmd("麦克风 (X)", r"C:\tmp\mic.wav")
    assert cmd[0] == "ffmpeg"
    assert "audio=麦克风 (X)" in cmd
    assert "C:\\tmp\\mic.wav" in cmd
    # 48 kHz 单声道 16 位 PCM，与 macOS 录音组件一致
    assert cmd[cmd.index("-ar") + 1] == "48000"
    assert cmd[cmd.index("-ac") + 1] == "1"
    assert cmd[cmd.index("-c:a") + 1] == "pcm_s16le"
