//! 平台差异集中处：把 macOS 专属的系统调用映射到 Windows / Linux 等价实现。
//! 录音组件本身（tingo-recorder）已跨平台，这里只处理 engine 直接调用的系统工具。

use std::path::Path;
use std::process::{Command, Stdio};

pub const IS_WINDOWS: bool = cfg!(target_os = "windows");
pub const IS_MACOS: bool = cfg!(target_os = "macos");

/// Windows 的 `canonicalize()` 会返回 `\\?\D:\...` 这种扩展长度路径，直接显示很难看，
/// 个别 Win32 API 也不认。去掉这个前缀，保留正常绝对路径。其它平台原样返回。
pub fn clean_path(path: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            // UNC 路径 \\?\UNC\server\share 要还原成 \\server\share
            if let Some(unc) = rest.strip_prefix("UNC\\") {
                return std::path::PathBuf::from(format!(r"\\{}", unc));
            }
            return std::path::PathBuf::from(rest);
        }
    }
    path
}

/// 用户主目录。Windows 默认没有 HOME，必须读 USERPROFILE（或 HOMEDRIVE+HOMEPATH），
/// 否则会得到相对路径（如 `Documents\Tingo`），导致存储目录、配置路径全部错乱。
pub fn home_dir() -> Option<std::path::PathBuf> {
    if IS_WINDOWS {
        if let Some(p) = std::env::var_os("USERPROFILE") {
            if !p.is_empty() { return Some(std::path::PathBuf::from(p)); }
        }
        if let (Some(drive), Some(path)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
            return Some(std::path::PathBuf::from(drive).join(path));
        }
        return None;
    }
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// 统一的子进程构造器：Windows 下强制 CREATE_NO_WINDOW，
/// 避免 GUI 程序调用 ffmpeg / recorder / tasklist / PowerShell 时弹出黑色控制台窗口。
/// 其它平台退化为普通 Command::new。
pub fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// 找 ffmpeg：优先 TINGO_FFMPEG 环境变量，再 exe 同目录，最后 PATH。
pub fn ffmpeg() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("TINGO_FFMPEG") {
        let p = std::path::PathBuf::from(p);
        if p.is_file() {
            return p;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["ffmpeg.exe", "ffmpeg"] {
                let p = dir.join(name);
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    std::path::PathBuf::from("ffmpeg")
}

/// 用系统默认程序打开文件 / 文件夹 / URI。
/// macOS: open <path>；Windows: explorer <path>（URI 交给 cmd start）；
/// Linux: xdg-open <path>。
pub fn open_path(target: &Path) -> std::io::Result<std::process::Child> {
    if IS_WINDOWS {
        // explorer 能开文件夹和文件；URL 用 cmd /c start 更稳
        let s = target.to_string_lossy();
        if s.starts_with("http://") || s.starts_with("https://") || s.contains("://") {
            return command("cmd")
                .args(["/C", "start", "", &s])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        return command("explorer").arg(target).spawn();
    }
    if IS_MACOS {
        return command("/usr/bin/open").arg(target).spawn();
    }
    command("xdg-open").arg(target).spawn()
}

/// 在系统文件管理器里显示并选中某个文件。
/// macOS: open -R <path>；Windows: explorer /select,<path>；Linux: xdg-open 所在目录。
pub fn reveal_path(path: &Path) -> std::io::Result<std::process::Child> {
    if IS_WINDOWS {
        let arg = format!("/select,{}", path.to_string_lossy());
        return command("explorer").arg(arg).spawn();
    }
    if IS_MACOS {
        return command("/usr/bin/open").arg("-R").arg(path).spawn();
    }
    let parent = path.parent().unwrap_or(path);
    command("xdg-open").arg(parent).spawn()
}

/// 正在运行的会议软件进程名集合（小写）。
/// macOS: ps -axo comm=；Windows: tasklist /fo csv；Linux: ps -e -o comm=。
pub fn running_process_names() -> std::collections::BTreeSet<String> {
    let output = if IS_WINDOWS {
        command("tasklist").args(["/fo", "csv", "/nh"]).output()
    } else if IS_MACOS {
        command("/bin/ps").args(["-axo", "comm="]).output()
    } else {
        command("ps").args(["-e", "-o", "comm="]).output()
    };
    let Ok(output) = output else { return Default::default() };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut names = std::collections::BTreeSet::new();
    for line in text.lines() {
        let raw = if IS_WINDOWS {
            // "WeChat.exe","1234",... → 取第一段，去掉引号
            line.split(',').next().unwrap_or("").trim().trim_matches('"')
        } else {
            line.trim().rsplit('/').next().unwrap_or("").trim()
        };
        // 去 .exe 后缀（Windows 进程名），统一小写
        let name = raw.strip_suffix(".exe").unwrap_or(raw).to_lowercase();
        if !name.is_empty() {
            names.insert(name);
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tasklist_csv() {
        let csv = "\"WeChat.exe\",\"1234\",\"Console\",\"1\",\"10,000 K\"\n\"TencentMeeting.exe\",\"5678\",\"Console\",\"1\",\"20,000 K\"\n";
        let names: Vec<String> = csv.lines().map(|line| {
            line.split(',').next().unwrap_or("").trim().trim_matches('"')
                .strip_suffix(".exe").unwrap_or(line.split(',').next().unwrap_or("").trim().trim_matches('"'))
                .to_lowercase()
        }).collect();
        assert_eq!(names, vec!["wechat", "tencentmeeting"]);
    }
}
