//! tingo-recorder（Windows 版）
//!
//! 对齐 macOS Swift recorder 的命令行契约，让 Rust engine 无需改动录音主链路：
//!
//! - 录音：  tingo-recorder <输出目录> [--no-mic] [--no-system]
//!           输出 <目录>/system.wav、<目录>/mic.wav（48 kHz 单声道 16 bit PCM）。
//!           stdout 先打印 READY；从 stdin 读到 "q" 或 EOF 后收尾，打印 STOPPED。
//!           失败打印 ERROR: … 并以非 0 退出。
//! - 解码：  tingo-recorder decode <输入> <输出.wav>
//!           ffmpeg 解码成 16 kHz 单声道 16 bit wav；成功打印 OK。
//! - PDF：   tingo-recorder pdf <输入.html> <输出.pdf>
//!           Edge 无头模式排版成 A4 PDF；成功打印 OK。
//! - 回收站：tingo-recorder trash <路径>
//!           PowerShell 调到回收站；成功打印 OK。
//! - 选目录：tingo-recorder choose-directory [起始路径]
//!           打印 JSON {"path": "..."} 或 {"path": null}。
//! - 系统语音：tingo-recorder speech-status|speech-install|speech-transcribe …
//!           Windows 无 Apple Speech，返回不可用信息。
//!
//! 设计：刻意只用标准库 + 系统已装工具（ffmpeg / msedge / PowerShell），不引入 wasapi
//! 等重型 crate。ffmpeg 的 dshow 回环采集在 Windows 上等价于 WASAPI loopback，且已在本机
//! 实测「立体声混音 / VB-CABLE」可用。

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SAMPLE_RATE: u32 = 48_000;

/// 统一子进程构造器：Windows 下强制 CREATE_NO_WINDOW，避免录音组件调用
/// ffmpeg / PowerShell 时弹出黑色控制台窗口（GUI 主程序无控制台时，
/// console 子进程会各自新建 conhost 窗口）。其它平台退化为 Command::new。
fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn fail(msg: &str) -> ! {
    println!("ERROR: {}", msg);
    let _ = io::stdout().flush();
    std::process::exit(1);
}

fn ok() -> ! {
    println!("OK");
    let _ = io::stdout().flush();
    std::process::exit(0);
}

