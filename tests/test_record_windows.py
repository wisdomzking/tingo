"""record.py 的 Windows 分流逻辑测试：平台判断、命令构造、进程管理、会议软件检测。"""

import subprocess
import sys
from pathlib import Path

import pytest

from tingo import record


@pytest.fixture
def as_windows(monkeypatch):
    monkeypatch.setattr(record.platform, "system", lambda: "Windows")


@pytest.fixture
def fake_shutil(monkeypatch):
    monkeypatch.setattr(record.shutil, "which", lambda name: r"C:\fake\ffmpeg.exe" if name == "ffmpeg" else None)


def test_system_audio_supported_windows(fake_shutil, as_windows, monkeypatch):
    from tingo import windows_audio

    monkeypatch.setattr(windows_audio, "system_audio_supported", lambda: True)
    assert record.system_audio_supported() is True
    monkeypatch.setattr(windows_audio, "system_audio_supported", lambda: False)
    assert record.system_audio_supported() is False


def test_system_audio_supported_windows_no_ffmpeg(as_windows, monkeypatch):
    monkeypatch.setattr(record.shutil, "which", lambda name: None)
    assert record.system_audio_supported() is False


def test_system_audio_supported_macos_unchanged(monkeypatch):
    monkeypatch.setattr(record.platform, "system", lambda: "Darwin")
    monkeypatch.setattr(record.platform, "mac_ver", lambda: ("15.0", (), ""))
    monkeypatch.setattr(record.shutil, "which", lambda name: "/usr/bin/swiftc" if name == "swiftc" else None)
    assert record.system_audio_supported() is True
    monkeypatch.setattr(record.platform, "mac_ver", lambda: ("14.0", (), ""))
    assert record.system_audio_supported() is False


def test_recording_start_windows_spawns_dshow_processes(tmp_path, fake_shutil, as_windows, monkeypatch):
    from tingo import windows_audio

    monkeypatch.setattr(windows_audio, "system_audio_supported", lambda: True)
    monkeypatch.setattr(windows_audio, "pick_system_device", lambda: "立体声混音 (Realtek)")
    monkeypatch.setattr(windows_audio, "pick_mic_device", lambda: "麦克风阵列 (Realtek)")

    spawned = []

    class FakeProc:
        def __init__(self, cmd, **kw):
            self.cmd = cmd
            self.stdin = type("S", (), {"write": lambda s, x: None, "flush": lambda s: None})()
            self.stderr = None
            self._rc = None

        def poll(self):
            return self._rc

        def communicate(self, data, timeout=None):
            self._rc = 0

        def wait(self, timeout=None):
            return 0

        def kill(self):
            self._rc = -1

    def fake_popen(cmd, **kw):
        p = FakeProc(cmd, **kw)
        spawned.append(p)
        return p

    monkeypatch.setattr(record.subprocess, "Popen", fake_popen)
    monkeypatch.setattr(record.time, "sleep", lambda s: None)

    rec = record.Recording(tmp_path, system_audio=True, mic=True)
    rec.start()
    assert rec.sources == ["system", "mic"]
    assert len(rec._procs) == 2
    system_cmd = " ".join(spawned[0].cmd)
    mic_cmd = " ".join(spawned[1].cmd)
    assert "立体声混音" in system_cmd and "system.wav" in system_cmd
    assert "麦克风阵列" in mic_cmd and "mic.wav" in mic_cmd
    rec.stop_capture()
    assert rec._procs == []


def test_recording_start_windows_no_system_device(tmp_path, fake_shutil, as_windows, monkeypatch):
    from tingo import windows_audio

    monkeypatch.setattr(windows_audio, "system_audio_supported", lambda: True)
    monkeypatch.setattr(windows_audio, "pick_system_device", lambda: None)
    rec = record.Recording(tmp_path, system_audio=True, mic=False)
    with pytest.raises(RuntimeError, match="系统声音回环设备"):
        rec.start()


def test_exited_detects_dead_process(tmp_path, monkeypatch):
    rec = record.Recording(tmp_path, system_audio=False, mic=False)

    class DeadProc:
        def poll(self):
            return 1

        @property
        def stderr(self):
            import io

            return io.BytesIO(b"ERROR: device busy\n")

        @property
        def stdout(self):
            return None

    rec._procs = [DeadProc()]
    monkeypatch.setattr(record.platform, "system", lambda: "Windows")
    assert rec.exited() == "device busy"


def test_running_meeting_apps_windows(monkeypatch):
    monkeypatch.setattr(record.platform, "system", lambda: "Windows")
    tasklist = '"TencentMeeting.exe","1234","Console"\n"WeMeetApp.exe","5678","Console"\n"chrome.exe","9","Console"\n'

    def fake_run(cmd, **kw):
        assert cmd == ["tasklist", "/fo", "csv", "/nh"]
        return subprocess.CompletedProcess(cmd, 0, stdout=tasklist)

    monkeypatch.setattr(record.subprocess, "run", fake_run)
    found = record.running_meeting_apps()
    assert "tencentmeeting" in found
    assert "wemeetapp" in found
    assert "chrome" not in found