/// 找 ffmpeg：优先环境变量 TINGO_FFMPEG，其次 exe 同目录，最后 PATH。
fn ffmpeg() -> PathBuf {
    if let Ok(p) = env::var("TINGO_FFMPEG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return p;
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["ffmpeg.exe", "ffmpeg"] {
                let p = dir.join(name);
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    PathBuf::from("ffmpeg")
}

/// 枚举 dshow 音频设备名（解析 ffmpeg -list_devices 的 stderr）。
fn list_audio_devices(ff: &Path) -> Vec<String> {
    let out = command(ff)
        .args(["-hide_banner", "-f", "dshow", "-list_devices", "true", "-i", "dummy"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stderr);
    let mut devices = Vec::new();
    for line in text.lines() {
        if !line.contains("(audio)") {
            continue;
        }
        // 形如：[dshow @ 0000] "立体声混音 (Realtek(R) Audio)" (audio)
        if let (Some(a), Some(b)) = (line.find('"'), line.rfind('"')) {
            if b > a {
                let name = line[a + 1..b].trim();
                if !name.is_empty() {
                    devices.push(name.to_string());
                }
            }
        }
    }
    devices
}

fn looks_like_loopback(name: &str) -> bool {
    let n = name.to_lowercase();
    ["立体声混音", "stereo mix", "vb-cable", "cable output", "virtual audio", "loopback", "what u hear"]
        .iter()
        .any(|k| n.contains(k))
}

fn looks_like_mic(name: &str) -> bool {
    let n = name.to_lowercase();
    if looks_like_loopback(&n) {
        return false;
    }
    ["microphone", "麦克风", "mic", "输入", "input"].iter().any(|k| n.contains(k))
}

/// 选回环设备（录系统声音）。环境变量 TINGO_SYSTEM_DEVICE 可强制指定。
fn pick_system_device(devices: &[String]) -> Option<String> {
    if let Ok(p) = env::var("TINGO_SYSTEM_DEVICE") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    // 优先「立体声混音 / Stereo Mix」：声卡内置，无需用户手动路由播放设备；
    // VB-CABLE 等虚拟线缆次之（需要用户把会议软件的音频输出指向它）。
    let stereo_mix = |d: &String| {
        let n = d.to_lowercase();
        n.contains("立体声混音") || n.contains("stereo mix") || n.contains("what u hear")
    };
    devices.iter().find(|d| stereo_mix(d))
        .or_else(|| devices.iter().find(|d| looks_like_loopback(d)))
        .cloned()
}

/// 选麦克风。环境变量 TINGO_MIC_DEVICE 可强制指定；没匹配到就用第一个非回环设备。
fn pick_mic_device(devices: &[String]) -> Option<String> {
    if let Ok(p) = env::var("TINGO_MIC_DEVICE") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    devices.iter().find(|d| looks_like_mic(d))
        .or_else(|| devices.iter().find(|d| !looks_like_loopback(d)))
        .cloned()
}

/// 起一路 dshow 录音进程，写 48 kHz 单声道 16 bit wav。
fn spawn_capture(ff: &Path, device: &str, out: &Path) -> Result<Child, String> {
    let input = format!("audio={}", device);
    let sr = SAMPLE_RATE.to_string();
    command(ff)
        .args([
            "-hide_banner", "-loglevel", "error", "-y",
            "-f", "dshow", "-i", &input,
            "-ac", "1", "-ar", &sr, "-c:a", "pcm_s16le",
        ])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动录音失败（{}）：{}", device, e))
}

/// 让 ffmpeg 正常收尾写完 wav 头：往 stdin 写 q，等它退出。
fn stop_capture(child: &mut Child) {
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// 录音主流程。
fn cmd_record(args: &[String]) {
    let Some(out_dir) = args.first() else {
        fail("用法：tingo-recorder <输出目录> [--no-mic] [--no-system]");
    };
    let with_mic = !args.iter().any(|a| a == "--no-mic");
    let with_system = !args.iter().any(|a| a == "--no-system");
    if !with_mic && !with_system {
        fail("系统声音和麦克风至少开一个");
    }
    if let Err(e) = fs::create_dir_all(out_dir) {
        fail(&format!("创建输出目录失败：{}", e));
    }

    let ff = ffmpeg();
    let devices = list_audio_devices(&ff);
    let mut children: Vec<(String, Child)> = Vec::new();

    if with_system {
        let dev = pick_system_device(&devices)
            .unwrap_or_else(|| fail("找不到系统声音回环设备（需启用「立体声混音」或安装 VB-CABLE）"));
        let path = Path::new(out_dir).join("system.wav");
        match spawn_capture(&ff, &dev, &path) {
            Ok(c) => children.push(("system".into(), c)),
            Err(e) => fail(&e),
        }
    }
    if with_mic {
        let dev = pick_mic_device(&devices).unwrap_or_else(|| fail("找不到麦克风设备"));
        let path = Path::new(out_dir).join("mic.wav");
        match spawn_capture(&ff, &dev, &path) {
            Ok(c) => children.push(("mic".into(), c)),
            Err(e) => {
                // 起了系统声音又起麦克风失败：先把系统声音收掉再报
                for (_, c) in children.iter_mut() { stop_capture(c); }
                fail(&e);
            }
        }
    }

    println!("READY");
    let _ = io::stdout().flush();

    // 从 stdin 读停止命令（engine 写 "q"）；EOF 也视为停止。
    let stdin = io::stdin();
    let mut line = String::new();
    let mut record_error: Option<String> = None;
    'watch: loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break 'watch, // EOF
            Ok(_) => {
                let t = line.trim();
                if t == "q" || t == "quit" || t == "STOP" || t == "stop" {
                    break 'watch;
                }
            }
            Err(_) => break 'watch,
        }
        // 任一录音进程提前退出 → 记录错误后跳出（用索引避免 iter_mut 借用冲突）
        for i in 0..children.len() {
            if let Ok(Some(code)) = children[i].1.try_wait() {
                if code.success() {
                    continue; // ffmpeg 被外部 q 停掉也算正常收尾
                }
                let err = children[i].1.stderr.as_mut()
                    .and_then(|mut s| {
                        let mut b = Vec::new();
                        let _ = std::io::Read::read_to_end(&mut s, &mut b);
                        String::from_utf8(b).ok()
                    })
                    .unwrap_or_default();
                let detail = err.lines().last().unwrap_or("").trim();
                record_error = Some(format!("{} 录音中断：{}", children[i].0,
                    if detail.is_empty() { "未知错误" } else { detail }));
                break 'watch;
            }
        }
    }

    // 先收掉所有录音进程，再决定是否报错（保证 wav 头写完）
    for (_, c) in children.iter_mut() { stop_capture(c); }
    if let Some(e) = record_error { fail(&e); }
    println!("STOPPED");
    let _ = io::stdout().flush();
}

/// 解码任意音视频为 16 kHz 单声道 16 bit wav。
fn cmd_decode(args: &[String]) {
    if args.len() < 2 {
        fail("用法：tingo-recorder decode <输入> <输出.wav>");
    }
    let ff = ffmpeg();
    let out = command(&ff)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i", &args[0],
               "-vn", "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"])
        .arg(&args[1])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let Ok(out) = out else { fail("启动解码失败：找不到 ffmpeg") };
    if out.status.success() && Path::new(&args[1]).is_file() {
        ok();
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let detail = err.lines().last().unwrap_or("解码失败").trim();
    fail(&format!("解码失败：{}", detail));
}

/// 找 Edge 浏览器（用于无头 PDF 排版）。
fn find_edge() -> Option<PathBuf> {
    if let Ok(p) = env::var("TINGO_EDGE") {
        let p = PathBuf::from(p);
        if p.is_file() { return Some(p); }
    }
    let candidates = [
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ];
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// HTML → PDF（Edge 无头模式）。
fn cmd_pdf(args: &[String]) {
    if args.len() < 2 {
        fail("用法：tingo-recorder pdf <输入.html> <输出.pdf>");
    }
    let Some(edge) = find_edge() else {
        fail("导出 PDF 失败：找不到 Microsoft Edge 浏览器");
    };
    let input = Path::new(&args[0]);
    let abs = if input.is_absolute() { input.to_path_buf() }
              else { env::current_dir().unwrap_or_default().join(input) };
    // file:/// URL：把反斜杠换成正斜杠，空格做最小编码
    let file_url = format!("file:///{}", abs.to_string_lossy().replace('\\', "/").replace(' ', "%20"));
    let arg = format!("--print-to-pdf={}", args[1]);
    let out = command(&edge)
        .args(["--headless", "--disable-gpu", "--no-pdf-header-footer", "--no-first-run",
               "--disable-extensions", &arg, &file_url])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() && Path::new(&args[1]).is_file() => ok(),
        Ok(o) => fail(&format!("导出 PDF 失败：Edge 退出码 {}", o.status)),
        Err(e) => fail(&format!("导出 PDF 失败：{}", e)),
    }
}

/// PowerShell 单引号字符串转义。
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// HTML → DOCX（Windows 用 Word COM；没有 Word 时可装 pandoc 兜底）。
fn cmd_docx(args: &[String]) {
    if args.len() < 2 {
        fail("用法：tingo-recorder docx <输入.html> <输出.docx>");
    }
    let input = Path::new(&args[0]);
    let abs_in = if input.is_absolute() { input.to_path_buf() }
                 else { env::current_dir().unwrap_or_default().join(input) };
    let abs_out = Path::new(&args[1]);
    let out_abs = if abs_out.is_absolute() { abs_out.to_path_buf() }
                  else { env::current_dir().unwrap_or_default().join(abs_out) };
    let script = format!(
        "$ErrorActionPreference='Stop'; $word = New-Object -ComObject Word.Application; $word.Visible = $false; \
         try {{ $doc = $word.Documents.Open('{}', $false, $true); \
         $doc.SaveAs2('{}', 16); $doc.Close($false) }} \
         finally {{ $word.Quit() }}",
        ps_quote(&abs_in.to_string_lossy()),
        ps_quote(&out_abs.to_string_lossy()),
    );
    let out = command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() && out_abs.is_file() => ok(),
        Ok(o) => fail(&format!("导出 Word 失败：{}", String::from_utf8_lossy(&o.stderr).trim())),
        Err(e) => fail(&format!("导出 Word 失败：{}", e)),
    }
}

/// 移到回收站（PowerShell + VisualBasic FileIO，可恢复）。
fn cmd_trash(args: &[String]) {
    let Some(path) = args.first() else { fail("用法：tingo-recorder trash <路径>") };
    let abs = fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let script = format!(
        "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile('{}','OnlyErrorDialogs','SendToRecycleBin')",
        ps_quote(&abs.to_string_lossy())
    );
    let out = command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() => ok(),
        Ok(o) => fail(&format!("移到回收站失败：{}", String::from_utf8_lossy(&o.stderr).trim())),
        Err(e) => fail(&format!("移到回收站失败：{}", e)),
    }
}

/// 选目录（PowerShell + FolderBrowserDialog），打印 JSON。
fn cmd_choose_dir(args: &[String]) {
    let start = args.first().cloned().unwrap_or_default();
    let mut script = String::from(
        "Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.FolderBrowserDialog;",
    );
    if !start.is_empty() {
        script.push_str(&format!("$d.SelectedPath = '{}';", ps_quote(&start)));
    }
    script.push_str("if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { $d.SelectedPath }");
    let out = command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", &script])
        .output();
    let path = out.ok().and_then(|o| {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    });
    // 手搓 JSON，避免引入 serde
    let json = match path {
        Some(p) => format!("{{\"path\": \"{}\"}}", p.replace('\\', "\\\\").replace('"', "\\\"")),
        None => "{\"path\": null}".to_string(),
    };
    println!("{}", json);
    let _ = io::stdout().flush();
    std::process::exit(0);
}

/// Apple Speech 在 Windows 不可用（engine 侧会据此禁用该 ASR 选项）。
fn cmd_speech(args: &[String]) {
    let sub = args.first().map(String::as_str).unwrap_or("speech-status");
    if sub == "speech-status" {
        println!("{{\"available\": false, \"ready\": false, \"locale\": \"\"}}");
        let _ = io::stdout().flush();
        std::process::exit(0);
    }
    fail("Apple 系统识别在 Windows 上不可用，请改用本地模型或云端识别 / Apple speech is unavailable on Windows");
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        fail("用法：tingo-recorder <输出目录> [--no-mic] [--no-system]，或 decode / pdf / trash / choose-directory");
    }
    match args[0].as_str() {
        "decode" => cmd_decode(&args[1..]),
        "pdf" => cmd_pdf(&args[1..]),
        "docx" => cmd_docx(&args[1..]),
        "trash" => cmd_trash(&args[1..]),
        "choose-directory" => cmd_choose_dir(&args[1..]),
        "speech-status" | "speech-install" | "speech-transcribe" => cmd_speech(&args),
        _ => cmd_record(&args),
    }
}
